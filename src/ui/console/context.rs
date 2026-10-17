//! Console context controls. Catalog work happens on invalidation or query edits;
//! the toolbar borrows metadata and emits intents after drawing.

use super::super::{Tab, theme};
use crate::model::{CompletionMetadata, DriverKind, ResolvedContext, SchemaSelection};
use crate::search::{SearchOptions, SearchPattern};
use egui::{Key, Modifiers, RichText, Ui};

#[derive(Debug, PartialEq, Eq)]
pub(in crate::ui) enum ContextIntent {
    Source(String),
    Database(Option<String>),
    Schema(SchemaSelection),
    LoadSchemas,
}

pub(super) struct CatalogView<'a> {
    pub(super) metadata: Option<&'a CompletionMetadata>,
    pub(super) loading: bool,
    pub(super) error: Option<&'a str>,
}

pub(in crate::ui) struct ContextUiState {
    label_dirty: bool,
    query_dirty: bool,
    label: String,
    tooltip: String,
    query: String,
    filtered: Vec<usize>,
    selected_missing: bool,
    open_last_frame: bool,
    active: usize,
    reveal_active: bool,
    seen_revision: Option<u64>,
    seen_pending: bool,
    #[cfg(test)]
    rebuilds: usize,
}

impl Default for ContextUiState {
    fn default() -> Self {
        Self {
            label_dirty: true,
            query_dirty: true,
            label: String::new(),
            tooltip: String::new(),
            query: String::new(),
            filtered: Vec::new(),
            selected_missing: false,
            open_last_frame: false,
            active: 0,
            reveal_active: false,
            seen_revision: None,
            seen_pending: false,
            #[cfg(test)]
            rebuilds: 0,
        }
    }
}

impl ContextUiState {
    /// Call when this tab's catalog is replaced, refreshed, or removed.
    pub(in crate::ui) fn invalidate(&mut self) {
        self.label_dirty = true;
        self.query_dirty = true;
    }

    /// A statement may alter the live search path without changing the catalog.
    pub(in crate::ui) fn invalidate_context(&mut self) {
        self.label_dirty = true;
    }

    fn refresh_label(
        &mut self,
        driver: DriverKind,
        selected: &SchemaSelection,
        resolved: Option<&ResolvedContext>,
        pending: bool,
        revision: u64,
    ) {
        if self.seen_revision != Some(revision) || self.seen_pending != pending {
            self.label_dirty = true;
            self.query_dirty = true;
            self.seen_revision = Some(revision);
            self.seen_pending = pending;
        }
        if !std::mem::take(&mut self.label_dirty) {
            return;
        }
        self.label.clear();
        self.tooltip.clear();
        if driver == DriverKind::Sqlite {
            self.label.push_str("main");
            self.tooltip.push_str(
                "SQLite has no switchable default schema. Use schema.table for temp or attached databases.",
            );
            return;
        }
        match selected {
            SchemaSelection::Default => {
                self.label.push_str("Default");
                if let Some(resolved) = resolved {
                    self.label.push_str(": ");
                    append_schemas(&mut self.label, &resolved.schemas);
                }
                self.tooltip.push_str("Configured schema: Default");
            }
            SchemaSelection::Named(name) => {
                self.label.push_str(name);
                self.tooltip.push_str("Configured schema: ");
                self.tooltip.push_str(name);
            }
        }
        if let Some(resolved) = resolved {
            self.tooltip.push_str("\nLast confirmed session: ");
            append_schemas(&mut self.tooltip, &resolved.schemas);
            if let Some(path) = &resolved.search_path {
                self.tooltip.push_str("\nSearch path: ");
                self.tooltip.push_str(path);
            }
        } else {
            self.tooltip.push_str(
                "\nThe connection's default schema/search path has not been confirmed yet.",
            );
        }
        self.tooltip
            .push_str("\nChoose a schema to apply it to this console's session.");
        if pending {
            self.tooltip.push_str("\nChanging schema…");
        }
        // The compact toolbar must not shape an arbitrarily long configured
        // path on every frame. Normal paths remain complete in the tooltip.
        bound_preview(&mut self.label, 120);
        bound_preview(&mut self.tooltip, 4096);
    }

