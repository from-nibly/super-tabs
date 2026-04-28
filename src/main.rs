use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};

use super_tabs_core::{
    CellState, PIPE_NAME, Schema, StyledText, UpdatePayload, WidthIndex, apply_default_style,
    clip_right_edge, decode_tab_name, encode_tab_name, fit_cell_to_width, parse_styled_string,
    solve_column_widths,
};
use zellij_tile::prelude::*;

#[derive(Clone)]
struct RenderConfig {
    overflow_above: String,
    overflow_below: String,
    indicator_active: String,
    indicator_fullscreen: String,
    indicator_sync: String,
    padding_top: usize,
    border: String,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            overflow_above: "  ^ +{count}".to_string(),
            overflow_below: "  v +{count}".to_string(),
            indicator_active: "*".to_string(),
            indicator_fullscreen: "Z".to_string(),
            indicator_sync: "S".to_string(),
            padding_top: 0,
            border: String::new(),
        }
    }
}

#[derive(Clone)]
struct TabRowState {
    cells: Vec<Option<CellState>>,
    last_mirrored_tab_name: Option<String>,
}

impl TabRowState {
    fn empty(schema: &Schema) -> Self {
        Self {
            cells: vec![None; schema.len()],
            last_mirrored_tab_name: None,
        }
    }
}

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    active_tab_idx: usize,
    mode_info: ModeInfo,
    pane_manifest: PaneManifest,
    render: RenderConfig,
    schema: Option<Schema>,
    rows_by_tab_position: BTreeMap<usize, TabRowState>,
    pane_to_tab_position: BTreeMap<u32, usize>,
    width_indexes: Vec<WidthIndex>,
    last_rows: usize,
    permissions_granted: bool,
    is_selectable: bool,
    pending_events: Vec<Event>,
    load_error: Option<String>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        if let Some(value) = configuration.get("overflow_above") {
            self.render.overflow_above = value.clone();
        }
        if let Some(value) = configuration.get("overflow_below") {
            self.render.overflow_below = value.clone();
        }
        if let Some(value) = configuration.get("indicator_active") {
            self.render.indicator_active = value.clone();
        }
        if let Some(value) = configuration.get("indicator_fullscreen") {
            self.render.indicator_fullscreen = value.clone();
        }
        if let Some(value) = configuration.get("indicator_sync") {
            self.render.indicator_sync = value.clone();
        }
        if let Some(value) = configuration.get("padding_top")
            && let Ok(padding_top) = value.parse::<usize>()
        {
            self.render.padding_top = padding_top;
        }
        if let Some(value) = configuration.get("border") {
            self.render.border = value.clone();
        } else if let Some(value) = configuration.get("border_char") {
            self.render.border = value.clone();
        }

        match Schema::from_config(&configuration) {
            Ok(schema) => {
                self.width_indexes = vec![WidthIndex::default(); schema.len()];
                self.schema = Some(schema);
            }
            Err(error) => {
                self.load_error = Some(format!("super-tabs config error: {error}"));
            }
        }

        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);

        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::ModeUpdate,
            EventType::Mouse,
            EventType::PermissionRequestResult,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;

        if let Event::PermissionRequestResult(status) = event {
            if status == PermissionStatus::Granted {
                self.permissions_granted = true;
                self.is_selectable = false;
                set_selectable(false);

                while !self.pending_events.is_empty() {
                    let cached_event = self.pending_events.remove(0);
                    self.update(cached_event);
                }
                should_render = true;
            }
            return should_render;
        }

        if !self.permissions_granted {
            self.pending_events.push(event);
            return false;
        }

        match event {
            Event::PermissionRequestResult(_) => {}
            Event::ModeUpdate(mode_info) => {
                if self.mode_info != mode_info {
                    should_render = true;
                }
                self.mode_info = mode_info;
            }
            Event::TabUpdate(tabs) => {
                let active_tab_index = tabs.iter().position(|tab| tab.active).unwrap_or(0);
                let active_tab_idx = active_tab_index + 1;
                if self.active_tab_idx != active_tab_idx || self.tabs != tabs {
                    should_render = true;
                }
                self.active_tab_idx = active_tab_idx;
                self.tabs = tabs;
                if self.reconcile_rows_with_tabs() {
                    should_render = true;
                }
            }
            Event::PaneUpdate(pane_manifest) => {
                self.pane_manifest = pane_manifest;
                self.rebuild_pane_lookup();
                should_render = true;
            }
            Event::Mouse(mouse_event) => match mouse_event {
                Mouse::LeftClick(row, _col) => {
                    if let Some(index) = self.get_tab_at_row(row as usize) {
                        switch_tab_to(index as u32);
                    }
                }
                Mouse::ScrollUp(_) => {
                    let previous_tab = max(self.active_tab_idx.saturating_sub(1), 1);
                    switch_tab_to(previous_tab as u32);
                }
                Mouse::ScrollDown(_) => {
                    let next_tab = min(self.active_tab_idx + 1, self.tabs.len());
                    switch_tab_to(next_tab as u32);
                }
                _ => {}
            },
            _ => {}
        }

        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        match pipe_message.name.as_str() {
            PIPE_NAME => self.handle_update_pipe(pipe_message.payload.as_deref()),
            "set_selectable" => {
                match pipe_message.payload.as_deref() {
                    Some("true") => {
                        self.is_selectable = true;
                        set_selectable(true);
                    }
                    Some("false") => {
                        self.is_selectable = false;
                        set_selectable(false);
                    }
                    _ => {}
                }
                false
            }
            "toggle_selectable" => {
                self.is_selectable = !self.is_selectable;
                set_selectable(self.is_selectable);
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        self.last_rows = rows;

        if !self.permissions_granted {
            self.render_message(
                rows,
                cols,
                "super-tabs: waiting for ReadApplicationState + ChangeApplicationState permissions",
            );
            return;
        }

        if let Some(error) = self.load_error.as_deref() {
            self.render_message(rows, cols, error);
            return;
        }

        if self.tabs.is_empty() {
            return;
        }

        self.render_vertical(rows, cols);
    }
}

