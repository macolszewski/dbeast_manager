//! Full cell values share the grid's permissions and pending edit buffer.

use super::*;
use super::super::json_view;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

pub(super) fn viewer(ui: &mut Ui, rs: &mut ResultSet) {
    let Some((row, column)) = rs.valid_selection() else {
        return;
    };
    let editing = rs.edits.editing_surface == CellSurface::Value
        && rs
            .edits
            .editing
            .as_ref()
            .is_some_and(|(r, c, _)| (*r, *c) == (row, column));
    let mut start = false;
    let mut apply = false;
    let mut cancel = false;
    theme::panel_header(ui, |ui| {
        let mut title = egui::text::LayoutJob::default();
        let font = egui::FontId::proportional(13.0);
        title.append(
            &rs.data.columns[column].name,
            0.0,
            egui::TextFormat::simple(font.clone(), theme::TEXT),
        );
        title.append(
            &format!(" · {}", rs.data.columns[column].type_name),
            0.0,
            egui::TextFormat::simple(font, theme::TEXT_MUTED),
        );
        ui.label(title);
        ui.add_space(8.0);
        if editing {
            apply = ui.button("Apply Value").clicked();
            cancel = ui.button("Cancel Value Edit").clicked();
        } else {
            start = ui
                .add_enabled(
                    rs.can_edit_text(row, column),
                    egui::Button::new("Edit Value"),
                )
                .on_disabled_hover_text(rs.text_read_only_reason(row, column).unwrap_or(""))
                .clicked();
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(rs.copy_cell(row, column));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let response = theme::tool_button(ui, "×", "Close Value Editor");
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close Value Editor")
            });
            if response.clicked() {
                rs.commit_find_buffer();
                rs.show_value_editor = false;
                rs.pending_grid_focus = true;
            }
        });
    });
    if start {
        rs.commit_find_buffer();
        let text = match rs.cell(row, column) {
            Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null | CellEdit::Default) => {
                String::new()
            }
            Shown::Value(value) => value.to_string(),
            Shown::Edit(CellEdit::Text(text)) => text.clone(),
        };
        rs.edits.editing = Some((row, column, text));
        rs.edits.editing_surface = CellSurface::Value;
        rs.edits.editing_needs_focus = true;
        rs.pending_grid_focus = false;
    }
    if editing || start {
        let id = egui::Id::new(("value_editor_text", rs.action_id, row, column));
        let owned_focus = ui
            .ctx()
            .memory(|memory| memory.has_focus(id) || memory.had_focus_last_frame(id));
        let submit = ui.is_enabled()
            && owned_focus
            && ui.input(|input| {
                input.key_pressed(Key::Enter)
                    && input.modifiers.matches_logically(Modifiers::COMMAND)
            });
        cancel |= ui.is_enabled()
            && owned_focus
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        if let Some((_, _, text)) = &mut rs.edits.editing {
            let minimum_size = ui.available_size();
            let response = egui::ScrollArea::vertical()
                .id_salt("value_edit_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .id(id)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .min_size(minimum_size)
                            .hint_text("Value editor text"),
                    )
                })
                .inner;
            // Preserve TextEdit's multiline role and selection nodes. Replacing
            // WidgetInfo would reset its role and copy the entire value again.
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label("Value editor text");
            });
            if rs.edits.editing_needs_focus {
                response.request_focus();
                rs.edits.editing_needs_focus = false;
            }
            rs.action_focus.editor = Some(id);
            apply |= submit || response.lost_focus();
        }
        if cancel {
            rs.edits.editing = None;
            rs.edits.editing_needs_focus = false;
            rs.pending_grid_focus = true;
        } else if apply {
            rs.commit_find_buffer();
            rs.pending_grid_focus = true;
        }
        return;
    }

    let key = ViewKey {
        result: rs.action_id,
        data: std::sync::Arc::as_ptr(&rs.data) as usize,
        row,
        column,
        revision: match rs.cell(row, column) {
            Shown::Value(_) => 0,
            Shown::Edit(_) => rs.edits.revision,
        },
        text_mode: false,
    };
    let text = match rs.cell(row, column) {
        Shown::Value(Value::Text(text) | Value::Decimal(text))
        | Shown::Edit(CellEdit::Text(text)) => text.as_str(),
        Shown::Value(Value::Null) | Shown::Edit(_) => {
            return plain_label(ui, "<null>");
        }
        Shown::Value(Value::Bytes(bytes)) => {
            return plain_label(ui, &format!("<{} bytes>", bytes.len()));
        }
        Shown::Value(value) => return plain_label(ui, &value.to_string()),
    };
    let json_candidate = matches!(rs.cell(row, column), Shown::Value(Value::Text(_)) | Shown::Edit(_))
        && json_view::looks_like_json(text);
    let mode_id = egui::Id::new("value_viewer_json_text");
    let mut as_text = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(mode_id))
        .unwrap_or(false);
    let key = ViewKey {
        text_mode: json_candidate && as_text,
        ..key
    };
    let source = || match rs.cell(row, column) {
        Shown::Value(_) => Source::Data(rs.data.clone(), row_index(row), column),
        Shown::Edit(_) => Source::Owned(text.into()),
    };
    let cache = ViewCache::of(ui.ctx());
    let mut cache = cache.lock().unwrap_or_else(|poison| poison.into_inner());
    let tree_state = egui::Id::new(("value_viewer_json_tree", key.result, row, column));
    let Some(view) = cache.get(ui.ctx(), key, text, json_candidate, tree_state, source) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new(format!("Preparing {}…", human_size(text.len())))
                    .color(theme::TEXT_MUTED),
            );
        });
        return;
    };
    if view.is_json() {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut as_text, false, "Tree")
                .on_hover_text("Collapsible JSON tree");
            ui.selectable_value(&mut as_text, true, "Text")
                .on_hover_text("Formatted JSON");
        });
        ui.ctx().data_mut(|data| data.insert_temp(mode_id, as_text));
    }
    match view {
        View::Plain(lines) if text.len() > INLINE_BYTES => lines.show(ui, text, false),
        View::Plain(_) => {
            egui::ScrollArea::both()
                .id_salt("value_viewer")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true))
                });
        }
        View::Tree(tree) => {
            if tree.show(ui) {
                let toggled = tree.toggled().clone();
                ui.ctx().data_mut(|data| data.insert_temp(tree_state, toggled));
            }
        }
        View::Pretty { text, lines, job } => match job {
            Some(job) => {
                egui::ScrollArea::both()
                    .id_salt("value_viewer")
                    .auto_shrink([false, false])
                    .show(ui, |ui| ui.add(egui::Label::new(job.clone()).selectable(true)));
            }
            None => lines.show(ui, text, true),
        },
    }
}