    fn refresh_filter(&mut self, schemas: &[String], selected: &SchemaSelection) {
        if !std::mem::take(&mut self.query_dirty) {
            return;
        }
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        self.filtered.clear();
        if !self.query.is_empty() {
            let pattern = SearchPattern::new(&self.query, SearchOptions::default());
            self.filtered.extend(
                schemas
                    .iter()
                    .enumerate()
                    .filter_map(|(index, name)| pattern.is_match(name).then_some(index)),
            );
        }
        self.selected_missing =
            matches!(selected, SchemaSelection::Named(name) if !schemas.contains(name));
        self.active = 0;
        self.reveal_active = true;
    }

    fn count(&self, schemas: &[String]) -> usize {
        if self.query.is_empty() {
            schemas.len()
        } else {
            self.filtered.len()
        }
    }

    fn schema_index(&self, position: usize) -> usize {
        if self.query.is_empty() {
            position
        } else {
            self.filtered[position]
        }
    }
}

fn append_schemas(text: &mut String, schemas: &[String]) {
    if schemas.is_empty() {
        text.push_str("<none>");
    } else {
        for (index, schema) in schemas.iter().enumerate() {
            if index != 0 {
                text.push_str(", ");
            }
            text.push_str(schema);
        }
    }
}

fn bound_preview(text: &mut String, characters: usize) {
    if let Some((byte, _)) = text.char_indices().nth(characters) {
        text.truncate(byte);
        text.push('…');
    }
}