impl State {
    fn get_focused_pane_title(&self, tab_position: usize) -> Option<String> {
        let panes = self.pane_manifest.panes.get(&tab_position)?;
        for pane in panes {
            if pane.is_focused && !pane.is_plugin {
                let title = pane.title.trim();
                if title.is_empty() || title.starts_with("Pane #") || title.starts_with("Tab #") {
                    return None;
                }
                return Some(title.to_string());
            }
        }
        None
    }

    fn rebuild_pane_lookup(&mut self) {
        self.pane_to_tab_position.clear();

        for (tab_position, panes) in &self.pane_manifest.panes {
            for pane in panes {
                self.pane_to_tab_position.insert(pane.id, *tab_position);
            }
        }
    }

    fn reconcile_rows_with_tabs(&mut self) -> bool {
        let Some(schema) = self.schema.clone() else {
            return false;
        };

        let live_positions: BTreeSet<usize> = self.tabs.iter().map(|tab| tab.position).collect();
        let stale_positions: Vec<usize> = self
            .rows_by_tab_position
            .keys()
            .copied()
            .filter(|position| !live_positions.contains(position))
            .collect();

        let mut changed = false;

        for stale_position in stale_positions {
            if let Some(row) = self.rows_by_tab_position.remove(&stale_position) {
                self.remove_row_widths(&row);
                changed = true;
            }
        }

        for tab in &self.tabs {
            if self.rows_by_tab_position.contains_key(&tab.position) {
                continue;
            }

            let Some(parsed_name) = decode_tab_name(&tab.name) else {
                continue;
            };

            let mut row = TabRowState::empty(&schema);
            let mut recognized_value = false;

            for (index, column) in schema.columns().iter().enumerate() {
                let plain_value = parsed_name.get(&column.name).cloned().unwrap_or_default();
                if plain_value.is_empty() {
                    continue;
                }

                recognized_value = true;
                let cell = CellState::from_plain_text(plain_value, &column.default_style);
                self.width_indexes[index].replace(None, cell.display_width());
                row.cells[index] = Some(cell);
            }

            if !recognized_value {
                continue;
            }

            row.last_mirrored_tab_name = Some(tab.name.clone());
            self.rows_by_tab_position.insert(tab.position, row);
            changed = true;
        }

        changed
    }

