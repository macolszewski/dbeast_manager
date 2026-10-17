//! A native query plan snapshot with cached, virtualized tree and raw views.

mod diagram;

use super::theme;
use crate::query_plan::{ActualMetrics, ExplainMode, PlanNode, QueryPlan};
use egui::{Align, Key, Layout, Modifiers, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

const ROW_HEIGHT: f32 = 24.0;
const PREVIEW_BYTES: usize = 4096;

/// Short strings stay in the model. Only truncated previews need an allocation.
#[derive(Default)]
struct Preview(Option<Box<str>>);

impl Preview {
    fn new(text: &str) -> Self {
        Self((text.len() > PREVIEW_BYTES).then(|| {
            let end = text.floor_char_boundary(PREVIEW_BYTES);
            format!("{}… [preview truncated]", &text[..end]).into_boxed_str()
        }))
    }

    fn text<'a>(&'a self, original: &'a str) -> &'a str {
        self.0.as_deref().unwrap_or(original)
    }
}

struct NodePreview {
    operation: Preview,
    object: Preview,
    rows: Preview,
    total_cost: Preview,
    startup_cost: Preview,
    details: Vec<(Preview, Preview)>,
}

impl NodePreview {
    fn new(node: &PlanNode) -> Self {
        Self {
            operation: Preview::new(&node.operation),
            object: Preview::new(&node.object),
            rows: Preview::new(&node.rows),
            total_cost: Preview::new(&node.total_cost),
            startup_cost: Preview::new(&node.startup_cost),
            details: node
                .details
                .iter()
                .map(|(k, v)| (Preview::new(k), Preview::new(v)))
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    OperationsTree,
    Raw,
    Diagram,
}

pub(super) struct PlanView {
    snapshot: u64,
    plan: QueryPlan,
    previews: Vec<NodePreview>,
    raw_previews: Vec<Preview>,
    sql_preview: Preview,
    warning_preview: Preview,
    caption: String,
    parent: Vec<Option<usize>>,
    expanded: Vec<bool>,
    visible: Vec<usize>,
    positions: Vec<Option<usize>>,
    selected: Option<usize>,
    reveal_selected: bool,
    mode: ViewMode,
    diagram: diagram::DiagramState,
    diagram_error: Option<String>,
    details: bool,
    #[cfg(test)]
    rebuilds: usize,
    #[cfg(test)]
    painted_rows: usize,
}

impl PlanView {
    pub(super) fn new(plan: QueryPlan) -> Self {
        let mut parent = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        for (index, node) in plan
            .nodes
            .iter()
            .enumerate()
            .filter(|_| plan.diagram.is_err())
        {
            while stack
                .last()
                .is_some_and(|&p| plan.nodes[p].subtree_end <= index)
            {
                stack.pop();
            }
            parent.push(stack.last().copied());
            if node.subtree_end > index + 1 {
                stack.push(index);
            }
        }
        let context = plan
            .context
            .as_ref()
            .map(|context| {
                let database = context.database.as_deref().unwrap_or("default database");
                let path = context
                    .search_path
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| context.schemas.join(", "));
                format!("{database} · {path}")
            })
            .unwrap_or_else(|| "session context".into());
        let client_ms = plan.elapsed.as_secs_f64() * 1000.0;
        let context = Preview::new(&context).text(&context).to_owned();
        // Client elapsed includes the round trip; server times come from the plan.
        let caption = match &plan.analysis {
            Some(analysis) if plan.mode == ExplainMode::Analyse => format!(
                "Actual execution · Client elapsed: {client_ms:.1} ms · Planning Time: {} · Execution Time: {} · {context}",
                milliseconds(analysis.summary.planning_time_ms.as_deref()),
                milliseconds(analysis.summary.execution_time_ms.as_deref()),
            ),
            _ => format!("Estimated plan · {client_ms:.1} ms · {context}"),
        };
        let mut view = Self {
            snapshot: NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed),
            previews: plan.nodes.iter().map(NodePreview::new).collect(),
            raw_previews: plan
                .raw_lines
                .iter()
                .map(|range| Preview::new(&plan.raw[range.clone()]))
                .collect(),
            sql_preview: Preview::new(&plan.sql),
            warning_preview: Preview::new(plan.warning.as_deref().unwrap_or_default()),
            caption,
            parent,
            expanded: vec![true; plan.nodes.len()],
            positions: vec![None; plan.nodes.len()],
            visible: Vec::with_capacity(plan.nodes.len()),
            selected: (!plan.nodes.is_empty()).then_some(0),
            mode: if plan.nodes.is_empty() {
                ViewMode::Raw
            } else {
                ViewMode::OperationsTree
            },
            diagram: Default::default(),
            diagram_error: plan.diagram.as_ref().err().map(ToString::to_string),
            plan,
            reveal_selected: false,
            details: true,
            #[cfg(test)]
            rebuilds: 0,
            #[cfg(test)]
            painted_rows: 0,
        };
        view.rebuild_visible();
        view
    }

    fn actual(&self, index: usize) -> Option<&ActualMetrics> {
        self.plan.analysis.as_ref()?.nodes.get(index)
    }

    fn parent_of(&self, index: usize) -> Option<usize> {
        self.plan.diagram.as_ref().map_or_else(
            |_| self.parent.get(index).copied().flatten(),
            |layout| layout.parent(index),
        )
    }

    fn set_mode(&mut self, mode: ViewMode) {
        if self.mode == mode {
            return;
        }
        if mode == ViewMode::OperationsTree {
            let mut ancestor = self.selected.and_then(|node| self.parent_of(node));
            let mut changed = false;
            while let Some(node) = ancestor {
                changed |= !self.expanded[node];
                self.expanded[node] = true;
                ancestor = self.parent_of(node);
            }
            if changed {
                self.rebuild_visible();
            }
            self.reveal_selected = true;
        }
        self.mode = mode;
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        self.positions.fill(None);
        let mut index = 0;
        while index < self.plan.nodes.len() {
            self.positions[index] = Some(self.visible.len());
            self.visible.push(index);
            index = if self.expanded[index] {
                index + 1
            } else {
                self.plan.nodes[index]
                    .subtree_end
                    .max(index + 1)
                    .min(self.plan.nodes.len())
            };
        }
        while let Some(selected) = self.selected {
            if self.positions[selected].is_some() {
                break;
            }
            self.selected = self.parent_of(selected);
        }
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
    }

    fn set_all(&mut self, expanded: bool) {
        if self.expanded.iter().any(|&old| old != expanded) {
            self.expanded.fill(expanded);
            self.rebuild_visible();
            self.reveal_selected = true;
        }
    }

    fn toggle(&mut self, index: usize) {
        if self.plan.nodes[index].subtree_end > index + 1 {
            self.expanded[index] = !self.expanded[index];
            self.rebuild_visible();
        }
    }

    fn navigate(&mut self, key: Key, page_rows: usize) {
        let Some(selected) = self.selected else {
            return;
        };
        let position = self.positions[selected].unwrap_or(0);
        let last = self.visible.len().saturating_sub(1);
        let destination = match key {
            Key::ArrowUp => Some(position.saturating_sub(1)),
            Key::ArrowDown => Some((position + 1).min(last)),
            Key::Home => Some(0),
            Key::End => Some(last),
            Key::PageUp => Some(position.saturating_sub(page_rows)),
            Key::PageDown => Some(position.saturating_add(page_rows).min(last)),
            Key::ArrowLeft => {
                if self.expanded[selected] && self.plan.nodes[selected].subtree_end > selected + 1 {
                    self.toggle(selected);
                } else if let Some(parent) = self.parent_of(selected) {
                    self.selected = Some(parent);
                }
                None
            }
            Key::ArrowRight => {
                if self.plan.nodes[selected].subtree_end > selected + 1 {
                    if self.expanded[selected] {
                        self.selected = Some(selected + 1);
                    } else {
                        self.toggle(selected);
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(position) = destination {
            self.selected = self.visible.get(position).copied();
        }
        self.reveal_selected = true;
    }

    pub(super) fn show(&mut self, ui: &mut Ui) {
        #[cfg(test)]
        {
            self.painted_rows = 0;
        }
        ui.push_id(("query_plan", self.snapshot), |ui| {
            ui.horizontal_wrapped(|ui| {
                theme::flat_widgets(ui);
                if copy_icon(ui).clicked() {
                    ui.ctx().copy_text(self.plan.raw.clone());
                }
                ui.separator();
                if ui
                    .selectable_label(self.mode == ViewMode::OperationsTree, "Operations Tree")
                    .clicked()
                {
                    self.set_mode(ViewMode::OperationsTree);
                }
                if ui
                    .selectable_label(self.mode == ViewMode::Raw, "Raw")
                    .clicked()
                {
                    self.set_mode(ViewMode::Raw);
                }
                let show_diagram = ui.add_enabled(
                    self.plan.diagram.is_ok(),
                    egui::Button::selectable(self.mode == ViewMode::Diagram, "Show Diagram"),
                );
                if let Some(error) = &self.diagram_error {
                    show_diagram.clone().on_disabled_hover_text(error);
                }
                if show_diagram.clicked() {
                    self.set_mode(ViewMode::Diagram);
                }
                if self.mode == ViewMode::OperationsTree {
                    ui.separator();
                    if icon(ui, "+", "Expand All", !self.plan.nodes.is_empty()).clicked() {
                        self.set_all(true);
                    }
                    if icon(ui, "−", "Collapse All", !self.plan.nodes.is_empty()).clicked() {
                        self.set_all(false);
                    }
                }
                if self.mode != ViewMode::Raw
                    && ui.selectable_label(self.details, "Details").clicked()
                {
                    self.details = !self.details;
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new(&self.caption)
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            )
            .on_hover_text(self.sql_preview.text(&self.plan.sql));
            if let Some(warning) = &self.plan.warning {
                let text = self.warning_preview.text(warning);
                ui.add(
                    egui::Label::new(RichText::new(text).small().color(theme::SQL_KEYWORD))
                        .truncate(),
                )
                .on_hover_text(text);
            }
            ui.separator();
            if self.mode == ViewMode::Raw {
                self.raw_ui(ui);
            } else {
                if self.details && ui.available_width() >= 420.0 {
                    let width = (ui.available_width() * 0.32).clamp(160.0, 300.0);
                    egui::Panel::right(ui.id().with("plan_properties"))
                        .resizable(true)
                        .default_size(width)
                        .size_range(140.0..=width.max(140.0))
                        .frame(egui::Frame::new().fill(theme::BG_EDITOR).inner_margin(6))
                        .show(ui, |ui| self.details_ui(ui));
                }
                match self.mode {
                    ViewMode::OperationsTree => self.tree_ui(ui),
                    ViewMode::Diagram => {
                        if let Ok(layout) = &self.plan.diagram {
                            let actual = self
                                .plan
                                .analysis
                                .as_ref()
                                .map(|analysis| analysis.nodes.as_slice());
                            self.diagram.show(
                                ui,
                                &self.plan.nodes,
                                actual,
                                layout,
                                &mut self.selected,
                            );
                        }
                    }
                    ViewMode::Raw => unreachable!(),
                }
            }
        });
    }

    fn tree_ui(&mut self, ui: &mut Ui) {
        if self.plan.nodes.is_empty() {
            ui.label("No operations available. Open Raw to inspect the original plan.");
            return;
        }
        let focus = ui.id().with("plan_tree_focus");
        ui.interact(
            ui.available_rect_before_wrap(),
            focus,
            Sense::focusable_noninteractive(),
        );
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                focus,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
        if ui.memory(|memory| memory.has_focus(focus)) && !ui.ctx().any_popup_open() {
            let page = (ui.available_height() / ROW_HEIGHT).max(1.0) as usize;
            for key in [
                Key::ArrowUp,
                Key::ArrowDown,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::Home,
                Key::End,
                Key::PageUp,
                Key::PageDown,
            ] {
                if ui.input_mut(|input| input.consume_key(Modifiers::NONE, key)) {
                    self.navigate(key, page);
                }
            }
        }
        let wide = ui.available_width() >= 720.0;
        let analysed = self.plan.analysis.is_some();
        let mut table = TableBuilder::new(ui)
            .id_salt("plan_operations")
            .striped(false)
            .resizable(true)
            .sense(Sense::click())
            .min_scrolled_height(0.0)
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(100.0).clip(true));
        if wide {
            table = table.column(Column::initial(150.0).at_least(60.0).clip(true));
        }
        table = table.column(
            Column::initial(if analysed { 86.0 } else { 64.0 })
                .at_least(36.0)
                .clip(true),
        );
        if analysed {
            table = table.column(Column::initial(52.0).at_least(36.0).clip(true));
        }
        table = table.column(
            Column::initial(if analysed { 112.0 } else { 86.0 })
                .at_least(45.0)
                .clip(true),
        );
        if wide {
            table = table.column(
                Column::initial(if analysed { 118.0 } else { 86.0 })
                    .at_least(45.0)
                    .clip(true),
            );
        }
        if std::mem::take(&mut self.reveal_selected)
            && let Some(position) = self.selected.and_then(|node| self.positions[node])
        {
            table = table.scroll_to_row(position, Some(Align::Center));
        }
        let mut clicked = None;
        let mut toggle = None;
        table
            .header(ROW_HEIGHT, |mut header| {
                header.col(|ui| {
                    ui.strong("Operation");
                });
                if wide {
                    header.col(|ui| {
                        ui.strong("Params");
                    });
                }
                let titles: &[&str] = if analysed {
                    &[
                        "Actual Rows",
                        "Loops",
                        "Actual Time (ms)",
                        "Startup Time (ms)",
                    ]
                } else {
                    &["Rows", "Total Cost", "Startup Cost"]
                };
                let count = titles.len() - usize::from(!wide);
                for title in &titles[..count] {
                    header.col(|ui| {
                        ui.strong(*title);
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, self.visible.len(), |mut row| {
                    #[cfg(test)]
                    {
                        self.painted_rows += 1;
                    }
                    let index = self.visible[row.index()];
                    let node = &self.plan.nodes[index];
                    let preview = &self.previews[index];
                    let selected = self.selected == Some(index);
                    row.set_selected(selected);
                    row.col(|ui| {
                        ui.add_space(
                            (node.depth.min(40) as f32 * 14.0).min(ui.available_width() * 0.4),
                        );
                        if node.subtree_end > index + 1 {
                            let response = ui.add_sized(
                                [18.0, 20.0],
                                egui::Button::new(if self.expanded[index] { "⌄" } else { "›" })
                                    .frame(false),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    if self.expanded[index] {
                                        "Collapse Operation"
                                    } else {
                                        "Expand Operation"
                                    },
                                )
                            });
                            if response.clicked() {
                                toggle = Some(index);
                            }
                        } else {
                            ui.add_space(18.0);
                        }
                        let text = preview.operation.text(&node.operation);
                        let response =
                            ui.add(egui::Label::new(text).truncate().sense(Sense::click()));
                        response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Button,
                                true,
                                selected,
                                format!("Plan operation: {text}"),
                            )
                        });
                        if response.clicked() {
                            clicked = Some(index);
                        }
                        if response.double_clicked() {
                            toggle = Some(index);
                        }
                        response.on_hover_text(text);
                    });
                    if wide {
                        row.col(|ui| {
                            cell(ui, preview.object.text(&node.object));
                        });
                    }
                    if let Some(actual) =
                        self.plan.analysis.as_ref().and_then(|a| a.nodes.get(index))
                    {
                        row.col(|ui| cell(ui, metric(actual.rows.as_deref())));
                        row.col(|ui| cell(ui, metric(actual.loops.as_deref())));
                        row.col(|ui| cell(ui, actual_time(actual)));
                        if wide {
                            row.col(|ui| cell(ui, startup_time(actual)));
                        }
                    } else {
                        row.col(|ui| {
                            cell(ui, preview.rows.text(&node.rows));
                        });
                        row.col(|ui| {
                            cell(ui, preview.total_cost.text(&node.total_cost));
                        });
                        if wide {
                            row.col(|ui| {
                                cell(ui, preview.startup_cost.text(&node.startup_cost));
                            });
                        }
                    }
                    if row.response().clicked() && toggle.is_none() {
                        clicked = Some(index);
                    }
                });
            });
        if let Some(index) = clicked {
            self.selected = Some(index);
            ui.memory_mut(|m| m.request_focus(focus));
        }
        if let Some(index) = toggle {
            self.toggle(index);
            ui.memory_mut(|m| m.request_focus(focus));
        }
    }

    fn raw_ui(&self, ui: &mut Ui) {
        egui::ScrollArea::both()
            .id_salt("raw_plan")
            .auto_shrink([false, false])
            .show_rows(ui, ROW_HEIGHT, self.plan.raw_lines.len(), |ui, rows| {
                for index in rows {
                    let original = &self.plan.raw[self.plan.raw_lines[index].clone()];
                    let text = self.raw_previews[index].text(original);
                    ui.horizontal(|ui| {
                        ui.set_min_height(ROW_HEIGHT);
                        ui.add(egui::Label::new(RichText::new(text).monospace()).extend());
                    });
                }
            });
    }

    fn details_ui(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.strong("Property");
            ui.add_space(28.0);
            ui.strong("Value");
        });
        ui.separator();
        let Some(index) = self.selected else {
            return;
        };
        let node = &self.plan.nodes[index];
        let preview = &self.previews[index];
        let operation = ("Operation", preview.operation.text(&node.operation));
        let params = ("Params", preview.object.text(&node.object));
        let total_cost = ("Total Cost", preview.total_cost.text(&node.total_cost));
        let startup_cost = (
            "Startup Cost",
            preview.startup_cost.text(&node.startup_cost),
        );
        // Actual values first; estimates stay available below them.
        let all = match self.actual(index) {
            Some(actual) => [
                operation,
                params,
                ("Actual Rows", metric(actual.rows.as_deref())),
                ("Loops", metric(actual.loops.as_deref())),
                ("Actual Time (ms)", actual_time(actual)),
                ("Startup Time (ms)", startup_time(actual)),
                ("Estimated Rows", preview.rows.text(&node.rows)),
                total_cost,
                startup_cost,
            ],
            None => [
                operation,
                params,
                ("Rows", preview.rows.text(&node.rows)),
                total_cost,
                startup_cost,
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
            ],
        };
        let fields = &all[..if self.actual(index).is_some() { 9 } else { 5 }];
        egui::ScrollArea::vertical()
            .id_salt("plan_properties_rows")
            .auto_shrink([false, false])
            .show_rows(
                ui,
                ROW_HEIGHT,
                fields.len() + node.details.len(),
                |ui, rows| {
                    for position in rows {
                        let (key, value) = if position < fields.len() {
                            fields[position]
                        } else {
                            let i = position - fields.len();
                            (
                                preview.details[i].0.text(&node.details[i].0),
                                preview.details[i].1.text(&node.details[i].1),
                            )
                        };
                        ui.horizontal(|ui| {
                            let width = ui.available_width() * 0.43;
                            ui.allocate_ui_with_layout(
                                egui::vec2(width, ROW_HEIGHT),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    ui.set_min_width(width);
                                    cell(ui, key);
                                },
                            );
                            cell(ui, if value.is_empty() { "N/A" } else { value });
                        });
                    }
                },
            );
    }
}

