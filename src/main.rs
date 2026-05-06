use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use super_tabs_core::{
    CellState, PIPE_NAME, SUPER_TAB_ID_KEY, Schema, StyledText, UpdatePayload, WidthIndex,
    apply_default_style, clip_right_edge, decode_super_tab_id, decode_tab_name,
    encode_tab_name_with_id, fit_cell_to_width, parse_styled_string, solve_column_widths,
};
use zellij_tile::prelude::*;

#[cfg(not(test))]
const STATE_DIR: &str = "/host/super-tabs";
const STATE_FILE_PREFIX: &str = "tab-";
const PENDING_FILE_PREFIX: &str = "pending-";

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

#[derive(Clone, PartialEq, Eq)]
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

#[derive(Debug, Serialize, Deserialize)]
struct PersistedTabState {
    version: u8,
    mirrored_name: String,
    cells: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingTabClaim {
    version: u8,
    observed_name: String,
    tab_id: String,
}

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    active_tab_idx: usize,
    mode_info: ModeInfo,
    pane_manifest: PaneManifest,
    render: RenderConfig,
    schema: Option<Schema>,
    plugin_id: Option<u32>,
    shared_state_host_folder: Option<PathBuf>,
    shared_state_mount_requested: bool,
    shared_state_ready: bool,
    debug_enabled: bool,
    rows_by_tab_id: BTreeMap<String, TabRowState>,
    pending_tab_id_by_position: BTreeMap<usize, String>,
    pane_to_tab_position: BTreeMap<u32, usize>,
    width_indexes: Vec<WidthIndex>,
    next_tab_id: u64,
    last_rows: usize,
    permissions_granted: bool,
    is_selectable: bool,
    pending_events: Vec<Event>,
    load_error: Option<String>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.plugin_id = Some(get_plugin_ids().plugin_id);
        if let Some(value) = configuration.get("debug") {
            self.debug_enabled = parse_bool_config(value);
        }

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
        match resolve_shared_state_host_folder(
            configuration.get("state_host_folder").map(String::as_str),
        ) {
            Ok(host_folder) => {
                self.shared_state_host_folder = Some(host_folder);
            }
            Err(error) => {
                self.load_error = Some(format!("super-tabs config error: {error}"));
            }
        }

