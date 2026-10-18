//! Shared action definitions and a context-preserving Find Action palette.

use super::{
    zoom,
    App, ServicesNotice, ServicesView, TabKind, results::GridAction, tabs::CloseScope, theme,
};
use egui::{Align, Context, Id, Key, KeyboardShortcut, Layout, Modifiers, RichText};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ActionId {
    Execute,
    ExecuteScript,
    ExplainPlan,
    ExplainAnalyse,
    Stop,
    NewConsole,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    Appearance,
    About,
    DataSources,
    McpServer,
    DatabaseExplorer,
    Files,
    Services,
    Run,
    ActivityMonitor,
    SpeedSearch,
    CollapseAll,
    ScrollFromEditor,
    NextTab,
    PreviousTab,
    CloseTab,
    CloseOtherTabs,
    CloseTabsToRight,
    CloseAllTabs,
    Find,
    Replace,
    GoToLineColumn,
    ReformatCode,
    FoldAll,
    UnfoldAll,
    FindOnCurrentPage,
    RelatedRows,
    CompareData,
    RecordView,
    AggregateView,
    ValueEditor,
    Transpose,
    LocalFilter,
    ClearLocalFilters,
    AddRow,
    DeleteRow,
    CloneRow,
    Submit,
    Revert,
    Reload,
    Commit,
    Rollback,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ActionSurface {
    Sql,
    Grid,
    Explorer,
    #[default]
    Other,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ActionContext {
    pub(super) tab_id: Option<u64>,
    pub(super) result: Option<usize>,
    pub(super) result_id: Option<u64>,
    pub(super) surface: ActionSurface,
    pub(super) focus: Option<Id>,
    pub(super) ds_id: Option<String>,
}

/// A small snapshot of grid capabilities; creating it must not scan its rows.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct GridActionState {
    pub(super) grid_visible: bool,
    pub(super) can_add_row: bool,
    pub(super) can_delete_row: bool,
    pub(super) can_clone_row: bool,
    pub(super) can_submit: bool,
    pub(super) has_selection: bool,
    /// Includes an active inline buffer that will be staged before dispatch.
    pub(super) has_changes: bool,
    pub(super) has_local_filters: bool,
}

pub(super) struct ActionDef {
    pub(super) id: ActionId,
    pub(super) name: &'static str,
    pub(super) shortcut: Option<KeyboardShortcut>,
    /// Lowercase search text, including common names for the same operation.
    keywords: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ActionAvailability {
    pub(super) enabled: bool,
    pub(super) reason: Option<&'static str>,
}

impl ActionAvailability {
    const ENABLED: Self = Self {
        enabled: true,
        reason: None,
    };

    const fn disabled(reason: &'static str) -> Self {
        Self {
            enabled: false,
            reason: Some(reason),
        }
    }
}

#[derive(Default)]
pub(super) struct FindAction {
    pub(super) visible: bool,
    query: String,
    matches: Vec<usize>,
    selected: usize,
    query_dirty: bool,
    focus_query: bool,
    reveal_selected: bool,
    context: Option<ActionContext>,
    pending: Option<(ActionId, ActionContext)>,
    error: Option<&'static str>,
}

pub(super) const FIND_ACTION: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::A);

pub(super) fn explain_mode(id: ActionId) -> Option<crate::query_plan::ExplainMode> {
    match id {
        ActionId::ExplainPlan => Some(crate::query_plan::ExplainMode::Plan),
        ActionId::ExplainAnalyse => Some(crate::query_plan::ExplainMode::Analyse),
        _ => None,
    }
}

const fn definition(
    id: ActionId,
    name: &'static str,
    keywords: &'static str,
    shortcut: Option<KeyboardShortcut>,
) -> ActionDef {
    ActionDef {
        id,
        name,
        keywords,
        shortcut,
    }
}

const fn command(key: Key) -> Option<KeyboardShortcut> {
    Some(KeyboardShortcut::new(Modifiers::COMMAND, key))
}

const fn command_shift(key: Key) -> Option<KeyboardShortcut> {
    Some(KeyboardShortcut::new(
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        key,
    ))
}

pub(super) static ACTIONS: &[ActionDef] = &[
    definition(
        ActionId::Execute,
        "Execute",
        "execute run query statement submit",
        command(Key::Enter),
    ),
    definition(
        ActionId::ExecuteScript,
        "Execute Script",
        "execute script run all statements",
        command_shift(Key::Enter),
    ),
    definition(
        ActionId::ExplainPlan,
        "Explain Plan",
        "explain plan query execution operations estimate",
        None,
    ),
    definition(
        ActionId::ExplainAnalyse,
        "Explain Analyse",
        "explain analyse analyze actual execution statistics time rows run",
        None,
    ),
    definition(
        ActionId::Stop,
        "Stop",
        "stop cancel running query",
        command(Key::F2),
    ),
    definition(
        ActionId::NewConsole,
        "Open New Query Console",
        "open new query console sql",
        command_shift(Key::L),
    ),
    definition(
        ActionId::ZoomIn,
        "Zoom In IDE",
        "zoom in ide bigger larger scale interface font size",
        zoom::ZOOM_IN,
    ),
    definition(
        ActionId::ZoomOut,
        "Zoom Out IDE",
        "zoom out ide smaller scale interface font size",
        zoom::ZOOM_OUT,
    ),
    definition(
        ActionId::ResetZoom,
        "Reset IDE Zoom",
        "reset ide zoom 100 scale interface default size",
        zoom::RESET,
    ),
    definition(
        ActionId::Appearance,
        "Appearance…",
        "appearance settings font size editor grid row height density striped rows stripes zoom theme",
        None,
    ),
    definition(
        ActionId::About,
        "About DBeast",
        "about dbeast version author credits license music",
        None,
    ),
    definition(
        ActionId::DataSources,
        "Data Sources and Drivers",
        "data sources and drivers connections settings",
        None,
    ),
    definition(
        ActionId::DatabaseExplorer,
        "Database Explorer",
        "database explorer tool window",
        command(Key::Num1),
    ),
    definition(
        ActionId::McpServer,
        "MCP Server",
        "mcp server settings agent local permissions sessions sharing",
        None,
    ),
    definition(
        ActionId::Files,
        "Files",
        "files tool window",
        command(Key::Num2),
    ),
    definition(
        ActionId::Services,
        "Services",
        "services results output tool window",
        command(Key::Num8),
    ),
    definition(
        ActionId::ActivityMonitor,
        "Activity Monitor",
        "activity monitor sessions pg_stat_activity locks blocking connections queries dashboard kill cancel terminate",
        None,
    ),
    definition(
        ActionId::Run,
        "Run",
        "run tool window export import dump restore pg_dump progress tasks",
        command(Key::Num4),
    ),
    definition(
        ActionId::SpeedSearch,
        "Speed Search",
        "speed search database explorer filter tree",
        command(Key::F),
    ),
    definition(
        ActionId::CollapseAll,
        "Collapse All",
        "collapse all database explorer tree",
        None,
    ),
    definition(
        ActionId::ScrollFromEditor,
        "Scroll from Editor",
        "scroll from editor database explorer reveal locate object",
        None,
    ),
    definition(
        ActionId::NextTab,
        "Next Tab",
        "next tab select editor",
        command_shift(Key::CloseBracket),
    ),
    definition(
        ActionId::PreviousTab,
        "Previous Tab",
        "previous tab select editor",
        command_shift(Key::OpenBracket),
    ),
    definition(
        ActionId::CloseTab,
        "Close Tab",
        "close tab editor",
        command(Key::W),
    ),
    definition(
        ActionId::CloseOtherTabs,
        "Close Other Tabs",
        "close other tabs editors",
        None,
    ),
    definition(
        ActionId::CloseTabsToRight,
        "Close Tabs to the Right",
        "close tabs to the right editors",
        None,
    ),
    definition(
        ActionId::CloseAllTabs,
        "Close All Tabs",
        "close all tabs editors",
        None,
    ),
    definition(
        ActionId::RelatedRows,
        "Related Rows",
        "go to related rows foreign key referenced referencing",
        Some(KeyboardShortcut::new(Modifiers::NONE, Key::F4)),
    ),
    definition(
        ActionId::CompareData,
        "Compare Data",
        "compare data diff differences fetched pages rows",
        None,
    ),
    definition(ActionId::Find, "Find", "find search text", command(Key::F)),
    definition(
        ActionId::Replace,
        "Replace",
        "replace find text",
        command(Key::R),
    ),
    definition(
        ActionId::GoToLineColumn,
        "Go to Line/Column",
        "go to line column navigate sql",
        command(Key::L),
    ),
    definition(
        ActionId::ReformatCode,
        "Reformat Code",
        "reformat code format sql",
        Some(KeyboardShortcut::new(
            Modifiers::COMMAND.plus(Modifiers::ALT),
            Key::L,
        )),
    ),
    definition(
        ActionId::FoldAll,
        "Fold All",
        "fold all collapse sql",
        command_shift(Key::Minus),
    ),
    definition(
        ActionId::UnfoldAll,
        "Unfold All",
        "unfold all expand sql",
        command_shift(Key::Equals),
    ),
    definition(
        ActionId::FindOnCurrentPage,
        "Find on Current Page",
        "find on current page search grid result",
        command(Key::F),
    ),
    definition(
        ActionId::RecordView,
        "Show Record View",
        "show record view row fields",
        command_shift(Key::Enter),
    ),
    definition(
        ActionId::AggregateView,
        "Show Aggregate View",
        "show aggregate view statistics sum average count median",
        None,
    ),
    definition(
        ActionId::ValueEditor,
        "Open in Value Editor",
        "open in value editor full cell",
        Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
    ),
    definition(
        ActionId::Transpose,
        "Transpose",
        "transpose rows columns grid",
        None,
    ),
    definition(
        ActionId::LocalFilter,
        "Enable Local Filter",
        "enable local filter columns values",
        None,
    ),
    definition(
        ActionId::ClearLocalFilters,
        "Clear Local Filter For All Columns",
        "clear local filter for all columns reset",
        None,
    ),
    definition(
        ActionId::AddRow,
        "Add Row",
        "add row insert new record",
        command(Key::N),
    ),
    definition(
        ActionId::DeleteRow,
        "Delete Row",
        "delete row remove record",
        command(Key::Backspace),
    ),
    definition(
        ActionId::CloneRow,
        "Clone Row",
        "clone row duplicate record",
        command(Key::D),
    ),
    definition(
        ActionId::Submit,
        "Submit",
        "submit apply pending edits changes",
        command(Key::Enter),
    ),
    definition(
        ActionId::Revert,
        "Revert",
        "revert discard pending edits changes",
        None,
    ),
    definition(
        ActionId::Reload,
        "Reload Page",
        "reload page refresh result",
        None,
    ),
    definition(ActionId::Commit, "Commit", "commit transaction", None),
    definition(
        ActionId::Rollback,
        "Roll Back",
        "roll back rollback transaction",
        None,
    ),
];

pub(super) fn action_definition(id: ActionId) -> &'static ActionDef {
    ACTIONS
        .iter()
        .find(|definition| definition.id == id)
        .expect("registered action")
}

