//! Result grid with in-place editing, and the output log of the Services tool window.

use super::theme;
use crate::export::{self, ExportFormat};
use crate::model::{DriverKind, Page, QueryResult, Value};
use crate::sqlgen::CellEdit;
use egui::{Align, Color32, Key, KeyboardShortcut, Layout, Modifiers, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

mod actions;
mod aggregate_view;
mod depth_view;
mod editability;
mod find;
mod layout;
mod local_filter;
mod record;
mod selection;
mod sort;
mod value;

const MODIFIED_BG: Color32 = Color32::from_rgb(0x23, 0x3A, 0x5C);
const INSERTED_BG: Color32 = Color32::from_rgb(0x26, 0x3D, 0x2B);
const DELETED_BG: Color32 = Color32::from_rgb(0x4A, 0x2A, 0x2C);
static NEXT_RESULT_ID: AtomicU64 = AtomicU64::new(1);

// Row actions of an editable grid, with their keys.
const ADD_ROW: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
const DELETE_ROW: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Backspace);
const CLONE_ROW: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);
const OPEN_RECORD_VIEW: KeyboardShortcut = KeyboardShortcut::new(
    Modifiers {
        shift: true,
        ..Modifiers::COMMAND
    },
    Key::Enter,
);
const OPEN_VALUE_EDITOR: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter);
const COPY_VALUE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::C);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CellSurface {
    #[default]
    Grid,
    Record,
    Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GridRow {
    /// Index into the fetched rows.
    Data(usize),
    /// Index into rows added in the grid and not yet submitted.
    New(usize),
}

/// Changes made in the grid that have not been submitted yet.
#[derive(Default)]
pub struct EditState {
    pub changed: HashMap<(usize, usize), CellEdit>,
    pub deleted: HashSet<usize>,
    pub inserted: Vec<Vec<CellEdit>>,
    /// Cell being typed into, with its text buffer.
    editing: Option<(GridRow, usize, String)>,
    show_preview: bool,
    editing_surface: CellSurface,
    editing_needs_focus: bool,
    /// Changes whenever a staged value may have changed, so views built from
    /// pending edits know they are stale.
    revision: u64,
}

static NEXT_EDIT_REVISION: AtomicU64 = AtomicU64::new(1);

impl EditState {
    fn touch(&mut self) {
        self.revision = NEXT_EDIT_REVISION.fetch_add(1, Ordering::Relaxed);
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct ResultOrigin {
    pub ds_id: String,
    #[serde(default)]
    pub source: Option<crate::model::SourceIdentity>,
    pub context: crate::model::ExecutionContext,
    #[serde(default)]
    pub bound_query: Option<std::sync::Arc<crate::model::CapturedRelatedQuery>>,
}

pub struct ResultSet {
    pub(super) origin: Option<ResultOrigin>,
    action_id: u64,
    action_focus: actions::ActionFocus,
    pending_related_rows: bool,
    pending_compare_data: bool,
    pub title: String,
    pub sql: String,
    pub data: std::sync::Arc<QueryResult>,
    pub page: Page,
    pub selected: Option<(GridRow, usize)>,
    /// Column index and ascending flag; sorting is done on the fetched page.
    pub sort: Option<(usize, bool)>,
    order: Vec<usize>,
    layout: layout::LayoutCache,
    find: find::FindState,
    local_filter: local_filter::LocalFilterState,
    selection: selection::SelectionState,
    aggregate: aggregate_view::AggregateState,
    editability: editability::ResultEditability,
    edit_metadata_request: Option<u64>,
    edit_preview: Option<String>,
    edit_status: String,
    /// The user tried to edit while the result had no edit authority.
    pub(in crate::ui) edit_requested: bool,
    pub edits: EditState,
    /// Columns shown as rows (Transpose).
    pub transposed: bool,
    /// Selected record shown as field/value pairs beside the grid.
    pub record_view: bool,
    /// Statistics of the selected rectangle in a panel beside the grid.
    pub aggregate_view: bool,
    pub(in crate::ui) depth: depth_view::DepthState,
    /// Full value of the selected cell, below the grid.
    pub show_value_editor: bool,
    focused_surface: CellSurface,
    /// Reveal selection after navigation or an edit starts; cleared once the cell is drawn.
    pending_reveal: bool,
    /// Grid controls return keyboard ownership to the grid after their action.
    pending_grid_focus: bool,
    /// Geo Viewer shown instead of the grid.
    pub geo: Option<GeoState>,
    geo_columns: Option<Vec<usize>>,
    /// Confirmation of geometry columns with large values, owned by this result.
    geo_detection: Option<GeoDetection>,
    /// The value panel's cache that may hold views of this result.
    value_cache: Option<std::sync::Weak<std::sync::Mutex<value::ViewCache>>>,
    /// `data` as JSON for the saved session; rows never change after fetching.
    /// File the work session keeps these rows in, written once.
    pub(super) session_file: String,
}

/// Geo Viewer of a result set: chosen column, SRID override and map position.
pub struct GeoState {
    pub column: usize,
    srid_text: String,
    view: super::map::MapView,
    /// Layer of the chosen column and SRID override, prepared off the UI thread when large.
    layer: super::map::LayerLoader,
    fit: bool,
}

impl GeoState {
    fn new(column: usize) -> Self {
        Self {
            column,
            srid_text: String::new(),
            view: super::map::MapView::default(),
            layer: Default::default(),
            fit: true,
        }
    }
}

/// Geometry detections running at once; others start when one finishes.
const GEO_DETECTIONS: usize = 2;
static RUNNING_GEO_DETECTIONS: AtomicU64 = AtomicU64::new(0);

/// The full parse of sampled values that were too large to check on the UI
/// thread. Dropping it, with its result, cancels the worker.
struct GeoDetection {
    candidates: Vec<usize>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    done: Option<std::sync::Arc<std::sync::Mutex<Option<Vec<usize>>>>>,
}

impl GeoDetection {
    fn pending(candidates: Vec<usize>) -> Self {
        Self {
            candidates,
            cancel: Default::default(),
            done: None,
        }
    }

    /// Starts the worker when a slot is free; the found columns once it finished.
    fn poll(
        &mut self,
        data: &std::sync::Arc<QueryResult>,
        ctx: &egui::Context,
    ) -> Option<Vec<usize>> {
        if let Some(done) = &self.done {
            return done.lock().unwrap_or_else(|p| p.into_inner()).take();
        }
        let started = RUNNING_GEO_DETECTIONS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < GEO_DETECTIONS as u64).then_some(n + 1)
        });
        if started.is_err() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return None;
        }
        let done = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (data, candidates, cancel, result, ctx) = (
            data.clone(),
            self.candidates.clone(),
            self.cancel.clone(),
            done.clone(),
            ctx.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("geometry-detection".into())
            .spawn(move || {
                let found = crate::geo::confirm_geometry_columns(&candidates, &data.rows, &cancel);
                drop(data);
                RUNNING_GEO_DETECTIONS.fetch_sub(1, Ordering::AcqRel);
                if !cancel.load(Ordering::Relaxed) {
                    *result.lock().unwrap_or_else(|p| p.into_inner()) = Some(found);
                    ctx.request_repaint();
                }
            });
        if spawned.is_err() {
            RUNNING_GEO_DETECTIONS.fetch_sub(1, Ordering::AcqRel);
            return Some(Vec::new());
        }
        self.done = Some(done);
        None
    }
}

impl Drop for ResultSet {
    fn drop(&mut self) {
        if let Some(cache) = &self.value_cache {
            value::release_dropped(cache, self.action_id);
        }
    }
}

impl Drop for GeoDetection {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Grid requests that need the application: running SQL or a file dialog.
pub enum GridAction {
    Rerun,
    Page(Page),
    PageSize(usize),
    /// Opens the Export Data dialog for the whole result.
    ExportData,
    Submit,
    RelatedRows,
    CompareData,
}

impl ResultSet {
    pub fn new(title: String, sql: String, data: QueryResult, page: Page) -> Self {
        let order = (0..data.rows.len()).collect();
        Self {
            origin: None,
            action_id: NEXT_RESULT_ID.fetch_add(1, Ordering::Relaxed),
            action_focus: Default::default(),
            pending_related_rows: false,
            pending_compare_data: false,
            title,
            sql,
            data: std::sync::Arc::new(data),
            page,
            selected: None,
            sort: None,
            order,
            layout: layout::LayoutCache::default(),
            find: find::FindState::default(),
            local_filter: local_filter::LocalFilterState::default(),
            selection: selection::SelectionState::default(),
            aggregate: aggregate_view::AggregateState::default(),
            editability: editability::ResultEditability::default(),
            edit_metadata_request: None,
            edit_preview: None,
            edit_status: "Read-only: Run the query again before editing this result.".into(),
            edit_requested: false,
            edits: EditState::default(),
            transposed: false,
            record_view: false,
            aggregate_view: false,
            depth: depth_view::DepthState::default(),
            show_value_editor: true,
            focused_surface: CellSurface::Grid,
            pending_reveal: false,
            pending_grid_focus: false,
            geo: None,
            geo_columns: None,
            geo_detection: None,
            value_cache: None,
            session_file: super::session::new_result_file(),
        }
    }

    /// Columns holding geometries. A bounded check runs here once; columns
    /// with large sampled values are confirmed on a worker and added when it
    /// finishes, so the list can grow by one step.
    pub fn geometry_columns(&mut self, ctx: &egui::Context) -> &[usize] {
        if self.geo_columns.is_none() {
            let detection =
                crate::geo::detect_geometry_columns(&self.data.columns, &self.data.rows);
            self.geo_columns = Some(detection.columns);
            if !detection.undecided.is_empty() {
                self.geo_detection = Some(GeoDetection::pending(detection.undecided));
            }
        }
        if let Some(detection) = &mut self.geo_detection
            && let Some(found) = detection.poll(&self.data, ctx)
        {
            self.geo_detection = None;
            if let Some(columns) = &mut self.geo_columns {
                columns.extend(found);
                columns.sort_unstable();
            }
        }
        self.geo_columns.as_deref().unwrap_or_default()
    }

    /// Keeps the viewer open with the same column and position when a page reloads.
    pub fn take_geo_from(&mut self, old: &mut ResultSet) {
        if let Some(mut g) = old.geo.take() {
            g.layer.reset();
            g.fit = false;
            self.geo = Some(g);
        }
    }

    fn toggle_sort(&mut self, col: usize) {
        self.set_sort(match self.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        });
    }

    pub fn set_sort(&mut self, sort: Option<(usize, bool)>) {
        self.invalidate_selection_order();
        self.layout.clear_order();
        self.find.invalidate();
        self.local_filter.invalidate();
        self.sort = sort.filter(|(c, _)| *c < self.data.columns.len());
        self.order = (0..self.data.rows.len()).collect();
        if let Some((col, asc)) = self.sort {
            let rows = &self.data.rows;
            let keys: Vec<sort::SortKey<'_>> =
                rows.iter().map(|row| sort::SortKey::of(&row[col])).collect();
            self.order.sort_by(|&a, &b| {
                let ord = sort::compare(&keys[a], &rows[a][col], &keys[b], &rows[b][col]);
                if asc { ord } else { ord.reverse() }
            });
        }
    }