        match Schema::from_config(&configuration) {
            Ok(schema) => {
                self.width_indexes = vec![WidthIndex::default(); schema.len()];
                self.debug_log(format!(
                    "load plugin_id={:?} columns={:?}",
                    self.plugin_id,
                    schema
                        .columns()
                        .iter()
                        .map(|column| column.name.as_str())
                        .collect::<Vec<_>>()
                ));
                self.schema = Some(schema);
            }
            Err(error) => {
                self.load_error = Some(format!("super-tabs config error: {error}"));
            }
        }

        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::FullHdAccess,
        ]);

        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::ModeUpdate,
            EventType::Mouse,
            EventType::PermissionRequestResult,
            EventType::Visible,
            EventType::HostFolderChanged,
            EventType::FailedToChangeHostFolder,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;

        if let Event::PermissionRequestResult(status) = event {
            self.debug_log(format!("permission_result={status:?}"));
            if status == PermissionStatus::Granted {
                self.permissions_granted = true;
                self.is_selectable = false;
                set_selectable(false);
                self.request_shared_state_mount();

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
                self.debug_log(format!(
                    "mode_update session={:?}",
                    self.mode_info.session_name
                ));
            }
            Event::Visible(is_visible) if is_visible && self.reconcile_rows_with_tabs() => {
                should_render = true;
            }
            Event::Visible(_) => {}
            Event::TabUpdate(tabs) => {
                self.debug_log(format!(
                    "tab_update {:?}",
                    tabs.iter()
                        .map(|tab| (tab.position, tab.active, tab.name.as_str()))
                        .collect::<Vec<_>>()
                ));
                let active_tab_index = tabs.iter().position(|tab| tab.active).unwrap_or(0);
                let active_tab_idx = active_tab_index + 1;
                if self.active_tab_idx != active_tab_idx || self.tabs != tabs {
                    should_render = true;
                }
                self.active_tab_idx = active_tab_idx;
                self.tabs = tabs;
                self.rebuild_pane_lookup();
                if self.reconcile_rows_with_tabs() {
                    should_render = true;
                }
            }
            Event::PaneUpdate(pane_manifest) => {
                self.pane_manifest = pane_manifest;
                self.rebuild_pane_lookup();
                self.debug_log(format!(
                    "pane_update lookup={:?}",
                    self.pane_to_tab_position
                ));
                should_render = true;
            }
            Event::HostFolderChanged(host_folder) => {
                self.shared_state_mount_requested = false;
                if self.shared_state_host_folder.as_ref() == Some(&host_folder) {
                    self.shared_state_ready = true;
                    self.debug_log(format!(
                        "shared_state_ready host_folder={}",
                        host_folder.display()
                    ));
                    if self.reconcile_rows_with_tabs() {
                        should_render = true;
                    }
                } else {
                    self.debug_log(format!(
                        "host_folder_changed ignored host_folder={}",
                        host_folder.display()
                    ));
                }
            }
            Event::FailedToChangeHostFolder(error) => {
                self.shared_state_mount_requested = false;
                self.shared_state_ready = false;
                let message = error.unwrap_or_else(|| "unknown error".to_string());
                self.load_error = Some(format!(
                    "super-tabs failed to mount shared state folder: {message}"
                ));
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
    fn debug_log(&self, message: impl AsRef<str>) {
        if !self.debug_enabled {
            return;
        }

        let plugin_id = self
            .plugin_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "?".to_string());
        let own_tab = self
            .own_plugin_tab_position()
            .map(|position| position.to_string())
            .unwrap_or_else(|| "?".to_string());
        let session = self
            .mode_info
            .session_name
            .clone()
            .unwrap_or_else(|| "?".to_string());
        eprintln!(
            "[super-tabs][session={session}][plugin={plugin_id}][own-tab={own_tab}] {}",
            message.as_ref()
        );
    }

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
        self.pane_to_tab_position = build_terminal_pane_lookup(&self.pane_manifest);
    }

    fn observe_tab_id(&mut self, tab_id: &str) {
        let Some(plugin_id) = self.plugin_id else {
            return;
        };

        let Some((minting_plugin_id, tab_id_number)) = parse_minted_super_tab_id(tab_id) else {
            return;
        };

        if minting_plugin_id != plugin_id {
            return;
        }

        self.next_tab_id = self.next_tab_id.max(tab_id_number.saturating_add(1));
    }

    fn mint_super_tab_id(&mut self) -> String {
        let plugin_id = self.plugin_id.unwrap_or(0);

        loop {
            let next_tab_id = self.next_tab_id.max(1);
            self.next_tab_id = next_tab_id.saturating_add(1);

            let candidate = format!("st-{plugin_id}-{next_tab_id}");
            let candidate_in_live_tabs = self
                .tabs
                .iter()
                .filter_map(|tab| decode_super_tab_id(&tab.name))
                .any(|tab_id| tab_id == candidate);
            let candidate_in_rows = self.rows_by_tab_id.contains_key(&candidate);
            let candidate_in_positions = self
                .pending_tab_id_by_position
                .values()
                .any(|tab_id| tab_id == &candidate);

            if !candidate_in_live_tabs && !candidate_in_rows && !candidate_in_positions {
                return candidate;
            }
        }
    }

    fn reconcile_rows_with_tabs(&mut self) -> bool {
        let Some(schema) = self.schema.clone() else {
            return false;
        };
        let allow_session_writes = self.is_session_write_leader();

        let tabs = self.tabs.clone();
        let mut next_rows = BTreeMap::new();
        let mut next_pending_tab_ids = BTreeMap::new();
        let mut next_width_indexes = vec![WidthIndex::default(); schema.len()];

        for tab in &tabs {
            let Some((mut tab_id, mut row, mut rename_pending)) =
                self.load_row_state_for_tab(tab, &schema, allow_session_writes)
            else {
                continue;
            };

            if next_rows.contains_key(&tab_id) {
                if allow_session_writes {
                    let duplicate_tab_id = tab_id;
                    tab_id = self.ensure_pending_tab_id_for_tab(tab);
                    row = TabRowState::empty(&schema);
                    let mirrored_name =
                        self.mirror_tab_row(tab.position, &tab_id, &mut row, &schema);
                    self.debug_log(format!(
                        "dedupe tab={} old_id={} new_id={} mirrored_name={}",
                        tab.position, duplicate_tab_id, tab_id, mirrored_name
                    ));
                    rename_pending = true;
                } else {
                    self.debug_log(format!(
                        "skip_dedupe tab={} id={} reason=follower",
                        tab.position, tab_id
                    ));
                    continue;
                }
            }

            if rename_pending {
                next_pending_tab_ids.insert(tab.position, tab_id.clone());
            }

            add_row_widths_to_indexes(&mut next_width_indexes, &row);
            next_rows.insert(tab_id, row);
        }

        let stale_pending_positions = self
            .pending_tab_id_by_position
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for pending_position in stale_pending_positions {
            if !tabs.iter().any(|tab| tab.position == pending_position) {
                self.pending_tab_id_by_position.remove(&pending_position);
            }
        }

        let changed = self.rows_by_tab_id != next_rows
            || self.pending_tab_id_by_position != next_pending_tab_ids
            || self.width_indexes != next_width_indexes;
        self.rows_by_tab_id = next_rows;
        self.pending_tab_id_by_position = next_pending_tab_ids;
        self.width_indexes = next_width_indexes;
        changed
    }

    fn load_row_state_for_tab(
        &mut self,
        tab: &TabInfo,
        schema: &Schema,
        allow_session_writes: bool,
    ) -> Option<(String, TabRowState, bool)> {
        if allow_session_writes {
            if let Some(tab_id) = self.refresh_pending_tab_id_for_tab(tab) {
                self.observe_tab_id(&tab_id);

                if let Some(row) = self.read_persisted_row_for_tab_id(&tab_id, schema) {
                    self.debug_log(format!(
                        "hydrate tab={} id={} source=pending-claim name={}",
                        tab.position, tab_id, tab.name
                    ));
                    return Some((tab_id, row, true));
                }

                let mut row = self.read_legacy_persisted_row(tab.position, tab, schema)?;
                let mirrored_name = self.mirror_tab_row(tab.position, &tab_id, &mut row, schema);
                self.debug_log(format!(
                    "resume_pending tab={} id={} mirrored_name={}",
                    tab.position, tab_id, mirrored_name
                ));
                return Some((tab_id, row, true));
            }
        }

        if let Some(tab_id) = self.live_tab_id_for_tab(tab) {
            self.observe_tab_id(&tab_id);
            if allow_session_writes {
                self.clear_pending_tab_claim_for_tab(tab);
            }

            if let Some(row) = self.read_persisted_row(&tab_id, tab, schema) {
                self.debug_log(format!(
                    "hydrate tab={} id={} source=cache name={}",
                    tab.position, tab_id, tab.name
                ));
                return Some((tab_id, row, false));
            }

            let row = self.build_row_state_from_tab_name(tab, schema);
            if row.is_some() {
                self.debug_log(format!(
                    "hydrate tab={} id={} source=tab-name name={}",
                    tab.position, tab_id, tab.name
                ));
            }
            return row.map(|row| (tab_id, row, false));
        }

        if !allow_session_writes {
            return None;
        }

        let mut row = self.read_legacy_persisted_row(tab.position, tab, schema)?;
        let tab_id = self.ensure_pending_tab_id_for_tab(tab);
        self.observe_tab_id(&tab_id);
        let mirrored_name = self.mirror_tab_row(tab.position, &tab_id, &mut row, schema);
        self.debug_log(format!(
            "migrate tab={} id={} mirrored_name={}",
            tab.position, tab_id, mirrored_name
        ));
        Some((tab_id, row, true))
    }

    fn read_legacy_persisted_row(
        &self,
        tab_position: usize,
        tab: &TabInfo,
        schema: &Schema,
    ) -> Option<TabRowState> {
        self.read_persisted_row_by_key(&tab_position.to_string(), tab_position, tab, schema)
            .or_else(|| self.build_row_state_from_tab_name(tab, schema))
    }

    fn load_cached_row_for_identity(
        &mut self,
        tab: &TabInfo,
        tab_id: &str,
        allow_live_name_mismatch: bool,
    ) -> Option<TabRowState> {
        let cached_row = self.rows_by_tab_id.remove(tab_id)?;

        if cached_row.last_mirrored_tab_name.as_deref() == Some(tab.name.as_str()) {
            return Some(cached_row);
        }

        if allow_live_name_mismatch {
            return Some(cached_row);
        }

        None
    }

    fn mirror_tab_row(
        &self,
        tab_position: usize,
        tab_id: &str,
        row: &mut TabRowState,
        schema: &Schema,
    ) -> String {
        let mirrored_name = encode_tab_name_with_id(schema.columns(), &row.cells, Some(tab_id));
        row.last_mirrored_tab_name = Some(mirrored_name.clone());
        self.write_persisted_row(tab_id, tab_position, &mirrored_name, row, schema);
        self.rename_live_tab(tab_position, &mirrored_name);
        mirrored_name
    }

    fn clear_pending_tab_claim_for_tab(&mut self, tab: &TabInfo) {
        self.pending_tab_id_by_position.remove(&tab.position);

        let Some(session_key) = self.session_key() else {
            return;
        };

        let pending_claim_key = self.pending_tab_claim_key_for_tab(tab);
        let path = pending_tab_claim_path(session_key.as_str(), &pending_claim_key);
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != ErrorKind::NotFound
        {
            eprintln!(
                "super-tabs: failed to clear pending tab claim for position {}: {error}",
                tab.position
            );
        }
    }

    fn pending_tab_claim_key_for_tab(&self, tab: &TabInfo) -> String {
        let mut terminal_pane_ids = self
            .pane_manifest
            .panes
            .get(&tab.position)
            .into_iter()
            .flatten()
            .filter(|pane| !pane.is_plugin)
            .map(|pane| pane.id)
            .collect::<Vec<_>>();
        terminal_pane_ids.sort_unstable();

        if terminal_pane_ids.is_empty() {
            return format!("position-{}", tab.position);
        }

        format!(
            "panes-{}",
            terminal_pane_ids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("_")
        )
    }

    fn refresh_pending_tab_id_for_tab(&mut self, tab: &TabInfo) -> Option<String> {
        let Some(session_key) = self.session_key() else {
            self.pending_tab_id_by_position.remove(&tab.position);
            return None;
        };
        let pending_claim_key = self.pending_tab_claim_key_for_tab(tab);
        let Some(claim) = read_pending_tab_claim(session_key.as_str(), &pending_claim_key) else {
            self.pending_tab_id_by_position.remove(&tab.position);
            return None;
        };

        if claim.version == 1 && claim.observed_name == tab.name && !claim.tab_id.is_empty() {
            self.pending_tab_id_by_position
                .insert(tab.position, claim.tab_id.clone());
            return Some(claim.tab_id);
        }

        self.clear_pending_tab_claim_for_tab(tab);
        None
    }

    fn ensure_pending_tab_id_for_tab(&mut self, tab: &TabInfo) -> String {
        if let Some(tab_id) = self.refresh_pending_tab_id_for_tab(tab) {
            return tab_id;
        }

        let Some(session_key) = self.session_key() else {
            let tab_id = self.mint_super_tab_id();
            self.pending_tab_id_by_position
                .insert(tab.position, tab_id.clone());
            return tab_id;
        };

        loop {
            let tab_id = self.mint_super_tab_id();
            let claim = PendingTabClaim {
                version: 1,
                observed_name: tab.name.clone(),
                tab_id: tab_id.clone(),
            };

            match create_pending_tab_claim(
                session_key.as_str(),
                &self.pending_tab_claim_key_for_tab(tab),
                self.plugin_id.unwrap_or(0),
                &claim,
            ) {
                Ok(true) => {
                    self.pending_tab_id_by_position
                        .insert(tab.position, tab_id.clone());
                    return tab_id;
                }
                Ok(false) => {
                    if let Some(tab_id) = self.refresh_pending_tab_id_for_tab(tab) {
                        return tab_id;
                    }
                }
                Err(error) => {
                    eprintln!(
                        "super-tabs: failed to create pending tab claim for position {}: {}",
                        tab.position, error
                    );
                    self.pending_tab_id_by_position
                        .insert(tab.position, tab_id.clone());
                    return tab_id;
                }
            }
        }
    }

    fn live_tab_id_for_tab(&self, tab: &TabInfo) -> Option<String> {
        decode_super_tab_id(&tab.name)
    }

    fn resolved_tab_id_for_tab(&self, tab: &TabInfo) -> Option<String> {
        self.pending_tab_id_by_position
            .get(&tab.position)
            .cloned()
            .or_else(|| self.live_tab_id_for_tab(tab))
    }

    fn tab_id_for_position(&self, tab_position: usize) -> Option<String> {
        self.tabs
            .iter()
            .find(|tab| tab.position == tab_position)
            .and_then(|tab| self.resolved_tab_id_for_tab(tab))
    }

    fn managed_tab_fallback_name(&self, tab: &TabInfo) -> Option<String> {
        let trimmed_name = tab.name.trim();
        if trimmed_name.is_empty() || trimmed_name.starts_with("Tab #") {
            return None;
        }

        let Some(parsed_name) = decode_tab_name(&tab.name) else {
            return Some(tab.name.clone());
        };
        let has_super_tab_id = parsed_name.contains_key(SUPER_TAB_ID_KEY);
        let has_other_values = parsed_name
            .iter()
            .any(|(key, value)| key != SUPER_TAB_ID_KEY && !value.is_empty());

        if has_super_tab_id && !has_other_values {
            return None;
        }

        Some(tab.name.clone())
    }

    fn build_row_state_from_tab_name(&self, tab: &TabInfo, schema: &Schema) -> Option<TabRowState> {
        let parsed_name = decode_tab_name(&tab.name)?;
        let mut row = TabRowState::empty(schema);
        let mut recognized_value = false;

        for (index, column) in schema.columns().iter().enumerate() {
            let plain_value = parsed_name.get(&column.name).cloned().unwrap_or_default();
            if plain_value.is_empty() {
                continue;
            }

            recognized_value = true;
            row.cells[index] = Some(CellState::from_plain_text(
                plain_value,
                &column.default_style,
            ));
        }

        if !recognized_value {
            return None;
        }

        row.last_mirrored_tab_name = Some(tab.name.clone());
        Some(row)
    }

    fn read_persisted_row(
        &self,
        tab_id: &str,
        tab: &TabInfo,
        schema: &Schema,
    ) -> Option<TabRowState> {
        self.read_persisted_row_by_key(tab_id, tab.position, tab, schema)
    }

    fn read_persisted_row_for_tab_id(&self, tab_id: &str, schema: &Schema) -> Option<TabRowState> {
        let session_key = self.session_key()?;
        let persisted = read_persisted_tab_state(session_key.as_str(), tab_id)?;
        row_state_from_persisted(&persisted, schema)
    }

    fn read_persisted_row_by_key(
        &self,
        tab_key: &str,
        tab_position: usize,
        tab: &TabInfo,
        schema: &Schema,
    ) -> Option<TabRowState> {
        let session_key = self.session_key()?;
        let path = persisted_tab_state_path(session_key.as_str(), tab_key);
        let persisted = read_persisted_tab_state(session_key.as_str(), tab_key)?;
        if persisted.version != 1 || persisted.mirrored_name != tab.name {
            self.debug_log(format!(
                "cache_miss tab={} key={} path={} mirrored_name={} live_name={}",
                tab_position,
                tab_key,
                path.display(),
                persisted.mirrored_name,
                tab.name
            ));
            return None;
        }

        row_state_from_persisted(&persisted, schema)
    }

    fn own_plugin_tab_position(&self) -> Option<usize> {
        let plugin_id = self.plugin_id?;
        self.pane_manifest
            .panes
            .iter()
            .find_map(|(tab_position, panes)| {
                panes
                    .iter()
                    .any(|pane| pane.is_plugin && pane.id == plugin_id)
                    .then_some(*tab_position)
            })
    }

    fn write_persisted_row(
        &self,
        tab_id: &str,
        tab_position: usize,
        mirrored_name: &str,
        row: &TabRowState,
        schema: &Schema,
    ) {
        let mut cells = BTreeMap::new();
        for (index, column) in schema.columns().iter().enumerate() {
            let Some(cell) = row.cells[index].as_ref() else {
                continue;
            };
            if !cell.raw_input.is_empty() {
                cells.insert(column.name.clone(), cell.raw_input.clone());
            }
        }

        let persisted = PersistedTabState {
            version: 1,
            mirrored_name: mirrored_name.to_string(),
            cells,
        };

        let Some(session_key) = self.session_key() else {
            self.debug_log(format!(
                "skip_persist tab={} id={} reason=no-session-key",
                tab_position, tab_id
            ));
            return;
        };

        let path = persisted_tab_state_path(session_key.as_str(), tab_id);

        if let Err(error) = write_persisted_tab_state(
            session_key.as_str(),
            tab_id,
            self.plugin_id.unwrap_or(0),
            &persisted,
        ) {
            eprintln!(
                "super-tabs: failed to persist tab state for position {tab_position} ({tab_id}): {error}"
            );
        } else {
            self.debug_log(format!(
                "persist tab={} id={} path={} mirrored_name={}",
                tab_position,
                tab_id,
                path.display(),
                mirrored_name
            ));
        }
    }

    fn handle_update_pipe(&mut self, payload: Option<&str>) -> bool {
        let Some(schema) = self.schema.clone() else {
            return false;
        };
        let allow_session_writes = self.is_session_write_leader();
        let Some(payload) = payload else {
            return false;
        };
        let Ok(payload) = UpdatePayload::parse(payload) else {
            self.debug_log("pipe ignored invalid payload");
            return false;
        };
        let resolved_tab_position = self.pane_to_tab_position.get(&payload.pane_id).copied();
        let Some(tab_position) = resolved_tab_position else {
            return false;
        };
        let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.position == tab_position)
            .cloned()
        else {
            return false;
        };
        let live_tab_id = self.live_tab_id_for_tab(&tab);
        let (tab_id, rename_pending, allow_legacy_position_fallback) = if allow_session_writes {
            let resolved_tab_id = self
                .refresh_pending_tab_id_for_tab(&tab)
                .or_else(|| live_tab_id.clone());
            let allow_legacy_position_fallback = resolved_tab_id.is_none();
            let tab_id = match resolved_tab_id {
                Some(tab_id) => {
                    self.observe_tab_id(&tab_id);
                    tab_id
                }
                None => self.ensure_pending_tab_id_for_tab(&tab),
            };
            let rename_pending = live_tab_id.as_deref() != Some(tab_id.as_str());
            (tab_id, rename_pending, allow_legacy_position_fallback)
        } else {
            let Some(tab_id) = live_tab_id.clone() else {
                self.debug_log(format!(
                    "pipe ignored follower unmanaged tab pane_id={} tab={}",
                    payload.pane_id, tab.position
                ));
                return false;
            };
            (tab_id, false, false)
        };
        self.debug_log(format!(
            "pipe pane_id={} resolved_tab={resolved_tab_position:?} tab_id={} updates={:?}",
            payload.pane_id, tab_id, payload.updates
        ));

        let mut row = self.row_state_for_update(
            &tab,
            &tab_id,
            &schema,
            rename_pending,
            allow_legacy_position_fallback,
        );
        let changed = apply_updates_to_row(&mut row, &schema, payload.updates);
        let mirrored_name =
            encode_tab_name_with_id(schema.columns(), &row.cells, Some(tab_id.as_str()));

        if !changed && row.last_mirrored_tab_name.as_deref() == Some(mirrored_name.as_str()) {
            self.rows_by_tab_id.insert(tab_id.clone(), row);
            if allow_session_writes && rename_pending {
                self.pending_tab_id_by_position.insert(tab_position, tab_id);
            } else if allow_session_writes {
                self.clear_pending_tab_claim_for_tab(&tab);
            }
            return false;
        }

        self.rows_by_tab_id.insert(tab_id.clone(), row.clone());
        if allow_session_writes && rename_pending {
            self.pending_tab_id_by_position
                .insert(tab_position, tab_id.clone());
        } else if allow_session_writes {
            self.clear_pending_tab_claim_for_tab(&tab);
        }

        if !allow_session_writes {
            self.recompute_width_indexes();
            return changed;
        }

        let mirrored_name = self.mirror_tab_row(tab_position, &tab_id, &mut row, &schema);
        self.rows_by_tab_id.insert(tab_id.clone(), row);
        self.debug_log(format!(
            "rename target_tab={} id={} mirrored_name={}",
            tab_position, tab_id, mirrored_name
        ));

        self.recompute_width_indexes();
        true
    }

    fn row_state_for_update(
        &mut self,
        tab: &TabInfo,
        tab_id: &str,
        schema: &Schema,
        allow_live_name_mismatch: bool,
        allow_legacy_position_fallback: bool,
    ) -> TabRowState {
        if let Some(row) = self.load_cached_row_for_identity(tab, tab_id, allow_live_name_mismatch)
        {
            return row;
        }

        if allow_live_name_mismatch {
            if let Some(row) = self.read_persisted_row_for_tab_id(tab_id, schema) {
                return row;
            }
        } else if let Some(row) = self.read_persisted_row(tab_id, tab, schema) {
            return row;
        }

        if allow_legacy_position_fallback
            && let Some(row) = self.read_legacy_persisted_row(tab.position, tab, schema)
        {
            return row;
        }

        self.build_row_state_from_tab_name(tab, schema)
            .unwrap_or_else(|| TabRowState::empty(schema))
    }

    fn recompute_width_indexes(&mut self) {
        let Some(schema) = self.schema.as_ref() else {
            return;
        };

        let mut width_indexes = vec![WidthIndex::default(); schema.len()];
        for row in self.rows_by_tab_id.values() {
            add_row_widths_to_indexes(&mut width_indexes, row);
        }
        self.width_indexes = width_indexes;
    }

    fn request_shared_state_mount(&mut self) {
        if self.shared_state_ready || self.shared_state_mount_requested {
            return;
        }

        let Some(host_folder) = self.shared_state_host_folder.clone() else {
            self.load_error = Some(
                "super-tabs: no shared state folder configured and HOME/XDG_DATA_HOME unavailable"
                    .to_string(),
            );
            return;
        };

        self.shared_state_mount_requested = true;
        self.debug_log(format!(
            "mount_shared_state host_folder={}",
            host_folder.display()
        ));
        change_host_folder(host_folder);
    }

    fn session_key(&self) -> Option<String> {
        if !self.shared_state_ready {
            return None;
        }

        self.mode_info
            .session_name
            .as_ref()
            .map(|session_name| sanitize_session_key(session_name))
            .filter(|session_name| !session_name.is_empty())
    }

    // Every plugin instance in the session observes the same pane manifest, so
    // elect a single writer deterministically from the shared plugin_url + id.
    fn is_session_write_leader(&self) -> bool {
        if !self.shared_state_ready {
            return false;
        }

        let Some(plugin_id) = self.plugin_id else {
            return true;
        };

        self.session_write_leader_plugin_id() == Some(plugin_id)
    }

    fn session_write_leader_plugin_id(&self) -> Option<u32> {
        let own_plugin_id = self.plugin_id?;
        let own_plugin_url = self
            .pane_manifest
            .panes
            .values()
            .flatten()
            .find(|pane| pane.is_plugin && pane.id == own_plugin_id)?
            .plugin_url
            .as_deref()?;

        self.pane_manifest
            .panes
            .values()
            .flatten()
            .filter(|pane| pane.is_plugin && pane.plugin_url.as_deref() == Some(own_plugin_url))
            .map(|pane| pane.id)
            .min()
    }

    fn rename_live_tab(&self, tab_position: usize, mirrored_name: &str) {
        rename_tab((tab_position + 1) as u32, mirrored_name.to_string());
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
        let Some(tab_id) = self.tab_id_for_position(tab.position) else {
            return self.render_unmanaged_row(tab);
        };
        let Some(row) = self.rows_by_tab_id.get(&tab_id) else {
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
        let fallback_name = self.managed_tab_fallback_name(tab).unwrap_or_else(|| {
            self.get_focused_pane_title(tab.position)
                .unwrap_or_else(|| format!("Tab {}", tab.position))
        });

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

fn add_row_widths_to_indexes(width_indexes: &mut [WidthIndex], row: &TabRowState) {
    for (index, cell) in row.cells.iter().enumerate() {
        if let Some(cell) = cell {
            width_indexes[index].replace(None, cell.display_width());
        }
    }
}

fn build_terminal_pane_lookup(pane_manifest: &PaneManifest) -> BTreeMap<u32, usize> {
    let mut pane_to_tab_position = BTreeMap::new();

    for (tab_position, panes) in &pane_manifest.panes {
        for pane in panes {
            if !pane.is_plugin {
                pane_to_tab_position.insert(pane.id, *tab_position);
            }
        }
    }

    pane_to_tab_position
}

#[cfg(test)]
fn state_dir() -> PathBuf {
    std::env::temp_dir().join("super-tabs-tests")
}

#[cfg(not(test))]
fn state_dir() -> PathBuf {
    PathBuf::from(STATE_DIR)
}

fn persisted_tab_state_path(session_key: &str, tab_key: &str) -> PathBuf {
    state_dir()
        .join(session_key)
        .join(format!("{STATE_FILE_PREFIX}{tab_key}.json"))
}

fn pending_tab_claim_path(session_key: &str, pending_claim_key: &str) -> PathBuf {
    state_dir()
        .join(session_key)
        .join(format!("{PENDING_FILE_PREFIX}{pending_claim_key}.json"))
}

fn read_persisted_tab_state(session_key: &str, tab_key: &str) -> Option<PersistedTabState> {
    let path = persisted_tab_state_path(session_key, tab_key);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_pending_tab_claim(session_key: &str, pending_claim_key: &str) -> Option<PendingTabClaim> {
    let path = pending_tab_claim_path(session_key, pending_claim_key);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_persisted_tab_state(
    session_key: &str,
    tab_key: &str,
    plugin_id: u32,
    persisted: &PersistedTabState,
) -> Result<(), String> {
    let path = persisted_tab_state_path(session_key, tab_key);
    let temp_path = path.with_extension(format!("json.tmp-{plugin_id}"));
    let content = serde_json::to_vec(persisted)
        .map_err(|error| format!("serialize persisted state: {error}"))?;

    let parent_dir = path
        .parent()
        .ok_or_else(|| "missing persisted state parent directory".to_string())?;
    fs::create_dir_all(parent_dir).map_err(|error| format!("create state dir: {error}"))?;

    let mut file =
        File::create(&temp_path).map_err(|error| format!("create temp file: {error}"))?;
    file.write_all(&content)
        .map_err(|error| format!("write temp file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync temp file: {error}"))?;
    drop(file);

    fs::rename(&temp_path, &path).map_err(|error| format!("rename temp file: {error}"))?;
    sync_parent_dir(&path);
    Ok(())
}

fn create_pending_tab_claim(
    session_key: &str,
    pending_claim_key: &str,
    plugin_id: u32,
    claim: &PendingTabClaim,
) -> Result<bool, String> {
    let path = pending_tab_claim_path(session_key, pending_claim_key);
    let temp_path = path.with_extension(format!("json.tmp-{plugin_id}"));
    let content =
        serde_json::to_vec(claim).map_err(|error| format!("serialize pending claim: {error}"))?;

    let parent_dir = path
        .parent()
        .ok_or_else(|| "missing pending claim parent directory".to_string())?;
    fs::create_dir_all(parent_dir).map_err(|error| format!("create state dir: {error}"))?;

    let mut file =
        File::create(&temp_path).map_err(|error| format!("create temp claim file: {error}"))?;
    file.write_all(&content)
        .map_err(|error| format!("write temp claim file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync temp claim file: {error}"))?;
    drop(file);

    fs::rename(&temp_path, &path).map_err(|error| format!("rename temp claim file: {error}"))?;
    sync_parent_dir(&path);
    Ok(true)
}

fn sync_parent_dir(path: &PathBuf) {
    let Some(parent_dir) = path.parent() else {
        return;
    };

    if let Ok(parent_dir) = File::open(parent_dir) {
        let _ = parent_dir.sync_all();
    }
}

fn row_state_from_persisted(persisted: &PersistedTabState, schema: &Schema) -> Option<TabRowState> {
    if persisted.version != 1 {
        return None;
    }

    let mut row = TabRowState::empty(schema);
    for (index, column) in schema.columns().iter().enumerate() {
        let Some(raw_input) = persisted.cells.get(&column.name) else {
            continue;
        };
        row.cells[index] = Some(CellState::from_raw(
            raw_input.clone(),
            &column.default_style,
        ));
    }
    row.last_mirrored_tab_name = Some(persisted.mirrored_name.clone());
    Some(row)
}

fn parse_minted_super_tab_id(tab_id: &str) -> Option<(u32, u64)> {
    let suffix = tab_id.strip_prefix("st-")?;
    let mut parts = suffix.split('-');
    let plugin_id = parts.next()?.parse::<u32>().ok()?;
    let tab_id_number = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((plugin_id, tab_id_number))
}

fn apply_updates_to_row(
    row: &mut TabRowState,
    schema: &Schema,
    updates: BTreeMap<String, String>,
) -> bool {
    let mut changed = false;

    for (column_name, raw_value) in updates {
        let Some(index) = schema.index_of(&column_name) else {
            continue;
        };

        let current_value = row.cells[index]
            .as_ref()
            .map(|cell| cell.raw_input.as_str())
            .unwrap_or("");
        if current_value == raw_value {
            continue;
        }

        let column = &schema.columns()[index];
        row.cells[index] = Some(CellState::from_raw(raw_value, &column.default_style));
        changed = true;
    }

    changed
}

fn sanitize_session_key(session_name: &str) -> String {
    session_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_bool_config(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn resolve_shared_state_host_folder(configured: Option<&str>) -> Result<PathBuf, String> {
    let path = match configured.map(str::trim).filter(|value| !value.is_empty()) {
        Some(configured) => expand_home_path(configured)?,
        None => default_shared_state_host_folder()?,
    };

    if !path.is_absolute() {
        return Err(format!(
            "state_host_folder must be an absolute path, got {}",
            path.display()
        ));
    }

    Ok(path)
}

fn default_shared_state_host_folder() -> Result<PathBuf, String> {
    if let Some(xdg_data_home) = std::env::var_os("XDG_DATA_HOME")
        && !xdg_data_home.is_empty()
    {
        return Ok(PathBuf::from(xdg_data_home));
    }

    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return Err("state_host_folder is required when HOME is unavailable".to_string());
    };

    Ok(PathBuf::from(home).join(".local").join("share"))
}

fn expand_home_path(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
            return Err("cannot expand ~ because HOME is unavailable".to_string());
        };
        return Ok(PathBuf::from(home));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
            return Err("cannot expand ~/ because HOME is unavailable".to_string());
        };
        return Ok(PathBuf::from(home).join(rest));
    }

    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_schema(columns: &str) -> Schema {
        let mut config = BTreeMap::new();
        config.insert("columns".to_string(), columns.to_string());

        for column in columns.split(',') {
            config.insert(format!("column_{column}"), "resize=resize".to_string());
        }

        Schema::from_config(&config).unwrap()
    }

    fn test_plugin_pane(plugin_id: u32) -> PaneInfo {
        test_plugin_pane_with_url(plugin_id, "file:/plugin.wasm")
    }

    fn test_plugin_pane_with_url(plugin_id: u32, plugin_url: &str) -> PaneInfo {
        PaneInfo {
            id: plugin_id,
            is_plugin: true,
            title: "super-tabs".to_string(),
            plugin_url: Some(plugin_url.to_string()),
            is_selectable: false,
            ..Default::default()
        }
    }

    fn test_terminal_pane(pane_id: u32) -> PaneInfo {
        PaneInfo {
            id: pane_id,
            is_plugin: false,
            title: format!("terminal-{pane_id}"),
            is_selectable: true,
            ..Default::default()
        }
    }

    fn test_pane_manifest(plugin_ids: &[u32]) -> PaneManifest {
        PaneManifest {
            panes: HashMap::from_iter(plugin_ids.iter().enumerate().map(
                |(tab_position, plugin_id)| (tab_position, vec![test_plugin_pane(*plugin_id)]),
            )),
            ..Default::default()
        }
    }

    fn test_pipe_state(plugin_id: u32, session_name: &str, running_plugin_ids: &[u32]) -> State {
        State {
            plugin_id: Some(plugin_id),
            shared_state_ready: true,
            mode_info: ModeInfo {
                session_name: Some(session_name.to_string()),
                ..Default::default()
            },
            pane_manifest: test_pane_manifest(running_plugin_ids),
            ..Default::default()
        }
    }

    #[test]
    fn pane_lookup_ignores_plugin_id_collisions() {
        let pane_manifest = PaneManifest {
            panes: std::collections::HashMap::from([
                (
                    0,
                    vec![
                        PaneInfo {
                            id: 0,
                            is_plugin: false,
                            is_focused: false,
                            is_fullscreen: false,
                            is_floating: false,
                            is_suppressed: false,
                            title: "tab0-shell".to_string(),
                            exited: false,
                            exit_status: None,
                            is_held: false,
                            pane_x: 0,
                            pane_content_x: 0,
                            pane_y: 0,
                            pane_content_y: 0,
                            pane_rows: 0,
                            pane_content_rows: 0,
                            pane_columns: 0,
                            pane_content_columns: 0,
                            cursor_coordinates_in_pane: None,
                            terminal_command: None,
                            plugin_url: None,
                            is_selectable: true,
                            index_in_pane_group: BTreeMap::new(),
                        },
                        PaneInfo {
                            id: 1,
                            is_plugin: true,
                            is_focused: false,
                            is_fullscreen: false,
                            is_floating: false,
                            is_suppressed: false,
                            title: "super-tabs".to_string(),
                            exited: false,
                            exit_status: None,
                            is_held: false,
                            pane_x: 0,
                            pane_content_x: 0,
                            pane_y: 0,
                            pane_content_y: 0,
                            pane_rows: 0,
                            pane_content_rows: 0,
                            pane_columns: 0,
                            pane_content_columns: 0,
                            cursor_coordinates_in_pane: None,
                            terminal_command: None,
                            plugin_url: Some("file:/plugin.wasm".to_string()),
                            is_selectable: false,
                            index_in_pane_group: BTreeMap::new(),
                        },
                    ],
                ),
                (
                    1,
                    vec![PaneInfo {
                        id: 1,
                        is_plugin: false,
                        is_focused: false,
                        is_fullscreen: false,
                        is_floating: false,
                        is_suppressed: false,
                        title: "tab1-shell".to_string(),
                        exited: false,
                        exit_status: None,
                        is_held: false,
                        pane_x: 0,
                        pane_content_x: 0,
                        pane_y: 0,
                        pane_content_y: 0,
                        pane_rows: 0,
                        pane_content_rows: 0,
                        pane_columns: 0,
                        pane_content_columns: 0,
                        cursor_coordinates_in_pane: None,
                        terminal_command: None,
                        plugin_url: None,
                        is_selectable: true,
                        index_in_pane_group: BTreeMap::new(),
                    }],
                ),
            ]),
        };

        let pane_to_tab_position = build_terminal_pane_lookup(&pane_manifest);

        assert_eq!(pane_to_tab_position.get(&0), Some(&0));
        assert_eq!(pane_to_tab_position.get(&1), Some(&1));
    }

    #[test]
    fn apply_updates_to_row_skips_unchanged_values() {
        let schema = test_schema("branch,status");

        let mut row = TabRowState::empty(&schema);
        row.cells[0] = Some(CellState::from_raw(
            "main",
            &schema.columns()[0].default_style,
        ));
        row.cells[1] = Some(CellState::from_raw(
            "dirty",
            &schema.columns()[1].default_style,
        ));

        let changed = apply_updates_to_row(
            &mut row,
            &schema,
            BTreeMap::from([
                ("branch".to_string(), "main".to_string()),
                ("status".to_string(), "dirty".to_string()),
            ]),
        );

        assert!(!changed);
        assert_eq!(row.cells[0].as_ref().unwrap().raw_input, "main");
        assert_eq!(row.cells[1].as_ref().unwrap().raw_input, "dirty");
    }

    #[test]
    fn resolved_tab_id_for_tab_prefers_pending_id_while_rename_is_pending() {
        let live_tab = TabInfo {
            position: 7,
            name: "__super_tabs_id=\"st-9\" | status=\"IDLE\"".to_string(),
            ..Default::default()
        };

        let state = State {
            tabs: vec![live_tab.clone()],
            pending_tab_id_by_position: BTreeMap::from([(7, "st-4".to_string())]),
            ..Default::default()
        };

        assert_eq!(
            state.resolved_tab_id_for_tab(&live_tab).as_deref(),
            Some("st-4")
        );
    }

    #[test]
    fn resolved_tab_id_for_tab_uses_pending_id_while_rename_is_pending() {
        let pending_tab = TabInfo {
            position: 16,
            name: "cue-ops".to_string(),
            ..Default::default()
        };

        let state = State {
            tabs: vec![pending_tab.clone()],
            pending_tab_id_by_position: BTreeMap::from([(16, "st-17".to_string())]),
            ..Default::default()
        };

        assert_eq!(
            state.resolved_tab_id_for_tab(&pending_tab).as_deref(),
            Some("st-17")
        );
    }

    #[test]
    fn row_state_for_update_discards_stale_cached_row_when_tab_name_changed() {
        let schema = test_schema("status");
        let stale_tab = TabInfo {
            position: 14,
            name: "__super_tabs_id=\"st-14\" | status=\"IDLE\"".to_string(),
            ..Default::default()
        };
        let live_tab = TabInfo {
            position: 14,
            name: "__super_tabs_id=\"st-14\" | status=\"ACTIVE\"".to_string(),
            ..Default::default()
        };

        let mut state = State {
            tabs: vec![live_tab.clone()],
            ..Default::default()
        };

        state.rows_by_tab_id.insert(
            "st-14".to_string(),
            state
                .build_row_state_from_tab_name(&stale_tab, &schema)
                .expect("stale tab name should decode"),
        );

        let row = state.row_state_for_update(&live_tab, "st-14", &schema, false, false);

        assert_eq!(row.cells[0].as_ref().unwrap().raw_input, "ACTIVE");
    }

    #[test]
    fn row_state_for_update_keeps_cached_row_while_tab_rename_event_is_pending() {
        let schema = test_schema("status");
        let legacy_tab = TabInfo {
            position: 2,
            name: "status=\"IDLE\"".to_string(),
            ..Default::default()
        };
        let cached_tab = TabInfo {
            position: 2,
            name: "__super_tabs_id=\"st-2\" | status=\"RUNNING\"".to_string(),
            ..Default::default()
        };

        let mut state = State {
            tabs: vec![legacy_tab.clone()],
            pending_tab_id_by_position: BTreeMap::from([(2, "st-2".to_string())]),
            ..Default::default()
        };

        state.rows_by_tab_id.insert(
            "st-2".to_string(),
            state
                .build_row_state_from_tab_name(&cached_tab, &schema)
                .expect("cached tab name should decode"),
        );

        let row = state.row_state_for_update(&legacy_tab, "st-2", &schema, true, false);

        assert_eq!(row.cells[0].as_ref().unwrap().raw_input, "RUNNING");
        assert_eq!(
            state.resolved_tab_id_for_tab(&legacy_tab).as_deref(),
            Some("st-2")
        );
    }

    #[test]
    fn session_write_leader_is_lowest_plugin_id_for_shared_plugin_url() {
        let leader = test_pipe_state(11, "main", &[11, 22]);
        let follower = test_pipe_state(22, "main", &[11, 22]);

        assert_eq!(leader.session_write_leader_plugin_id(), Some(11));
        assert!(leader.is_session_write_leader());
        assert!(!follower.is_session_write_leader());
    }

    #[test]
    fn session_write_leader_ignores_other_plugin_urls() {
        let state = State {
            plugin_id: Some(22),
            shared_state_ready: true,
            pane_manifest: PaneManifest {
                panes: HashMap::from([
                    (0, vec![test_plugin_pane_with_url(11, "zellij:link")]),
                    (1, vec![test_plugin_pane(22)]),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(state.session_write_leader_plugin_id(), Some(22));
        assert!(state.is_session_write_leader());
    }

    #[test]
    fn session_write_leader_waits_for_pane_manifest() {
        let state = State {
            plugin_id: Some(11),
            shared_state_ready: true,
            ..Default::default()
        };

        assert!(!state.is_session_write_leader());
    }

    #[test]
    fn pending_tab_claim_key_tracks_terminal_panes_across_position_shift() {
        let state_before_close = State {
            pane_manifest: PaneManifest {
                panes: HashMap::from([(7, vec![test_terminal_pane(19), test_terminal_pane(20)])]),
                ..Default::default()
            },
            ..Default::default()
        };
        let tab_before_close = TabInfo {
            position: 7,
            name: "status=\"IDLE\"".to_string(),
            ..Default::default()
        };
        let state_after_close = State {
            pane_manifest: PaneManifest {
                panes: HashMap::from([(6, vec![test_terminal_pane(20), test_terminal_pane(19)])]),
                ..Default::default()
            },
            ..Default::default()
        };
        let tab_after_close = TabInfo {
            position: 6,
            name: tab_before_close.name.clone(),
            ..Default::default()
        };

        assert_eq!(
            state_before_close.pending_tab_claim_key_for_tab(&tab_before_close),
            "panes-19_20"
        );
        assert_eq!(
            state_after_close.pending_tab_claim_key_for_tab(&tab_after_close),
            "panes-19_20"
        );
    }

    #[test]
    fn follower_reconcile_skips_legacy_migration() {
        let schema = test_schema("status");
        let mut follower = test_pipe_state(22, "main", &[11, 22]);
        follower.schema = Some(schema.clone());
        follower.width_indexes = vec![WidthIndex::default(); schema.len()];
        follower.tabs = vec![TabInfo {
            position: 2,
            name: "status=\"IDLE\"".to_string(),
            ..Default::default()
        }];

        assert!(!follower.reconcile_rows_with_tabs());
        assert!(follower.rows_by_tab_id.is_empty());
        assert!(follower.pending_tab_id_by_position.is_empty());
    }

    #[test]
    fn session_key_waits_for_shared_state_mount() {
        let state = State {
            mode_info: ModeInfo {
                session_name: Some("main".to_string()),
                ..Default::default()
            },
            shared_state_ready: false,
            ..Default::default()
        };

        assert_eq!(state.session_key(), None);
    }

    #[test]
    fn resolves_configured_shared_state_host_folder() {
        let path = resolve_shared_state_host_folder(Some("/tmp/super-tabs-state")).unwrap();

        assert_eq!(path, PathBuf::from("/tmp/super-tabs-state"));
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