/// Preserve the platform's modifier notation while showing literal bracket keys.
pub(super) fn format_action_shortcut(ctx: &Context, shortcut: &KeyboardShortcut) -> String {
    let mut text = ctx.format_shortcut(shortcut);
    if matches!(shortcut.logical_key, Key::OpenBracket | Key::CloseBracket)
        && let Some(prefix) = text.strip_suffix(shortcut.logical_key.name())
    {
        let prefix_len = prefix.len();
        text.truncate(prefix_len);
        text.push_str(shortcut.logical_key.symbol_or_name());
    }
    text
}

impl FindAction {
    fn refresh_matches(&mut self) -> bool {
        if !std::mem::take(&mut self.query_dirty) {
            return false;
        }
        self.matches.clear();
        let query = self.query.trim().to_lowercase();
        self.matches
            .extend(ACTIONS.iter().enumerate().filter_map(|(index, action)| {
                query
                    .split_whitespace()
                    .all(|word| {
                        action
                            .keywords
                            .split_whitespace()
                            .any(|candidate| candidate.starts_with(word))
                    })
                    .then_some(index)
            }));
        self.matches
            .sort_by_key(|&index| !ACTIONS[index].keywords.starts_with(&query));
        self.selected = 0;
        self.reveal_selected = true;
        self.error = None;
        true
    }