fn plain_label(ui: &mut Ui, text: &str) {
    egui::ScrollArea::both()
        .id_salt("value_viewer")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true))
        });
}

fn row_index(row: GridRow) -> usize {
    match row {
        GridRow::Data(index) | GridRow::New(index) => index,
    }
}

fn human_size(bytes: usize) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    }
}

/// Values up to this size are prepared on the UI thread and drawn as one label;
/// larger ones are prepared on a worker and drawn row by row.
const INLINE_BYTES: usize = 64 * 1024;
/// Prepared views kept at once, across all result sets: the active cell and
/// one more, so two panels drawn in the same frame do not rebuild each other.
const CACHED_VIEWS: usize = 2;

/// Identifies the shown text without hashing it: a fetched value never changes,
/// and every change to staged values gets a new edit revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewKey {
    result: u64,
    data: usize,
    row: GridRow,
    column: usize,
    revision: u64,
    text_mode: bool,
}

/// Text a worker reads; a fetched value is shared with the result, not copied.
enum Source {
    Data(std::sync::Arc<crate::model::QueryResult>, usize, usize),
    Owned(std::sync::Arc<str>),
}

impl Source {
    fn text(&self) -> &str {
        match self {
            Source::Data(data, row, column) => match &data.rows[*row][*column] {
                Value::Text(text) | Value::Decimal(text) => text,
                _ => "",
            },
            Source::Owned(text) => text,
        }
    }
}