    fn remove_row_widths(&mut self, row: &TabRowState) {
        for (index, cell) in row.cells.iter().enumerate() {
            if let Some(cell) = cell {
                self.width_indexes[index].remove(cell.display_width());
            }
        }
    }

    fn handle_update_pipe(&mut self, payload: Option<&str>) -> bool {
        let Some(schema) = self.schema.clone() else {
            return false;
        };
        let Some(payload) = payload else {
            return false;
        };
        let Ok(payload) = UpdatePayload::parse(payload) else {
            return false;
        };
        let Some(&tab_position) = self.pane_to_tab_position.get(&payload.pane_id) else {
            return false;
        };

        let mut row = self
            .rows_by_tab_position
            .remove(&tab_position)
            .unwrap_or_else(|| TabRowState::empty(&schema));
        let mut changed = false;

        for (column_name, raw_value) in payload.updates {
            let Some(index) = schema.index_of(&column_name) else {
                continue;
            };

            let column = &schema.columns()[index];
            let cell = CellState::from_raw(raw_value, &column.default_style);
            let old_width = row.cells[index].as_ref().map(CellState::display_width);
            self.width_indexes[index].replace(old_width, cell.display_width());
            row.cells[index] = Some(cell);
            changed = true;
        }

        if !changed {
            self.rows_by_tab_position.insert(tab_position, row);
            return false;
        }

        let mirrored_name = encode_tab_name(schema.columns(), &row.cells);
        row.last_mirrored_tab_name = Some(mirrored_name.clone());
        self.rows_by_tab_position.insert(tab_position, row);
        self.rename_live_tab(tab_position, &mirrored_name);
        true
    }

    fn rename_live_tab(&self, tab_position: usize, mirrored_name: &str) {
        rename_tab(tab_position as u32, mirrored_name.to_string());
    }

    fn expand_overflow_format(&self, format: &str, count: usize) -> String {
        format.replace("{count}", &count.to_string())
    }

    fn build_indicator_text(&self, tab: &TabInfo) -> String {
        let mut indicators = String::new();
        if tab.is_fullscreen_active {
            indicators.push_str(&self.render.indicator_fullscreen);
        }
        if tab.is_sync_panes_active {
            indicators.push_str(&self.render.indicator_sync);
        }
        if tab.active {
            indicators.push_str(&self.render.indicator_active);
        }
        indicators
    }

    fn max_indicator_width(&self) -> usize {
        let mut indicator_text = String::new();
        indicator_text.push_str(&self.render.indicator_fullscreen);
        indicator_text.push_str(&self.render.indicator_sync);
        indicator_text.push_str(&self.render.indicator_active);
        parse_styled_string(&indicator_text).display_width()
    }