    fn move_selection(&mut self, backwards: bool) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = if backwards {
            self.selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1)
        } else {
            (self.selected + 1) % self.matches.len()
        };
        self.error = None;
        self.reveal_selected = true;
    }
}

impl App {
    pub(super) fn capture_action_context(&self, ctx: &Context) -> ActionContext {
        let focus = ctx.memory(|memory| memory.focused());
        let explorer_focused = super::explorer::action_has_focus(ctx);
        let Some(tab) = self.tabs.get(self.active) else {
            return ActionContext {
                focus,
                surface: if explorer_focused {
                    ActionSurface::Explorer
                } else {
                    ActionSurface::Other
                },
                ds_id: self.current_ds(),
                ..Default::default()
            };
        };
        let result = match tab.kind {
            TabKind::Table { .. } => (!tab.results.is_empty()).then_some(0),
            TabKind::Console { .. } => match tab.view {
                ServicesView::Result(index) if index < tab.results.len() => Some(index),
                _ => None,
            },
        };
        let grid_focused = result.is_some_and(|index| tab.results[index].action_has_focus(ctx));
        let surface = if explorer_focused {
            ActionSurface::Explorer
        } else if grid_focused {
            ActionSurface::Grid
        } else if super::console::action_has_focus(tab, ctx)
            || matches!(tab.kind, TabKind::Console { .. })
        {
            ActionSurface::Sql
        } else if result.is_some() {
            ActionSurface::Grid
        } else {
            ActionSurface::Other
        };
        ActionContext {
            tab_id: Some(tab.id),
            result,
            result_id: result.map(|index| tab.results[index].action_identity()),
            surface,
            focus,
            ds_id: Some(tab.ds_id.clone()),
        }
    }

    pub(super) fn open_find_action(&mut self, ctx: &Context) {
        if self.pending_close.is_some()
            || self.dialog.is_some()
            || self.about_open()
            || self.scope_chooser.is_some()
            || self.related_rows.is_open()
            || self.structure_compare.is_open()
            || self.compare.is_open()
        {
            return;
        }
        if self.find_action.visible {
            self.find_action.focus_query = true;
            return;
        }
        let context = self.capture_action_context(ctx);
        self.find_action.visible = true;
        self.find_action.context = Some(context);
        self.find_action.query.clear();
        self.find_action.query_dirty = true;
        self.find_action.focus_query = true;
        self.find_action.error = None;
        ctx.request_repaint();
    }