    /// What a cell currently shows: the pending edit if any, else the fetched value.
    fn cell(&self, row: GridRow, col: usize) -> Shown<'_> {
        match row {
            GridRow::Data(i) => match self.edits.changed.get(&(i, col)) {
                Some(e) => Shown::Edit(e),
                None => Shown::Value(&self.data.rows[i][col]),
            },
            GridRow::New(j) => Shown::Edit(&self.edits.inserted[j][col]),
        }
    }

    #[cfg(test)]
    pub fn selected_value(&self) -> Option<Value> {
        let (row, c) = self.valid_selection()?;
        Some(match self.cell(row, c) {
            Shown::Value(v) => v.clone(),
            Shown::Edit(CellEdit::Text(s)) => Value::Text(s.clone()),
            Shown::Edit(_) => Value::Null,
        })
    }

    fn valid_selection(&self) -> Option<(GridRow, usize)> {
        self.selected.filter(|&(row, col)| {
            let exists = match row {
                GridRow::Data(index) => index < self.data.rows.len(),
                GridRow::New(index) => index < self.edits.inserted.len(),
            };
            // Geo Viewer displays the fetched page independently of grid filters.
            exists
                && col < self.data.columns.len()
                && (self.geo.is_some() || self.display_index(row).is_some())
        })
    }

    fn moved_selection(&self, row_delta: isize, col_delta: isize) -> Option<(GridRow, usize)> {
        let last_col = self.data.columns.len().checked_sub(1)?;
        let last_row = self.row_count().checked_sub(1)?;
        let Some((row, col)) = self.valid_selection() else {
            return self.display_row(0).map(|row| (row, 0));
        };
        let index = self.display_index(row)?;
        let index = index.saturating_add_signed(row_delta).min(last_row);
        let col = col.saturating_add_signed(col_delta).min(last_col);
        self.display_row(index).map(|row| (row, col))
    }

    fn note_refused_edit(&mut self) {
        if self.edit_metadata().is_none() {
            self.edit_requested = true;
        }
    }

    fn set_cell(&mut self, row: GridRow, col: usize, edit: CellEdit) {
        if !self.can_edit_cell(row, col)
            || (matches!(edit, CellEdit::Text(_)) && !self.can_edit_text(row, col))
            || (edit == CellEdit::Default && !self.can_set_default(row, col))
        {
            self.note_refused_edit();
            return;
        }
        self.edit_preview = None;
        self.aggregate.invalidate();
        self.layout.clear_transposed();
        self.find.invalidate();
        self.local_filter.invalidate();
        self.edits.touch();
        match row {
            GridRow::Data(i) => {
                let original = &self.data.rows[i][col];
                let unchanged = match (&edit, original) {
                    (CellEdit::Null, Value::Null) => true,
                    (CellEdit::Text(s), v) if *v != Value::Null => *s == v.to_string(),
                    _ => false,
                };
                if unchanged {
                    self.edits.changed.remove(&(i, col));
                } else {
                    self.edits.changed.insert((i, col), edit);
                }
            }
            GridRow::New(j) => self.edits.inserted[j][col] = edit,
        }
    }

    fn add_row(&mut self) {
        if !self.can_add_row() {
            self.note_refused_edit();
            return;
        }
        self.edit_preview = None;
        self.find.invalidate();
        self.local_filter.invalidate();
        self.pending_grid_focus = true;
        self.edits.touch();
        self.edits
            .inserted
            .push(vec![CellEdit::Default; self.data.columns.len()]);
        let row = GridRow::New(self.edits.inserted.len() - 1);
        self.select_cell((row, 0), false);
        self.pending_reveal = true;
        self.edits.editing = Some((row, 0, String::new()));
        self.edits.editing_surface = CellSurface::Grid;
        self.edits.editing_needs_focus = true;
    }

    /// Copies a row into a new one. Primary key columns are left to their
    /// defaults so a generated key does not collide with the original.
    fn clone_row(&mut self, row: GridRow) {
        if !self.can_clone_row(row) {
            self.note_refused_edit();
            return;
        }
        self.edit_preview = None;
        self.find.invalidate();
        self.local_filter.invalidate();
        self.pending_grid_focus = true;
        let values = (0..self.data.columns.len())
            .map(|c| {
                // The key and columns the grid cannot set are left to their defaults.
                if self.primary_key_columns().contains(&c)
                    || self
                        .edit_metadata()
                        .is_none_or(|metadata| !metadata.writable[c])
                {
                    return CellEdit::Default;
                }
                match self.cell(row, c) {
                    Shown::Value(Value::Null) => CellEdit::Null,
                    Shown::Value(v) => CellEdit::Text(v.to_string()),
                    Shown::Edit(e) => e.clone(),
                }
            })
            .collect();
        self.edits.touch();
        self.edits.inserted.push(values);
        let col = self.selected.map_or(0, |(_, c)| c);
        self.select_cell((GridRow::New(self.edits.inserted.len() - 1), col), false);
        self.pending_reveal = true;
    }

    fn delete_row(&mut self, row: GridRow) {
        if !self.can_delete_row(row) {
            self.note_refused_edit();
            return;
        }
        self.edit_preview = None;
        self.invalidate_selection_order();
        self.find.invalidate();
        self.local_filter.invalidate();
        self.pending_grid_focus = true;
        self.layout.clear_transposed();
        if self
            .edits
            .editing
            .as_ref()
            .is_some_and(|(editing_row, _, _)| *editing_row == row)
        {
            self.edits.editing = None;
        }
        match row {
            GridRow::Data(i) => {
                if !self.edits.deleted.remove(&i) {
                    self.edits.deleted.insert(i);
                }
            }
            GridRow::New(j) => {
                self.edits.touch();
                self.edits.inserted.remove(j);
                if let Some((GridRow::New(index), _, _)) = &mut self.edits.editing
                    && *index > j
                {
                    *index -= 1;
                }
                self.selected = None;
            }
        }
    }

    pub fn revert(&mut self) {
        self.edit_preview = None;
        self.invalidate_selection_order();
        self.find.invalidate();
        self.local_filter.invalidate();
        self.pending_grid_focus = true;
        self.layout.clear_transposed();
        self.edits = EditState::default();
        self.edits.touch();
        self.selected = None;
    }

    /// Parameterized statements for the pending edits, as they will be executed.
    pub fn pending_mutations(
        &self,
        driver: DriverKind,
    ) -> Result<Vec<crate::model::BoundUpdate>, String> {
        let metadata = self.edit_metadata().ok_or_else(|| {
            self.edit_read_only_reason()
                .unwrap_or("This result is read-only.")
                .to_string()
        })?;
        crate::sqlgen::bound_mutations(
            driver,
            metadata,
            &self.data,
            &crate::sqlgen::PendingEdits {
                changed: &self.edits.changed,
                deleted: &self.edits.deleted,
                inserted: &self.edits.inserted,
            },
        )
    }

    /// Primary key column positions of an editable result.
    pub fn primary_key_columns(&self) -> &[usize] {
        self.edit_metadata()
            .map_or(&[], |metadata| metadata.primary_key.as_slice())
    }

    fn pending_preview(&mut self, driver: DriverKind) -> &str {
        if self.edit_preview.is_none() {
            let preview = if self.edit_metadata().is_some() {
                match self.pending_mutations(driver) {
                    Ok(mutations) => {
                        // Display only: execution binds the values separately.
                        let statements = mutations
                            .iter()
                            .map(|mutation| {
                                let values = mutation
                                    .params
                                    .iter()
                                    .enumerate()
                                    .map(|(index, value)| {
                                        let shown = crate::sqlgen::literal(driver, value)
                                            .replace(['\n', '\r'], " ");
                                        format!("{} = {shown}", index + 1)
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                if values.is_empty() {
                                    format!("{};", mutation.sql)
                                } else {
                                    format!("{};\n-- parameters: {values}", mutation.sql)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("{statements}\n\n-- Values are sent as bound parameters, never inside the SQL text.")
                    }
                    Err(reason) => format!("Cannot preview changes: {reason}"),
                }
            } else {
                self.edit_read_only_reason()
                    .unwrap_or("This result is read-only.")
                    .to_owned()
            };
            self.edit_preview = Some(preview);
        }
        self.edit_preview.as_deref().unwrap_or_default()
    }

    fn export_page(&self, format: ExportFormat, driver: DriverKind) -> String {
        let rows = self.order.iter().map(|&i| self.data.rows[i].as_slice());
        export::render_rows(format, &self.data, rows, &self.title, driver)
    }
}

enum Shown<'a> {
    Value(&'a Value),
    Edit(&'a CellEdit),
}

/// Toolbar above the grid: paging, reload, editing and export.
fn compare_button(ui: &mut Ui, result: &ResultSet) -> egui::Response {
    let enabled = result.edits.is_empty() && result.edits.editing.is_none();
    ui.add_enabled(enabled, egui::Button::new("Compare Data"))
        .on_hover_text("Compare fetched pages with another open result")
        .on_disabled_hover_text("Submit or revert pending changes before comparing data")
}

pub fn grid_toolbar(
    ui: &mut Ui,
    rs: &mut ResultSet,
    driver: DriverKind,
    page_size: usize,
    pageable: bool,
) -> Option<GridAction> {
    let mut action =
        std::mem::take(&mut rs.pending_related_rows).then_some(GridAction::RelatedRows);
    if std::mem::take(&mut rs.pending_compare_data) {
        action = Some(GridAction::CompareData);
    }
    ui.add_space(2.0);
    let controls = |ui: &mut Ui| {
        theme::flat_widgets(ui);
        let n = rs.data.rows.len();
        let from = rs.page.skip + 1;
        let more = if rs.data.truncated { "+" } else { "" };
        // A page that stopped on its byte budget holds fewer rows than asked.
        let size_limited = rs.data.truncated && n < rs.page.limit;
        let range = if n == 0 {
            "0 rows".to_string()
        } else if size_limited {
            format!(
                "{from}-{} of {}{more} ({} MB page limit)",
                rs.page.skip + n,
                rs.page.skip + n,
                crate::model::PAGE_BYTES >> 20
            )
        } else {
            format!("{from}-{} of {}{more}", rs.page.skip + n, rs.page.skip + n)
        };
        let prev =
            theme::tool_button_enabled(ui, pageable && rs.page.skip > 0, "◀", "Previous page");
        if prev.clicked() {
            action = Some(GridAction::Page(Page {
                skip: rs.page.skip.saturating_sub(page_size),
                limit: page_size,
            }));
        }
        ui.label(RichText::new(range).color(theme::TEXT_MUTED));
        let next = theme::tool_button_enabled(ui, pageable && rs.data.truncated, "▶", "Next page");
        if next.clicked() {
            action = Some(GridAction::Page(Page {
                skip: rs.page.skip + n,
                limit: page_size,
            }));
        }
        egui::ComboBox::from_id_salt(("page_size", &rs.sql))
            .selected_text(page_size.to_string())
            .width(60.0)
            .show_ui(ui, |ui| {
                for size in [100, 200, 500, 1000, 5000] {
                    if ui
                        .selectable_label(size == page_size, size.to_string())
                        .clicked()
                    {
                        action = Some(GridAction::PageSize(size));
                    }
                }
            })
            .response
            .on_hover_text("Page size");
        ui.separator();
        let reload_enabled = rs.edits.is_empty() && rs.edits.editing.is_none();
        if theme::tool_button_enabled(ui, reload_enabled, "⟳", "Reload page")
            .on_disabled_hover_text("Submit or revert pending changes before reloading")
            .clicked()
        {
            action = Some(GridAction::Rerun);
        }
        if rs.can_submit_edits() || !rs.edits.is_empty() || rs.edits.editing.is_some() {
            ui.separator();
            if rs.can_add_row() {
                let add_tip = format!("Add Row ({})", ui.ctx().format_shortcut(&ADD_ROW));
                if theme::tool_button(ui, RichText::new("+").size(15.0), &add_tip).clicked() {
                    rs.add_row();
                }
                let can_delete = rs
                    .valid_selection()
                    .is_some_and(|(row, _)| rs.can_delete_row(row));
                if theme::tool_button_enabled(
                    ui,
                    can_delete,
                    RichText::new("−").size(15.0),
                    &format!("Delete Row ({})", ui.ctx().format_shortcut(&DELETE_ROW)),
                )
                .clicked()
                    && let Some((row, _)) = rs.selected
                {
                    rs.delete_row(row);
                }
            }
            let dirty = !rs.edits.is_empty() || rs.edits.editing.is_some();
            if theme::tool_button_enabled(ui, dirty, "Revert", "Revert all changes").clicked() {
                rs.revert();
            }
            let submit = egui::Button::new(RichText::new("Submit").color(if dirty {
                Color32::WHITE
            } else {
                theme::TEXT_MUTED
            }))
            .fill(if dirty {
                theme::ACCENT
            } else {
                Color32::TRANSPARENT
            });
            if ui
                .add_enabled(dirty && rs.can_submit_edits(), submit)
                .on_hover_text(theme::keys("Apply changes to the database (⌘↵)"))
                .clicked()
            {
                rs.commit_find_buffer();
                action = Some(GridAction::Submit);
            }
            if dirty
                && ui
                    .toggle_value(&mut rs.edits.show_preview, "DML")
                    .on_hover_text("Preview generated SQL")
                    .clicked()
            {
                rs.pending_grid_focus = true;
            }
        }

        // Reserve space for the two menus before admitting secondary actions.
        // Widget count is fixed; this never depends on rows or column metadata.
        let reserved =
            toolbar_button_width(ui, "Options  ⏷") + toolbar_button_width(ui, "Export  ⏷") + 30.0;
        let mut overflow = [false; RESULT_VIEWS.len()];
        for (index, label) in RESULT_VIEWS.iter().enumerate() {
            if (index != 5 || !rs.geometry_columns(ui.ctx()).is_empty())
                && (index != 6 || rs.depth_may_apply())
                && ui.available_width() >= toolbar_button_width(ui, label) + reserved
            {
                result_view_action(ui, rs, index, false);
            } else {
                overflow[index] = true;
            }
        }
        let compare_visible =
            ui.available_width() >= toolbar_button_width(ui, "Compare Data") + reserved;
        if compare_visible && compare_button(ui, rs).clicked() {
            action = Some(GridAction::CompareData);
        }
        theme::tool_menu(ui, "Options  ⏷", "Show Options Menu", |ui| {
            if !compare_visible && compare_button(ui, rs).clicked() {
                action = Some(GridAction::CompareData);
                ui.close();
            }
            for (index, hidden) in overflow.into_iter().enumerate() {
                if hidden && result_view_action(ui, rs, index, true) {
                    ui.close();
                }
            }
            if overflow.iter().any(|hidden| *hidden) {
                ui.separator();
            }
            if ui.button("Clear Local Filter For All Columns").clicked() {
                rs.clear_local_filters();
                ui.close();
            }
            if ui
                .checkbox(&mut rs.show_value_editor, "Show Value Editor")
                .changed()
            {
                rs.commit_find_buffer();
                rs.pending_grid_focus = true;
                ui.close();
            }
        });

        ui.separator();
        theme::tool_menu(ui, "Export  ⏷", "Export data", |ui| {
            if ui.button("Export Data…").on_hover_text("The whole result, to a file").clicked() {
                action = Some(GridAction::ExportData);
                ui.close();
            }
            ui.separator();
            ui.label(
                RichText::new("Fetched page to clipboard")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
            for f in ExportFormat::ALL {
                if ui.button(format!("Copy as {}", f.label())).clicked() {
                    ui.ctx().copy_text(rs.export_page(f, driver));
                    rs.pending_grid_focus = true;
                    ui.close();
                }
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // The edit state stays visible however narrow the toolbar gets.
            if ui.available_width() > 120.0 {
                ui.label(
                    RichText::new(format!("{} ms", rs.data.elapsed.as_millis()))
                        .color(theme::TEXT_MUTED),
                );
            }
            edit_status_chip(ui, rs);
        });
    };
    ui.horizontal(controls);

    rs.pending_grid_focus |= action.is_some();
    if rs.edits.show_preview && !rs.edits.is_empty() {
        let sql = rs.pending_preview(driver);
        egui::Frame::new()
            .fill(theme::BG_EDITOR)
            .inner_margin(6)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("dml_preview")
                    .max_height(120.0)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(sql).monospace()).selectable(true));
                    });
            });
    }
    action
}

const RESULT_VIEWS: [&str; 7] = [
    "Find",
    "Local Filter",
    "Transpose",
    "Record View",
    "Aggregate View",
    "🌐",
    "Depth View",
];

fn toolbar_button_width(ui: &Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font, theme::TEXT)
            .size()
            .x
    }) + ui.spacing().button_padding.x * 2.0
        + ui.spacing().item_spacing.x
}

