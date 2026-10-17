//! Lazy foreign-key navigation. A chooser pins result/row identities, never a
//! display-row offset; only the selected FK values are copied at dispatch.

use super::{App, ServicesNotice, theme};
use crate::backend::{RelatedMetadataRequest, RelatedSourceGuard};
use crate::model::{
    DriverKind, ForeignKey, RelatedDirection, RelatedMetadata, ResolvedContext, SourceIdentity,
    Value,
};
use egui::{Context, Id, Key, Modifiers, RichText};
use std::sync::Arc;

#[derive(Default)]
pub(super) struct RelatedRowsState {
    popup: Option<Chooser>,
    next_request: u64,
}

impl RelatedRowsState {
    pub(super) fn is_open(&self) -> bool {
        self.popup.is_some()
    }
    pub(super) fn cancel(&mut self) {
        self.popup = None;
    }
    fn next_request(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.next_request
    }
}

struct Anchor {
    tab_id: u64,
    result_id: u64,
    row: usize,
    session_epoch: u64,
    source: SourceIdentity,
    context: ResolvedContext,
    focus: Option<Id>,
}
struct Choice {
    relation: ForeignKey,
    label: String,
    description: String,
    reason: Option<String>,
}
struct Group {
    direction: RelatedDirection,
    request_id: u64,
    loading: bool,
    error: Option<String>,
    choices: Vec<Choice>,
}
impl Group {
    fn new(direction: RelatedDirection, request_id: u64) -> Self {
        Self {
            direction,
            request_id,
            loading: true,
            error: None,
            choices: Vec::new(),
        }
    }
}
struct Chooser {
    anchor: Anchor,
    groups: [Group; 2],
    // Rebuilt only after metadata arrives, not while painting or pressing arrows.
    enabled: Vec<(usize, usize)>,
    selected: usize,
    reveal_selected: bool,
    error: Option<String>,
}
impl Chooser {
    fn rebuild_navigation(&mut self) {
        self.enabled.clear();
        for (group, state) in self.groups.iter().enumerate() {
            self.enabled.extend(
                state
                    .choices
                    .iter()
                    .enumerate()
                    .filter(|(_, choice)| choice.reason.is_none())
                    .map(|(choice, _)| (group, choice)),
            );
        }
        self.selected = self.selected.min(self.enabled.len().saturating_sub(1));
    }
}
fn direction_name(direction: RelatedDirection) -> &'static str {
    match direction {
        RelatedDirection::Referenced => "Referenced Rows",
        RelatedDirection::Referencing => "Referencing Rows",
    }
}