    pub(super) fn action_availability(
        &self,
        id: ActionId,
        context: &ActionContext,
    ) -> ActionAvailability {
        use ActionId::*;
        let disabled = ActionAvailability::disabled;
        if self.pending_close.is_some()
            || self.dialog.is_some()
            || self.about_open()
            || self.scope_chooser.is_some()
            || self.related_rows.is_open()
            || self.structure_compare.is_open()
        {
            return disabled("Close the current dialog first");
        }
        if matches!(
            id,
            DataSources | McpServer | DatabaseExplorer | Files | Services | Run | ZoomIn | ZoomOut | ResetZoom
                | Appearance
                | About
        ) {
            return ActionAvailability::ENABLED;
        }
        if id == ActivityMonitor {
            return match context.ds_id.as_deref().and_then(|id| self.source(id)) {
                Some(ds) if ds.driver == crate::model::DriverKind::Postgres && !ds.id.starts_with("file:") => ActionAvailability::ENABLED,
                _ => disabled("Select a PostgreSQL data source"),
            };
        }
        if id == SpeedSearch || (id == Find && context.surface == ActionSurface::Explorer) {
            return ActionAvailability::ENABLED;
        }
        if id == CollapseAll {
            return if self.expanded.is_empty() {
                disabled("No expanded database objects")
            } else {
                ActionAvailability::ENABLED
            };
        }
        if id == NewConsole {
            return if context
                .ds_id
                .as_deref()
                .is_some_and(|id| self.source(id).is_some())
            {
                ActionAvailability::ENABLED
            } else {
                disabled("Select a data source first")
            };
        }
        let Some(index) = context
            .tab_id
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
        else {
            return disabled("Open an editor tab first");
        };
        let tab = &self.tabs[index];
        if let Some(mode) = explain_mode(id) {
            return self
                .explain_blocked_reason(index, mode)
                .map_or(ActionAvailability::ENABLED, disabled);
        }
        if matches!(
            id,
            Execute | ExecuteScript | Submit | Reload | Commit | Rollback
        ) && self.source_is_saving(&tab.ds_id)
        {
            return disabled("Wait for the data source file to finish saving");
        }
        let console = matches!(tab.kind, TabKind::Console { .. });
        let result = context.result.and_then(|index| tab.results.get(index));
        let targets_result = matches!(
            id,
            FindOnCurrentPage
                | RelatedRows
                | CompareData
                | RecordView
                | AggregateView
                | ValueEditor
                | Transpose
                | LocalFilter
                | ClearLocalFilters
                | AddRow
                | DeleteRow
                | CloneRow
                | Submit
                | Revert
                | Reload
        ) || (id == Find && context.surface != ActionSurface::Sql)
            || (id == Execute
                && (!console || context.surface == ActionSurface::Grid)
                && context.result.is_some());
        if targets_result
            && context.result.is_some()
            && result.is_none_or(|result| Some(result.action_identity()) != context.result_id)
        {
            return disabled("The result changed; reopen Find Action");
        }
        if id == Reload
            && let Some(result_index) = context.result
            && let Some(reason) = self.result_replay_blocked_reason(index, result_index)
        {
            return disabled(reason);
        }
        if tab.bound_query.is_some() && matches!(id, ExecuteScript | Replace | ReformatCode) {
            return disabled("Related rows use a fixed query with bound parameters");
        }
        if id == CompareData {
            return context.result.map_or_else(
                || disabled("Select a result grid first"),
                |result| {
                    self.compare_blocked_reason(index, result)
                        .map_or(ActionAvailability::ENABLED, disabled)
                },
            );
        }
        if id == RelatedRows {
            return context.result.map_or_else(
                || disabled("Select a result grid first"),
                |result| {
                    self.related_rows_blocked_reason(index, result)
                        .map_or(ActionAvailability::ENABLED, disabled)
                },
            );
        }
        let grid = result.map(|result| result.action_state());
        match id {
            ScrollFromEditor if self.source(&tab.ds_id).is_none() => {
                disabled("Choose a data source for this tab")
            }
            ScrollFromEditor => ActionAvailability::ENABLED,
            NextTab | PreviousTab | CloseOtherTabs if self.tabs.len() < 2 => {
                disabled("No other editor tabs are open")
            }
            CloseTabsToRight if index + 1 >= self.tabs.len() => {
                disabled("No editor tabs to the right")
            }
            NextTab | PreviousTab | CloseTab | CloseOtherTabs | CloseTabsToRight | CloseAllTabs => {
                ActionAvailability::ENABLED
            }
            Stop if tab.is_running() => ActionAvailability::ENABLED,
            Stop => disabled("No query is running in this tab"),
            Execute | ExecuteScript | Commit | Rollback | Submit | Reload
                if tab.context_pending.is_some() =>
            {
                disabled("Wait for the schema change to finish")
            }
            Execute | ExecuteScript | Commit | Rollback | Submit | Reload if tab.is_running() => {
                disabled("Wait for the running query to finish")
            }
            Revert if matches!(tab.running, Some(super::RunKind::Edits { .. })) => {
                disabled("Wait for the pending changes to finish submitting")
            }
            ExecuteScript if !console => disabled("Open a query console to execute a script"),
            Execute | ExecuteScript if self.source(&tab.ds_id).is_none() => {
                disabled("Choose a data source for this tab")
            }
            Execute
                if (!console || context.surface == ActionSurface::Grid)
                    && grid.is_some_and(|grid| grid.has_changes && !grid.can_submit) =>
            {
                disabled("This result is read-only; revert the pending changes before executing")
            }
            Execute | ExecuteScript => ActionAvailability::ENABLED,
            Commit | Rollback if !tab.in_tx => disabled("No transaction is active in this tab"),
            Commit | Rollback => ActionAvailability::ENABLED,
            Find if context.surface == ActionSurface::Sql && console => ActionAvailability::ENABLED,
            Replace | GoToLineColumn | ReformatCode | FoldAll | UnfoldAll if !console => {
                disabled("This action requires a query console")
            }
            UnfoldAll if tab.folds.is_empty() => disabled("No SQL regions are folded"),
            Replace | GoToLineColumn | ReformatCode | FoldAll | UnfoldAll => {
                ActionAvailability::ENABLED
            }
            _ => {
                let Some(grid) = grid else {
                    return disabled("Select a result grid first");
                };
                if !grid.grid_visible && !matches!(id, Submit | Revert | Reload) {
                    return disabled("Switch from Geo Viewer to the result grid");
                }
                match id {
                    Submit if !grid.can_submit => disabled("This result is read-only"),
                    AddRow if !grid.can_add_row => {
                        disabled("Adding rows is available only in table editors")
                    }
                    AddRow | DeleteRow | CloneRow if tab.is_running() => {
                        disabled("Wait for the running query to finish")
                    }
                    DeleteRow | CloneRow | ValueEditor if !grid.has_selection => {
                        disabled("Select a cell first")
                    }
                    DeleteRow if !grid.can_delete_row => {
                        disabled("Deleting rows is available only in table editors")
                    }
                    CloneRow if !grid.can_clone_row => disabled("This row cannot be cloned"),
                    Submit | Revert if !grid.has_changes => {
                        disabled("There are no pending changes")
                    }
                    Reload if grid.has_changes => {
                        disabled("Submit or revert pending changes before reloading")
                    }
                    ClearLocalFilters if !grid.has_local_filters => {
                        disabled("No local filters are set")
                    }
                    _ => ActionAvailability::ENABLED,
                }
            }
        }
    }

    fn action_checked(&self, id: ActionId, context: &ActionContext) -> bool {
        match id {
            ActionId::DatabaseExplorer => return self.show_explorer,
            ActionId::Files => return self.show_files,
            ActionId::Services => return self.show_services && !self.show_run,
            ActionId::Run => return self.show_run,
            _ => {}
        }
        let Some(result) = context
            .tab_id
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .and_then(|tab| context.result.and_then(|index| tab.results.get(index)))
            .filter(|result| Some(result.action_identity()) == context.result_id)
        else {
            return false;
        };
        match id {
            ActionId::RecordView => result.record_view,
            ActionId::AggregateView => result.aggregate_view,
            ActionId::ValueEditor => result.show_value_editor,
            ActionId::Transpose => result.transposed,
            _ => false,
        }
    }