enum View {
    Plain(json_view::TextLines),
    Tree(json_view::JsonTree),
    Pretty {
        text: String,
        lines: json_view::TextLines,
        /// The whole formatted text as one label, for small values.
        job: Option<std::sync::Arc<egui::text::LayoutJob>>,
    },
}

impl View {
    fn is_json(&self) -> bool {
        !matches!(self, View::Plain(_))
    }

    fn prepare(
        text: &str,
        json_candidate: bool,
        text_mode: bool,
        toggled: std::collections::HashSet<u32>,
        cancel: &AtomicBool,
    ) -> Option<View> {
        let json = json_candidate
            .then(|| serde_json::from_str::<serde_json::Value>(text).ok())
            .flatten();
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        Some(match json {
            None => View::Plain(json_view::TextLines::index(text, false, cancel)?),
            Some(json) if text_mode => {
                let pretty = serde_json::to_string_pretty(&json).unwrap_or_default();
                drop(json);
                let job = (pretty.len() <= INLINE_BYTES)
                    .then(|| std::sync::Arc::new(json_view::highlighted_text(&pretty)));
                let lines = json_view::TextLines::index(&pretty, true, cancel)?;
                View::Pretty {
                    text: pretty,
                    lines,
                    job,
                }
            }
            Some(json) => View::Tree(json_view::JsonTree::build(&json, toggled, cancel)?),
        })
    }
}

struct Job {
    key: ViewKey,
    cancel: std::sync::Arc<AtomicBool>,
    /// Filled by the worker when it stops, with `None` if it was cancelled.
    done: std::sync::Arc<Mutex<Option<Option<View>>>>,
}

/// Prepared views of the most recently shown cells, shared by all result sets.
#[derive(Default)]
pub(in crate::ui) struct ViewCache {
    views: Vec<(ViewKey, View)>,
    job: Option<Job>,
    #[cfg(test)]
    prepared: usize,
}

impl ViewCache {
    fn of(ctx: &egui::Context) -> std::sync::Arc<Mutex<ViewCache>> {
        ctx.data_mut(|data| {
            data.get_temp_mut_or_default::<std::sync::Arc<Mutex<ViewCache>>>(egui::Id::new(
                "value_viewer_cache",
            ))
            .clone()
        })
    }