impl App {
    pub(super) fn related_rows_blocked_reason(
        &self,
        index: usize,
        result: usize,
    ) -> Option<&'static str> {
        let Some(tab) = self.tabs.get(index) else {
            return Some("The source tab is no longer open");
        };
        let Some(rs) = tab.results.get(result) else {
            return Some("The result is no longer available");
        };
        if tab.is_running() || tab.context_pending.is_some() {
            return Some("Wait for the running operation to finish");
        }
        if self.source_is_saving(&tab.ds_id) {
            return Some("Wait for the data source file to finish saving");
        }
        if tab.in_tx {
            return Some("Commit or roll back the source transaction before opening Related Rows");
        }
        if tab.ds_id.starts_with("file:") {
            return Some("Related Rows requires a persistent database source");
        }
        if let Some(reason) = self.result_context_blocked_reason(index, result) {
            return Some(reason);
        }
        if let Err(reason) = rs.related_row_anchor() {
            return Some(reason);
        }
        let Some(origin) = rs.origin.as_ref().and_then(|origin| origin.source.as_ref()) else {
            return Some("Execute the query again to establish its data source");
        };
        if origin.driver == DriverKind::Sqlite
            && (origin.file.is_empty()
                || origin.file == ":memory:"
                || origin.file.contains("mode=memory"))
        {
            return Some("Related Rows requires a persistent SQLite database file");
        }
        let Some(context) = rs.data.context.as_ref() else {
            return Some("Execute the query again to establish its database context");
        };
        if context.temporary_schema.is_some() {
            return Some("Related Rows cannot transfer temporary objects to another session");
        }
        None
    }

    pub(super) fn request_related_rows(&mut self, tab_index: usize, result_index: usize) {
        if self.related_rows.is_open() {
            return;
        }
        if let Some(rs) = self
            .tabs
            .get_mut(tab_index)
            .and_then(|tab| tab.results.get_mut(result_index))
        {
            rs.stage_action_buffer();
        }
        if let Some(reason) = self.related_rows_blocked_reason(tab_index, result_index) {
            self.notice = Some(ServicesNotice::info(format!("Related Rows: {reason}")));
            return;
        }
        let tab = &self.tabs[tab_index];
        let rs = &tab.results[result_index];
        if let Err(reason) = crate::related::candidate(&rs.data) {
            self.notice = Some(ServicesNotice::info(format!("Related Rows: {reason}")));
            return;
        }
        let anchor = Anchor {
            tab_id: tab.id,
            result_id: rs.action_identity(),
            row: rs.related_row_anchor().expect("validated row").0,
            session_epoch: rs
                .data
                .source
                .as_ref()
                .expect("validated provenance")
                .session_epoch,
            source: rs.origin.as_ref().unwrap().source.as_ref().unwrap().clone(),
            context: rs.data.context.as_ref().unwrap().clone(),
            focus: self.pending_action_focus.or(rs.related_focus_id()),
        };
        let referenced = self.related_rows.next_request();
        let referencing = self.related_rows.next_request();
        self.related_rows.popup = Some(Chooser {
            anchor,
            groups: [
                Group::new(RelatedDirection::Referenced, referenced),
                Group::new(RelatedDirection::Referencing, referencing),
            ],
            enabled: Vec::new(),
            selected: 0,
            reveal_selected: false,
            error: None,
        });
        self.pending_action_focus = None;
        self.pending_editor_focus = false;
        self.send_related_request(0);
        self.send_related_request(1);
    }

    fn related_anchor_indices(&self, anchor: &Anchor) -> Option<(usize, usize)> {
        let index = self.tabs.iter().position(|tab| tab.id == anchor.tab_id)?;
        if self.active != index {
            return None;
        }
        let result = self.tabs[index]
            .results
            .iter()
            .position(|rs| rs.action_identity() == anchor.result_id)?;
        if self.related_rows_blocked_reason(index, result).is_some() {
            return None;
        }
        let rs = &self.tabs[index].results[result];
        (rs.data.source.as_ref()?.session_epoch == anchor.session_epoch
            && rs.data.context.as_ref()? == &anchor.context
            && rs.origin.as_ref()?.source.as_ref()? == &anchor.source
            && anchor.row < rs.data.rows.len())
        .then_some((index, result))
    }

    fn send_related_request(&self, group_index: usize) {
        let Some(popup) = &self.related_rows.popup else {
            return;
        };
        let Some((index, result)) = self.related_anchor_indices(&popup.anchor) else {
            return;
        };
        let tab = &self.tabs[index];
        let rs = &tab.results[result];
        let Some(mut ds) = self.source(&tab.ds_id).cloned() else {
            return;
        };
        if ds.driver == DriverKind::Postgres {
            ds.database.clone_from(&popup.anchor.source.database);
        }
        let group = &popup.groups[group_index];
        self.backend.load_related_metadata(RelatedMetadataRequest {
            console_id: tab.id,
            result_id: rs.action_identity(),
            request_id: group.request_id,
            ds,
            source_identity: popup.anchor.source.clone(),
            expected_context: popup.anchor.context.clone(),
            source: rs.data.source.as_ref().unwrap().clone(),
            direction: group.direction,
        });
    }

    pub(super) fn related_metadata_loaded(
        &mut self,
        console_id: u64,
        result_id: u64,
        request_id: u64,
        direction: RelatedDirection,
        result: Result<RelatedMetadata, String>,
    ) {
        let Some(mut popup) = self.related_rows.popup.take() else {
            return;
        };
        let group_index = popup
            .groups
            .iter()
            .position(|group| group.direction == direction && group.request_id == request_id);
        if popup.anchor.tab_id != console_id
            || popup.anchor.result_id != result_id
            || group_index.is_none()
        {
            self.related_rows.popup = Some(popup);
            return;
        }
        let Some((index, result_index)) = self.related_anchor_indices(&popup.anchor) else {
            return;
        };
        let rs = &self.tabs[index].results[result_index];
        let group = &mut popup.groups[group_index.unwrap()];
        group.loading = false;
        match result {
            Err(error) => group.error = Some(error),
            Ok(metadata) => {
                group.error = None;
                group.choices = metadata
                    .keys
                    .into_iter()
                    .map(|relation| {
                        let table = match direction {
                            RelatedDirection::Referenced => &relation.target,
                            RelatedDirection::Referencing => &relation.source,
                        };
                        let label = format!(
                            "{}: {}.{} · {}",
                            direction_name(direction),
                            table.schema,
                            table.name,
                            relation.name
                        );
                        let description = relation
                            .columns
                            .iter()
                            .map(|column| {
                                format!("{} → {}", column.source_name, column.target_name)
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        let reason = crate::related::relation_blocked_reason(
                            popup.anchor.source.driver,
                            &relation,
                        )
                        .map(str::to_owned)
                        .or_else(|| {
                            match crate::related::columns(&rs.data, &relation, direction) {
                                Err(reason) => Some(reason),
                                Ok(columns) => columns.into_iter().find_map(|col| {
                                    match rs.data.rows[popup.anchor.row].get(col) {
                                        None => Some("The result row is incomplete".into()),
                                        Some(Value::Null) => Some(
                                            "A NULL key does not identify a related row".into(),
                                        ),
                                        Some(Value::Float(number)) if !number.is_finite() => Some(
                                            "A non-finite key cannot identify a related row".into(),
                                        ),
                                        _ => None,
                                    }
                                }),
                            }
                        });
                        Choice {
                            relation,
                            label,
                            description,
                            reason,
                        }
                    })
                    .collect();
            }
        }
        popup.rebuild_navigation();
        self.related_rows.popup = Some(popup);
    }

    pub(super) fn related_rows_ui(&mut self, ctx: &Context) {
        let Some(mut popup) = self.related_rows.popup.take() else {
            return;
        };
        let Some((index, result_index)) = self.related_anchor_indices(&popup.anchor) else {
            return;
        };
        let mut close = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        let mut open = None;
        let mut retry = None;
        if !popup.enabled.is_empty() {
            ctx.input_mut(|input| {
                if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    popup.selected = (popup.selected + 1) % popup.enabled.len();
                    popup.reveal_selected = true;
                }
                if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    popup.selected =
                        (popup.selected + popup.enabled.len() - 1) % popup.enabled.len();
                    popup.reveal_selected = true;
                }
                if input.consume_key(Modifiers::NONE, Key::Enter) {
                    open = Some(popup.enabled[popup.selected]);
                }
            });
        }
        let selected = popup.enabled.get(popup.selected).copied();
        let modal = theme::Dialog::new(Id::new("related_rows"), "Related Rows")
            .default_size([540.0, 360.0])
            .show(ctx, |ui| {
            ui.label(
                RichText::new("Opens committed data in a separate query console.")
                    .color(theme::TEXT_MUTED),
            );
            for (group_index, group) in popup.groups.iter().enumerate() {
                ui.separator();
                ui.strong(direction_name(group.direction));
                if group.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading related rows…");
                    });
                }
                if let Some(error) = &group.error {
                    ui.label(RichText::new(error).color(theme::RED));
                    if ui
                        .button(format!("Retry {}", direction_name(group.direction)))
                        .clicked()
                    {
                        retry = Some(group_index);
                    }
                }
                if !group.loading && group.error.is_none() && group.choices.is_empty() {
                    ui.label("No related rows available");
                }
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt(("related_choices", group_index))
                    .max_height(170.0);
                if popup.reveal_selected
                    && let Some((selected_group, choice)) = selected
                    && selected_group == group_index
                {
                    // Set the offset before show_rows chooses its virtual range.
                    scroll = scroll.vertical_scroll_offset(
                        (choice as f32 * (26.0 + ui.spacing().item_spacing.y) - 65.0).max(0.0),
                    );
                }
                scroll.show_rows(ui, 26.0, group.choices.len(), |ui, range| {
                    for choice_index in range {
                        let choice = &group.choices[choice_index];
                        let response = ui.add_enabled(
                            choice.reason.is_none(),
                            egui::Button::selectable(
                                selected == Some((group_index, choice_index)),
                                &choice.label,
                            )
                            .truncate()
                            .min_size(egui::vec2(ui.available_width(), 26.0)),
                        );
                        let response = response
                            .on_hover_text(&choice.description)
                            .on_disabled_hover_text(choice.reason.as_deref().unwrap_or(""));
                        if response.clicked() {
                            open = Some((group_index, choice_index));
                        }
                        if let Some(reason) = &choice.reason {
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_description(reason.as_str())
                            });
                        }
                    }
                });
            }
            if let Some(error) = &popup.error {
                ui.label(RichText::new(error).color(theme::RED));
            }
            theme::dialog_footer(ui, |_| {}, |ui| {
                close |= theme::dialog_button(ui, "Cancel").clicked();
            });
        });
        popup.reveal_selected = false;
        close |= modal.should_close();
        if close {
            self.pending_action_focus = popup.anchor.focus;
            return;
        }
        if let Some((group, choice)) = open {
            let choice = &popup.groups[group].choices[choice];
            if choice.reason.is_none() {
                let rs = &self.tabs[index].results[result_index];
                match crate::related::capture(
                    &rs.data,
                    popup.anchor.row,
                    &choice.relation,
                    popup.groups[group].direction,
                    popup.anchor.source.clone(),
                    popup.anchor.context.clone(),
                ) {
                    Ok(query) => {
                        let guard = RelatedSourceGuard {
                            console_id: popup.anchor.tab_id,
                            session_epoch: popup.anchor.session_epoch,
                        };
                        self.open_related_query(index, Arc::new(query), guard);
                        return;
                    }
                    Err(error) => popup.error = Some(error),
                }
            }
        }
        if let Some(group) = retry {
            popup.groups[group].request_id = self.related_rows.next_request();
            popup.groups[group].error = None;
            popup.groups[group].loading = true;
            popup.groups[group].choices.clear();
            popup.rebuild_navigation();
        }
        self.related_rows.popup = Some(popup);
        if let Some(group) = retry {
            self.send_related_request(group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Column, ColumnOrigin, DataSource, ExecutionContext, ForeignKeyColumn, OriginColumn,
        OriginTable, Page, PhysicalTable, QueryResult, ResultSource,
    };
    use crate::ui::{
        TabKind,
        actions::tests::isolated_app,
        results::{GridRow, ResultOrigin, ResultSet},
    };

    fn relation(index: usize) -> ForeignKey {
        ForeignKey {
            name: format!("FK {index}"),
            catalog_id: index.to_string(),
            source: PhysicalTable {
                schema: "main".into(),
                name: "people".into(),
                oid: None,
            },
            target: PhysicalTable {
                schema: "main".into(),
                name: "companies".into(),
                oid: None,
            },
            columns: vec![ForeignKeyColumn {
                source_name: "company_id".into(),
                source_origin: OriginColumn::Named("company_id".into()),
                source_type: "INTEGER".into(),
                target_name: "id".into(),
                target_origin: OriginColumn::Named("id".into()),
                target_type: "INTEGER".into(),
            }],
        }
    }

    fn fixture() -> App {
        let ctx = Context::default();
        let mut app = isolated_app(&ctx);
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.file = "/private/tmp/related-ui-no-io.sqlite".into();
        let identity = ds.identity();
        let ds_id = ds.id.clone();
        app.config.data_sources.push(ds);
        let index = app.add_tab(
            "source".into(),
            ds_id.clone(),
            TabKind::Console {
                sql: "select company_id as company from people".into(),
            },
        );
        let context = ResolvedContext {
            schemas: vec!["main".into()],
            ..Default::default()
        };
        app.tabs[index].resolved_context = Some(context.clone());
        let mut rs = ResultSet::new(
            "Result".into(),
            String::new(),
            QueryResult {
                columns: vec![Column {
                    name: "company".into(),
                    type_name: "INTEGER".into(),
                }],
                rows: vec![vec![Value::Int(7)], vec![Value::Int(2)]],
                source: Some(ResultSource {
                    session_epoch: 42,
                    columns: vec![Some(ColumnOrigin {
                        table: OriginTable::Named {
                            schema: "main".into(),
                            table: "people".into(),
                        },
                        column: OriginColumn::Named("company_id".into()),
                    })],
                    exact_columns: vec![true],
                }),
                context: Some(context.clone()),
                ..Default::default()
            },
            Page::first(500),
        );
        rs.origin = Some(ResultOrigin {
            ds_id,
            source: Some(identity.clone()),
            context: ExecutionContext::default(),
            bound_query: None,
        });
        rs.selected = Some((GridRow::Data(0), 0));
        let anchor = Anchor {
            tab_id: app.tabs[index].id,
            result_id: rs.action_identity(),
            row: 0,
            session_epoch: 42,
            source: identity,
            context,
            focus: Some(Id::new("grid")),
        };
        app.tabs[index].results.push(rs);
        app.related_rows.popup = Some(Chooser {
            anchor,
            groups: [
                Group::new(RelatedDirection::Referenced, 1),
                Group::new(RelatedDirection::Referencing, 2),
            ],
            enabled: Vec::new(),
            selected: 0,
            reveal_selected: false,
            error: None,
        });
        app.related_rows.next_request = 2;
        app
    }

    fn deliver(app: &mut App, request: u64, keys: Vec<ForeignKey>) {
        let tab = &app.tabs[0];
        app.related_metadata_loaded(
            tab.id,
            tab.results[0].action_identity(),
            request,
            RelatedDirection::Referenced,
            Ok(RelatedMetadata {
                table: relation(0).source,
                keys,
            }),
        );
    }

    #[test]
    fn readonly_alias_result_maps_without_primary_key_and_stays_on_original_row() {
        let mut app = fixture();
        app.tabs[0].results[0].set_sort(Some((0, true)));
        deliver(&mut app, 1, vec![relation(0)]);
        let popup = app.related_rows.popup.as_ref().unwrap();
        assert!(popup.groups[0].choices[0].reason.is_none());
        assert_eq!(popup.anchor.row, 0);
        assert_eq!(popup.enabled, [(0, 0)]);
        let query = crate::related::capture(
            &app.tabs[0].results[0].data,
            popup.anchor.row,
            &popup.groups[0].choices[0].relation,
            RelatedDirection::Referenced,
            popup.anchor.source.clone(),
            popup.anchor.context.clone(),
        )
        .unwrap();
        assert_eq!(query.values, [Value::Int(7)]);
    }

    #[test]
    fn stale_tokens_are_ignored_and_changed_context_cancels_the_chooser() {
        let mut app = fixture();
        deliver(&mut app, 99, vec![relation(0)]);
        assert!(app.related_rows.popup.as_ref().unwrap().groups[0].loading);
        assert!(
            app.related_rows.popup.as_ref().unwrap().groups[0]
                .choices
                .is_empty()
        );
        app.tabs[0].resolved_context = Some(ResolvedContext {
            schemas: vec!["other".into()],
            ..Default::default()
        });
        deliver(&mut app, 1, vec![relation(0)]);
        assert!(!app.related_rows.is_open());
    }

    #[test]
    fn null_and_partial_keys_are_disabled_and_dirty_source_is_blocked() {
        let mut app = fixture();
        std::sync::Arc::make_mut(&mut app.tabs[0].results[0].data).rows[0][0] = Value::Null;
        let mut partial = relation(1);
        partial.columns[0].source_origin = OriginColumn::Named("missing".into());
        deliver(&mut app, 1, vec![relation(0), partial]);
        let popup = app.related_rows.popup.as_ref().unwrap();
        assert!(popup.enabled.is_empty());
        assert!(
            popup.groups[0].choices[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("NULL")
        );
        assert!(
            popup.groups[0].choices[1]
                .reason
                .as_ref()
                .unwrap()
                .contains("all columns")
        );
        app.tabs[0].results[0]
            .edits
            .changed
            .insert((0, 0), crate::sqlgen::CellEdit::Text("3".into()));
        assert!(
            app.related_rows_blocked_reason(0, 0)
                .unwrap()
                .contains("pending changes")
        );
    }

    #[test]
    fn pinned_console_keeps_find_and_execute_but_rejects_sql_mutation_commands() {
        use crate::ui::actions::{ActionContext, ActionId, ActionSurface};
        let mut app = fixture();
        let popup = app.related_rows.popup.take().unwrap();
        let query = crate::related::capture(
            &app.tabs[0].results[0].data,
            0,
            &relation(0),
            RelatedDirection::Referenced,
            popup.anchor.source,
            popup.anchor.context,
        )
        .unwrap();
        app.tabs[0].bound_query = Some(Arc::new(query));
        let context = ActionContext {
            tab_id: Some(app.tabs[0].id),
            surface: ActionSurface::Sql,
            ds_id: Some(app.tabs[0].ds_id.clone()),
            ..Default::default()
        };
        for action in [
            ActionId::Replace,
            ActionId::ReformatCode,
            ActionId::ExecuteScript,
        ] {
            assert!(!app.action_availability(action, &context).enabled);
        }
        for action in [ActionId::Find, ActionId::Execute, ActionId::GoToLineColumn] {
            assert!(app.action_availability(action, &context).enabled);
        }
        assert!(!app.apply_console_action(0, ActionId::ReformatCode, &Context::default()));
    }

    #[test]
    fn palette_navigation_preserves_its_original_search_focus_on_cancel() {
        let mut app = fixture();
        app.related_rows.cancel();
        let search_focus = Id::new("grid_find_query");
        app.pending_action_focus = Some(search_focus);
        // Metadata lookup validates an existing session only. This isolated App
        // has none, so no connection or filesystem access can be started.
        app.request_related_rows(0, 0);
        assert_eq!(
            app.related_rows.popup.as_ref().unwrap().anchor.focus,
            Some(search_focus)
        );
        assert_eq!(app.pending_action_focus, None);
        for group in &mut app.related_rows.popup.as_mut().unwrap().groups {
            group.loading = false;
        }
        let mut harness = egui_kittest::Harness::builder()
            .with_size([800.0, 650.0])
            .build_ui_state(|ui, app: &mut App| app.related_rows_ui(ui.ctx()), app);
        harness.run_steps(3);
        harness.key_press(Key::Escape);
        harness.run_steps(2);
        assert!(!harness.state().related_rows.is_open());
        assert_eq!(harness.state().pending_action_focus, Some(search_focus));
    }

    #[test]
    fn keyboard_reveals_virtualized_relationships_and_escape_restores_focus() {
        use egui_kittest::kittest::{NodeT, Queryable};
        let mut app = fixture();
        deliver(&mut app, 1, (0..80).map(relation).collect());
        app.related_rows.popup.as_mut().unwrap().groups[1].loading = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size([800.0, 650.0])
            .build_ui_state(|ui, app: &mut App| app.related_rows_ui(ui.ctx()), app);
        harness.run_steps(3);
        for _ in 0..45 {
            harness.key_press(Key::ArrowDown);
            harness.run_steps(1);
        }
        harness.run_steps(3);
        assert_eq!(
            harness
                .state()
                .related_rows
                .popup
                .as_ref()
                .unwrap()
                .selected,
            45
        );
        let choice = harness.get_by_label("Referenced Rows: main.companies · FK 45");
        assert!(choice.rect().min.y >= 0.0 && choice.rect().max.y < 650.0);
        assert_eq!(
            choice.accesskit_node().toggled(),
            Some(egui::accesskit::Toggled::True)
        );
        harness.key_press(Key::Escape);
        harness.run_steps(2);
        assert!(!harness.state().related_rows.is_open());
        assert_eq!(harness.state().pending_action_focus, Some(Id::new("grid")));
    }
}