/// `in_menu`: drawn in the Options menu, where an icon alone has no name.
fn result_view_action(ui: &mut Ui, rs: &mut ResultSet, index: usize, in_menu: bool) -> bool {
    let mut clicked = false;
    match index {
        0 => {
            let find_button = ui.add_enabled(rs.geo.is_none(), egui::Button::new("Find"));
            find_button.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Find on Current Page")
            });
            if find_button
                .on_hover_text(format!(
                    "Find on Current Page ({})",
                    ui.ctx()
                        .format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::F))
                ))
                .clicked()
            {
                clicked = true;
                rs.open_find();
            }
        }
        1 => {
            let local = ui.add_enabled(
                rs.geo.is_none(),
                egui::Button::selectable(rs.local_filter.enabled, "Local Filter"),
            );
            local.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    rs.geo.is_none(),
                    rs.local_filter.enabled,
                    "Enable Local Filter",
                )
            });
            if local
                .on_hover_text("Enable Local Filter (current page only)")
                .clicked()
            {
                clicked = true;
                rs.toggle_local_filter();
            }
        }
        2 => {
            if ui
                .toggle_value(&mut rs.transposed, "Transpose")
                .on_hover_text("Show columns as rows (one column per record)")
                .clicked()
            {
                clicked = true;
                rs.commit_find_buffer();
                rs.find.invalidate();
                rs.aggregate.invalidate();
                rs.pending_reveal = true;
                rs.pending_grid_focus = true;
            }
        }
        3 => {
            let record_tip = format!(
                "Show the selected record ({})",
                ui.ctx().format_shortcut(&OPEN_RECORD_VIEW)
            );
            if ui
                .toggle_value(&mut rs.record_view, "Record View")
                .on_hover_text(record_tip)
                .clicked()
            {
                clicked = true;
                if rs.record_view {
                    rs.aggregate_view = false;
                }
                rs.pending_reveal = true;
                rs.pending_grid_focus = true;
            }
        }
        4 => {
            let aggregate = ui.add_enabled(
                rs.geo.is_none(),
                egui::Button::selectable(rs.aggregate_view, "Aggregate View"),
            );
            aggregate.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    rs.geo.is_none(),
                    rs.aggregate_view,
                    "Aggregate View",
                )
            });
            if aggregate.on_hover_text("Show Aggregate View").clicked() {
                clicked = true;
                rs.commit_find_buffer();
                rs.aggregate_view = !rs.aggregate_view;
                if rs.aggregate_view {
                    rs.record_view = false;
                }
                rs.pending_grid_focus = true;
            }
        }
        5 => {
            let geometry_column = rs.geometry_columns(ui.ctx()).first().copied();
            let globe = egui::Button::selectable(
                rs.geo.is_some(),
                RichText::new(if in_menu { "🌐 Geo Viewer" } else { "🌐" }).size(if in_menu { 13.0 } else { 15.0 }),
            );
            let tip = if geometry_column.is_none() {
                "Show Geo Viewer (no geometry columns in this result)"
            } else {
                "Show Geo Viewer"
            };
            if ui
                .add_enabled(geometry_column.is_some(), globe)
                .on_hover_text(tip)
                .on_disabled_hover_text(tip)
                .clicked()
            {
                clicked = true;
                rs.pending_grid_focus = true;
                rs.geo = match rs.geo {
                    Some(_) => None,
                    None => Some(GeoState::new(geometry_column.unwrap())),
                };
            }
        }
        6 => {
            let available = rs.geo.is_none() && rs.depth_may_apply();
            let depth = ui.add_enabled(
                available,
                egui::Button::selectable(rs.depth.open, "Depth View"),
            );
            depth.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    available,
                    rs.depth.open,
                    "Depth View",
                )
            });
            if depth
                .on_hover_text("Show rows in depth by one to three date columns, on one time axis")
                .on_disabled_hover_text("Depth View needs a date or time column in this result")
                .clicked()
            {
                clicked = true;
                rs.toggle_depth_view();
            }
        }
        _ => unreachable!("fixed result view action"),
    }
    clicked
}

/// Interactions collected while drawing the grid, applied once it is drawn.
#[derive(Default)]
struct GridEvents {
    editor_id: Option<egui::Id>,
    clicked_header: Option<usize>,
    selected_column: Option<usize>,
    selected_row: Option<GridRow>,
    select_all: bool,
    clicked_cell: Option<(GridRow, usize, CellSurface)>,
    extend_selection: bool,
    drag_anchor: Option<(GridRow, usize)>,
    drag_to: Option<(GridRow, usize)>,
    selection_range: Option<selection::CellRange>,
    start_edit: Option<(GridRow, usize, CellSurface)>,
    /// Enter or a double click on a result without edit authority.
    edit_refused: bool,
    set_edits: Vec<(GridRow, usize, CellEdit)>,
    delete: Option<GridRow>,
    add_row: bool,
    clone_row: Option<GridRow>,
    editing: Option<(GridRow, usize, String)>,
    finish_edit: Option<bool>,
    return_focus: bool,
    toggle_transpose: bool,
    show_record: bool,
    close_record: bool,
    show_aggregate: bool,
    close_aggregate: bool,
    show_value_editor: bool,
    open_find: bool,
    related_rows: bool,
    compare_data: bool,
    local_filter_open: Option<(usize, egui::Rect)>,
    editing_surface: CellSurface,
    editing_needs_focus: bool,
    reveal: Option<(GridRow, usize)>,
    revealed: bool,
    reveal_rect: Option<egui::Rect>,
    reveal_after: bool,
}

/// Width of a monospace character at the default grid font size.
const CHAR_WIDTH: f32 = 7.6;
const DEFAULT_GRID_FONT: f32 = 13.0;

/// Row height, stripes and width scale of the grid, read once per table.
struct GridLook {
    row_height: f32,
    striped: bool,
    scale: f32,
}

impl GridLook {
    fn of(ui: &Ui) -> Self {
        let appearance = theme::appearance(ui.ctx());
        Self {
            row_height: appearance.grid_row_height(),
            striped: appearance.striped_rows,
            scale: appearance.grid_font_size / DEFAULT_GRID_FONT,
        }
    }
}

pub fn grid(ui: &mut Ui, rs: &mut ResultSet, tiles: &mut super::map::TileCache) {
    let geo = rs.geo.is_some();
    if geo && !rs.record_view {
        rs.action_focus = Default::default();
        geo_view(ui, rs, tiles);
        return;
    }
    if rs.depth.open {
        rs.action_focus = Default::default();
        depth_view::show(ui, rs);
        return;
    }
    if !rs.record_view && rs.edits.editing_surface == CellSurface::Record {
        rs.edits.editing_surface = CellSurface::Grid;
        rs.edits.editing_needs_focus = rs.edits.editing.is_some();
        rs.pending_reveal = true;
    }
    if !rs.record_view {
        rs.focused_surface = CellSurface::Grid;
    }
    let focus_id = ui.id().with("result_grid_focus");
    rs.action_focus.grid = Some(focus_id);
    ui.interact(
        ui.available_rect_before_wrap(),
        focus_id,
        Sense::focusable_noninteractive(),
    );
    if !ui.ctx().any_popup_open() && std::mem::take(&mut rs.pending_grid_focus) {
        ui.ctx().memory_mut(|m| m.request_focus(focus_id));
    }
    ui.ctx().memory_mut(|m| {
        m.set_focus_lock_filter(
            focus_id,
            egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        );
    });
    // Filters and Find belong to the grid; Geo Viewer shows the whole page.
    if !geo {
        rs.local_filter_ui(ui, focus_id);
        rs.find_bar(ui, focus_id);
    }
    rs.sync_selection();
    if !ui.input(|input| input.pointer.primary_down()) {
        rs.selection.dragging = false;
    }
    let reveal = std::mem::take(&mut rs.pending_reveal)
        .then(|| rs.valid_selection())
        .flatten();
    let mut ev = GridEvents {
        selection_range: rs.selection_bounds(),
        reveal,
        editing: rs.edits.editing.take(),
        editing_surface: rs.edits.editing_surface,
        editing_needs_focus: rs.edits.editing_needs_focus,
        ..Default::default()
    };
    // Only this grid's keyboard focus owns navigation and row shortcuts.
    if ui.is_enabled()
        && ui.ctx().memory(|m| m.has_focus(focus_id))
        && ev.editing.is_none()
        && !ui.ctx().text_edit_focused()
        && !ui.ctx().any_popup_open()
    {
        grid_keys(ui, rs, &mut ev);
    }
    if rs.record_view {
        record::panel(ui, rs, &mut ev);
    }
    if rs.aggregate_view && !geo {
        aggregate_view::panel(ui, rs, &mut ev);
    }
    let grid_viewport = ui.available_rect_before_wrap();
    // Value/Record/Aggregate panels may appear after a cell is selected. Keep
    // table and popup identities independent of the panels' automatic UI IDs.
    ui.scope_builder(
        egui::UiBuilder::new().id(ui.id().with("result_grid_table")),
        |ui| {
            if geo {
                // Record View stays beside the map, as it does beside the grid.
                geo_view(ui, rs, tiles);
            } else if rs.transposed {
                transposed_grid(ui, rs, &mut ev);
            } else {
                normal_grid(ui, rs, &mut ev);
            }
        },
    );

    selection::drag_autoscroll(ui, rs, &mut ev, grid_viewport);
    if let Some((row, col, surface)) = ev.clicked_cell {
        rs.select_cell(
            (row, col),
            ev.extend_selection && surface == CellSurface::Grid,
        );
        rs.focused_surface = surface;
        ui.ctx().memory_mut(|m| m.request_focus(focus_id));
    }
    if let Some(anchor) = ev.drag_anchor {
        rs.select_cell(anchor, false);
        rs.selection.dragging = true;
        rs.focused_surface = CellSurface::Grid;
        ui.ctx().memory_mut(|m| m.request_focus(focus_id));
    }
    if let Some(target) = ev.drag_to {
        rs.select_cell(target, true);
    }
    if ev.select_all {
        rs.select_all_cells();
        rs.pending_grid_focus = true;
    }
    if let Some(column) = ev.selected_column {
        rs.select_column(column);
        rs.pending_grid_focus = true;
    }
    if let Some(row) = ev.selected_row {
        rs.select_row(row);
        rs.pending_grid_focus = true;
    }
    // A newly selected cell/header may be outside the editor's viewport, so
    // its old TextEdit cannot always report lost_focus during this frame.
    if ev.finish_edit.is_none()
        && (ev.clicked_cell.is_some()
            || ev.drag_anchor.is_some()
            || ev.select_all
            || ev.selected_column.is_some()
            || ev.selected_row.is_some()
            || ev.clicked_header.is_some()
            || ev.toggle_transpose)
    {
        ev.finish_edit = Some(true);
    }
    let mut editing = ev.editing;
    match ev.finish_edit {
        Some(true) => {
            if let Some((row, c, text)) = editing.take() {
                rs.set_cell(row, c, CellEdit::Text(text));
            }
        }
        Some(false) => editing = None,
        None => {}
    }
    if ev.related_rows {
        rs.pending_related_rows = true;
        ui.ctx().request_repaint();
    }
    if ev.compare_data {
        rs.pending_compare_data = true;
        ui.ctx().request_repaint();
    }
    if ev.return_focus {
        ui.ctx().memory_mut(|m| m.request_focus(focus_id));
    }
    rs.edits.editing = editing;
    if let Some(id) = ev.editor_id {
        rs.action_focus.editor = Some(id);
    } else if rs.edits.editing.is_none() {
        rs.action_focus.editor = None;
    }
    rs.edits.editing_needs_focus = ev.editing_needs_focus;
    if let Some(c) = ev.clicked_header {
        rs.toggle_sort(c);
        rs.pending_grid_focus = true;
    }
    for (row, c, e) in ev.set_edits {
        rs.set_cell(row, c, e);
    }
    if let Some(row) = ev.delete {
        rs.delete_row(row);
    }
    if let Some(row) = ev.clone_row {
        rs.clone_row(row);
    }
    if ev.add_row {
        rs.add_row();
    }
    if ev.edit_refused
        || ev
            .start_edit
            .is_some_and(|(row, c, _)| !rs.can_edit_text(row, c))
    {
        rs.note_refused_edit();
    }
    if let Some((row, c, surface)) = ev.start_edit
        && rs.can_edit_text(row, c)
    {
        let text = match rs.cell(row, c) {
            Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null | CellEdit::Default) => {
                String::new()
            }
            Shown::Value(v) => v.to_string(),
            Shown::Edit(CellEdit::Text(s)) => s.clone(),
        };
        rs.select_cell((row, c), false);
        rs.focused_surface = surface;
        rs.edits.editing = Some((row, c, text));
        rs.edits.editing_surface = surface;
        rs.edits.editing_needs_focus = true;
        rs.pending_reveal = true;
    }
    if ev.toggle_transpose {
        rs.transposed = !rs.transposed;
        rs.find.invalidate();
        rs.aggregate.invalidate();
        rs.pending_reveal = true;
    }
    if ev.show_record {
        rs.record_view = true;
        rs.aggregate_view = false;
        rs.pending_reveal = true;
    }
    if ev.close_record {
        rs.pending_grid_focus = true;
        rs.record_view = false;
        rs.focused_surface = CellSurface::Grid;
        rs.edits.editing_surface = CellSurface::Grid;
    }
    if ev.show_aggregate {
        rs.commit_find_buffer();
        rs.aggregate_view = true;
        rs.record_view = false;
        rs.pending_grid_focus = true;
    }
    if ev.close_aggregate {
        rs.aggregate_view = false;
        rs.pending_grid_focus = true;
    }
    if ev.clicked_cell.is_some()
        || ev.drag_anchor.is_some()
        || ev.drag_to.is_some()
        || ev.select_all
        || ev.selected_column.is_some()
        || ev.selected_row.is_some()
        || ev.show_aggregate
        || ev.close_aggregate
    {
        ui.ctx().request_repaint();
    }
    if ev.show_value_editor {
        rs.show_value_editor = true;
    }
    if let Some((column, anchor)) = ev.local_filter_open {
        rs.open_local_filter(column, anchor);
    }
    if ev.open_find {
        rs.open_find();
    }
    rs.pending_reveal |= ev.reveal_after || (ev.reveal.is_some() && !ev.revealed);
    if !ui.ctx().any_popup_open()
        && std::mem::take(&mut rs.pending_grid_focus)
        && rs.edits.editing.is_none()
    {
        ui.ctx().memory_mut(|m| m.request_focus(focus_id));
    }
}