    /// The view for `key`, prepared now for small text or on a worker for large
    /// text; `None` while the worker runs. Only the newest request is kept.
    fn get(
        &mut self,
        ctx: &egui::Context,
        key: ViewKey,
        text: &str,
        json_candidate: bool,
        tree_state: egui::Id,
        source: impl FnOnce() -> Source,
    ) -> Option<&mut View> {
        if let Some(job) = &self.job {
            let finished = job.done.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some(view) = finished {
                let job = self.job.take().expect("job is present");
                if let Some(view) = view.filter(|_| job.key == key) {
                    self.insert(job.key, view);
                }
            } else if job.key != key {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        if let Some(at) = self.views.iter().position(|(k, _)| *k == key) {
            let entry = self.views.remove(at);
            self.views.push(entry);
            return self.views.last_mut().map(|(_, view)| view);
        }
        let toggled = ctx
            .data(|data| data.get_temp::<std::collections::HashSet<u32>>(tree_state))
            .unwrap_or_default();
        if text.len() <= INLINE_BYTES {
            let view = View::prepare(text, json_candidate, key.text_mode, toggled, &AtomicBool::new(false))?;
            self.insert(key, view);
            return self.views.last_mut().map(|(_, view)| view);
        }
        if self.job.is_none() {
            let cancel = std::sync::Arc::new(AtomicBool::new(false));
            let done = std::sync::Arc::new(Mutex::new(None));
            let source = source();
            let (worker_cancel, worker_done, ctx) = (cancel.clone(), done.clone(), ctx.clone());
            let text_mode = key.text_mode;
            std::thread::Builder::new()
                .name("value-viewer".into())
                .spawn(move || {
                    let view = View::prepare(source.text(), json_candidate, text_mode, toggled, &worker_cancel)
                        .filter(|_| !worker_cancel.load(Ordering::Relaxed));
                    drop(source);
                    *worker_done.lock().unwrap_or_else(|p| p.into_inner()) = Some(view);
                    ctx.request_repaint();
                })
                .expect("spawn value viewer worker");
            self.job = Some(Job { key, cancel, done });
        }
        None
    }

    fn insert(&mut self, key: ViewKey, view: View) {
        #[cfg(test)]
        {
            self.prepared += 1;
        }
        self.views.retain(|(k, _)| *k != key);
        if self.views.len() >= CACHED_VIEWS {
            self.views.remove(0);
        }
        self.views.push((key, view));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::Role;
    use egui_kittest::kittest::{NodeT, Queryable};

    fn harness() -> egui_kittest::Harness<'static, ResultSet> {
        let mut result = editability::tests::editable_result();
        result.selected = Some((GridRow::Data(0), 1));
        egui_kittest::Harness::builder()
            .with_size([700.0, 260.0])
            .build_ui_state(viewer, result)
    }

    #[test]
    fn multiline_value_apply_and_cancel_share_pending_edits() {
        let mut h = harness();
        h.get_by_label("Edit Value").click();
        h.run_steps(3);
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.get_by_role_and_label(Role::MultilineTextInput, "Value editor text")
            .type_text("Łódź\nsecond line");
        h.run_steps(2);
        assert!(h.state().edits.changed.is_empty());
        h.get_by_label("Apply Value").click();
        h.run_steps(3);
        assert_eq!(
            h.state().edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("Łódź\nsecond line".into()))
        );
        assert_eq!(h.state().data.rows[0][1], Value::Text("Ada".into()));
        assert!(h.state().edits.editing.is_none());

        h.get_by_label("Edit Value").click();
        h.run_steps(3);
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.get_by_role_and_label(Role::MultilineTextInput, "Value editor text")
            .type_text("discard");
        h.run_steps(2);
        h.key_press(Key::Escape);
        h.run_steps(3);
        assert_eq!(
            h.state().edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("Łódź\nsecond line".into()))
        );
        assert!(h.state().edits.editing.is_none());
    }

    #[test]
    fn generated_and_restored_values_have_no_edit_entry() {
        let mut h = harness();
        h.state_mut().selected = Some((GridRow::Data(0), 2));
        h.run_steps(2);
        assert!(h.get_by_label("Edit Value").accesskit_node().is_disabled());
        assert!(h.query_by_label("Value editor text").is_none());
        h.state_mut().selected = Some((GridRow::Data(0), 1));
        h.state_mut().set_edit_read_only("Run the query again.");
        h.run_steps(2);
        assert!(h.get_by_label("Edit Value").accesskit_node().is_disabled());
        h.get_by_label("Edit Value").click();
        h.run_steps(2);
        assert!(h.state().edits.editing.is_none());
    }