    fn render_tab_content(&self, tab: &TabInfo, cols: usize) -> StyledText {
        let Some(schema) = self.schema.as_ref() else {
            return StyledText::plain(tab.name.clone());
        };
        let Some(row) = self.rows_by_tab_position.get(&tab.position) else {
            return self.render_unmanaged_row(tab);
        };

        let border_width = parse_styled_string(&self.render.border).display_width();
        let indicator_width = self.max_indicator_width();
        let indicator_padding = usize::from(indicator_width > 0);
        let available_width = cols
            .saturating_sub(border_width)
            .saturating_sub(indicator_width + indicator_padding);
        let natural_widths: Vec<usize> = self.width_indexes.iter().map(WidthIndex::max).collect();
        let column_widths =
            solve_column_widths(schema.columns(), &natural_widths, available_width, 1);

        let mut content = StyledText::new();
        for (index, column) in schema.columns().iter().enumerate() {
            let styled = row.cells[index]
                .as_ref()
                .map(|cell| cell.styled_text.clone())
                .unwrap_or_else(|| apply_default_style(&column.default_style, ""));
            let fitted = fit_cell_to_width(&styled, column.resize_spec, column_widths[index]);
            let fitted_width = fitted.display_width();
            content.extend(fitted);

            let padding = column_widths[index].saturating_sub(fitted_width);
            if padding > 0 {
                content.push_plain(" ".repeat(padding));
            }

            if index + 1 < schema.len() {
                content.push_plain(" ");
            }
        }

        let indicators = self.build_indicator_text(tab);
        if !indicators.is_empty() {
            if !schema.is_empty() {
                content.push_plain(" ");
            }
            content.extend(parse_styled_string(&indicators));
        }

        clip_right_edge(&content, cols.saturating_sub(border_width))
    }

    fn render_unmanaged_row(&self, tab: &TabInfo) -> StyledText {
        let fallback_name = if !tab.name.trim().is_empty() && !tab.name.starts_with("Tab #") {
            tab.name.clone()
        } else {
            self.get_focused_pane_title(tab.position)
                .unwrap_or_else(|| format!("Tab {}", tab.position))
        };

        let mut content = StyledText::plain(fallback_name);
        let indicators = self.build_indicator_text(tab);
        if !indicators.is_empty() {
            if !content.plain_text().is_empty() {
                content.push_plain(" ");
            }
            content.extend(parse_styled_string(&indicators));
        }
        content
    }

    fn build_line(&self, content: &StyledText, cols: usize, is_selected: bool) -> String {
        let border = parse_styled_string(&self.render.border);
        let border_width = border.display_width();
        let effective_cols = cols.saturating_sub(border_width);
        let content = clip_right_edge(content, effective_cols);
        let content_width = content.display_width();
        let padding_needed = effective_cols.saturating_sub(content_width);
        let mut line = String::new();
        let has_fill = is_selected && content.segments.iter().any(|segment| segment.style.fill);

        if has_fill {
            line.push_str("\x1b[7m");

            for segment in &content.segments {
                let mut swapped_style = segment.style.clone();
                std::mem::swap(&mut swapped_style.fg, &mut swapped_style.bg);
                swapped_style.fill = false;

                if swapped_style.has_any_style() {
                    line.push_str("\x1b[0m\x1b[7m");
                    line.push_str(&swapped_style.to_ansi());
                }
                line.push_str(&segment.text);
            }

            if padding_needed > 0 {
                line.push_str(&" ".repeat(padding_needed));
            }

            line.push_str("\x1b[0m");
        } else {
            line.push_str(&content.to_ansi());
            if padding_needed > 0 {
                line.push_str(&" ".repeat(padding_needed));
            }
        }

        if border_width > 0 {
            line.push_str(&border.to_ansi());
        }

        line
    }

    fn build_empty_line(&self, cols: usize) -> String {
        let border = parse_styled_string(&self.render.border);
        let border_width = border.display_width();

        if border_width == 0 {
            return " ".repeat(cols);
        }

        let effective_cols = cols.saturating_sub(border_width);
        let mut line = " ".repeat(effective_cols);
        line.push_str(&border.to_ansi());
        line
    }

    fn render_message(&self, rows: usize, cols: usize, message: &str) {
        let mut lines = vec![self.build_line(&StyledText::plain(message), cols, false)];
        while lines.len() < rows {
            lines.push(self.build_empty_line(cols));
        }

        for (index, line) in lines.iter().enumerate() {
            if index + 1 < lines.len() {
                println!("{}\x1b[m", line);
            } else {
                print!("{}\x1b[m", line);
            }
        }
    }