fn grid_keys(ui: &mut Ui, rs: &ResultSet, ev: &mut GridEvents) {
    let mut delta = None;
    let mut copy = false;
    ui.input_mut(|i| {
        for (key, horizontal, vertical) in [
            (Key::ArrowLeft, -1, 0),
            (Key::ArrowRight, 1, 0),
            (Key::ArrowUp, 0, -1),
            (Key::ArrowDown, 0, 1),
        ] {
            let extend = rs.focused_surface == CellSurface::Grid
                && i.consume_shortcut(&KeyboardShortcut::new(Modifiers::SHIFT, key));
            if extend || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, key)) {
                ev.extend_selection = extend;
                delta = Some(if rs.focused_surface == CellSurface::Record {
                    (0, vertical)
                } else if rs.transposed {
                    (horizontal, vertical)
                } else {
                    (vertical, horizontal)
                });
            }
        }
        if rs.focused_surface == CellSurface::Grid
            && i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::A))
        {
            ev.select_all = true;
        }
        if i.consume_shortcut(&OPEN_RECORD_VIEW) {
            ev.show_record = true;
        }
        if i.consume_shortcut(&OPEN_VALUE_EDITOR) {
            ev.show_value_editor = true;
        }
        copy = i.consume_shortcut(&COPY_VALUE);
        i.events.retain(|event| {
            if matches!(event, egui::Event::Copy) {
                copy = true;
                false
            } else {
                true
            }
        });
        if rs.can_add_row() && i.consume_shortcut(&ADD_ROW) {
            ev.add_row = true;
        }
        if let Some((row, col)) = rs.valid_selection() {
            if rs.can_edit_text(row, col)
                && i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::Enter))
            {
                ev.start_edit = Some((row, col, rs.focused_surface));
            } else if rs.edit_metadata().is_none()
                && i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::Enter))
            {
                ev.edit_refused = true;
            }
            if rs.can_delete_row(row) && i.consume_shortcut(&DELETE_ROW) {
                ev.delete = Some(row);
            }
            if rs.can_clone_row(row) && i.consume_shortcut(&CLONE_ROW) {
                ev.clone_row = Some(row);
            }
        }
    });
    if let Some((row_delta, col_delta)) = delta
        && let Some((row, col)) = rs.moved_selection(row_delta, col_delta)
    {
        ev.clicked_cell = Some((row, col, rs.focused_surface));
        ev.reveal_after = true;
    }
    if copy {
        let value = if rs.focused_surface == CellSurface::Record {
            rs.valid_selection()
                .map(|(row, col)| rs.copy_cell(row, col))
        } else {
            rs.copy_selection()
        };
        if let Some(value) = value {
            ui.ctx().copy_text(value);
        }
    }
}

/// Edit state in the toolbar as a short word with the full sentence in its
/// tooltip and accessible name, instead of a whole line above the grid.
fn edit_status_chip(ui: &mut Ui, rs: &ResultSet) {
    if rs.edit_metadata_pending() {
        ui.spinner();
        return;
    }
    let read_only = rs.edit_read_only_reason();
    let (text, color) = match read_only {
        Some(_) => ("Read-only", theme::SQL_KEYWORD),
        None => ("Editable", theme::TEXT_MUTED),
    };
    let response = ui
        .add(egui::Label::new(RichText::new(text).small().color(color)).sense(Sense::hover()))
        .on_hover_text(read_only.unwrap_or(
            "Rows are identified by the primary key; every change must affect exactly one row.",
        ));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &rs.edit_status));
}

/// Text width of a column: its name or the widest of the first values.
fn text_width(chars: usize) -> f32 {
    (chars.min(40) as f32 * CHAR_WIDTH + 20.0).max(60.0)
}

fn row_label(rs: &ResultSet, display_index: usize, row: GridRow) -> String {
    match row {
        GridRow::Data(_) => (rs.page.skip + display_index + 1).to_string(),
        GridRow::New(_) => "+".to_string(),
    }
}

/// Column name selects its cells; a separate icon cycles the local sort order.
fn column_name(ui: &mut Ui, rs: &ResultSet, c: usize, ev: &mut GridEvents) {
    let col = &rs.data.columns[c];
    // An arrow only on the sorted column; elsewhere a quiet
    // hint appears under the pointer, so headers do not compete with names.
    let hovered = ui.ui_contains_pointer();
    let arrow = match rs.sort {
        Some((sc, true)) if sc == c => RichText::new("↑").color(theme::TEXT),
        Some((sc, false)) if sc == c => RichText::new("↓").color(theme::TEXT),
        _ if hovered => RichText::new("↕").color(theme::TEXT_MUTED),
        _ => RichText::new(" "),
    };
    let key = if rs.primary_key_columns().contains(&c) {
        "🔑"
    } else {
        ""
    };
    ui.horizontal(|ui| {
        let reserved = 22.0 + if rs.local_filter.enabled { 26.0 } else { 0.0 };
        let label_width = (ui.available_width() - reserved).max(0.0);
        let r = ui
            .allocate_ui_with_layout(
                egui::vec2(label_width, 20.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_min_width(label_width);
                    ui.add(
                        egui::Label::new(RichText::new(format!("{key}{}", col.name)).strong())
                            .sense(Sense::click())
                            .truncate(),
                    )
                },
            )
            .inner
            .on_hover_text(format!("{}: {}\nSelect Column", col.name, col.type_name));
        r.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                format!("Select Column: {}", col.name),
            )
        });
        if r.clicked() {
            ev.selected_column = Some(c);
        }
        let sort = ui.add_sized(
            egui::vec2(16.0, 20.0),
            egui::Button::new(arrow).frame(false),
        );
        let sort_label = format!("Sort by {}", col.name);
        sort.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &sort_label));
        if sort.on_hover_text(sort_label).clicked() {
            ev.clicked_header = Some(c);
        }
        if let Some(anchor) = local_filter::header_button(ui, rs, c) {
            ev.local_filter_open = Some((c, anchor));
        }
    });
}

/// Grey cell with the record number, shared by both layouts.
fn row_number_cell(ui: &mut Ui, label: String, row: GridRow, ev: &mut GridEvents) {
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, theme::BG_PANEL);
    grid_lines(ui);
    ui.add_space(6.0);
    let response = ui.add(
        egui::Label::new(RichText::new(&label).color(theme::TEXT_MUTED).monospace())
            .sense(Sense::click()),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("Select Row: {label}"),
        )
    });
    if response.clicked() {
        ev.selected_row = Some(row);
    }
}

#[cfg(test)]
thread_local! {
    /// Cells whose contents were built, to check that drawing follows the viewport.
    static CELLS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One value cell: background for pending changes, in-place editor, context menu.
fn data_cell(
    row: &mut egui_extras::TableRow<'_, '_>,
    rs: &ResultSet,
    grid_row: GridRow,
    c: usize,
    surface: CellSurface,
    ev: &mut GridEvents,
) {
    let editable = rs.can_edit_cell(grid_row, c);
    let text_editable = rs.can_edit_text(grid_row, c);
    let deleted = matches!(grid_row, GridRow::Data(i) if rs.edits.deleted.contains(&i));
    let modified = matches!(grid_row, GridRow::Data(i) if rs.edits.changed.contains_key(&(i, c)));
    let shown = rs.cell(grid_row, c);
    let selected = if surface == CellSurface::Grid {
        rs.display_index(grid_row).is_some_and(|index| {
            ev.selection_range
                .is_some_and(|range| range.contains(index, c))
        })
    } else {
        rs.selected == Some((grid_row, c))
    };
    let editing_here = ev.editing_surface == surface
        && ev
            .editing
            .as_ref()
            .is_some_and(|(row, col, _)| *row == grid_row && *col == c);
    let (_, resp) = row.col(|ui| {
        #[cfg(test)]
        CELLS_BUILT.with(|cells| cells.set(cells.get() + 1));
        if surface == CellSurface::Grid && ev.reveal == Some((grid_row, c)) {
            let rect = ui.max_rect();
            ev.reveal_rect = Some(rect);
            // Resizable table widths may settle one frame after a new column
            // appears. Drawing a clipped cell does not finish its reveal.
            let clip = ui.clip_rect().expand(ui.ctx().pixels_per_point().recip());
            ev.revealed = clip.top() <= rect.top() && rect.bottom() <= clip.bottom();
        }
        let bg = match grid_row {
            GridRow::New(_) => Some(INSERTED_BG),
            _ if deleted => Some(DELETED_BG),
            _ if modified => Some(MODIFIED_BG),
            _ => None,
        };
        if let Some(bg) = bg {
            ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
        }
        if selected && !editing_here {
            ui.painter()
                .rect_filled(ui.max_rect(), 0.0, theme::BG_SELECTED);
            if let Some(color) = bg {
                let rect = ui.max_rect().shrink(2.0);
                ui.painter().line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    egui::Stroke::new(2.0, color),
                );
            }
        }
        if !editing_here && rs.find.matches_cell(grid_row, c) {
            if bg.is_none() && !selected {
                let fill = if rs.selected == Some((grid_row, c)) {
                    theme::SEARCH_CURRENT_MATCH
                } else {
                    theme::SEARCH_MATCH
                };
                ui.painter().rect_filled(ui.max_rect(), 0.0, fill);
            } else {
                // Preserve pending-change colors while marking the matching cell.
                let rect = ui.max_rect().shrink(2.0);
                ui.painter().line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    egui::Stroke::new(2.0, theme::SEARCH_MARKER),
                );
            }
        }
        grid_lines(ui);
        ui.add_space(6.0);
        if rs.selected == Some((grid_row, c)) {
            ui.painter().rect_stroke(
                ui.max_rect(),
                0.0,
                egui::Stroke::new(1.0, theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        match &mut ev.editing {
            Some((er, ec, text))
                if *er == grid_row && *ec == c && ev.editing_surface == surface =>
            {
                let out = ui.add(
                    egui::TextEdit::singleline(text)
                        .font(egui::TextStyle::Monospace)
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY),
                );
                ev.editor_id = Some(out.id);
                if ev.editing_needs_focus {
                    out.request_focus();
                    ev.editing_needs_focus = false;
                }
                let (escape, enter) = ui.input_mut(|i| {
                    (
                        i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::Escape)),
                        i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::Enter)),
                    )
                });
                // Keep Submit's shortcut available to the parent after committing this buffer.
                let submit = ui.input(|i| {
                    i.key_pressed(Key::Enter) && i.modifiers.matches_logically(Modifiers::COMMAND)
                });
                if escape || enter || submit {
                    ev.finish_edit = Some(enter || submit);
                    ev.return_focus = true;
                } else if out.lost_focus() {
                    ev.finish_edit = Some(true);
                }
            }
            _ => cell(ui, shown, deleted, selected),
        }
    });
    if resp.clicked() && !editing_here {
        ev.clicked_cell = Some((grid_row, c, surface));
        ev.extend_selection =
            surface == CellSurface::Grid && resp.ctx.input(|input| input.modifiers.shift);
    }
    if resp.secondary_clicked() && !selected && !editing_here {
        ev.clicked_cell = Some((grid_row, c, surface));
    }
    if surface == CellSurface::Grid && !editing_here {
        if resp.drag_started() {
            ev.drag_anchor = Some((grid_row, c));
        }
        if (rs.selection.dragging || ev.drag_anchor.is_some())
            && resp.contains_pointer()
            && resp.ctx.input(|input| input.pointer.primary_down())
        {
            ev.drag_to = Some((grid_row, c));
        }
    }
    if text_editable && resp.double_clicked() && !deleted {
        ev.start_edit = Some((grid_row, c, surface));
    } else if resp.double_clicked() && rs.edit_metadata().is_none() {
        ev.edit_refused = true;
    }
    resp.context_menu(|ui| {
        if compare_button(ui, rs).clicked() {
            ev.compare_data = true;
            ui.close_kind(egui::UiKind::Menu);
        }
        ui.menu_button("Go To", |ui| {
            let reason = rs.related_row_anchor().err();
            if ui
                .add_enabled(
                    reason.is_none(),
                    egui::Button::new("Related Rows").shortcut_text("F4"),
                )
                .on_disabled_hover_text(reason.unwrap_or(""))
                .clicked()
            {
                ev.clicked_cell = Some((grid_row, c, surface));
                ev.related_rows = true;
                ui.close_kind(egui::UiKind::Menu);
            }
        });
        ui.separator();
        if ui.button("Copy value").clicked() {
            ui.ctx().copy_text(rs.copy_cell(grid_row, c));
            ev.return_focus = true;
            ui.close();
        }
        if ui.button("Copy row (TSV)").clicked() {
            ui.ctx().copy_text(rs.copy_row(grid_row));
            ev.return_focus = true;
            ui.close();
        }
        if ui
            .add(
                egui::Button::new("Copy").shortcut_text(
                    ui.ctx()
                        .format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::C)),
                ),
            )
            .on_hover_text("Copy selected cells as quoted TSV")
            .clicked()
        {
            if let Some(text) = rs.copy_selection() {
                ui.ctx().copy_text(text);
            }
            ev.return_focus = true;
            ui.close();
        }
        let transpose = if rs.transposed {
            "✔ Transpose"
        } else {
            "Transpose"
        };
        if ui
            .add(
                egui::Button::new("Find on Current Page").shortcut_text(
                    ui.ctx()
                        .format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::F)),
                ),
            )
            .clicked()
        {
            ev.clicked_cell = Some((grid_row, c, surface));
            ev.open_find = true;
            ui.close();
        }
        if ui.button(transpose).clicked() {
            ev.toggle_transpose = true;
            ev.return_focus = true;
            ui.close();
        }
        if ui.button("Show Aggregate View").clicked() {
            ev.show_aggregate = true;
            ev.return_focus = true;
            ui.close();
        }
        ui.separator();
        let shortcut = ui.ctx().format_shortcut(&OPEN_RECORD_VIEW);
        if ui
            .add(egui::Button::new("Show Record View").shortcut_text(shortcut))
            .clicked()
        {
            ev.clicked_cell = Some((grid_row, c, CellSurface::Record));
            ev.show_record = true;
            ev.return_focus = true;
            ui.close();
        }
        let shortcut = ui.ctx().format_shortcut(&OPEN_VALUE_EDITOR);
        if ui
            .add(egui::Button::new("Open in Value Editor").shortcut_text(shortcut))
            .clicked()
        {
            ev.clicked_cell = Some((grid_row, c, surface));
            ev.show_value_editor = true;
            ev.return_focus = true;
            ui.close();
        }
        {
            ui.separator();
            let reason = rs.cell_read_only_reason(grid_row, c).unwrap_or("");
            if ui
                .add_enabled(text_editable, egui::Button::new("Edit"))
                .on_disabled_hover_text(rs.text_read_only_reason(grid_row, c).unwrap_or(""))
                .clicked()
            {
                ev.start_edit = Some((grid_row, c, surface));
                ev.return_focus = true;
                ui.close();
            }
            if ui
                .add_enabled(editable, egui::Button::new("Set NULL"))
                .on_disabled_hover_text(reason)
                .clicked()
            {
                ev.set_edits.push((grid_row, c, CellEdit::Null));
                ev.return_focus = true;
                ui.close();
            }
            if ui
                .add_enabled(
                    rs.can_set_default(grid_row, c),
                    egui::Button::new("Set DEFAULT"),
                )
                .on_disabled_hover_text("DEFAULT is not available for this cell or database.")
                .clicked()
            {
                ev.set_edits.push((grid_row, c, CellEdit::Default));
                ev.return_focus = true;
                ui.close();
            }
            ui.separator();
            let shortcut = |s: &KeyboardShortcut| ui.ctx().format_shortcut(s);
            let (add, delete, clone) = (
                shortcut(&ADD_ROW),
                shortcut(&DELETE_ROW),
                shortcut(&CLONE_ROW),
            );
            if ui
                .add_enabled(
                    rs.can_add_row(),
                    egui::Button::new("Add Row").shortcut_text(add),
                )
                .clicked()
            {
                ev.add_row = true;
                ev.return_focus = true;
                ui.close();
            }
            let label = if deleted { "Restore Row" } else { "Delete Row" };
            if ui
                .add_enabled(
                    rs.can_delete_row(grid_row),
                    egui::Button::new(label).shortcut_text(delete),
                )
                .clicked()
            {
                ev.delete = Some(grid_row);
                ev.return_focus = true;
                ui.close();
            }
            if ui
                .add_enabled(
                    rs.can_clone_row(grid_row),
                    egui::Button::new("Clone Row").shortcut_text(clone),
                )
                .clicked()
            {
                ev.clone_row = Some(grid_row);
                ev.return_focus = true;
                ui.close();
            }
        }
    });
}