pub(super) fn schema_selector(
    ui: &mut Ui,
    tab: &mut Tab,
    driver: DriverKind,
    catalog: &CatalogView<'_>,
    blocked_reason: Option<&str>,
) -> Option<ContextIntent> {
    let pending = tab.context_pending.is_some();
    let state = &mut tab.context_ui;
    state.refresh_label(
        driver,
        &tab.schema,
        tab.resolved_context.as_ref(),
        pending,
        tab.context_revision,
    );
    let mut intent = None;
    let enabled = driver != DriverKind::Sqlite && blocked_reason.is_none();
    let response = ui
        .push_id(("schema_switch", tab.id), |ui| {
            ui.add_enabled(
                enabled,
                egui::Button::new(RichText::new(if pending {
                    "Changing schema…"
                } else {
                    &state.label
                }))
                .right_text("⏷")
                .min_size(egui::vec2(
                    ui.available_width().min(150.0),
                    ui.spacing().interact_size.y,
                ))
                .wrap_mode(egui::TextWrapMode::Truncate),
            )
        })
        .inner;
    let popup_id = egui::Id::new(("schema_popup", tab.id));
    if !enabled {
        egui::Popup::close_id(ui.ctx(), popup_id);
    }
    // A ComboBox adds its own ScrollArea. Nesting a virtualized ScrollArea
    // inside it freezes the outer height while async catalog rows arrive,
    // leaving accessible rows outside the pointer clip. Keep one viewport.
    let popup = egui::Popup::menu(&response)
        .id(popup_id)
        .width(260.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            let opened = !state.open_last_frame;
            if opened {
                state.query.clear();
                state.query_dirty = true;
                if catalog.metadata.is_none() && !catalog.loading && catalog.error.is_none() {
                    intent = Some(ContextIntent::LoadSchemas);
                }
            }
            ui.set_min_width(240.0);
            let mut step = 0;
            let mut activate = false;
            if state.open_last_frame {
                ui.input_mut(|input| {
                    if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        step = 1;
                    }
                    if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        step = -1;
                    }
                    activate = input.consume_key(Modifiers::NONE, Key::Enter);
                });
            }
            let search = ui.add(
                egui::TextEdit::singleline(&mut state.query)
                    .id(egui::Id::new(("schema_search", tab.id)))
                    .hint_text("Search schemas")
                    .desired_width(f32::INFINITY),
            );
            search.widget_info(|| {
                let mut info = egui::WidgetInfo::text_edit(true, &state.query, &state.query, "");
                info.label = Some("Search schemas".into());
                info
            });
            if opened {
                search.request_focus();
            }
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    search.id,
                    egui::EventFilter {
                        vertical_arrows: true,
                        ..Default::default()
                    },
                )
            });
            if search.changed() {
                state.query_dirty = true;
            }
            let schemas = catalog
                .metadata
                .map_or(&[][..], |metadata| metadata.schemas.as_slice());
            state.refresh_filter(schemas, &tab.schema);
            let count = state.count(schemas);
            // Default stays visible, including with an empty catalog.
            let named_offset = usize::from(state.query.is_empty());
            let options = count + named_offset;
            if options > 0 && step != 0 {
                state.active = if step > 0 {
                    (state.active + 1) % options
                } else {
                    state.active.checked_sub(1).unwrap_or(options - 1)
                };
                state.reveal_active = true;
            }
            let default =
                ui.selectable_label(matches!(tab.schema, SchemaSelection::Default), "Default");
            if default.clicked() || (activate && named_offset != 0 && state.active == 0) {
                intent = Some(ContextIntent::Schema(SchemaSelection::Default));
                ui.close();
            }
            ui.separator();
            if state.selected_missing
                && let SchemaSelection::Named(name) = &tab.schema
            {
                let response = ui
                    .selectable_label(true, name)
                    .on_hover_text("Not present in the loaded metadata");
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::SelectableLabel,
                        true,
                        true,
                        format!("Schema: {name}"),
                    )
                });
                if response.clicked() {
                    intent = Some(ContextIntent::Schema(tab.schema.clone()));
                    ui.close();
                }
            }
            if catalog.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading schemas…");
                });
            } else if let Some(error) = catalog.error {
                ui.label(RichText::new("Could not load schemas").color(theme::RED))
                    .on_hover_text(error);
                if ui.button("Retry loading schemas").clicked() {
                    intent = Some(ContextIntent::LoadSchemas);
                }
            } else if schemas.is_empty() {
                ui.label(RichText::new("No schemas loaded").color(theme::TEXT_MUTED));
            } else if count == 0 {
                ui.label(RichText::new("No matching schemas").color(theme::TEXT_MUTED));
            }
            let row_height = ui.text_style_height(&egui::TextStyle::Button) + 6.0;
            let scroll = egui::ScrollArea::vertical()
                .id_salt(("schema_choices", tab.id))
                .max_height(260.0);
            // scroll_to_rect from a virtualized row cannot reveal an
            // offscreen target; supply its offset before showing rows.
            let scroll = if std::mem::take(&mut state.reveal_active) {
                scroll.vertical_scroll_offset(
                    state.active.saturating_sub(named_offset) as f32
                        * (row_height + ui.spacing().item_spacing.y),
                )
            } else {
                scroll
            };
            scroll.show_rows(ui, row_height, count, |ui, range| {
                for position in range {
                    let name = &schemas[state.schema_index(position)];
                    let current =
                        matches!(&tab.schema, SchemaSelection::Named(selected) if selected == name);
                    let keyboard = state.active == position + named_offset;
                    let response = ui.add_sized(
                        [ui.available_width(), row_height],
                        egui::Button::selectable(current || keyboard, name)
                            .right_text("")
                            .wrap_mode(egui::TextWrapMode::Truncate),
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::SelectableLabel,
                            true,
                            current,
                            format!("Schema: {name}"),
                        )
                    });
                    if response.clicked() || (activate && keyboard) {
                        intent = Some(ContextIntent::Schema(SchemaSelection::Named(name.clone())));
                        ui.close();
                    }
                }
            });
        });
    state.open_last_frame = popup.is_some();
    response.widget_info(|| {
        let mut info =
            egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, enabled, "Default schema");
        info.current_text_value = Some(if pending {
            "Changing schema…".to_owned()
        } else {
            state.label.clone()
        });
        info
    });
    let response = response.on_hover_text(&state.tooltip);
    if driver == DriverKind::Sqlite {
        response.on_disabled_hover_text(&state.tooltip);
    } else if let Some(reason) = blocked_reason {
        response.on_disabled_hover_text(reason);
    }
    intent
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PickerHarness {
        tab: Tab,
        metadata: Option<CompletionMetadata>,
        intents: Vec<ContextIntent>,
    }

    fn picker_harness() -> egui_kittest::Harness<'static, PickerHarness> {
        let mut app = crate::ui::actions::tests::isolated_app(&egui::Context::default());
        let index = app.add_tab(
            "Console".into(),
            "source".into(),
            crate::ui::TabKind::Console { sql: String::new() },
        );
        let tab = app.tabs.remove(index);
        egui_kittest::Harness::builder()
            .with_size([600.0, 400.0])
            .build_ui_state(
                |ui, state: &mut PickerHarness| {
                    let catalog = CatalogView {
                        metadata: state.metadata.as_ref(),
                        loading: false,
                        error: None,
                    };
                    if let Some(intent) =
                        schema_selector(ui, &mut state.tab, DriverKind::Postgres, &catalog, None)
                    {
                        state.intents.push(intent);
                    }
                },
                PickerHarness {
                    tab,
                    metadata: None,
                    intents: Vec::new(),
                },
            )
    }

    #[test]
    fn schema_choice_click_after_async_catalog_arrives_emits_an_intent() {
        use egui_kittest::kittest::Queryable;
        let mut h = picker_harness();
        h.get_by_label("Default schema").click();
        h.run_steps(3);
        assert_eq!(h.state().intents, [ContextIntent::LoadSchemas]);
        h.state_mut().metadata = Some(CompletionMetadata {
            schemas: vec!["public".into(), "shop".into()],
            ..Default::default()
        });
        h.state_mut().tab.context_ui.invalidate();
        h.run_steps(3);
        h.get_by_label("Schema: shop").click();
        h.run_steps(3);
        assert_eq!(
            h.state().intents.last(),
            Some(&ContextIntent::Schema(SchemaSelection::Named(
                "shop".into()
            )))
        );
    }

    #[test]
    fn pending_schema_caption_is_exposed_to_accessibility_until_acknowledged() {
        use egui_kittest::kittest::Queryable;
        let mut h = picker_harness();
        h.state_mut().tab.context_pending = Some(crate::model::ExecutionContext::default());
        h.run_steps(3);
        assert_eq!(
            h.get_by_label("Default schema").value().as_deref(),
            Some("Changing schema…")
        );
        h.state_mut().tab.context_pending = None;
        h.run_steps(3);
        assert_eq!(
            h.get_by_label("Default schema").value().as_deref(),
            Some("Default")
        );
    }

    #[test]
    fn schema_filter_is_unicode_aware_and_does_no_idle_catalog_work() {
        let schemas = vec![
            "public".into(),
            "Łódź".into(),
            "EmptySchema".into(),
            "emptyschema".into(),
        ];
        let mut state = ContextUiState {
            query: "ŁÓD".into(),
            ..Default::default()
        };
        state.refresh_filter(&schemas, &SchemaSelection::Default);
        assert_eq!(state.filtered, [1]);
        let capacity = state.filtered.capacity();
        for _ in 0..100 {
            state.refresh_filter(&schemas, &SchemaSelection::Default);
        }
        assert_eq!(state.rebuilds, 1);
        assert_eq!(state.filtered.capacity(), capacity);
        state.query = "empty".into();
        state.query_dirty = true;
        state.refresh_filter(&schemas, &SchemaSelection::Default);
        assert_eq!(state.filtered, [2, 3]);
        state.query.clear();
        state.query_dirty = true;
        state.refresh_filter(&schemas, &SchemaSelection::Default);
        assert!(state.filtered.is_empty());
        assert_eq!(state.count(&schemas), 4);
    }

    #[test]
    fn default_label_needs_console_confirmation_and_retains_server_order() {
        let mut state = ContextUiState::default();
        state.refresh_label(
            DriverKind::Postgres,
            &SchemaSelection::Default,
            None,
            false,
            0,
        );
        assert_eq!(state.label, "Default");
        assert!(state.tooltip.contains("not been confirmed"));
        let resolved = ResolvedContext {
            database: Some("db".into()),
            search_path: Some("\"$user\", public".into()),
            schemas: vec!["alice".into(), "public".into()],
            ..Default::default()
        };
        state.invalidate_context();
        state.refresh_label(
            DriverKind::Postgres,
            &SchemaSelection::Default,
            Some(&resolved),
            false,
            0,
        );
        assert_eq!(state.label, "Default: alice, public");
        assert!(state.tooltip.contains("\"$user\", public"));
        let pointer = state.label.as_ptr();
        state.refresh_label(
            DriverKind::Postgres,
            &SchemaSelection::Default,
            Some(&resolved),
            false,
            0,
        );
        assert_eq!(state.label.as_ptr(), pointer);
    }

    #[test]
    fn sqlite_never_presents_an_attached_database_as_default() {
        let mut state = ContextUiState::default();
        state.refresh_label(
            DriverKind::Sqlite,
            &SchemaSelection::Named("attached".into()),
            None,
            false,
            0,
        );
        assert_eq!(state.label, "main");
        assert!(state.tooltip.contains("schema.table"));
    }

    #[test]
    fn missing_selected_schema_is_kept_without_inventing_catalog_entries() {
        let mut state = ContextUiState::default();
        state.refresh_filter(&["Public".into()], &SchemaSelection::Named("public".into()));
        assert!(state.selected_missing);
        state.invalidate();
        state.refresh_filter(
            &["Public".into(), "public".into()],
            &SchemaSelection::Named("public".into()),
        );
        assert!(!state.selected_missing);
    }

    #[test]
    fn pending_keeps_the_confirmed_selection_and_catalog_invalidation_is_explicit() {
        let mut state = ContextUiState::default();
        let selected = SchemaSelection::Named("confirmed".into());
        state.refresh_label(DriverKind::MySql, &selected, None, false, 1);
        state.refresh_label(DriverKind::MySql, &selected, None, true, 2);
        assert_eq!(state.label, "confirmed");
        assert!(state.tooltip.contains("Changing schema"));
        state.query = "new".into();
        state.refresh_filter(&["old".into()], &selected);
        assert!(state.filtered.is_empty());
        state.invalidate();
        state.refresh_filter(&["old".into(), "new_empty_schema".into()], &selected);
        assert_eq!(state.filtered, [1]);
    }

    #[test]
    #[ignore = "release-only schema catalog cache benchmark"]
    fn benchmark_schema_catalog_cache() {
        // Debug timing cannot substantiate UI performance claims.
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(!cfg!(debug_assertions), "run with --release");
        }
        for size in [1_000, 10_000] {
            let schemas: Vec<_> = (0..size).map(|i| format!("tenant_Łódź_{i:05}")).collect();
            let mut state = ContextUiState {
                query: "łódź_000".into(),
                ..Default::default()
            };
            let before = std::time::Instant::now();
            for _ in 0..100 {
                state.invalidate();
                state.refresh_filter(std::hint::black_box(&schemas), &SchemaSelection::Default);
                std::hint::black_box(&state.filtered);
            }
            let rebuild_us = before.elapsed().as_secs_f64() * 1e6 / 100.0;
            let before = std::time::Instant::now();
            for _ in 0..100_000 {
                std::hint::black_box(&mut state)
                    .refresh_filter(std::hint::black_box(&schemas), &SchemaSelection::Default);
            }
            let idle_ns = before.elapsed().as_secs_f64() * 1e9 / 100_000.0;
            println!(
                "schemas={size} rebuild={rebuild_us:.3}us idle={idle_ns:.3}ns matches={} index_capacity={}",
                state.filtered.len(),
                state.filtered.capacity()
            );
        }
    }
}