fn metric(value: Option<&str>) -> &str {
    value.unwrap_or("N/A")
}

/// Missing server values have no unit; a known value keeps the column unit.
fn milliseconds(value: Option<&str>) -> String {
    value.map_or_else(|| "N/A".into(), |value| format!("{value} ms"))
}

fn actual_time(actual: &ActualMetrics) -> &str {
    if actual.never_executed {
        "Not executed"
    } else {
        metric(actual.total_ms.as_deref())
    }
}

fn startup_time(actual: &ActualMetrics) -> &str {
    if actual.never_executed {
        "Not executed"
    } else {
        metric(actual.startup_ms.as_deref())
    }
}

fn cell(ui: &mut Ui, value: &str) {
    ui.add(egui::Label::new(value).truncate())
        .on_hover_text(value);
}

fn copy_icon(ui: &mut Ui) -> egui::Response {
    let response = icon(ui, "", "Copy Original Query Plan", true);
    let center = response.rect.center();
    let stroke = ui.style().interact(&response).fg_stroke;
    let painter = ui.painter();
    painter.line_segment(
        [
            center + egui::vec2(-6.0, 4.0),
            center + egui::vec2(-6.0, -7.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + egui::vec2(-6.0, -7.0),
            center + egui::vec2(2.0, -7.0),
        ],
        stroke,
    );
    painter.rect_stroke(
        egui::Rect::from_min_size(center + egui::vec2(-3.0, -4.0), egui::vec2(9.0, 11.0)),
        1.0,
        stroke,
        egui::StrokeKind::Inside,
    );
    response
}

fn icon(ui: &mut Ui, glyph: &str, name: &str, enabled: bool) -> egui::Response {
    let response = theme::tool_button_enabled(ui, enabled, glyph, name);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::{NodeT, Queryable};

    fn plan(count: usize) -> QueryPlan {
        let nodes: Vec<_> = (0..count)
            .map(|index| PlanNode {
                depth: usize::from(index != 0),
                subtree_end: if index == 0 { count } else { index + 1 },
                operation: format!("Operation {index}"),
                rows: index.to_string(),
                ..Default::default()
            })
            .collect();
        QueryPlan {
            mode: Default::default(),
            sql: "SELECT * FROM cities".into(),
            raw: "original\nŁódź\n".into(),
            raw_lines: vec![0..8, 9..16],
            diagram: crate::query_plan::diagram::build(&nodes).map(Box::new),
            nodes,
            warning: None,
            elapsed: Default::default(),
            context: None,
            analysis: None,
        }
    }

    fn analysed_plan(count: usize) -> QueryPlan {
        use crate::query_plan::{PlanAnalysis, PlanSummary};
        let mut data = plan(count);
        data.mode = ExplainMode::Analyse;
        data.analysis = Some(Box::new(PlanAnalysis {
            nodes: (0..count)
                .map(|index| ActualMetrics {
                    startup_ms: (index != 1).then(|| "0.004".into()),
                    total_ms: Some("0.512".into()),
                    rows: Some(format!("{}", index * 10)),
                    loops: Some(if index == 2 { "0" } else { "1" }.into()),
                    never_executed: index == 2,
                })
                .collect(),
            summary: PlanSummary {
                planning_time_ms: Some("0.081".into()),
                execution_time_ms: None,
            },
        }));
        data
    }

    #[test]
    fn analysed_view_separates_actual_metrics_from_estimates() {
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, view: &mut PlanView| view.show(ui),
                PlanView::new(analysed_plan(4)),
            );
        h.run_steps(3);
        let caption = h
            .query_by_label_contains("Actual execution")
            .expect("actual execution caption");
        let caption = caption
            .value()
            .or_else(|| caption.accesskit_node().label())
            .unwrap_or_default();
        assert!(caption.contains("Client elapsed: 0.0 ms"), "{caption}");
        assert!(caption.contains("Planning Time: 0.081 ms"), "{caption}");
        assert!(caption.contains("Execution Time: N/A ·"), "{caption}");
        for header in [
            "Actual Rows",
            "Loops",
            "Actual Time (ms)",
            "Startup Time (ms)",
        ] {
            // Tree header and the matching Details property.
            assert_eq!(h.query_all_by_label(header).count(), 2, "{header}");
        }
        assert!(
            h.query_by_label("Total Cost").is_some(),
            "estimate in Details"
        );
        assert!(h.query_by_label("Estimated Rows").is_some());
        assert_eq!(h.query_all_by_label("Not executed").count(), 2);
        assert!(h.query_by_label("30").is_some());
        // Startup Time of operation 1 is missing: no zero, no unit.
        h.get_by_label("Plan operation: Operation 1").click();
        h.run_steps(2);
        assert!(h.query_all_by_label("N/A").count() >= 2);

        let estimated = PlanView::new(plan(2));
        assert!(estimated.caption.starts_with("Estimated plan · "));
    }

    #[test]
    fn navigation_preserves_cache_and_collapse_selects_visible_parent() {
        let mut view = PlanView::new(plan(100));
        let allocation = view.visible.as_ptr();
        for _ in 0..80 {
            view.navigate(Key::ArrowDown, 10);
        }
        assert_eq!(view.selected, Some(80));
        assert_eq!(view.rebuilds, 1);
        assert_eq!(view.visible.as_ptr(), allocation);
        view.set_all(false);
        assert_eq!(view.visible, [0]);
        assert_eq!(view.selected, Some(0));
        view.navigate(Key::ArrowRight, 10);
        assert_eq!(view.visible.len(), 100);
        view.navigate(Key::ArrowRight, 10);
        assert_eq!(view.selected, Some(1));
        view.navigate(Key::ArrowLeft, 10);
        assert_eq!(view.selected, Some(0));
    }

    #[test]
    fn bounded_previews_keep_utf8_and_original_raw_copy() {
        let mut data = plan(1);
        data.raw = "Ł".repeat(5000);
        data.raw_lines = std::iter::once(0..data.raw.len()).collect();
        data.nodes[0].operation = data.raw.clone();
        data.nodes[0].details = vec![("Long property".into(), data.raw.clone())];
        let view = PlanView::new(data);
        let text = view.raw_previews[0].text(&view.plan.raw);
        assert!(text.ends_with("[preview truncated]"));
        assert!(text.len() < 4200);
        assert_eq!(view.plan.raw.len(), 10000);
        let mut h = egui_kittest::Harness::builder()
            .with_size([800.0, 450.0])
            .build_ui_state(|ui, view: &mut PlanView| view.show(ui), view);
        h.run_steps(3);
        h.get_by_label("Copy Original Query Plan").click();
        h.run_steps(1);
        assert!(h.output().platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == &h.state().plan.raw)));
    }

    #[test]
    fn virtual_tree_reveals_keyboard_selection_without_rebuilding() {
        let mut h = egui_kittest::Harness::builder()
            .with_size([900.0, 400.0])
            .build_ui_state(
                |ui, view: &mut PlanView| view.show(ui),
                PlanView::new(plan(5000)),
            );
        h.run_steps(3);
        assert!(h.state().painted_rows < 30);
        h.get_by_label("Plan operation: Operation 0").click();
        h.run_steps(2);
        h.key_press(Key::End);
        h.run_steps(4);
        assert_eq!(h.state().selected, Some(4999));
        assert!(
            h.get_by_label("Plan operation: Operation 4999")
                .rect()
                .bottom()
                <= 400.0
        );
        assert_eq!(h.state().rebuilds, 1);
        assert!(h.state().painted_rows < 30);
    }

    #[test]
    fn replacing_a_snapshot_resets_the_previous_tree_scroll() {
        let mut h = egui_kittest::Harness::builder()
            .with_size([900.0, 400.0])
            .build_ui_state(
                |ui, view: &mut PlanView| view.show(ui),
                PlanView::new(plan(5000)),
            );
        h.run_steps(3);
        h.get_by_label("Plan operation: Operation 0").click();
        h.run_steps(2);
        h.key_press(Key::End);
        h.run_steps(4);
        assert!(h.query_by_label("Plan operation: Operation 4999").is_some());
        let old_snapshot = h.state().snapshot;
        *h.state_mut() = PlanView::new(plan(5000));
        h.run_steps(4);
        assert_ne!(h.state().snapshot, old_snapshot);
        assert_eq!(h.state().selected, Some(0));
        assert!(h.query_by_label("Plan operation: Operation 0").is_some());
        assert!(h.query_by_label("Plan operation: Operation 4999").is_none());
    }

    #[test]
    fn unknown_plan_opens_raw_and_preserves_warning() {
        let mut data = plan(0);
        data.warning = Some("Unknown native plan".into());
        let view = PlanView::new(data);
        assert_eq!(view.mode, ViewMode::Raw);
        let mut h = egui_kittest::Harness::builder()
            .with_size([500.0, 250.0])
            .build_ui_state(|ui, view: &mut PlanView| view.show(ui), view);
        h.run_steps(3);
        assert!(h.query_by_label("Unknown native plan").is_some());
        assert!(h.query_by_label("original").is_some());
    }

    #[test]
    fn diagram_selection_reveals_ancestors_when_returning_to_tree() {
        let mut view = PlanView::new(plan(100));
        assert!(
            view.parent.is_empty(),
            "reuse the worker layout parent index"
        );
        view.set_all(false);
        view.set_mode(ViewMode::Diagram);
        view.selected = Some(83);
        view.set_mode(ViewMode::OperationsTree);
        assert_eq!(view.selected, Some(83));
        assert!(view.expanded[0]);
        assert_eq!(view.positions[83], Some(83));
        assert!(view.reveal_selected);
        let rebuilds = view.rebuilds;
        view.set_mode(ViewMode::Raw);
        view.set_mode(ViewMode::OperationsTree);
        assert_eq!(view.rebuilds, rebuilds);
    }

    #[test]
    fn unavailable_diagram_keeps_tree_navigation_and_raw_usable() {
        let mut data = plan(3);
        data.diagram = Err(crate::query_plan::diagram::DiagramError::TooManyNodes);
        let mut view = PlanView::new(data);
        assert_eq!(view.parent, [None, Some(0), Some(0)]);
        view.navigate(Key::ArrowDown, 10);
        assert_eq!(view.selected, Some(1));
        view.navigate(Key::ArrowLeft, 10);
        assert_eq!(view.selected, Some(0));
        let mut h = egui_kittest::Harness::builder()
            .with_size([800.0, 450.0])
            .build_ui_state(|ui, view: &mut PlanView| view.show(ui), view);
        h.run_steps(3);
        assert!(
            h.get_by_label("Show Diagram")
                .accesskit_node()
                .is_disabled()
        );
        h.get_by_label("Raw").click();
        h.run_steps(2);
        assert!(h.query_by_label("original").is_some());
    }

    #[test]
    fn diagram_camera_persists_between_views_and_resets_for_new_snapshot() {
        let mut view = PlanView::new(plan(4));
        view.set_mode(ViewMode::Diagram);
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(|ui, view: &mut PlanView| view.show(ui), view);
        h.run_steps(3);
        let label = "Plan operation 1: Operation 0 · ";
        let initial = h.get_by_label(label).rect().width();
        h.get_by_label("Zoom In").click();
        h.run_steps(3);
        let zoomed = h.get_by_label(label).rect().width();
        assert!((zoomed / initial - 1.25).abs() < 0.01);
        h.get_by_label("Raw").click();
        h.run_steps(2);
        h.get_by_label("Show Diagram").click();
        h.run_steps(3);
        assert!((h.get_by_label(label).rect().width() - zoomed).abs() < 0.1);
        let mut replacement = PlanView::new(plan(4));
        replacement.set_mode(ViewMode::Diagram);
        *h.state_mut() = replacement;
        h.run_steps(3);
        assert!((h.get_by_label(label).rect().width() - initial).abs() < 0.1);
    }

    #[test]
    #[ignore = "release CPU view benchmark; no GPU timing"]
    fn benchmark_plan_view_cache() {
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(!cfg!(debug_assertions), "run with --release");
        }
        fn median(samples: &mut [std::time::Duration]) -> std::time::Duration {
            samples.sort_unstable();
            samples[samples.len() / 2]
        }
        for count in [100, 5000, 100000] {
            // Fixture generation and destruction are outside the measured region.
            drop(PlanView::new(plan(count)));
            let mut builds = Vec::new();
            let mut navigation = Vec::new();
            let mut collapse_expand = Vec::new();
            for _ in 0..7 {
                let data = plan(count);
                let start = std::time::Instant::now();
                let mut view = PlanView::new(data);
                builds.push(start.elapsed());
                let start = std::time::Instant::now();
                for _ in 0..10000 {
                    view.navigate(Key::ArrowDown, 20);
                }
                navigation.push(start.elapsed());
                let start = std::time::Instant::now();
                view.set_all(false);
                view.set_all(true);
                collapse_expand.push(start.elapsed());
            }
            let mut h = egui_kittest::Harness::builder()
                .with_size([1100.0, 420.0])
                .build_ui_state(
                    |ui, view: &mut PlanView| view.show(ui),
                    PlanView::new(plan(count)),
                );
            h.run_steps(10);
            let mut idle = Vec::new();
            for _ in 0..7 {
                let start = std::time::Instant::now();
                h.run_steps(100);
                idle.push(start.elapsed() / 100);
            }
            eprintln!(
                "plan nodes={count} samples=7 median_new={:?} median_navigation10000={:?} median_collapse_expand={:?} median_idle_frame={:?}",
                median(&mut builds),
                median(&mut navigation),
                median(&mut collapse_expand),
                median(&mut idle)
            );
        }
        // Analyse adds aligned metrics; construction and idle frames must stay flat.
        let mut analysed_builds = Vec::new();
        for _ in 0..7 {
            let data = analysed_plan(100000);
            let start = std::time::Instant::now();
            let view = PlanView::new(data);
            analysed_builds.push(start.elapsed());
            std::hint::black_box(&view);
        }
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 420.0])
            .build_ui_state(
                |ui, view: &mut PlanView| view.show(ui),
                PlanView::new(analysed_plan(100000)),
            );
        h.run_steps(10);
        let mut analysed_idle = Vec::new();
        for _ in 0..7 {
            let start = std::time::Instant::now();
            h.run_steps(100);
            analysed_idle.push(start.elapsed() / 100);
        }
        eprintln!(
            "plan variant=analyse nodes=100000 samples=7 median_new={:?} median_idle_frame={:?}",
            median(&mut analysed_builds),
            median(&mut analysed_idle)
        );
        let mut populated_builds = Vec::with_capacity(7);
        for sample in 0..8 {
            let mut data = plan(100000);
            for (index, node) in data.nodes.iter_mut().enumerate() {
                node.object = format!("public.orders_partition_{index}");
                node.startup_cost = "0.42".into();
                node.total_cost = "4271.36".into();
                node.details = [
                    ("Node Type", "Index Scan"),
                    ("Parallel Aware", "false"),
                    ("Scan Direction", "Forward"),
                    ("Index Name", "orders_customer_created_at_idx"),
                    ("Index Cond", "(customer_id = 4821)"),
                    (
                        "Filter",
                        "((status = 'paid') AND (created_at >= '2026-01-01'))",
                    ),
                ]
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            }
            // Include one warmup, then isolate construction from all fixture work.
            let start = std::time::Instant::now();
            let view = PlanView::new(data);
            let elapsed = start.elapsed();
            if sample != 0 {
                populated_builds.push(elapsed);
            }
            std::hint::black_box(&view);
        }
        eprintln!(
            "plan variant=properties nodes=100000 properties=600000 samples=7 median_new={:?}",
            median(&mut populated_builds)
        );
    }
}