/// Records as rows and columns as columns.
fn normal_grid(ui: &mut Ui, rs: &ResultSet, ev: &mut GridEvents) {
    let cols = rs.data.columns.len();
    let widths = rs.normal_widths();
    let row_count = rs.row_count();
    let look = GridLook::of(ui);
    let row_label_w = ((rs.page.skip + row_count).max(1).to_string().len() as f32
        * CHAR_WIDTH
        * look.scale
        + 16.0)
        .max(36.0);

    let scroll = egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let minimum = if rs.local_filter.enabled { 100.0 } else { 74.0 };
            let resized = ColumnWidths::load(ui, egui::Id::new((&rs.sql, "rows")));
            let width_of = |c: usize| resized.width(c, widths[c] * look.scale, minimum);
            let spacing = ui.spacing().item_spacing.x;
            let window = column_window(ui, row_label_w + spacing, cols, width_of);
            if let Some((_, c)) = ev.reveal
                && !window.range.contains(&c)
            {
                reveal_outside_window(ui, ev, row_label_w + spacing, c, width_of);
            }
            let mut table = TableBuilder::new(ui)
                .id_salt((&rs.sql, "rows"))
                .striped(look.striped)
                .resizable(false)
                .sense(Sense::click_and_drag())
                .min_scrolled_height(0.0)
                .auto_shrink([false, false])
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(row_label_w));
            table = with_window_columns(table, &window, width_of);
            if let Some((row, _)) = ev.reveal
                && let Some(index) = rs.display_index(row)
            {
                table = table.scroll_to_row(index, None);
            }
            table
                .header(26.0, |mut header| {
                    header.col(|ui| {
                        header_cell_bg(ui);
                        let all = ui.add(egui::Label::new("▧").sense(Sense::click()));
                        all.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                "Select All Cells",
                            )
                        });
                        if all.clicked() {
                            ev.select_all = true;
                        }
                    });
                    spacer(&mut header, window.before);
                    for c in window.range.clone() {
                        header.col(|ui| {
                            header_cell_bg(ui);
                            ui.add_space(6.0);
                            column_name(ui, rs, c, ev);
                            resized.handle(ui, c, width_of(c), minimum);
                        });
                    }
                    spacer(&mut header, window.after);
                })
                .body(|body| {
                    body.rows(look.row_height, row_count, |mut row| {
                        let r = row.index();
                        let grid_row = rs.display_row(r).unwrap();
                        row.col(|ui| row_number_cell(ui, row_label(rs, r, grid_row), grid_row, ev));
                        spacer(&mut row, window.before);
                        for c in window.range.clone() {
                            data_cell(&mut row, rs, grid_row, c, CellSurface::Grid, ev);
                        }
                        spacer(&mut row, window.after);
                    });
                });
            // The nested vertical table consumes both scroll axes. Request horizontal
            // reveal only after it returns, so this reaches the surrounding ScrollArea.
            if let Some(rect) = ev.reveal_rect {
                ui.scroll_to_rect(rect, None);
            }
        });
    finish_horizontal_reveal(ui, ev, scroll.inner_rect);
}

/// Key of the transposed view's first column in `ColumnWidths`.
const NAME_COLUMN: usize = usize::MAX;

/// Widths the user dragged, kept in memory per query text so they survive a
/// rerun. Tables build only visible columns, so they own their widths instead
/// of the table's per-index state.
struct ColumnWidths {
    id: egui::Id,
    widths: HashMap<usize, f32>,
}

impl ColumnWidths {
    fn load(ui: &Ui, id: egui::Id) -> Self {
        let id = id.with("column_widths");
        Self {
            id,
            widths: ui.data(|data| data.get_temp(id)).unwrap_or_default(),
        }
    }

    fn width(&self, key: usize, default: f32, minimum: f32) -> f32 {
        self.widths.get(&key).copied().unwrap_or(default).max(minimum)
    }

    /// Drag handle on the right edge of a header cell; double-click restores
    /// the automatic width.
    fn handle(&self, ui: &mut Ui, key: usize, width: f32, minimum: f32) {
        let cell = ui.max_rect();
        let rect = egui::Rect::from_x_y_ranges(cell.right() - 5.0..=cell.right(), cell.y_range());
        let response = ui.interact(rect, self.id.with(key), Sense::click_and_drag());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Resize Column")
        });
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
            ui.painter().vline(
                cell.right() - 1.0,
                cell.y_range(),
                egui::Stroke::new(2.0, theme::ACCENT),
            );
        }
        let change = if response.double_clicked() {
            Some(None)
        } else if response.dragged() && response.drag_delta().x != 0.0 {
            Some(Some((width + response.drag_delta().x).max(minimum)))
        } else {
            None
        };
        if let Some(change) = change {
            ui.data_mut(|data| {
                let widths = data.get_temp_mut_or_default::<HashMap<usize, f32>>(self.id);
                match change {
                    Some(width) => widths.insert(key, width),
                    None => widths.remove(&key),
                };
            });
        }
    }
}

/// Columns of a horizontally scrolled table to build this frame. `offset` is
/// the distance from the table's left edge to the first windowed column.
fn column_window(
    ui: &Ui,
    offset: f32,
    count: usize,
    width_of: impl Fn(usize) -> f32,
) -> layout::ColumnWindow {
    let origin = ui.max_rect().left() + offset;
    let clip = ui.clip_rect();
    layout::column_window(
        (0..count).map(&width_of),
        ui.spacing().item_spacing.x,
        egui::Rangef::new(clip.left() - origin, clip.right() - origin),
    )
}

fn with_window_columns<'a>(
    mut table: TableBuilder<'a>,
    window: &layout::ColumnWindow,
    width_of: impl Fn(usize) -> f32,
) -> TableBuilder<'a> {
    if window.before > 0.0 {
        table = table.column(Column::exact(window.before));
    }
    for index in window.range.clone() {
        table = table.column(Column::exact(width_of(index)).clip(true));
    }
    if window.after > 0.0 {
        table = table.column(Column::exact(window.after));
    }
    table
}

/// Stands in for the columns outside the window.
fn spacer(row: &mut egui_extras::TableRow<'_, '_>, width: f32) {
    if width > 0.0 {
        row.col(|_| {});
    }
}

/// Scrolls toward a column that is not built this frame; the reveal finishes
/// once the column is in the window and drawn.
fn reveal_outside_window(
    ui: &Ui,
    ev: &mut GridEvents,
    offset: f32,
    index: usize,
    width_of: impl Fn(usize) -> f32,
) {
    let spacing = ui.spacing().item_spacing.x;
    let left = ui.max_rect().left() + offset + (0..index).map(|i| width_of(i) + spacing).sum::<f32>();
    let clip = ui.clip_rect();
    let rect = egui::Rect::from_x_y_ranges(left..=left + width_of(index), clip.y_range());
    ui.scroll_to_rect(rect, None);
    ev.revealed = false;
}

fn finish_horizontal_reveal(ui: &Ui, ev: &mut GridEvents, viewport: egui::Rect) {
    if let Some(rect) = ev.reveal_rect {
        let tolerance = ui.ctx().pixels_per_point().recip();
        ev.revealed &= if rect.width() <= viewport.width() + tolerance {
            viewport.left() - tolerance <= rect.left()
                && rect.right() <= viewport.right() + tolerance
        } else {
            // A cell wider than the viewport is revealed once it fills it.
            rect.left() <= viewport.left() + tolerance
                && viewport.right() - tolerance <= rect.right()
        };
    }
}

/// Transposed view: one row per column, one column per record.
fn transposed_grid(ui: &mut Ui, rs: &ResultSet, ev: &mut GridEvents) {
    let cols = rs.data.columns.len();
    let row_count = rs.row_count();
    let name_w = text_width(
        rs.data
            .columns
            .iter()
            .map(|c| c.name.chars().count() + 2)
            .max()
            .unwrap_or(4),
    );
    let look = GridLook::of(ui);
    let scroll = egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let resized = ColumnWidths::load(ui, egui::Id::new((&rs.sql, "transposed")));
            let name_minimum = if rs.local_filter.enabled { 126.0 } else { 100.0 };
            let name_w = resized.width(NAME_COLUMN, name_w, name_minimum);
            let width_of = |index: usize| {
                resized.width(index, rs.transposed_width(rs.display_row(index).unwrap()), 40.0)
            };
            let spacing = ui.spacing().item_spacing.x;
            let window = column_window(ui, name_w + spacing, row_count, width_of);
            if let Some((row, _)) = ev.reveal
                && let Some(index) = rs.display_index(row)
                && !window.range.contains(&index)
            {
                reveal_outside_window(ui, ev, name_w + spacing, index, width_of);
            }
            let mut table = TableBuilder::new(ui)
                .id_salt((&rs.sql, "transposed"))
                .striped(look.striped)
                .resizable(false)
                .sense(Sense::click_and_drag())
                .min_scrolled_height(0.0)
                .auto_shrink([false, false])
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(name_w).clip(true));
            table = with_window_columns(table, &window, width_of);
            if let Some((_, col)) = ev.reveal {
                table = table.scroll_to_row(col, None);
            }
            table
                .header(26.0, |mut header| {
                    header.col(|ui| {
                        header_cell_bg(ui);
                        resized.handle(ui, NAME_COLUMN, name_w, name_minimum);
                    });
                    spacer(&mut header, window.before);
                    for r in window.range.clone() {
                        let row = rs.display_row(r).unwrap();
                        header.col(|ui| {
                            header_cell_bg(ui);
                            ui.add_space(6.0);
                            row_number_cell(ui, row_label(rs, r, row), row, ev);
                            resized.handle(ui, r, width_of(r), 40.0);
                        });
                    }
                    spacer(&mut header, window.after);
                })
                .body(|body| {
                    body.rows(look.row_height, cols, |mut row| {
                        let c = row.index();
                        row.col(|ui| {
                            ui.painter()
                                .rect_filled(ui.max_rect(), 0.0, theme::BG_PANEL);
                            grid_lines(ui);
                            ui.add_space(6.0);
                            column_name(ui, rs, c, ev);
                        });
                        spacer(&mut row, window.before);
                        for index in window.range.clone() {
                            let grid_row = rs.display_row(index).unwrap();
                            data_cell(&mut row, rs, grid_row, c, CellSurface::Grid, ev);
                        }
                        spacer(&mut row, window.after);
                    });
                });
            // The nested vertical table consumes both scroll axes. Request horizontal
            // reveal only after it returns, so this reaches the surrounding ScrollArea.
            if let Some(rect) = ev.reveal_rect {
                ui.scroll_to_rect(rect, None);
            }
        });
    finish_horizontal_reveal(ui, ev, scroll.inner_rect);
}