    fn values_harness(values: Vec<Value>) -> egui_kittest::Harness<'static, ResultSet> {
        let mut result = editability::tests::editable_result();
        let data = std::sync::Arc::make_mut(&mut result.data);
        data.rows = values
            .into_iter()
            .map(|value| vec![Value::Int(1), value, Value::Int(0)])
            .collect();
        result.order = (0..data.rows.len()).collect();
        result.selected = Some((GridRow::Data(0), 1));
        egui_kittest::Harness::builder()
            .with_size([700.0, 260.0])
            .build_ui_state(viewer, result)
    }

    fn prepared(h: &egui_kittest::Harness<'static, ResultSet>) -> usize {
        ViewCache::of(&h.ctx).lock().unwrap().prepared
    }

    fn wait_for_worker(h: &mut egui_kittest::Harness<'static, ResultSet>) {
        for _ in 0..500 {
            h.run_steps(1);
            let idle = ViewCache::of(&h.ctx).lock().unwrap().job.is_none();
            if idle && h.query_by_label_contains("Preparing").is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("value viewer worker did not finish");
    }

    #[test]
    fn json_is_parsed_once_across_frames_including_invalid_json() {
        let mut h = values_harness(vec![
            Value::Text(r#"{"vip": false, "n": 1}"#.into()),
            Value::Text("{ not json".into()),
            Value::Text(
                r#"{"id": 7, "name": "Łódź", "tags": ["a", "b"], "nested": {"deep": {"x": 1}, "n": null}, "empty": []}"#
                    .into(),
            ),
        ]);
        h.run_steps(5);
        assert!(h.query_by_label("\"vip\"").is_some());
        assert_eq!(prepared(&h), 1);
        h.state_mut().selected = Some((GridRow::Data(1), 1));
        h.run_steps(5);
        assert!(h.query_by_label("{ not json").is_some());
        assert!(h.query_by_label("Tree").is_none());
        assert_eq!(prepared(&h), 2, "invalid JSON is remembered too");
        h.state_mut().selected = Some((GridRow::Data(0), 1));
        h.run_steps(3);
        assert_eq!(prepared(&h), 2, "both cells stay cached");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        h.state_mut().selected = Some((GridRow::Data(2), 1));
        h.run_steps(3);
        h.render().unwrap().save(dir.join("value-json-tree.png")).unwrap();
    }

    #[test]
    fn large_values_are_prepared_off_the_ui_thread_without_copying_them() {
        let big = format!("[{}]", vec!["{\"id\": 1}"; 200_000].join(","));
        let mut h = values_harness(vec![Value::Text(big)]);
        let data = std::sync::Arc::downgrade(&h.state().data);
        h.run_steps(1);
        wait_for_worker(&mut h);
        assert_eq!(prepared(&h), 1);
        // Only the rows on screen exist as widgets, not 200 001 tree rows.
        let rows = h.query_all_by_label("{").count();
        assert!((1..100).contains(&rows), "{rows} rows drawn");
        h.run_steps(5);
        assert_eq!(prepared(&h), 1);
        // The worker borrowed the fetched value through the shared result.
        assert_eq!(data.strong_count(), 1);
        h.get_by_label("Text").click();
        h.run_steps(1);
        wait_for_worker(&mut h);
        assert!(h.query_all_by_label_contains("\"id\": 1").count() < 200);
    }

    #[test]
    fn stale_worker_results_are_dropped_when_the_cell_or_edit_changes() {
        let big = |c: char| format!("[\"{}\"]", c.to_string().repeat(4 << 20));
        let mut h = values_harness(vec![Value::Text(big('a')), Value::Text(big('b'))]);
        h.run_steps(1);
        // Select another cell while the first is still being prepared.
        h.state_mut().selected = Some((GridRow::Data(1), 1));
        wait_for_worker(&mut h);
        {
            let cache = ViewCache::of(&h.ctx);
            let cache = cache.lock().unwrap();
            assert_eq!(cache.views.len(), 1);
            assert_eq!(cache.views[0].0.row, GridRow::Data(1));
        }
        assert!(h.query_by_label_contains("bbbb").is_some());
        // A staged edit of the same cell gets a new revision and a new view.
        h.state_mut()
            .set_cell(GridRow::Data(1), 1, CellEdit::Text(format!("[\"{}\"]", "c".repeat(1 << 20))));
        h.run_steps(1);
        wait_for_worker(&mut h);
        assert!(h.query_by_label_contains("cccc").is_some());
        assert!(h.query_by_label_contains("bbbb").is_none());
    }
}
