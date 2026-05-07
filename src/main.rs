use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
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
const LEADER_FILE_NAME: &str = "writer-leader.json";
const LEADER_TTL_MS: u128 = 30_000;
const WRITE_ROLE_CACHE_TTL_MS: u128 = 1_000;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WriterLeaderClaim {
    version: u8,
    plugin_id: u32,
    observed_at_ms: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteRole {
    Leader,
    Follower,
    Unavailable,
}

impl WriteRole {
    fn can_write(self) -> bool {
        matches!(self, Self::Leader)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum IdentityState {
    Managed,
    Pending { claim_key: String },
    UnmanagedManual,
    UnmanagedDefault,
}

impl IdentityState {
    fn log_label(&self) -> String {
        match self {
            Self::Managed => "managed".to_string(),
            Self::Pending { claim_key } => format!("pending({claim_key})"),
            Self::UnmanagedManual => "unmanaged-manual".to_string(),
            Self::UnmanagedDefault => "unmanaged-default".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowSource {
    Cached,
    PersistedById,
    PersistedByLegacyPosition,
    LiveTabName,
    EmptyNewManagedRow,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowLoadPolicy {
    Stable,
    PendingRename { legacy_position_fallback: bool },
}

impl RowLoadPolicy {
    fn pending_rename(legacy_position_fallback: bool) -> Self {
        Self::PendingRename {
            legacy_position_fallback,
        }
    }

    fn allow_live_name_mismatch(self) -> bool {
        matches!(self, Self::PendingRename { .. })
    }

    fn allow_legacy_position_fallback(self) -> bool {
        match self {
            Self::Stable => false,
            Self::PendingRename {
                legacy_position_fallback,
            } => legacy_position_fallback,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeferredPipeReason {
    UnresolvedPane,
    MissingTab,
    SharedStateUnavailable,
}

impl DeferredPipeReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::UnresolvedPane => "unresolved-pane",
            Self::MissingTab => "missing-tab",
            Self::SharedStateUnavailable => "shared-state-unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropPipeReason {
    FollowerUnmanagedTab,
}

impl DropPipeReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::FollowerUnmanagedTab => "follower-unmanaged-tab",
        }
    }
}

struct LoadedTabRow {
    tab_id: String,
    identity: IdentityState,
    row: TabRowState,
    row_source: RowSource,
    rename_pending: bool,
}

struct LoadedRowState {
    row: TabRowState,
    source: RowSource,
}

struct ReconcilePlan {
    rows_by_tab_id: BTreeMap<String, TabRowState>,
    pending_tab_id_by_position: BTreeMap<usize, String>,
    width_indexes: Vec<WidthIndex>,
    effects: Vec<ReconcileEffect>,
}

enum ReconcileEffect {
    PersistRow {
        tab_position: usize,
        tab_id: String,
        mirrored_name: String,
        row: TabRowState,
    },
    RenameTab {
        tab_position: usize,
        mirrored_name: String,
    },
}

struct ResolvedTabTarget {
    tab_id: String,
    identity: IdentityState,
    row_load_policy: RowLoadPolicy,
}

impl ResolvedTabTarget {
    fn rename_pending(&self) -> bool {
        self.row_load_policy.allow_live_name_mismatch()
    }
}

enum TabTargetResolution {
    Ready(ResolvedTabTarget),
    Unmanaged(IdentityState),
    Defer(DeferredPipeReason),
    Drop(DropPipeReason),
}

#[derive(Clone)]
struct SharedStateStore {
    session_key: String,
    plugin_id: u32,
}

impl SharedStateStore {
    fn new(session_key: String, plugin_id: u32) -> Self {
        Self {
            session_key,
            plugin_id,
        }
    }

    fn persisted_tab_state_path(&self, tab_key: &str) -> PathBuf {
        state_dir()
            .join(&self.session_key)
            .join(format!("{STATE_FILE_PREFIX}{tab_key}.json"))
    }

    fn pending_tab_claim_path(&self, pending_claim_key: &str) -> PathBuf {
        state_dir()
            .join(&self.session_key)
            .join(format!("{PENDING_FILE_PREFIX}{pending_claim_key}.json"))
    }

    fn writer_leader_claim_path(&self) -> PathBuf {
        state_dir().join(&self.session_key).join(LEADER_FILE_NAME)
    }

    fn read_persisted_tab_state(&self, tab_key: &str) -> Option<PersistedTabState> {
        read_json_file(self.persisted_tab_state_path(tab_key))
    }

    fn write_persisted_tab_state(
        &self,
        tab_key: &str,
        persisted: &PersistedTabState,
    ) -> Result<(), String> {
        write_json_file(
            &self.persisted_tab_state_path(tab_key),
            self.plugin_id,
            persisted,
            "persisted state",
        )
    }

    fn read_pending_tab_claim(&self, pending_claim_key: &str) -> Option<PendingTabClaim> {
        read_json_file(self.pending_tab_claim_path(pending_claim_key))
    }

    fn write_pending_tab_claim(
        &self,
        pending_claim_key: &str,
        claim: &PendingTabClaim,
    ) -> Result<(), String> {
        write_json_file(
            &self.pending_tab_claim_path(pending_claim_key),
            self.plugin_id,
            claim,
            "pending claim",
        )
    }

    fn clear_pending_tab_claim(&self, pending_claim_key: &str) -> Result<(), String> {
        let path = self.pending_tab_claim_path(pending_claim_key);
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn read_writer_leader_claim(&self) -> Option<WriterLeaderClaim> {
        read_json_file(self.writer_leader_claim_path())
    }

    fn write_writer_leader_claim(&self, claim: &WriterLeaderClaim) -> Result<(), String> {
        write_json_file(
            &self.writer_leader_claim_path(),
            self.plugin_id,
            claim,
            "writer leader",
        )
    }

    fn claim_writer_leader(&self) -> bool {
        let now_ms = current_time_ms();
        let claim = WriterLeaderClaim {
            version: 1,
            plugin_id: self.plugin_id,
            observed_at_ms: now_ms,
        };

        match self.read_writer_leader_claim() {
            Some(current) if current.version == 1 && current.plugin_id == self.plugin_id => {
                self.write_writer_leader_claim(&claim).is_ok()
            }
            Some(current)
                if current.version == 1
                    && now_ms.saturating_sub(current.observed_at_ms) <= LEADER_TTL_MS =>
            {
                false
            }
            _ => self.write_writer_leader_claim(&claim).is_ok_and(|()| {
                self.read_writer_leader_claim().is_some_and(|current| {
                    current.version == 1 && current.plugin_id == self.plugin_id
                })
            }),
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
    pending_pipe_updates: Vec<UpdatePayload>,
    cached_write_role: Option<WriteRole>,
    cached_write_role_checked_at_ms: u128,
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
                if self.replay_pending_pipe_updates() {
                    should_render = true;
                }
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
                self.reconcile_rows_with_tabs();
                self.replay_pending_pipe_updates();
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
                    if self.replay_pending_pipe_updates() {
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
        let Some(plan) = self.plan_reconcile_rows_with_tabs() else {
            return false;
        };

        self.apply_reconcile_plan(plan)
    }

    fn plan_reconcile_rows_with_tabs(&mut self) -> Option<ReconcilePlan> {
        let Some(schema) = self.schema.clone() else {
            return None;
        };
        let write_role = self.write_role();

        let tabs = self.tabs.clone();
        let mut next_rows = BTreeMap::new();
        let mut next_pending_tab_ids = BTreeMap::new();
        let mut next_width_indexes = vec![WidthIndex::default(); schema.len()];
        let mut effects = Vec::new();

        for tab in &tabs {
            let Some(mut loaded) =
                self.load_row_state_for_tab(tab, &schema, write_role, &mut effects)
            else {
                if write_role.can_write()
                    && is_default_zellij_tab_name(&tab.name)
                    && self.has_terminal_panes_for_tab(tab.position)
                {
                    let target = self.ensure_pending_target_for_tab(tab, false);
                    self.debug_log(format!(
                        "claim_default tab={} id={} identity={} source={:?}",
                        tab.position,
                        target.tab_id,
                        target.identity.log_label(),
                        RowSource::None
                    ));
                    next_pending_tab_ids.insert(tab.position, target.tab_id);
                }
                continue;
            };

            if next_rows.contains_key(&loaded.tab_id) {
                if write_role.can_write() {
                    let duplicate_tab_id = loaded.tab_id;
                    let target = self.ensure_pending_target_for_tab(tab, false);
                    loaded.tab_id = target.tab_id;
                    loaded.identity = target.identity;
                    loaded.row = TabRowState::empty(&schema);
                    loaded.row_source = RowSource::EmptyNewManagedRow;
                    let mirrored_name = self.plan_mirror_tab_row(
                        tab.position,
                        &loaded.tab_id,
                        &mut loaded.row,
                        &schema,
                        &mut effects,
                    );
                    self.debug_log(format!(
                        "dedupe tab={} old_id={} new_id={} identity={} source={:?} mirrored_name={}",
                        tab.position,
                        duplicate_tab_id,
                        loaded.tab_id,
                        loaded.identity.log_label(),
                        loaded.row_source,
                        mirrored_name
                    ));
                    loaded.rename_pending = true;
                } else {
                    self.debug_log(format!(
                        "skip_dedupe tab={} id={} reason=follower",
                        tab.position, loaded.tab_id
                    ));
                    continue;
                }
            }

            if loaded.rename_pending {
                next_pending_tab_ids.insert(tab.position, loaded.tab_id.clone());
            }

            add_row_widths_to_indexes(&mut next_width_indexes, &loaded.row);
            next_rows.insert(loaded.tab_id, loaded.row);
        }

        Some(ReconcilePlan {
            rows_by_tab_id: next_rows,
            pending_tab_id_by_position: next_pending_tab_ids,
            width_indexes: next_width_indexes,
            effects,
        })
    }

    fn apply_reconcile_plan(&mut self, plan: ReconcilePlan) -> bool {
        let changed = self.rows_by_tab_id != plan.rows_by_tab_id
            || self.pending_tab_id_by_position != plan.pending_tab_id_by_position
            || self.width_indexes != plan.width_indexes;

        self.rows_by_tab_id = plan.rows_by_tab_id;
        self.pending_tab_id_by_position = plan.pending_tab_id_by_position;
        self.width_indexes = plan.width_indexes;

        for effect in plan.effects {
            self.apply_reconcile_effect(effect);
        }

        changed
    }

    fn apply_reconcile_effect(&self, effect: ReconcileEffect) {
        match effect {
            ReconcileEffect::PersistRow {
                tab_position,
                tab_id,
                mirrored_name,
                row,
            } => {
                let Some(schema) = self.schema.as_ref() else {
                    return;
                };
                self.write_persisted_row(&tab_id, tab_position, &mirrored_name, &row, schema);
            }
            ReconcileEffect::RenameTab {
                tab_position,
                mirrored_name,
            } => self.rename_live_tab(tab_position, &mirrored_name),
        }
    }

    fn load_row_state_for_tab(
        &mut self,
        tab: &TabInfo,
        schema: &Schema,
        write_role: WriteRole,
        effects: &mut Vec<ReconcileEffect>,
    ) -> Option<LoadedTabRow> {
        match self.resolve_tab_target(tab, write_role) {
            TabTargetResolution::Ready(target) => {
                self.load_reconcile_row_for_target(tab, schema, target, effects)
            }
            TabTargetResolution::Unmanaged(identity) if write_role.can_write() => {
                self.debug_log(format!(
                    "legacy_probe tab={} identity={}",
                    tab.position,
                    identity.log_label()
                ));
                let loaded_row = self.read_legacy_row_state(tab.position, tab, schema)?;
                let target = self.ensure_pending_target_for_tab(tab, true);
                self.mirror_reconcile_row_for_target(
                    tab, schema, target, loaded_row, "migrate", effects,
                )
            }
            TabTargetResolution::Unmanaged(_)
            | TabTargetResolution::Defer(_)
            | TabTargetResolution::Drop(_) => None,
        }
    }

    fn load_reconcile_row_for_target(
        &mut self,
        tab: &TabInfo,
        schema: &Schema,
        target: ResolvedTabTarget,
        effects: &mut Vec<ReconcileEffect>,
    ) -> Option<LoadedTabRow> {
        let rename_pending = target.rename_pending();
        match &target.identity {
            IdentityState::Pending { .. } => {
                if let Some(row) = self.read_persisted_row_for_tab_id(&target.tab_id, schema) {
                    let loaded_row = LoadedRowState {
                        row,
                        source: RowSource::PersistedById,
                    };
                    return self.mirror_reconcile_row_for_target(
                        tab, schema, target, loaded_row, "hydrate", effects,
                    );
                }

                let loaded_row = self.read_legacy_row_state(tab.position, tab, schema)?;
                self.mirror_reconcile_row_for_target(
                    tab,
                    schema,
                    target,
                    loaded_row,
                    "resume_pending",
                    effects,
                )
            }
            IdentityState::Managed => {
                if let Some(row) = self.read_persisted_row(&target.tab_id, tab, schema) {
                    self.debug_log(format!(
                        "hydrate tab={} id={} identity={} source={:?} name={}",
                        tab.position,
                        target.tab_id,
                        target.identity.log_label(),
                        RowSource::PersistedById,
                        tab.name
                    ));
                    return Some(LoadedTabRow {
                        tab_id: target.tab_id,
                        identity: target.identity,
                        row,
                        row_source: RowSource::PersistedById,
                        rename_pending,
                    });
                }

                let row = self.build_row_state_from_tab_name(tab, schema);
                if row.is_some() {
                    self.debug_log(format!(
                        "hydrate tab={} id={} identity={} source={:?} name={}",
                        tab.position,
                        target.tab_id,
                        target.identity.log_label(),
                        RowSource::LiveTabName,
                        tab.name
                    ));
                }
                row.map(|row| LoadedTabRow {
                    tab_id: target.tab_id,
                    identity: target.identity,
                    row,
                    row_source: RowSource::LiveTabName,
                    rename_pending,
                })
            }
            IdentityState::UnmanagedManual | IdentityState::UnmanagedDefault => None,
        }
    }

    fn mirror_reconcile_row_for_target(
        &self,
        tab: &TabInfo,
        schema: &Schema,
        target: ResolvedTabTarget,
        mut loaded_row: LoadedRowState,
        action: &str,
        effects: &mut Vec<ReconcileEffect>,
    ) -> Option<LoadedTabRow> {
        let rename_pending = target.rename_pending();
        let mirrored_name = self.plan_mirror_tab_row(
            tab.position,
            &target.tab_id,
            &mut loaded_row.row,
            schema,
            effects,
        );
        self.debug_log(format!(
            "{} tab={} id={} identity={} source={:?} mirrored_name={}",
            action,
            tab.position,
            target.tab_id,
            target.identity.log_label(),
            loaded_row.source,
            mirrored_name
        ));
        Some(LoadedTabRow {
            tab_id: target.tab_id,
            identity: target.identity,
            row: loaded_row.row,
            row_source: loaded_row.source,
            rename_pending,
        })
    }

    fn read_legacy_row_state(
        &self,
        tab_position: usize,
        tab: &TabInfo,
        schema: &Schema,
    ) -> Option<LoadedRowState> {
        if let Some(row) =
            self.read_persisted_row_by_key(&tab_position.to_string(), tab_position, tab, schema)
        {
            return Some(LoadedRowState {
                row,
                source: RowSource::PersistedByLegacyPosition,
            });
        }

        self.build_row_state_from_tab_name(tab, schema)
            .map(|row| LoadedRowState {
                row,
                source: RowSource::LiveTabName,
            })
    }

    fn load_cached_row_for_identity(
        &mut self,
        tab: &TabInfo,
        tab_id: &str,
        policy: RowLoadPolicy,
    ) -> Option<LoadedRowState> {
        let cached_row = self.rows_by_tab_id.remove(tab_id)?;

        if cached_row.last_mirrored_tab_name.as_deref() == Some(tab.name.as_str()) {
            return Some(LoadedRowState {
                row: cached_row,
                source: RowSource::Cached,
            });
        }

        if policy.allow_live_name_mismatch() {
            return Some(LoadedRowState {
                row: cached_row,
                source: RowSource::Cached,
            });
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

    fn plan_mirror_tab_row(
        &self,
        tab_position: usize,
        tab_id: &str,
        row: &mut TabRowState,
        schema: &Schema,
        effects: &mut Vec<ReconcileEffect>,
    ) -> String {
        let mirrored_name = encode_tab_name_with_id(schema.columns(), &row.cells, Some(tab_id));
        row.last_mirrored_tab_name = Some(mirrored_name.clone());
        effects.push(ReconcileEffect::PersistRow {
            tab_position,
            tab_id: tab_id.to_string(),
            mirrored_name: mirrored_name.clone(),
            row: row.clone(),
        });
        effects.push(ReconcileEffect::RenameTab {
            tab_position,
            mirrored_name: mirrored_name.clone(),
        });
        mirrored_name
    }

    fn clear_pending_tab_claim_for_tab(&mut self, tab: &TabInfo) {
        self.pending_tab_id_by_position.remove(&tab.position);

        let Some(store) = self.state_store() else {
            return;
        };

        let pending_claim_key = self.pending_tab_claim_key_for_tab(tab);
        if let Err(error) = store.clear_pending_tab_claim(&pending_claim_key) {
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

    fn has_terminal_panes_for_tab(&self, tab_position: usize) -> bool {
        self.pane_manifest
            .panes
            .get(&tab_position)
            .is_some_and(|panes| panes.iter().any(|pane| !pane.is_plugin))
    }

    fn resolve_tab_target(&mut self, tab: &TabInfo, write_role: WriteRole) -> TabTargetResolution {
        if write_role.can_write()
            && let Some(tab_id) = self.refresh_pending_tab_id_for_tab(tab)
        {
            self.observe_tab_id(&tab_id);
            return TabTargetResolution::Ready(ResolvedTabTarget {
                tab_id,
                identity: IdentityState::Pending {
                    claim_key: self.pending_tab_claim_key_for_tab(tab),
                },
                row_load_policy: RowLoadPolicy::pending_rename(false),
            });
        }

        if let Some(tab_id) = self.live_tab_id_for_tab(tab) {
            self.observe_tab_id(&tab_id);
            if write_role.can_write() {
                self.clear_pending_tab_claim_for_tab(tab);
            }
            return TabTargetResolution::Ready(ResolvedTabTarget {
                tab_id,
                identity: IdentityState::Managed,
                row_load_policy: RowLoadPolicy::Stable,
            });
        }

        if write_role.can_write() {
            return TabTargetResolution::Unmanaged(self.unmanaged_identity_for_tab(tab));
        }

        if write_role == WriteRole::Unavailable {
            TabTargetResolution::Defer(DeferredPipeReason::SharedStateUnavailable)
        } else {
            TabTargetResolution::Drop(DropPipeReason::FollowerUnmanagedTab)
        }
    }

    fn ensure_pending_target_for_tab(
        &mut self,
        tab: &TabInfo,
        legacy_position_fallback: bool,
    ) -> ResolvedTabTarget {
        let tab_id = self.ensure_pending_tab_id_for_tab(tab);
        self.observe_tab_id(&tab_id);

        ResolvedTabTarget {
            tab_id,
            identity: IdentityState::Pending {
                claim_key: self.pending_tab_claim_key_for_tab(tab),
            },
            row_load_policy: RowLoadPolicy::pending_rename(legacy_position_fallback),
        }
    }

    fn refresh_pending_tab_id_for_tab(&mut self, tab: &TabInfo) -> Option<String> {
        let Some(store) = self.state_store() else {
            self.pending_tab_id_by_position.remove(&tab.position);
            return None;
        };
        let pending_claim_key = self.pending_tab_claim_key_for_tab(tab);
        let Some(claim) = store.read_pending_tab_claim(&pending_claim_key) else {
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

        let Some(store) = self.state_store() else {
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

            match store.write_pending_tab_claim(&self.pending_tab_claim_key_for_tab(tab), &claim) {
                Ok(()) => {
                    self.pending_tab_id_by_position
                        .insert(tab.position, tab_id.clone());
                    return tab_id;
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
        match self.unmanaged_identity_for_tab(tab) {
            IdentityState::UnmanagedManual => Some(tab.name.clone()),
            IdentityState::UnmanagedDefault => None,
            IdentityState::Managed | IdentityState::Pending { .. } => None,
        }
    }

    fn unmanaged_identity_for_tab(&self, tab: &TabInfo) -> IdentityState {
        let trimmed_name = tab.name.trim();
        if trimmed_name.is_empty() || is_default_zellij_tab_name(trimmed_name) {
            return IdentityState::UnmanagedDefault;
        }

        let Some(parsed_name) = decode_tab_name(&tab.name) else {
            return IdentityState::UnmanagedManual;
        };
        let has_super_tab_id = parsed_name.contains_key(SUPER_TAB_ID_KEY);
        let has_other_values = parsed_name
            .iter()
            .any(|(key, value)| key != SUPER_TAB_ID_KEY && !value.is_empty());

        if has_super_tab_id && !has_other_values {
            return IdentityState::UnmanagedDefault;
        }

        IdentityState::UnmanagedManual
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
        let store = self.state_store()?;
        let persisted = store.read_persisted_tab_state(tab_id)?;
        row_state_from_persisted(&persisted, schema)
    }

    fn read_persisted_row_by_key(
        &self,
        tab_key: &str,
        tab_position: usize,
        tab: &TabInfo,
        schema: &Schema,
    ) -> Option<TabRowState> {
        let store = self.state_store()?;
        let persisted = store.read_persisted_tab_state(tab_key)?;
        if persisted.version != 1 || persisted.mirrored_name != tab.name {
            self.debug_log(format!(
                "cache_miss tab={} key={} mirrored_name={} live_name={}",
                tab_position, tab_key, persisted.mirrored_name, tab.name
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

        let Some(store) = self.state_store() else {
            self.debug_log(format!(
                "skip_persist tab={} id={} reason=no-session-key",
                tab_position, tab_id
            ));
            return;
        };

        if let Err(error) = store.write_persisted_tab_state(tab_id, &persisted) {
            eprintln!(
                "super-tabs: failed to persist tab state for position {tab_position} ({tab_id}): {error}"
            );
        } else {
            self.debug_log(format!(
                "persist tab={} id={} mirrored_name={}",
                tab_position, tab_id, mirrored_name
            ));
        }
    }

    fn handle_update_pipe(&mut self, payload: Option<&str>) -> bool {
        let Some(payload) = payload else {
            return false;
        };
        let Ok(payload) = UpdatePayload::parse(payload) else {
            self.debug_log("pipe ignored invalid payload");
            return false;
        };
        self.handle_update_payload(payload)
    }

    fn handle_update_payload(&mut self, payload: UpdatePayload) -> bool {
        let Some(schema) = self.schema.clone() else {
            return false;
        };
        let write_role = self.write_role();
        let resolved_tab_position = self.pane_to_tab_position.get(&payload.pane_id).copied();
        let Some(tab_position) = resolved_tab_position else {
            self.defer_pipe_update(payload, DeferredPipeReason::UnresolvedPane);
            return false;
        };
        let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.position == tab_position)
            .cloned()
        else {
            self.defer_pipe_update(payload, DeferredPipeReason::MissingTab);
            return false;
        };
        let target = match self.resolve_tab_target(&tab, write_role) {
            TabTargetResolution::Ready(target) => target,
            TabTargetResolution::Unmanaged(identity) if write_role.can_write() => {
                self.debug_log(format!(
                    "pipe unmanaged tab={} identity={}",
                    tab.position,
                    identity.log_label()
                ));
                self.ensure_pending_target_for_tab(&tab, true)
            }
            TabTargetResolution::Unmanaged(_) => {
                self.drop_pipe_update(&payload, DropPipeReason::FollowerUnmanagedTab, tab.position);
                return false;
            }
            TabTargetResolution::Defer(reason) => {
                self.defer_pipe_update(payload, reason);
                return false;
            }
            TabTargetResolution::Drop(reason) => {
                self.drop_pipe_update(&payload, reason, tab.position);
                return false;
            }
        };
        self.debug_log(format!(
            "pipe pane_id={} resolved_tab={resolved_tab_position:?} tab_id={} updates={:?}",
            payload.pane_id, target.tab_id, payload.updates
        ));

        let mut loaded_row =
            self.row_state_for_update(&tab, &target.tab_id, &schema, target.row_load_policy);
        let changed = apply_updates_to_row(&mut loaded_row.row, &schema, payload.updates);
        let mirrored_name = encode_tab_name_with_id(
            schema.columns(),
            &loaded_row.row.cells,
            Some(target.tab_id.as_str()),
        );
        self.debug_log(format!(
            "pipe row tab={} id={} identity={} source={:?} rename_pending={}",
            tab_position,
            target.tab_id,
            target.identity.log_label(),
            loaded_row.source,
            target.rename_pending()
        ));

        if !changed
            && loaded_row.row.last_mirrored_tab_name.as_deref() == Some(mirrored_name.as_str())
        {
            self.rows_by_tab_id
                .insert(target.tab_id.clone(), loaded_row.row);
            if write_role.can_write() && target.rename_pending() {
                self.pending_tab_id_by_position
                    .insert(tab_position, target.tab_id);
            } else if write_role.can_write() {
                self.clear_pending_tab_claim_for_tab(&tab);
            }
            return false;
        }

        self.rows_by_tab_id
            .insert(target.tab_id.clone(), loaded_row.row.clone());
        if write_role.can_write() && target.rename_pending() {
            self.pending_tab_id_by_position
                .insert(tab_position, target.tab_id.clone());
        } else if write_role.can_write() {
            self.clear_pending_tab_claim_for_tab(&tab);
        }

        if !write_role.can_write() {
            self.recompute_width_indexes();
            return changed;
        }

        let mirrored_name =
            self.mirror_tab_row(tab_position, &target.tab_id, &mut loaded_row.row, &schema);
        self.rows_by_tab_id
            .insert(target.tab_id.clone(), loaded_row.row);
        self.debug_log(format!(
            "rename target_tab={} id={} mirrored_name={}",
            tab_position, target.tab_id, mirrored_name
        ));

        self.recompute_width_indexes();
        true
    }

    fn defer_pipe_update(&mut self, payload: UpdatePayload, reason: DeferredPipeReason) {
        self.debug_log(format!(
            "pipe deferred pane_id={} reason={} updates={:?}",
            payload.pane_id,
            reason.as_str(),
            payload.updates
        ));

        if self.pending_pipe_updates.contains(&payload) {
            return;
        }

        self.pending_pipe_updates.push(payload);
        if self.pending_pipe_updates.len() > 64 {
            self.pending_pipe_updates.remove(0);
        }
    }

    fn drop_pipe_update(
        &self,
        payload: &UpdatePayload,
        reason: DropPipeReason,
        tab_position: usize,
    ) {
        self.debug_log(format!(
            "pipe dropped pane_id={} tab={} reason={} updates={:?}",
            payload.pane_id,
            tab_position,
            reason.as_str(),
            payload.updates
        ));
    }

    fn replay_pending_pipe_updates(&mut self) -> bool {
        if self.pending_pipe_updates.is_empty() {
            return false;
        }

        let pending_updates = std::mem::take(&mut self.pending_pipe_updates);
        let mut changed = false;
        for payload in pending_updates {
            if self.handle_update_payload(payload) {
                changed = true;
            }
        }
        changed
    }

    fn row_state_for_update(
        &mut self,
        tab: &TabInfo,
        tab_id: &str,
        schema: &Schema,
        policy: RowLoadPolicy,
    ) -> LoadedRowState {
        if let Some(loaded) = self.load_cached_row_for_identity(tab, tab_id, policy) {
            return loaded;
        }

        if policy.allow_live_name_mismatch() {
            if let Some(row) = self.read_persisted_row_for_tab_id(tab_id, schema) {
                return LoadedRowState {
                    row,
                    source: RowSource::PersistedById,
                };
            }
        } else if let Some(row) = self.read_persisted_row(tab_id, tab, schema) {
            return LoadedRowState {
                row,
                source: RowSource::PersistedById,
            };
        }

        if policy.allow_legacy_position_fallback()
            && let Some(loaded) = self.read_legacy_row_state(tab.position, tab, schema)
        {
            return loaded;
        }

        if let Some(row) = self.build_row_state_from_tab_name(tab, schema) {
            return LoadedRowState {
                row,
                source: RowSource::LiveTabName,
            };
        }

        LoadedRowState {
            row: TabRowState::empty(schema),
            source: RowSource::EmptyNewManagedRow,
        }
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

    fn state_store(&self) -> Option<SharedStateStore> {
        Some(SharedStateStore::new(
            self.session_key()?,
            self.plugin_id.unwrap_or(0),
        ))
    }

    fn write_role(&mut self) -> WriteRole {
        let now_ms = current_time_ms();
        if let Some(cached_write_role) = self.cached_write_role
            && now_ms.saturating_sub(self.cached_write_role_checked_at_ms)
                <= WRITE_ROLE_CACHE_TTL_MS
        {
            return cached_write_role;
        }

        let write_role = self.compute_write_role();
        self.cached_write_role = Some(write_role);
        self.cached_write_role_checked_at_ms = now_ms;
        write_role
    }

    fn compute_write_role(&self) -> WriteRole {
        if !self.shared_state_ready {
            return WriteRole::Unavailable;
        }

        if self.plugin_id.is_none() {
            return WriteRole::Leader;
        }
        let Some(store) = self.state_store() else {
            return WriteRole::Unavailable;
        };

        if store.claim_writer_leader() {
            WriteRole::Leader
        } else {
            WriteRole::Follower
        }
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

fn is_default_zellij_tab_name(name: &str) -> bool {
    let Some(number) = name.trim().strip_prefix("Tab #") else {
        return false;
    };

    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
fn state_dir() -> PathBuf {
    std::env::temp_dir().join("super-tabs-tests")
}

#[cfg(not(test))]
fn state_dir() -> PathBuf {
    PathBuf::from(STATE_DIR)
}

fn read_json_file<T>(path: PathBuf) -> Option<T>
where
    T: DeserializeOwned,
{
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_json_file<T>(path: &PathBuf, plugin_id: u32, value: &T, label: &str) -> Result<(), String>
where
    T: Serialize,
{
    let temp_path = path.with_extension(format!("json.tmp-{plugin_id}"));
    let content =
        serde_json::to_vec(value).map_err(|error| format!("serialize {label}: {error}"))?;

    let parent_dir = path
        .parent()
        .ok_or_else(|| format!("missing {label} parent directory"))?;
    fs::create_dir_all(parent_dir).map_err(|error| format!("create state dir: {error}"))?;

    let mut file =
        File::create(&temp_path).map_err(|error| format!("create temp {label} file: {error}"))?;
    file.write_all(&content)
        .map_err(|error| format!("write temp {label} file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync temp {label} file: {error}"))?;
    drop(file);

    fs::rename(&temp_path, path).map_err(|error| format!("rename temp {label} file: {error}"))?;
    sync_parent_dir(path);
    Ok(())
}

fn current_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
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

    fn unique_session_name(label: &str) -> String {
        format!("super-tabs-{label}-{}", current_time_ms())
    }

    fn cleanup_session(session_name: &str) {
        let session_key = sanitize_session_key(session_name);
        let _ = std::fs::remove_dir_all(state_dir().join(session_key));
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

        let loaded = state.row_state_for_update(&live_tab, "st-14", &schema, RowLoadPolicy::Stable);

        assert_eq!(loaded.source, RowSource::LiveTabName);
        assert_eq!(loaded.row.cells[0].as_ref().unwrap().raw_input, "ACTIVE");
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

        let loaded = state.row_state_for_update(
            &legacy_tab,
            "st-2",
            &schema,
            RowLoadPolicy::pending_rename(false),
        );

        assert_eq!(loaded.source, RowSource::Cached);
        assert_eq!(loaded.row.cells[0].as_ref().unwrap().raw_input, "RUNNING");
        assert_eq!(
            state.resolved_tab_id_for_tab(&legacy_tab).as_deref(),
            Some("st-2")
        );
    }

    #[test]
    fn session_write_leader_claim_allows_one_writer() {
        let session_name = unique_session_name("writer-leader");
        cleanup_session(&session_name);
        let mut leader = test_pipe_state(11, &session_name, &[11, 22]);
        let mut follower = test_pipe_state(22, &session_name, &[11, 22]);

        assert_eq!(leader.write_role(), WriteRole::Leader);
        assert_eq!(follower.write_role(), WriteRole::Follower);

        cleanup_session(&session_name);
    }

    #[test]
    fn session_write_leader_reclaims_stale_claim() {
        let session_name = unique_session_name("writer-leader-stale");
        cleanup_session(&session_name);
        let session_key = sanitize_session_key(&session_name);
        SharedStateStore::new(session_key.clone(), 11)
            .write_writer_leader_claim(&WriterLeaderClaim {
                version: 1,
                plugin_id: 11,
                observed_at_ms: 0,
            })
            .unwrap();

        let mut replacement = test_pipe_state(22, &session_name, &[11, 22]);

        assert_eq!(replacement.write_role(), WriteRole::Leader);
        assert_eq!(
            SharedStateStore::new(session_key, 22)
                .read_writer_leader_claim()
                .unwrap()
                .plugin_id,
            22
        );

        cleanup_session(&session_name);
    }

    #[test]
    fn session_write_leader_waits_for_shared_state_mount() {
        let mut state = State {
            plugin_id: Some(22),
            mode_info: ModeInfo {
                session_name: Some("main".to_string()),
                ..Default::default()
            },
            shared_state_ready: false,
            ..Default::default()
        };

        assert_eq!(state.write_role(), WriteRole::Unavailable);
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
    fn default_zellij_tab_gets_pending_claim_after_panes_are_known() {
        let session_name = unique_session_name("default-tab-claim");
        cleanup_session(&session_name);
        let schema = test_schema("status");
        let mut state = test_pipe_state(11, &session_name, &[11]);
        state.schema = Some(schema.clone());
        state.width_indexes = vec![WidthIndex::default(); schema.len()];
        state.tabs = vec![TabInfo {
            position: 16,
            name: "Tab #18".to_string(),
            ..Default::default()
        }];

        assert!(!state.reconcile_rows_with_tabs());
        assert!(state.pending_tab_id_by_position.is_empty());

        state.pane_manifest = PaneManifest {
            panes: HashMap::from([(16, vec![test_terminal_pane(33), test_terminal_pane(34)])]),
            ..Default::default()
        };

        assert!(state.reconcile_rows_with_tabs());
        let tab_id = state.pending_tab_id_by_position.get(&16).unwrap();
        assert!(state.rows_by_tab_id.is_empty());
        assert_eq!(
            state.pending_tab_claim_key_for_tab(&state.tabs[0]),
            "panes-33_34"
        );
        assert_eq!(
            SharedStateStore::new(sanitize_session_key(&session_name), 11)
                .read_pending_tab_claim(&state.pending_tab_claim_key_for_tab(&state.tabs[0]))
                .unwrap()
                .tab_id,
            *tab_id
        );

        cleanup_session(&session_name);
    }

    #[test]
    fn pipe_update_waits_for_pane_manifest() {
        let session_name = unique_session_name("deferred-pipe");
        cleanup_session(&session_name);
        let schema = test_schema("status");
        let mut state = test_pipe_state(11, &session_name, &[11]);
        state.schema = Some(schema.clone());
        state.width_indexes = vec![WidthIndex::default(); schema.len()];
        state.tabs = vec![TabInfo {
            position: 0,
            name: "Tab #1".to_string(),
            ..Default::default()
        }];

        let payload = UpdatePayload {
            version: 1,
            pane_id: 33,
            updates: BTreeMap::from([("status".to_string(), "READY".to_string())]),
        };

        assert!(!state.handle_update_payload(payload));
        assert_eq!(state.pending_pipe_updates.len(), 1);

        state.pane_manifest = PaneManifest {
            panes: HashMap::from([(0, vec![test_terminal_pane(33)])]),
            ..Default::default()
        };
        state.rebuild_pane_lookup();

        assert!(state.replay_pending_pipe_updates());
        assert!(state.pending_pipe_updates.is_empty());
        let tab_id = state.pending_tab_id_by_position.get(&0).unwrap();
        let row = state.rows_by_tab_id.get(tab_id).unwrap();
        assert_eq!(row.cells[0].as_ref().unwrap().raw_input, "READY");

        cleanup_session(&session_name);
    }

    #[test]
    fn metadata_pipe_update_preserves_persisted_styled_status() {
        let session_name = unique_session_name("styled-status-preserve");
        cleanup_session(&session_name);
        let schema = test_schema("status,directory,branch");
        let mut state = test_pipe_state(11, &session_name, &[11]);
        state.schema = Some(schema.clone());
        state.width_indexes = vec![WidthIndex::default(); schema.len()];
        state.tabs = vec![TabInfo {
            position: 0,
            name: "Tab #1".to_string(),
            ..Default::default()
        }];
        state.pane_manifest = PaneManifest {
            panes: HashMap::from([(0, vec![test_plugin_pane(11), test_terminal_pane(33)])]),
            ..Default::default()
        };
        state.rebuild_pane_lookup();

        let styled_status = "#[fg=#ebdbb2,bg=#504945,bold]IDLE".to_string();
        assert!(state.handle_update_payload(UpdatePayload {
            version: 1,
            pane_id: 33,
            updates: BTreeMap::from([("status".to_string(), styled_status.clone())]),
        }));

        let tab_id = state.pending_tab_id_by_position.get(&0).unwrap().clone();
        let mirrored_name = state
            .rows_by_tab_id
            .get(&tab_id)
            .unwrap()
            .last_mirrored_tab_name
            .clone()
            .unwrap();
        state.tabs[0].name = mirrored_name;

        assert!(state.handle_update_payload(UpdatePayload {
            version: 1,
            pane_id: 33,
            updates: BTreeMap::from([
                ("directory".to_string(), "infrastructure".to_string()),
                ("branch".to_string(), "main".to_string()),
            ]),
        }));

        let row = state.rows_by_tab_id.get(&tab_id).unwrap();
        assert_eq!(row.cells[0].as_ref().unwrap().raw_input, styled_status);

        let persisted = SharedStateStore::new(sanitize_session_key(&session_name), 11)
            .read_persisted_tab_state(&tab_id)
            .unwrap();
        assert_eq!(persisted.cells.get("status").unwrap(), &styled_status);
        assert_eq!(persisted.cells.get("directory").unwrap(), "infrastructure");
        assert_eq!(persisted.cells.get("branch").unwrap(), "main");

        cleanup_session(&session_name);
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