/// Geo Viewer: the rows with a geometry in the chosen column on a map.
fn geo_view(ui: &mut Ui, rs: &mut ResultSet, tiles: &mut super::map::TileCache) {
    let columns = rs.geometry_columns(ui.ctx()).to_vec();
    let names: Vec<String> = rs.data.columns.iter().map(|c| c.name.clone()).collect();
    let selected_row = rs.selected.and_then(|(r, _)| match r {
        GridRow::Data(i) => Some(i),
        GridRow::New(_) => None,
    });
    let Some(state) = rs.geo.as_mut() else { return };
    if !columns.contains(&state.column) {
        state.column = columns.first().copied().unwrap_or(0);
    }
    let srid_override = state.srid_text.trim().parse::<u32>().ok();
    let (prepared, fresh) = state
        .layer
        .get(&rs.data, (state.column, srid_override), ui.ctx());
    state.fit |= fresh;
    let empty = super::map::MapLayer::default();
    let (layer, features) = match prepared {
        Some(p) => (Some(&p.info), &p.map),
        None => (None, &empty),
    };

    let mut fit = std::mem::take(&mut state.fit);
    ui.horizontal(|ui| {
        ui.label(super::map::muted("Geometry:"));
        if columns.len() > 1 {
            egui::ComboBox::from_id_salt(("geo_column", &rs.sql))
                .selected_text(&names[state.column])
                .show_ui(ui, |ui| {
                    for &c in &columns {
                        ui.selectable_value(&mut state.column, c, &names[c]);
                    }
                });
        } else {
            ui.label(&names[state.column]);
        }
        ui.separator();
        ui.label(super::map::muted("SRID:"));
        let hint = layer
            .and_then(|l| l.srid)
            .map_or_else(|| "?".to_string(), |s| s.to_string());
        ui.add(
            egui::TextEdit::singleline(&mut state.srid_text)
                .hint_text(hint)
                .desired_width(60.0),
        )
        .on_hover_text(
            "Leave empty to use the SRID stored with the data; enter an EPSG code to override it",
        );
        let source = if srid_override.is_some() {
            "set manually"
        } else {
            layer.map_or("", |l| l.srid_source)
        };
        if !source.is_empty() {
            ui.label(super::map::muted(format!("({source})")));
        }
        ui.separator();
        let Some(layer) = layer else {
            ui.spinner();
            ui.label(super::map::muted("Preparing geometries…"));
            return;
        };
        let objects = if features.len() == 1 {
            "1 object".to_string()
        } else {
            format!("{} objects", features.len())
        };
        ui.label(objects);
        if layer.skipped > 0 {
            ui.label(super::map::muted(format!(
                "· {} rows without geometry",
                layer.skipped
            )));
        }
        if let Some(e) = &layer.error {
            ui.label(RichText::new(e).color(theme::RED));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::tool_button(ui, "Fit", "Zoom to all objects").clicked() {
                fit = true;
            }
        });
    });
    let data = &rs.data;
    let geo_column = state.column;
    let tooltip = |ui: &mut Ui, row: usize| {
        egui::Grid::new("geo_tooltip")
            .num_columns(2)
            .spacing([10.0, 2.0])
            .show(ui, |ui| {
                for (c, col) in data.columns.iter().enumerate().take(12) {
                    let value = if c == geo_column {
                        format!("<{}>", col.type_name)
                    } else {
                        data.rows[row][c].cell_preview(60)
                    };
                    ui.label(super::map::muted(&col.name));
                    ui.label(RichText::new(value).monospace());
                    ui.end_row();
                }
            });
    };
    let out = super::map::show(
        ui,
        &mut state.view,
        tiles,
        features,
        selected_row,
        fit,
        &tooltip,
    );
    if let Some(row) = out.clicked {
        rs.selected = Some((GridRow::Data(row), geo_column));
    }
}

/// Right and bottom border of a grid cell.
fn grid_lines(ui: &Ui) {
    let r = ui.max_rect();
    let stroke = egui::Stroke::new(1.0, theme::GRID_LINE);
    ui.painter()
        .line_segment([r.right_top(), r.right_bottom()], stroke);
    ui.painter()
        .line_segment([r.left_bottom(), r.right_bottom()], stroke);
}

fn header_cell_bg(ui: &Ui) {
    let r = ui.max_rect();
    ui.painter().rect_filled(r, 0.0, theme::BG_PANEL);
    let stroke = egui::Stroke::new(1.0, theme::GRID_LINE);
    ui.painter()
        .line_segment([r.right_top(), r.right_bottom()], stroke);
    ui.painter().line_segment(
        [r.left_bottom(), r.right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_INPUT),
    );
}

/// More characters than the widest grid column can show; the label still
/// truncates to the column width.
const CELL_PREVIEW_CHARS: usize = 1024;

fn cell(ui: &mut Ui, shown: Shown<'_>, deleted: bool, selected: bool) {
    let mut text = match shown {
        Shown::Value(v) => match v {
            Value::Null => RichText::new("<null>").italics().color(theme::TEXT_MUTED),
            Value::Bool(_) => RichText::new(v.to_cell()).color(theme::SQL_KEYWORD),
            Value::Bytes(_) => RichText::new(v.to_cell()).color(theme::TEXT_MUTED),
            _ if v.is_numeric() => {
                RichText::new(v.cell_preview(CELL_PREVIEW_CHARS)).color(theme::SQL_NUMBER)
            }
            _ => RichText::new(v.cell_preview(CELL_PREVIEW_CHARS)).color(theme::TEXT),
        },
        Shown::Edit(CellEdit::Null) => RichText::new("<null>").italics().color(theme::TEXT_MUTED),
        Shown::Edit(CellEdit::Default) => RichText::new("<default>")
            .italics()
            .color(theme::TEXT_MUTED),
        Shown::Edit(CellEdit::Text(s)) => {
            RichText::new(crate::model::cell_preview(s, CELL_PREVIEW_CHARS)).color(theme::TEXT)
        }
    };
    if selected {
        text = text.color(theme::TEXT);
    }
    if deleted {
        text = text.strikethrough();
    }
    let numeric = matches!(shown, Shown::Value(v) if v.is_numeric());
    let label = egui::Label::new(text.font(theme::grid_font(ui)))
        .truncate()
        .selectable(false);
    if numeric {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(label));
    } else {
        ui.add(label);
    }
}

/// Resizable panel under the grid with the selected cell's full value,
/// like a value editor. Drag its top edge to change the height.
pub fn value_panel(ui: &mut Ui, id: egui::Id, rs: &mut ResultSet) {
    if !rs.show_value_editor || rs.valid_selection().is_none() {
        value::release_hidden(rs);
        return;
    }
    egui::Panel::bottom(id)
        .resizable(true)
        .default_size(150.0)
        .size_range(70.0..=600.0)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .inner_margin(egui::Margin {
                    left: 8,
                    right: 8,
                    top: 0,
                    bottom: 6,
                }),
        )
        .show(ui, |ui| value::viewer(ui, rs));
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LogKind {
    Sql,
    Info,
    Error,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub time: chrono::DateTime<chrono::Local>,
    pub kind: LogKind,
    pub text: String,
}

/// Messages a tab keeps; older ones are removed, with a note at the top.
const LOG_ENTRIES: usize = 10_000;
/// Text a tab's log keeps in total.
const LOG_BYTES: usize = 8 << 20;
/// Text kept of one message; the rest is replaced by a note.
const LOG_ENTRY_BYTES: usize = 64 << 10;

struct LogEntry {
    line: LogLine,
    /// "[time] " for Info and Error lines, formatted once.
    stamp: String,
    /// Display lines before this entry since the log was created.
    top: u64,
    rows: u32,
}

impl LogEntry {
    /// Heap bytes the entry holds, which is what the log's byte limit counts.
    fn owned_bytes(&self) -> usize {
        self.line.text.capacity() + self.stamp.capacity()
    }
}

/// The text of a log message made of `parts`, copying at most
/// [`LOG_ENTRY_BYTES`] of them; a cut message ends with a note saying how
/// many bytes were left out.
pub fn log_text(parts: &[&str]) -> String {
    let total: usize = parts.iter().map(|part| part.len()).sum();
    if total <= LOG_ENTRY_BYTES {
        return parts.concat();
    }
    let mut text = String::with_capacity(LOG_ENTRY_BYTES + 64);
    for part in parts {
        let room = LOG_ENTRY_BYTES - text.len();
        if part.len() <= room {
            text.push_str(part);
            continue;
        }
        let mut end = room;
        while !part.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&part[..end]);
        break;
    }
    let rest = total - text.len();
    text.push_str(&format!("\n… {rest} more bytes of this message are not kept"));
    text
}

/// A tab's Output log, bounded by messages and bytes. Drawing lays out only
/// the messages in view.
#[derive(Default)]
pub struct OutputLog {
    entries: std::collections::VecDeque<LogEntry>,
    bytes: usize,
    removed: u64,
    rows: u64,
}