    /// Shared by menu items, the palette and shortcut handlers. Never synthesize keys.
    pub(super) fn dispatch_action(
        &mut self,
        id: ActionId,
        context: &ActionContext,
        ctx: &Context,
    ) -> bool {
        use ActionId::*;
        let availability = self.action_availability(id, context);
        if !availability.enabled {
            self.notice = Some(ServicesNotice::info(format!(
                "{}: {}",
                action_definition(id).name,
                availability.reason.unwrap_or("Unavailable")
            )));
            return false;
        }
        let index = context
            .tab_id
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id));
        match id {
            ZoomIn => self.zoom_ide(ctx, zoom::Step::In),
            ZoomOut => self.zoom_ide(ctx, zoom::Step::Out),
            ResetZoom => self.zoom_ide(ctx, zoom::Step::Reset),
            Appearance => self.open_appearance(),
            About => self.open_about(),
            DataSources => self.open_data_sources(),
            McpServer => self.open_mcp_settings(),
            DatabaseExplorer | Files | Services | Run => self.apply_tool_window_action(id, ctx),
            ActivityMonitor => {
                if let Some(ds_id) = context.ds_id.clone() {
                    let database = self.tabs.get(self.active).filter(|t| t.ds_id == ds_id).and_then(|t| t.database.clone());
                    self.open_activity_monitor(&ds_id, database);
                }
            }
            SpeedSearch => self.focus_explorer_search(ctx),
            Find if context.surface == ActionSurface::Explorer => self.focus_explorer_search(ctx),
            CollapseAll => {
                self.show_explorer = true;
                self.show_files = false;
                self.pending_editor_focus = false;
                self.pending_action_focus = None;
                self.collapse_explorer();
            }
            NewConsole => {
                self.stage_active_cell();
                self.new_console(context.ds_id.clone().expect("enabled data source"));
                self.focus_active_editor(ctx);
            }
            NextTab | PreviousTab => {
                if let Some(id) = context.tab_id {
                    self.select_tab_by_id(id);
                }
                self.select_adjacent_tab(id == PreviousTab);
                self.focus_active_editor(ctx);
            }
            CloseTab | CloseOtherTabs | CloseTabsToRight | CloseAllTabs => {
                let scope = match id {
                    CloseTab => CloseScope::Tab,
                    CloseOtherTabs => CloseScope::OtherTabs,
                    CloseTabsToRight => CloseScope::TabsToRight,
                    _ => CloseScope::AllTabs,
                };
                self.request_close_tabs(context.tab_id.expect("enabled tab"), scope);
                if self.pending_close.is_none() {
                    self.focus_active_editor(ctx);
                }
            }
            _ => {
                let index = index.expect("enabled tab");
                if self.active != index {
                    self.select_tab_by_id(self.tabs[index].id);
                }
                match id {
                    ScrollFromEditor => {
                        self.pending_editor_focus = false;
                        self.pending_action_focus = None;
                        self.show_files = false;
                        self.scroll_from_editor();
                    }
                    Execute => {
                        if (matches!(self.tabs[index].kind, TabKind::Table { .. })
                            || context.surface == ActionSurface::Grid)
                            && let Some(result) = context.result
                        {
                            self.tabs[index].results[result].stage_action_buffer();
                            if !self.tabs[index].results[result].edits.is_empty() {
                                self.grid_action(index, result, GridAction::Submit);
                            } else {
                                self.run_tab(index, false);
                            }
                        } else {
                            self.run_tab(index, false);
                        }
                    }
                    ExecuteScript => self.run_tab(index, true),
                    ExplainPlan | ExplainAnalyse => {
                        self.explain_tab(index, explain_mode(id).unwrap());
                    }
                    Stop => {
                        self.tabs[index].log(super::results::LogKind::Info, "cancel requested");
                        self.backend.cancel(self.tabs[index].id);
                    }
                    Commit | Rollback => self.end_transaction(index, id == Commit),
                    Find if context.surface == ActionSurface::Sql => {
                        return self.apply_console_action(index, Find, ctx);
                    }
                    Replace | GoToLineColumn | ReformatCode | FoldAll | UnfoldAll => {
                        return self.apply_console_action(index, id, ctx);
                    }
                    _ => {
                        let result = context.result.expect("enabled result");
                        if matches!(self.tabs[index].kind, TabKind::Console { .. }) {
                            self.show_services = true;
                            self.tabs[index].view = ServicesView::Result(result);
                        }
                        self.tabs[index].results[result].stage_action_buffer();
                        match id {
                            Submit => self.grid_action(index, result, GridAction::Submit),
                            Reload => self.grid_action(index, result, GridAction::Rerun),
                            RelatedRows => self.request_related_rows(index, result),
                            CompareData => self.open_compare_data(index, result),
                            _ => {
                                let local = if id == Find { FindOnCurrentPage } else { id };
                                return self.tabs[index].results[result]
                                    .apply_local_action(local, ctx);
                            }
                        }
                    }
                }
            }
        }
        ctx.request_repaint();
        true
    }

    pub(super) fn handle_action_shortcuts(&mut self, ctx: &Context) {
        if ctx.input_mut(|input| input.consume_shortcut(&FIND_ACTION)) {
            self.open_find_action(ctx);
            return;
        }
        if self.find_action.visible
            || self.related_rows.is_open()
            || self.structure_compare.is_open()
            || self.compare.is_open()
            || self.pending_close.is_some()
            || self.dialog.is_some()
            || self.about_open()
            || self.scope_chooser.is_some()
        {
            return;
        }
        if ctx.input(|input| input.key_pressed(Key::F4))
            && !ctx.text_edit_focused()
            && self.tabs.get(self.active).is_some_and(|tab| {
                tab.results
                    .iter()
                    .any(|result| result.related_action_has_focus(ctx))
            })
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F4))
        {
            let context = self.capture_action_context(ctx);
            self.dispatch_action(ActionId::RelatedRows, &context, ctx);
            return;
        }
        // Zoom IDE is matched by physical key: with ⌥ and ⇧ the logical key is a
        // symbol that differs between keyboard layouts.
        if let Some(step) = zoom::pressed(ctx) {
            let id = match step {
                zoom::Step::In => ActionId::ZoomIn,
                zoom::Step::Out => ActionId::ZoomOut,
                zoom::Step::Reset => ActionId::ResetZoom,
            };
            let context = self.capture_action_context(ctx);
            self.dispatch_action(id, &context, ctx);
            return;
        }
        // Text editing shortcuts remain scoped to the editor/grid that owns focus.
        for id in [
            ActionId::NewConsole,
            ActionId::DatabaseExplorer,
            ActionId::Files,
            ActionId::Services,
            ActionId::Run,
            ActionId::NextTab,
            ActionId::PreviousTab,
            ActionId::CloseTab,
        ] {
            let shortcut = action_definition(id)
                .shortcut
                .expect("global action shortcut");
            if ctx.input_mut(|input| input.consume_shortcut(&shortcut)) {
                let context = self.capture_action_context(ctx);
                self.dispatch_action(id, &context, ctx);
                break;
            }
        }
    }

    fn restore_action_focus(&mut self, context: &ActionContext) {
        if context
            .tab_id
            .is_none_or(|id| self.tabs.iter().any(|tab| tab.id == id))
            && let Some(focus) = context.focus
        {
            // Modal backgrounds are disabled this frame, so requesting focus now
            // would be discarded before those widgets become focusable again.
            self.pending_action_focus = Some(focus);
        }
    }

    pub(super) fn apply_pending_action(&mut self, ctx: &Context) {
        if self.find_action.visible
            || self.related_rows.is_open()
            || self.structure_compare.is_open()
            || self.compare.is_open()
            || self.pending_close.is_some()
            || ctx.memory(|memory| memory.top_modal_layer().is_some())
        {
            return;
        }
        if let Some((action, context)) = self.find_action.pending.take() {
            // Modal layers survive until the end of the next pass. Execute only
            // once the target widgets can accept the action's focus requests.
            self.restore_action_focus(&context);
            self.dispatch_action(action, &context, ctx);
        }
    }

    pub(super) fn find_action_window(&mut self, ctx: &Context) {
        if !self.find_action.visible {
            return;
        }
        let mut state = std::mem::take(&mut self.find_action);
        let Some(context) = state.context.take() else {
            return;
        };
        let mut activate = None;
        let mut close = false;
        let width = (ctx.content_rect().width() - 40.0).clamp(220.0, 640.0);
        let modal = egui::Modal::new(Id::new("find_action"))
            .area(egui::Modal::default_area(Id::new("find_action")).anchor(
                egui::Align2::CENTER_TOP,
                egui::vec2(0.0, ctx.content_rect().height() * 0.18),
            ))
            // A popup, so the workspace stays readable behind it.
            .backdrop_color(egui::Color32::from_black_alpha(36))
            .frame(
                egui::Frame::popup(&ctx.style_of(ctx.theme()))
                    .fill(theme::BG_PANEL)
                    .inner_margin(10),
            )
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Find Action").strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let response = theme::tool_button(ui, "×", "Close Find Action");
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                "Close Find Action",
                            )
                        });
                        close |= response.clicked();
                    });
                });
                // Consume navigation before the single-line editor handles Enter
                // and surrenders focus; the palette owns these keys while open.
                let (previous, next, enter, escape) = ui.input_mut(|input| {
                    (
                        input.consume_key(Modifiers::NONE, Key::ArrowUp),
                        input.consume_key(Modifiers::NONE, Key::ArrowDown),
                        input.consume_key(Modifiers::NONE, Key::Enter),
                        input.consume_key(Modifiers::NONE, Key::Escape),
                    )
                });
                close |= escape;
                let query = ui.add(
                    egui::TextEdit::singleline(&mut state.query)
                        .id(Id::new("find_action_query"))
                        .hint_text("Type an action name")
                        .desired_width(f32::INFINITY),
                );
                query.widget_info(|| {
                    let mut info =
                        egui::WidgetInfo::text_edit(true, &state.query, &state.query, "");
                    info.label = Some("Find Action".into());
                    info
                });
                if std::mem::take(&mut state.focus_query) {
                    query.request_focus();
                }
                state.query_dirty |= query.changed();
                if state.refresh_matches() {
                    state.selected = state
                        .matches
                        .iter()
                        .position(|&index| {
                            self.action_availability(ACTIONS[index].id, &context)
                                .enabled
                        })
                        .unwrap_or(0);
                }
                if previous || next {
                    state.move_selection(previous);
                }
                if enter {
                    activate = state
                        .matches
                        .get(state.selected)
                        .map(|&index| ACTIONS[index].id);
                }
                ui.separator();
                if state.matches.is_empty() {
                    ui.label(RichText::new("No actions found").color(theme::TEXT_MUTED));
                } else {
                    let reveal = std::mem::take(&mut state.reveal_selected);
                    egui::ScrollArea::vertical()
                        .id_salt("find_action_results")
                        .max_height(310.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (position, &index) in state.matches.iter().enumerate() {
                                let action = &ACTIONS[index];
                                let availability = self.action_availability(action.id, &context);
                                let selected = position == state.selected;
                                let label = if self.action_checked(action.id, &context) {
                                    RichText::new(format!("✓ {}", action.name))
                                } else {
                                    RichText::new(action.name)
                                };
                                let mut button = egui::Button::selectable(selected, label);
                                if let Some(shortcut) = action.shortcut {
                                    button = button
                                        .shortcut_text(format_action_shortcut(ctx, &shortcut));
                                }
                                let response = ui
                                    .push_id(action.id, |ui| {
                                        ui.add_enabled(
                                            availability.enabled,
                                            button.min_size(egui::vec2(ui.available_width(), 28.0)),
                                        )
                                    })
                                    .inner;
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::Button,
                                        availability.enabled,
                                        selected,
                                        action.name,
                                    )
                                });
                                if response.clicked() {
                                    state.selected = position;
                                    activate = Some(action.id);
                                }
                                if reveal && selected {
                                    response.scroll_to_me(Some(Align::Center));
                                }
                                if let Some(reason) = availability.reason {
                                    response.on_disabled_hover_text(reason);
                                }
                            }
                        });
                }
                let selected_reason = state.matches.get(state.selected).and_then(|&index| {
                    self.action_availability(ACTIONS[index].id, &context).reason
                });
                ui.separator();
                ui.label(
                    RichText::new(
                        state
                            .error
                            .or(selected_reason)
                            .unwrap_or("↑↓ to navigate · Enter to execute · Esc to close"),
                    )
                    .small()
                    .color(theme::TEXT_MUTED),
                );
            });
        close |= modal.should_close();
        if close {
            self.restore_action_focus(&context);
            state.visible = false;
        } else if let Some(action) = activate {
            let availability = self.action_availability(action, &context);
            if availability.enabled {
                state.pending = Some((action, context.clone()));
                state.visible = false;
                ctx.request_repaint();
            } else {
                state.error = availability.reason;
                state.focus_query = true;
            }
        }
        if state.visible {
            state.context = Some(context);
        }
        self.find_action = state;
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// No configuration/session load, environment mutation, disk writes or queries.
    pub(in crate::ui) fn isolated_app(ctx: &Context) -> App {
        App {
            branding: Default::default(),
            startup_splash: Default::default(),
            integrated_titlebar: false,
            backend: crate::backend::Backend::new(ctx.clone()),
            config: crate::config::Config::default(),
            sources: Default::default(),
            meta: Default::default(),
            metadata_loading: Default::default(),
            metadata_request_id: 0,
            metadata_errors: Default::default(),
            expanded: Default::default(),
            selected: None,
            explorer_ui: Default::default(),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            dialog: None,
            group_dialog: None,
            config_loaded: true,
            config_loading_error: None,
            config_apply_pending: None,
            config_dirty: false,
            config_saves_pending: 0,
            closing: false,
            close_ready: false,
            filter: String::new(),
            show_explorer: true,
            show_services: true,
            show_files: false,
            show_run: false,
            transfers: Default::default(),
            export_dialog: None,
            import_dialog: None,
            dump_dialog: None,
            activity: Vec::new(),
            activity_next_id: 0,
            transfer_request: 0,
            files_panel: Default::default(),
            files: Vec::new(),
            notice: None,
            tiles: super::super::map::TileCache::new(String::new(), std::env::temp_dir()),
            scope_chooser: None,
            session_writer: Default::default(),
            session_loading: None,
            find_action: Default::default(),
            appearance_dialog: None,
            about: Default::default(),
            related_rows: Default::default(),
            compare: Default::default(),
            structure_compare: Default::default(),
            schema_graph: Default::default(),
            graph_layouts: Default::default(),
            pending_close: None,
            reveal_active_tab: false,
            pending_editor_focus: false,
            pending_action_focus: None,
            mcp: Default::default(),
        }
    }

    fn result() -> super::super::ResultSet {
        super::super::ResultSet::new(
            "Result".into(),
            "select 1".into(),
            crate::model::QueryResult::default(),
            crate::model::Page::first(500),
        )
    }

    #[test]
    fn dispatch_rechecks_result_identity_and_tab_existence() {
        let ctx = Context::default();
        let mut app = isolated_app(&ctx);
        let source = crate::model::DataSource::new(crate::model::DriverKind::Sqlite);
        let source_id = source.id.clone();
        app.config.data_sources.push(source);
        app.add_tab(
            "Console".into(),
            source_id,
            TabKind::Console {
                sql: "select 1".into(),
            },
        );
        app.tabs[0].results.push(result());
        app.tabs[0].view = ServicesView::Result(0);
        let mut context = app.capture_action_context(&ctx);
        context.surface = ActionSurface::Grid;
        assert!(
            app.action_availability(ActionId::AggregateView, &context)
                .enabled
        );

        // An identical SQL string and index still represent a different result.
        app.tabs[0].results[0] = result();
        assert!(!app.dispatch_action(ActionId::AggregateView, &context, &ctx));
        assert!(!app.tabs[0].results[0].aggregate_view);
        assert!(!app.action_availability(ActionId::Execute, &context).enabled);
        let sql_context = ActionContext {
            surface: ActionSurface::Sql,
            ..context.clone()
        };
        assert!(
            app.action_availability(ActionId::Execute, &sql_context)
                .enabled
        );
        assert!(
            app.action_availability(ActionId::ReformatCode, &context)
                .enabled
        );

        app.tabs[0].kind = TabKind::Table {
            schema: None,
            table: "records".into(),
            filter: String::new(),
            order: String::new(),
            select_rowid: false,
        };
        assert!(!app.dispatch_action(ActionId::Execute, &context, &ctx));
        app.tabs[0].results.clear();
        assert!(!app.dispatch_action(ActionId::Submit, &context, &ctx));
        assert!(!app.dispatch_action(ActionId::Execute, &context, &ctx));
        // A fresh context after a failed initial fetch can retry the table query.
        // Only a context that previously targeted a now-missing result is stale.
        let retry = app.capture_action_context(&ctx);
        assert_eq!(retry.result, None);
        assert!(app.action_availability(ActionId::Execute, &retry).enabled);
        assert!(!app.action_availability(ActionId::Submit, &retry).enabled);
        app.tabs.clear();
        assert!(!app.dispatch_action(ActionId::Execute, &context, &ctx));
    }

    #[test]
    fn reload_revalidation_preserves_pending_rows_and_checks_running_state() {
        let ctx = Context::default();
        let mut app = isolated_app(&ctx);
        app.add_tab(
            "Console".into(),
            "test".into(),
            TabKind::Console {
                sql: "select 1".into(),
            },
        );
        app.tabs[0].results.push(result());
        app.tabs[0].view = ServicesView::Result(0);
        let context = app.capture_action_context(&ctx);
        assert!(app.action_availability(ActionId::Reload, &context).enabled);
        app.tabs[0].results[0].edits.inserted.push(Vec::new());
        assert!(!app.dispatch_action(ActionId::Reload, &context, &ctx));
        assert_eq!(app.tabs[0].results[0].edits.inserted.len(), 1);
        assert!(!app.action_availability(ActionId::Stop, &context).enabled);
        app.tabs[0].running = Some(super::super::RunKind::Query);
        assert!(app.action_availability(ActionId::Stop, &context).enabled);
        assert!(!app.action_availability(ActionId::Reload, &context).enabled);
    }

    #[test]
    fn grid_execute_and_revert_respect_console_mutation_authority() {
        let ctx = Context::default();
        let mut app = isolated_app(&ctx);
        let source = crate::model::DataSource::new(crate::model::DriverKind::Sqlite);
        let source_id = source.id.clone();
        app.config.data_sources.push(source);
        app.add_tab(
            "Console".into(),
            source_id,
            TabKind::Console {
                sql: "select 1".into(),
            },
        );
        app.tabs[0].results.push(result());
        app.tabs[0].view = ServicesView::Result(0);
        let mut context = app.capture_action_context(&ctx);
        context.surface = ActionSurface::Grid;
        app.tabs[0].results[0]
            .edits
            .changed
            .insert((0, 0), crate::sqlgen::CellEdit::Text("reference".into()));
        assert!(!app.action_availability(ActionId::Execute, &context).enabled);
        assert!(!app.action_availability(ActionId::Submit, &context).enabled);
        assert!(app.action_availability(ActionId::Revert, &context).enabled);
        let sql_context = ActionContext {
            surface: ActionSurface::Sql,
            ..context.clone()
        };
        assert!(
            app.action_availability(ActionId::Execute, &sql_context)
                .enabled
        );
        app.tabs[0].running = Some(super::super::RunKind::Edits {
            result_id: context.result_id.unwrap(),
            request_id: 1,
        });
        assert!(!app.dispatch_action(ActionId::Revert, &context, &ctx));
        assert_eq!(app.tabs[0].results[0].edits.changed.len(), 1);
    }

    #[test]
    fn grid_commands_reveal_the_captured_console_result() {
        let ctx = Context::default();
        let mut app = isolated_app(&ctx);
        app.add_tab(
            "Console".into(),
            "test".into(),
            TabKind::Console {
                sql: "select 1".into(),
            },
        );
        app.tabs[0].results.push(result());
        app.tabs[0].view = ServicesView::Result(0);
        let context = app.capture_action_context(&ctx);
        app.show_services = false;
        app.tabs[0].view = ServicesView::Output;
        assert!(app.dispatch_action(ActionId::AggregateView, &context, &ctx));
        assert!(app.show_services);
        assert!(matches!(app.tabs[0].view, ServicesView::Result(0)));
        assert!(app.action_checked(ActionId::AggregateView, &context));
        assert!(app.dispatch_action(ActionId::RecordView, &context, &ctx));
        assert!(app.action_checked(ActionId::RecordView, &context));
        assert!(!app.action_checked(ActionId::AggregateView, &context));
    }

    #[test]
    fn palette_focus_restoration_is_deferred_until_background_is_enabled() {
        let ctx = Context::default();
        let mut app = isolated_app(&ctx);
        let focus = Id::new("previous_search_button");
        app.restore_action_focus(&ActionContext {
            focus: Some(focus),
            ..Default::default()
        });
        assert_eq!(app.pending_action_focus, Some(focus));
        assert_eq!(ctx.memory(|memory| memory.focused()), None);
        app.focus_active_editor(&ctx);
        assert!(app.pending_editor_focus);
        assert_eq!(app.pending_action_focus, None);
    }

    #[test]
    fn search_matches_words_aliases_and_caches_unchanged_queries() {
        let mut state = FindAction {
            query: "  NEW query  ".into(),
            query_dirty: true,
            ..Default::default()
        };
        assert!(state.refresh_matches());
        assert_eq!(
            state
                .matches
                .iter()
                .map(|&index| ACTIONS[index].id)
                .collect::<Vec<_>>(),
            [ActionId::NewConsole]
        );
        let allocation = state.matches.as_ptr();
        for _ in 0..100 {
            assert!(!state.refresh_matches());
            assert_eq!(state.matches.as_ptr(), allocation);
        }
        state.query = "grid transpose".into();
        state.query_dirty = true;
        assert!(state.refresh_matches());
        assert_eq!(ACTIONS[state.matches[0]].id, ActionId::Transpose);
        state.query = "żółć unknown".into();
        state.query_dirty = true;
        state.refresh_matches();
        assert!(state.matches.is_empty());
    }

    #[test]
    fn keyboard_selection_wraps_and_empty_results_are_safe() {
        let mut state = FindAction {
            query: "tab".into(),
            query_dirty: true,
            ..Default::default()
        };
        state.refresh_matches();
        assert_eq!(ACTIONS[state.matches[0]].id, ActionId::NextTab);
        state.move_selection(true);
        assert_eq!(state.selected, state.matches.len() - 1);
        state.move_selection(false);
        assert_eq!(state.selected, 0);
        state.move_selection(false);
        assert_eq!(
            ACTIONS[state.matches[state.selected]].id,
            ActionId::PreviousTab
        );
        state.matches.clear();
        state.move_selection(true);
        state.move_selection(false);
    }

    #[test]
    fn compare_data_is_found_by_its_full_visible_name() {
        let mut state = FindAction {
            query: "Compare Data".into(),
            query_dirty: true,
            ..Default::default()
        };
        state.refresh_matches();
        assert_eq!(
            state
                .matches
                .iter()
                .map(|&index| ACTIONS[index].id)
                .collect::<Vec<_>>(),
            [ActionId::CompareData]
        );
    }

    #[test]
    fn every_action_has_a_unique_definition_and_search_name() {
        let mut ids = std::collections::HashSet::new();
        for definition in ACTIONS {
            assert!(ids.insert(definition.id));
            assert!(!definition.name.is_empty());
            assert!(
                definition
                    .keywords
                    .chars()
                    .all(|character| !character.is_uppercase())
            );
        }
    }
}