    fn render_vertical(&mut self, rows: usize, cols: usize) {
        let top_padding = self.render.padding_top;
        let available_rows = rows.saturating_sub(top_padding);
        let tab_count = self.tabs.len();
        let active_index = self.active_tab_idx.saturating_sub(1);
        let (start_index, end_index, tabs_above, tabs_below) =
            calculate_visible_range(tab_count, available_rows, active_index);
        let mut lines: Vec<String> = Vec::with_capacity(rows);

        for _ in 0..top_padding {
            lines.push(self.build_empty_line(cols));
        }

        if tabs_above > 0 {
            let indicator_text =
                self.expand_overflow_format(&self.render.overflow_above, tabs_above);
            let styled = parse_styled_string(&indicator_text);
            lines.push(self.build_line(&styled, cols, false));
        }

        for index in start_index..end_index {
            if let Some(tab) = self.tabs.get(index) {
                let content = self.render_tab_content(tab, cols);
                lines.push(self.build_line(&content, cols, tab.active));
            }
        }

        if tabs_below > 0 {
            let indicator_text =
                self.expand_overflow_format(&self.render.overflow_below, tabs_below);
            let styled = parse_styled_string(&indicator_text);
            lines.push(self.build_line(&styled, cols, false));
        }

        while lines.len() < rows {
            lines.push(self.build_empty_line(cols));
        }

        for (index, line) in lines.iter().enumerate() {
            if index + 1 < lines.len() {
                println!("{}\x1b[m", line);
            } else {
                print!("{}\x1b[m", line);
            }
        }
    }

    fn get_tab_at_row(&self, row: usize) -> Option<usize> {
        if self.tabs.is_empty() || row < self.render.padding_top {
            return None;
        }

        let available_rows = self.last_rows.saturating_sub(self.render.padding_top);
        let tab_count = self.tabs.len();
        let active_index = self.active_tab_idx.saturating_sub(1);
        let (start_index, end_index, tabs_above, _tabs_below) =
            calculate_visible_range(tab_count, available_rows, active_index);

        let content_start_row = self.render.padding_top + usize::from(tabs_above > 0);

        if tabs_above > 0 && row == self.render.padding_top {
            let target = start_index.saturating_sub(1);
            return Some(target + 1);
        }

        let row_in_content = row.saturating_sub(content_start_row);
        let clicked_tab_index = start_index + row_in_content;

        if clicked_tab_index < end_index && clicked_tab_index < tab_count {
            return Some(clicked_tab_index + 1);
        }

        if row_in_content >= end_index.saturating_sub(start_index) {
            let target = end_index.min(tab_count.saturating_sub(1));
            return Some(target + 1);
        }

        None
    }
}

fn calculate_visible_range(
    tab_count: usize,
    available_rows: usize,
    active_index: usize,
) -> (usize, usize, usize, usize) {
    if tab_count == 0 {
        return (0, 0, 0, 0);
    }

    if tab_count <= available_rows {
        return (0, tab_count, 0, 0);
    }

    let max_visible = available_rows.saturating_sub(2);
    if max_visible == 0 {
        return (0, 0, tab_count, 0);
    }

    let mut start_index = active_index;
    let mut end_index = active_index + 1;
    let mut room_left = max_visible.saturating_sub(1);
    let mut alternate = false;

    while room_left > 0 {
        if !alternate && start_index > 0 {
            start_index -= 1;
            room_left -= 1;
        } else if alternate && end_index < tab_count {
            end_index += 1;
            room_left -= 1;
        } else if start_index > 0 {
            start_index -= 1;
            room_left -= 1;
        } else if end_index < tab_count {
            end_index += 1;
            room_left -= 1;
        } else {
            break;
        }
        alternate = !alternate;
    }

    (
        start_index,
        end_index,
        start_index,
        tab_count.saturating_sub(end_index),
    )
}