impl OutputLog {
    pub fn push(&mut self, mut line: LogLine) {
        if line.text.len() > LOG_ENTRY_BYTES {
            // A new buffer: truncating in place would keep the whole allocation.
            line.text = log_text(&[&line.text]);
        } else if line.text.capacity() > line.text.len() + 64 {
            line.text.shrink_to_fit();
        }
        let stamp = match line.kind {
            LogKind::Sql => String::new(),
            LogKind::Info | LogKind::Error => {
                line.time.format("[%Y-%m-%d %H:%M:%S] ").to_string()
            }
        };
        let rows = line.text.split('\n').count() as u32;
        let entry = LogEntry {
            line,
            stamp,
            top: self.rows,
            rows,
        };
        self.bytes += entry.owned_bytes();
        self.entries.push_back(entry);
        self.rows += u64::from(rows);
        while self.entries.len() > LOG_ENTRIES || self.bytes > LOG_BYTES {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= entry.owned_bytes();
            self.removed += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn last(&self) -> Option<&LogLine> {
        self.entries.back().map(|entry| &entry.line)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogLine> {
        self.entries.iter().map(|entry| &entry.line)
    }

    /// The newest `count` messages, oldest first.
    pub fn recent(&self, count: usize) -> Vec<LogLine> {
        self.entries
            .iter()
            .skip(self.entries.len().saturating_sub(count))
            .map(|entry| entry.line.clone())
            .collect()
    }

    pub fn from_lines(lines: Vec<LogLine>) -> Self {
        let mut log = Self::default();
        for line in lines {
            log.push(line);
        }
        log
    }

    /// Display lines from the first kept message to the end.
    fn kept_rows(&self) -> u64 {
        self.entries
            .front()
            .map_or(0, |first| self.rows - first.top)
    }
}

pub fn output(ui: &mut Ui, log: &OutputLog) {
    if log.is_empty() {
        theme::empty_text(ui, "Nothing to show");
        return;
    }
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    let notice = u64::from(log.removed > 0);
    let base = log.entries.front().map_or(0, |first| first.top);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_viewport(ui, |ui, viewport| {
            let total = (notice + log.kept_rows()) as f32 * row_height;
            ui.set_min_height(total);
            let origin = ui.max_rect().min;
            let place = |ui: &mut Ui, row: u64, label: egui::Label| {
                let rect = egui::Rect::from_min_size(
                    origin + egui::vec2(0.0, row as f32 * row_height),
                    egui::vec2(ui.max_rect().width(), row_height),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(Layout::top_down(Align::Min)),
                    |ui| ui.add(label.extend().selectable(true)),
                );
            };
            if notice == 1 && viewport.min.y < row_height {
                place(
                    ui,
                    0,
                    egui::Label::new(
                        RichText::new(format!(
                            "{} earlier messages were removed. Output keeps the last {} messages, up to {} MiB.",
                            log.removed,
                            LOG_ENTRIES,
                            LOG_BYTES >> 20
                        ))
                        .italics()
                        .color(theme::TEXT_MUTED),
                    ),
                );
            }
            let first_row = ((viewport.min.y / row_height).floor().max(0.0) as u64)
                .saturating_sub(notice);
            let last_row = (viewport.max.y / row_height).ceil().max(0.0) as u64;
            let start = log
                .entries
                .partition_point(|entry| entry.top - base + u64::from(entry.rows) <= first_row);
            for entry in log.entries.range(start..) {
                let row = notice + entry.top - base;
                if row > last_row {
                    break;
                }
                let line = &entry.line;
                let color = match line.kind {
                    LogKind::Sql => theme::TEXT,
                    LogKind::Info => theme::TEXT_MUTED,
                    LogKind::Error => theme::RED,
                };
                let mut job = egui::text::LayoutJob::default();
                let format = egui::TextFormat::simple(font.clone(), color);
                job.append(&entry.stamp, 0.0, format.clone());
                job.append(&line.text, 0.0, format);
                place(ui, row, egui::Label::new(job));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Column;

    fn rs() -> ResultSet {
        let data = QueryResult {
            columns: ["id", "name"]
                .iter()
                .map(|n| Column {
                    name: n.to_string(),
                    type_name: String::new(),
                })
                .collect(),
            rows: vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Null],
            ],
            ..Default::default()
        };
        ResultSet::new("t".into(), "select".into(), data, Page::first(500)).with_test_table()
    }

    fn sql(rs: &ResultSet) -> Vec<String> {
        rs.pending_mutations(DriverKind::Postgres)
            .unwrap()
            .into_iter()
            .map(|mutation| mutation.sql)
            .collect()
    }

    #[test]
    fn edits_become_dml() {
        let mut rs = rs();
        rs.set_cell(GridRow::Data(0), 1, CellEdit::Text("b".into()));
        rs.set_cell(GridRow::Data(1), 1, CellEdit::Text("c".into()));
        rs.delete_row(GridRow::Data(1));
        rs.add_row();
        rs.set_cell(GridRow::New(0), 1, CellEdit::Text("new".into()));
        // Values are bound; DELETE also checks every shown column for conflicts.
        assert_eq!(
            sql(&rs),
            [
                r#"DELETE FROM "main"."test" WHERE "id" IS NOT DISTINCT FROM $1 AND "name" IS NOT DISTINCT FROM $2"#,
                r#"UPDATE "main"."test" SET "name" = $1 WHERE "id" IS NOT DISTINCT FROM $2 AND "name" IS NOT DISTINCT FROM $3"#,
                r#"INSERT INTO "main"."test" ("name") VALUES ($1)"#,
            ]
        );
        let mutations = rs.pending_mutations(DriverKind::Postgres).unwrap();
        assert_eq!(mutations[0].params, [Value::Int(2), Value::Null]);
        assert_eq!(
            mutations[1].params,
            [Value::Text("b".into()), Value::Int(1), Value::Text("a".into())]
        );
        assert_eq!(mutations[2].params, [Value::Text("new".into())]);
        assert_eq!(mutations[2].source_row, 2);
    }

    #[test]
    fn cloned_row_copies_values_but_not_the_key() {
        let mut rs = rs();
        rs.set_cell(GridRow::Data(0), 1, CellEdit::Text("changed".into()));
        rs.clone_row(GridRow::Data(0));
        rs.clone_row(GridRow::Data(1));
        assert_eq!(rs.selected, Some((GridRow::New(1), 0)));
        let mutations = rs.pending_mutations(DriverKind::Postgres).unwrap();
        assert_eq!(mutations[1].sql, r#"INSERT INTO "main"."test" ("name") VALUES ($1)"#);
        assert_eq!(mutations[1].params, [Value::Text("changed".into())]);
        assert_eq!(mutations[2].params, [Value::Null]);
    }

    #[test]
    fn typing_the_original_value_drops_the_change() {
        let mut rs = rs();
        rs.set_cell(GridRow::Data(0), 1, CellEdit::Text("x".into()));
        rs.set_cell(GridRow::Data(0), 1, CellEdit::Text("a".into()));
        rs.set_cell(GridRow::Data(1), 1, CellEdit::Null);
        assert!(rs.edits.is_empty());
    }

    #[test]
    fn results_without_edit_metadata_cannot_add_delete_or_submit() {
        let data = rs().data.as_ref().clone();
        let plain = ResultSet::new("t".into(), "select".into(), data, Page::first(500));
        assert!(!plain.can_add_row());
        assert!(!plain.can_delete_row(GridRow::Data(0)));
        assert!(!plain.can_edit_cell(GridRow::Data(0), 1));
        assert!(plain.pending_mutations(DriverKind::Postgres).is_err());
    }

    #[test]
    fn record_navigation_uses_display_order_and_keeps_the_column() {
        let mut rs = rs();
        rs.set_sort(Some((0, false)));
        rs.selected = Some((GridRow::Data(1), 1));
        assert_eq!(rs.moved_selection(-1, 0), Some((GridRow::Data(1), 1)));
        rs.selected = rs.moved_selection(1, 0);
        assert_eq!(rs.selected, Some((GridRow::Data(0), 1)));
        rs.clone_row(GridRow::Data(0));
        rs.selected = Some((GridRow::Data(0), 1));
        assert_eq!(rs.moved_selection(1, 0), Some((GridRow::New(0), 1)));
        rs.selected = Some((GridRow::New(0), 1));
        assert_eq!(rs.moved_selection(1, 1), rs.selected);
    }

    #[test]
    fn empty_and_stale_selections_are_safe() {
        let mut rs = rs();
        rs.selected = Some((GridRow::New(42), 99));
        assert!(rs.selected_value().is_none());
        assert_eq!(rs.moved_selection(1, 0), Some((GridRow::Data(0), 0)));
        let empty = ResultSet::new(
            String::new(),
            String::new(),
            QueryResult::default(),
            Page::first(500),
        );
        assert!(empty.moved_selection(1, 1).is_none());
    }

    struct GridHarness {
        rs: ResultSet,
        tiles: super::super::map::TileCache,
        sql: String,
        submitted: Vec<String>,
        toolbar: bool,
    }

    fn grid_harness(record_view: bool) -> egui_kittest::Harness<'static, GridHarness> {
        let mut result = rs();
        result.record_view = record_view;
        result.show_value_editor = false;
        egui_kittest::Harness::builder()
            .with_size([1000.0, 500.0])
            .build_ui_state(
                |ui, state: &mut GridHarness| {
                    ui.add(egui::TextEdit::singleline(&mut state.sql).hint_text("SQL editor"));
                    if state.toolbar {
                        grid_toolbar(ui, &mut state.rs, DriverKind::Postgres, 500, true);
                    }
                    value_panel(ui, egui::Id::new("test_value_panel"), &mut state.rs);
                    grid(ui, &mut state.rs, &mut state.tiles);
                    if ui.input_mut(|i| {
                        i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter))
                    }) {
                        state.submitted = state
                            .rs
                            .pending_mutations(DriverKind::Postgres)
                            .unwrap()
                            .into_iter()
                            .map(|mutation| mutation.sql)
                            .collect();
                    }
                },
                GridHarness {
                    rs: result,
                    tiles: super::super::map::TileCache::new(String::new(), std::env::temp_dir()),
                    sql: String::new(),
                    submitted: Vec::new(),
                    toolbar: false,
                },
            )
    }

    #[test]
    fn geo_viewer_is_offered_after_large_geometries_are_confirmed_in_the_background() {
        let geojson = format!(
            "{{\"type\": \"LineString\", \"coordinates\": [{}[0, 0]]}}",
            "[21.0, 52.0], ".repeat(40_000)
        );
        let data = QueryResult {
            columns: vec![Column {
                name: "shape".into(),
                type_name: "text".into(),
            }],
            rows: vec![vec![Value::Text(geojson)]; 10],
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut rs = ResultSet::new("geo".into(), "select".into(), data.clone(), Page::first(500));
        let start = std::time::Instant::now();
        assert!(rs.geometry_columns(&ctx).is_empty(), "not decided on the UI thread");
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
        for _ in 0..500 {
            if !rs.geometry_columns(&ctx).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(rs.geometry_columns(&ctx), [0]);
        assert!(rs.geo_detection.is_none());

        // Closing a result stops its detection and releases the rows.
        let mut closed = ResultSet::new("geo".into(), "select".into(), data, Page::first(500));
        let rows = std::sync::Arc::downgrade(&closed.data);
        closed.geometry_columns(&ctx);
        drop(closed);
        for _ in 0..500 {
            if rows.strong_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(rows.strong_count(), 0);
    }

    fn log_line(kind: LogKind, text: String) -> LogLine {
        LogLine {
            time: chrono::Local::now(),
            kind,
            text,
        }
    }

    #[test]
    fn output_log_keeps_bounded_messages_and_bytes() {
        let mut log = OutputLog::default();
        for i in 0..LOG_ENTRIES + 5 {
            log.push(log_line(LogKind::Info, format!("message {i}")));
        }
        assert_eq!(log.entries.len(), LOG_ENTRIES);
        assert_eq!(log.removed, 5);
        assert_eq!(log.iter().next().unwrap().text, "message 5");
        // A message far over the limit keeps a small buffer, not its allocation.
        log.push(log_line(LogKind::Sql, format!("select '{}'", "ż".repeat(4 << 20))));
        let last = log.last().unwrap();
        assert!(last.text.capacity() < LOG_ENTRY_BYTES + 128, "{}", last.text.capacity());
        assert!(last.text.ends_with("bytes of this message are not kept"));
        assert!(last.text.is_char_boundary(last.text.find('\n').unwrap()));
        // A short message from an oversized buffer is shrunk too.
        let mut roomy = String::with_capacity(1 << 20);
        roomy.push_str("short");
        log.push(log_line(LogKind::Info, roomy));
        assert!(log.last().unwrap().text.capacity() < 128);
        for _ in 0..500 {
            log.push(log_line(LogKind::Sql, "x".repeat(LOG_ENTRY_BYTES * 4)));
        }
        let owned: usize = log.entries.iter().map(LogEntry::owned_bytes).sum();
        assert_eq!(log.bytes, owned);
        assert!(log.bytes <= LOG_BYTES);
        // Statement messages are built without copying the whole statement.
        let statement = "ą".repeat(1 << 20);
        let text = log_text(&["db", "> ", &statement]);
        assert!(text.capacity() <= LOG_ENTRY_BYTES + 64);
        assert!(text.starts_with("db> ąą"));
        assert!(text.ends_with(&format!("{} more bytes of this message are not kept", statement.len() + 4 - text.find('\n').unwrap())));
        assert_eq!(log_text(&["a", "> ", "b"]), "a> b");
        assert_eq!(log.recent(2).len(), 2);
    }

    #[test]
    fn output_draws_only_visible_messages_and_follows_the_end() {
        use egui_kittest::kittest::Queryable;
        let mut log = OutputLog::default();
        for i in 0..LOG_ENTRIES + 20 {
            let text = if i % 3 == 0 {
                format!("select {i}\nfrom t\nwhere x = {i}")
            } else {
                format!("message {i}")
            };
            log.push(log_line(if i % 3 == 0 { LogKind::Sql } else { LogKind::Info }, text));
        }
        let mut h = egui_kittest::Harness::builder()
            .with_size([600.0, 300.0])
            .build_ui_state(|ui, log: &mut OutputLog| output(ui, log), log);
        h.run_steps(4);
        let last = format!("message {}", LOG_ENTRIES + 19);
        assert!(h.query_by_label_contains(&last).is_some(), "sticks to the end");
        let drawn = h.query_all_by_label_contains("message").count()
            + h.query_all_by_label_contains("select").count();
        assert!(drawn < 60, "{drawn} messages laid out");
        assert!(h.query_by_label_contains("earlier messages were removed").is_none());
        // The note about removed messages is the first row.
        let mut short = OutputLog::default();
        for i in 0..3 {
            short.push(log_line(LogKind::Error, format!("failure {i}")));
        }
        short.removed = 20;
        *h.state_mut() = short;
        h.run_steps(3);
        assert!(h.query_by_label_contains("failure 0").is_some());
        assert!(h.query_by_label_contains("20 earlier messages were removed").is_some());
    }

    #[test]
    #[ignore = "benchmark: cargo test --release --lib output_frame_cost -- --ignored --nocapture"]
    fn output_frame_cost_benchmark() {
        // The drawing loop before the log was bounded and virtualized.
        fn old_output(ui: &mut Ui, log: &[LogLine]) {
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in log {
                        let ts = line.time.format("[%Y-%m-%d %H:%M:%S]").to_string();
                        let rt = match line.kind {
                            LogKind::Sql => RichText::new(&line.text).color(theme::TEXT),
                            _ => RichText::new(format!("{ts} {}", line.text)).color(theme::TEXT_MUTED),
                        };
                        ui.add(egui::Label::new(rt.monospace()).selectable(true));
                    }
                });
        }
        for count in [1_000, 10_000] {
            let lines: Vec<LogLine> = (0..count)
                .map(|i| log_line(if i % 3 == 0 { LogKind::Sql } else { LogKind::Info },
                    if i % 3 == 0 { format!("select {i}\nfrom t\nwhere x = {i}") } else { format!("message {i}") }))
                .collect();
            let mut old = egui_kittest::Harness::builder()
                .with_size([800.0, 400.0])
                .build_ui_state(|ui, lines: &mut Vec<LogLine>| old_output(ui, lines), lines.clone());
            let mut new = egui_kittest::Harness::builder()
                .with_size([800.0, 400.0])
                .build_ui_state(|ui, log: &mut OutputLog| output(ui, log), OutputLog::from_lines(lines));
            let time = |step: &mut dyn FnMut()| {
                for _ in 0..3 { step(); }
                let start = std::time::Instant::now();
                for _ in 0..10 { step(); }
                start.elapsed() / 10
            };
            let before = time(&mut || old.step());
            let after = time(&mut || new.step());
            println!("{count} messages: before {before:?}/frame, after {after:?}/frame");
        }
    }

    fn wide_harness(records: usize, columns: usize, transposed: bool) -> egui_kittest::Harness<'static, GridHarness> {
        let data = QueryResult {
            columns: (0..columns)
                .map(|c| Column {
                    name: format!("c{c}"),
                    type_name: String::new(),
                })
                .collect(),
            rows: (0..records)
                .map(|r| (0..columns).map(|c| Value::Text(format!("v{r}_{c}"))).collect())
                .collect(),
            ..Default::default()
        };
        let mut result = ResultSet::new("t".into(), "select".into(), data, Page::first(records));
        result.transposed = transposed;
        result.show_value_editor = false;
        egui_kittest::Harness::builder()
            .with_size([1000.0, 500.0])
            .build_ui_state(
                |ui, state: &mut GridHarness| grid(ui, &mut state.rs, &mut state.tiles),
                GridHarness {
                    rs: result,
                    tiles: super::super::map::TileCache::new(String::new(), std::env::temp_dir()),
                    sql: String::new(),
                    submitted: Vec::new(),
                    toolbar: false,
                },
            )
    }

    /// Cells built and time per frame once layout has settled.
    fn frame_cost(h: &mut egui_kittest::Harness<'static, GridHarness>) -> (usize, std::time::Duration) {
        h.run_steps(5);
        CELLS_BUILT.with(|cells| cells.set(0));
        let start = std::time::Instant::now();
        const FRAMES: u32 = 10;
        for _ in 0..FRAMES {
            h.step();
        }
        let elapsed = start.elapsed() / FRAMES;
        (CELLS_BUILT.with(|cells| cells.get()) / FRAMES as usize, elapsed)
    }

    #[test]
    fn wide_grids_build_only_visible_cells_and_still_reveal_far_ones() {
        use egui_kittest::kittest::Queryable;
        let mut few = wide_harness(500, 20, true);
        let mut many = wide_harness(5000, 20, true);
        let (few_cells, _) = frame_cost(&mut few);
        let (many_cells, _) = frame_cost(&mut many);
        assert_eq!(few_cells, many_cells, "same viewport, same cells");
        assert!(many_cells < 1000, "{many_cells} cells");
        // Keyboard navigation to the last record scrolls it into view.
        many.state_mut().rs.select_cell((GridRow::Data(4999), 3), false);
        many.state_mut().rs.pending_reveal = true;
        many.run_steps(8);
        assert!(!many.state().rs.pending_reveal);
        assert!(many.query_by_label("v4999_3").is_some());
        assert!(many.query_by_label("v0_3").is_none());

        let mut wide = wide_harness(50, 2000, false);
        let (cells, _) = frame_cost(&mut wide);
        assert!(cells < 1000, "{cells} cells");
        wide.state_mut().rs.select_cell((GridRow::Data(1), 1999), false);
        wide.state_mut().rs.pending_reveal = true;
        wide.run_steps(8);
        assert!(!wide.state().rs.pending_reveal);
        assert!(wide.query_by_label("v1_1999").is_some());
    }

    #[test]
    fn dragging_a_header_edge_resizes_only_that_column() {
        use egui_kittest::kittest::Queryable;
        let mut h = wide_harness(5, 3, false);
        h.run_steps(3);
        let cell = |h: &egui_kittest::Harness<'static, GridHarness>, label: &str| {
            h.get_by_label(label).rect().left()
        };
        let before = (cell(&h, "v0_1"), cell(&h, "v0_2"));
        let edge = h
            .get_all_by_label("Resize Column")
            .map(|node| node.rect())
            .find(|rect| rect.left() > before.0 && rect.right() < before.1)
            .expect("handle of c1");
        let (edge, y) = (edge.center().x, edge.center().y);
        h.drag_at(egui::pos2(edge, y));
        h.hover_at(egui::pos2(edge + 30.0, y));
        h.run_steps(1);
        h.hover_at(egui::pos2(edge + 60.0, y));
        h.run_steps(1);
        h.drop_at(egui::pos2(edge + 60.0, y));
        h.run_steps(3);
        assert_eq!(cell(&h, "v0_1"), before.0);
        assert!(cell(&h, "v0_2") > before.1 + 40.0, "column c1 grew");
    }

    #[test]
    #[ignore = "benchmark: cargo test --release --lib grid_frame_cost -- --ignored --nocapture"]
    fn grid_frame_cost_benchmark() {
        for (records, columns, transposed) in [(500, 20, true), (5000, 20, true), (50, 2000, false), (50, 20, false)] {
            let mut h = wide_harness(records, columns, transposed);
            let (cells, time) = frame_cost(&mut h);
            println!("records={records} columns={columns} transposed={transposed}: {cells} cells/frame, {time:?}/frame");
        }
    }

    #[test]
    fn grid_keyboard_navigation_respects_focus_and_transpose() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.get_by_label("a").click();
        h.run_steps(2);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(1), 1)));
        h.state_mut().rs.transposed = true;
        h.run_steps(2);
        h.key_press(Key::ArrowLeft);
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(0), 1)));
        h.key_press(Key::ArrowUp);
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(0), 0)));
        h.get_by_role(egui::accesskit::Role::TextInput).focus();
        h.run_steps(2);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(0), 0)));
        h.key_press_modifiers(Modifiers::COMMAND, Key::N);
        h.run_steps(2);
        assert!(h.state().rs.edits.inserted.is_empty());
    }

    #[test]
    fn record_edit_uses_one_editor_and_creates_pending_dml() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(true);
        h.state_mut().rs.selected = Some((GridRow::Data(0), 1));
        h.run_steps(2);
        h.get_all_by_label("a")
            .max_by(|a, b| a.rect().center().x.total_cmp(&b.rect().center().x))
            .unwrap()
            .click();
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert!(h.state().rs.edits.editing.is_some());
        assert!(h.state().rs.edits.editing_surface == CellSurface::Record);
        assert_eq!(
            h.get_all_by_role(egui::accesskit::Role::TextInput).count(),
            2
        );
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.event(egui::Event::Text("edited in record".into()));
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert!(h.state().rs.edits.editing.is_none());
        let mutations = h.state().rs.pending_mutations(DriverKind::Postgres).unwrap();
        assert_eq!(mutations.len(), 1);
        assert_eq!(
            mutations[0].params,
            [
                Value::Text("edited in record".into()),
                Value::Int(1),
                Value::Text("a".into())
            ]
        );
        h.key_press(Key::ArrowUp);
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(0), 0)));
    }

    #[test]
    fn value_editor_shortcut_does_not_start_an_inline_edit() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.get_by_label("a").click();
        h.run_steps(2);
        h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
        h.run_steps(2);
        assert!(h.state().rs.show_value_editor);
        assert!(h.state().rs.edits.editing.is_none());
    }

    #[test]
    fn escape_cancels_edit_and_returns_navigation_focus() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.get_by_label("a").click();
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        h.event(egui::Event::Text("discard".into()));
        h.run_steps(2);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().rs.edits.is_empty());
        assert!(h.state().rs.edits.editing.is_none());
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(1), 1)));
    }

    #[test]
    fn copy_uses_the_pending_selected_value() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.get_by_label("a").click();
        h.run_steps(2);
        h.state_mut()
            .rs
            .set_cell(GridRow::Data(0), 1, CellEdit::Text("pending".into()));
        h.event(egui::Event::Copy);
        h.run_steps(1);
        assert!(h.output().platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "pending")
        }));
    }

    fn large_grid_harness(transposed: bool) -> egui_kittest::Harness<'static, GridHarness> {
        let mut h = grid_harness(false);
        let mut data = (*rs().data).clone();
        data.rows = (0..60)
            .map(|index| vec![Value::Int(index), Value::Text(format!("row{index}"))])
            .collect();
        let mut result = ResultSet::new(
            "large".into(),
            "select large".into(),
            data,
            Page::first(500),
        )
        .with_test_table();
        result.transposed = transposed;
        result.show_value_editor = false;
        h.state_mut().rs = result;
        h.run_steps(2);
        h
    }

    #[test]
    fn navigation_reveals_offscreen_rows_and_transposed_records() {
        use egui_kittest::kittest::Queryable;
        for transposed in [false, true] {
            let mut h = large_grid_harness(transposed);
            h.get_by_label("row0").click();
            h.run_steps(2);
            let next = if transposed {
                Key::ArrowRight
            } else {
                Key::ArrowDown
            };
            for _ in 0..55 {
                h.key_press(next);
                h.run_steps(2);
            }
            h.run_steps(20);
            assert_eq!(h.state().rs.selected, Some((GridRow::Data(55), 1)));
            h.key_press(Key::Enter);
            h.run_steps(20);
            let editor = h
                .get_all_by_role(egui::accesskit::Role::TextInput)
                .find(|node| node.value().as_deref() == Some("row55"))
                .expect("the selected cell's editor must be visible");
            let rect = editor.rect();
            assert!(rect.left() >= 0.0 && rect.right() <= 1000.0, "{rect:?}");
            assert!(rect.top() >= 0.0 && rect.bottom() <= 500.0, "{rect:?}");
            assert!(!h.state().rs.pending_reveal);
        }
    }

    #[test]
    fn adding_a_row_reveals_the_new_editor() {
        use egui_kittest::kittest::Queryable;
        for transposed in [false, true] {
            let mut h = large_grid_harness(transposed);
            h.get_by_label("row0").click();
            h.run_steps(2);
            h.key_press_modifiers(Modifiers::COMMAND, Key::N);
            h.run_steps(30);
            assert_eq!(h.state().rs.selected, Some((GridRow::New(0), 0)));
            assert_eq!(
                h.get_all_by_role(egui::accesskit::Role::TextInput).count(),
                2
            );
            let editor = h
                .get_all_by_role(egui::accesskit::Role::TextInput)
                .last()
                .unwrap();
            let rect = editor.rect();
            assert!(rect.left() >= 0.0 && rect.right() <= 1000.0, "{rect:?}");
            assert!(rect.top() >= 0.0 && rect.bottom() <= 500.0, "{rect:?}");
            assert!(!h.state().rs.pending_reveal);
        }
    }

    #[test]
    fn revealing_a_cell_wider_than_the_viewport_finishes() {
        let mut h = grid_harness(false);
        let mut data = (*rs().data).clone();
        data.rows[0][1] = Value::Text("wide value ".repeat(10));
        h.state_mut().rs =
            ResultSet::new("wide".into(), "select wide".into(), data, Page::first(500));
        h.set_size(egui::vec2(240.0, 500.0));
        h.state_mut().rs.selected = Some((GridRow::Data(0), 1));
        h.state_mut().rs.pending_reveal = true;
        h.run_steps(30);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(0), 1)));
        assert!(!h.state().rs.pending_reveal);
    }

    #[test]
    fn record_shortcut_only_opens_from_grid_focus() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.get_by_role(egui::accesskit::Role::TextInput).focus();
        h.run_steps(2);
        h.key_press_modifiers(OPEN_RECORD_VIEW.modifiers, Key::Enter);
        h.run_steps(2);
        assert!(!h.state().rs.record_view);
        h.get_by_label("a").click();
        h.run_steps(2);
        h.key_press_modifiers(OPEN_RECORD_VIEW.modifiers, Key::Enter);
        h.run_steps(2);
        assert!(h.state().rs.record_view);
        assert!(h.state().rs.edits.editing.is_none());
    }

    #[test]
    fn submit_commits_inline_buffer_before_parent_handles_shortcut() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.get_by_label("a").click();
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.event(egui::Event::Text("ready to submit".into()));
        h.run_steps(2);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
        h.run_steps(2);
        assert!(h.state().rs.edits.editing.is_none());
        assert_eq!(
            h.state().submitted,
            [r#"UPDATE "main"."test" SET "name" = $1 WHERE "id" IS NOT DISTINCT FROM $2 AND "name" IS NOT DISTINCT FROM $3"#]
        );
    }

    #[test]
    fn first_context_menu_survives_the_value_panel_appearing() {
        use egui_kittest::kittest::Queryable;
        let mut h = grid_harness(false);
        h.state_mut().rs.show_value_editor = true;
        h.run_steps(2);
        assert_eq!(h.state().rs.selected, None);
        h.get_by_label("a").click_secondary();
        h.run_steps(3);
        assert_eq!(h.state().rs.selected, Some((GridRow::Data(0), 1)));
        h.get_by_label_contains("Clone Row");
        h.get_by_label_contains("Add Row");
    }

    #[test]
    fn context_clone_and_dml_preview_keep_grid_row_shortcuts() {
        use egui_kittest::kittest::Queryable;
        for show_preview in [false, true] {
            let mut h = grid_harness(false);
            h.state_mut().toolbar = true;
            h.run_steps(2);
            h.get_by_label("a").click_secondary();
            h.run_steps(2);
            h.get_by_label_contains("Clone Row").click();
            h.run_steps(3);
            assert_eq!(h.state().rs.edits.inserted.len(), 1);
            // Opening the menu selects its target cell; cloning keeps that column.
            assert_eq!(h.state().rs.selected, Some((GridRow::New(0), 1)));
            if show_preview {
                h.get_by_label("DML").click();
                h.run_steps(3);
                assert!(h.state().rs.edits.show_preview);
            }
            h.key_press_modifiers(Modifiers::COMMAND, Key::Backspace);
            h.run_steps(3);
            assert!(
                h.state().rs.edits.inserted.is_empty(),
                "preview={show_preview}, selected={:?}, pending_focus={}",
                h.state().rs.selected,
                h.state().rs.pending_grid_focus
            );
            h.get_by_role(egui::accesskit::Role::TextInput).focus();
            h.run_steps(2);
            h.key_press_modifiers(Modifiers::COMMAND, Key::N);
            h.run_steps(2);
            assert!(h.state().rs.edits.inserted.is_empty());
        }
    }
}
