//! Camera and bounded rendering for the immutable worker-prepared plan geometry.

use super::super::theme;
use crate::query_plan::{
    ActualMetrics, PlanNode,
    diagram::{
        ClusterKey, DiagramLayout, QueryBudget, ViewRequest, VisibleDiagram, WorldPoint, WorldRect,
    },
};
use egui::{Color32, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Camera {
    center: WorldPoint,
    scale: f64,
}
impl Default for Camera {
    fn default() -> Self {
        Self {
            center: WorldPoint { x: 0.0, y: 0.0 },
            scale: 1.0,
        }
    }
}
impl Camera {
    fn world(&self, screen: Pos2, canvas: Rect) -> WorldPoint {
        WorldPoint {
            x: self.center.x + (screen.x - canvas.center().x) as f64 / self.scale,
            y: self.center.y + (screen.y - canvas.center().y) as f64 / self.scale,
        }
    }
    fn screen(&self, world: WorldPoint, canvas: Rect) -> Pos2 {
        egui::pos2(
            ((world.x - self.center.x) * self.scale) as f32 + canvas.center().x,
            ((world.y - self.center.y) * self.scale) as f32 + canvas.center().y,
        )
    }
    fn viewport(&self, canvas: Rect) -> WorldRect {
        WorldRect {
            min: self.world(canvas.min, canvas),
            max: self.world(canvas.max, canvas),
        }
    }
    fn fit(&mut self, bounds: WorldRect, canvas: Rect) {
        if canvas.width() <= 0.0 || canvas.height() <= 0.0 {
            return;
        }
        let width = (canvas.width() as f64 - 32.0).max(1.0);
        let height = (canvas.height() as f64 - 32.0).max(1.0);
        self.scale = (width / bounds.width().max(1.0))
            .min(height / bounds.height().max(1.0))
            .clamp(1e-12, 4.0);
        self.center = bounds.center();
    }
    fn zoom(&mut self, scale: f64, anchor: Pos2, canvas: Rect) {
        if !scale.is_finite() || scale <= 0.0 {
            return;
        }
        let before = self.world(anchor, canvas);
        self.scale = scale.clamp(1e-12, 4.0);
        let after = self.world(anchor, canvas);
        self.center.x += before.x - after.x;
        self.center.y += before.y - after.y;
    }
    fn pan(&mut self, delta: Vec2) {
        self.center.x -= delta.x as f64 / self.scale;
        self.center.y -= delta.y as f64 / self.scale;
    }
    fn reveal(&mut self, rect: WorldRect, canvas: Rect, overview: bool) {
        if overview {
            self.scale = self.scale.max(1.0);
        }
        let viewport = self.viewport(canvas);
        if overview || !viewport.contains(rect.min) || !viewport.contains(rect.max) {
            self.center = rect.center();
        }
    }
}
#[derive(Clone, Copy)]
struct Press {
    point: Pos2,
    camera: Camera,
    dragged: bool,
}
#[derive(Clone, Copy)]
enum CameraAction {
    In,
    Out,
    Actual,
    Fit,
}

struct CardText {
    label: String,
    base: Vec<Arc<egui::Galley>>,
    scaled: Vec<Arc<egui::Galley>>,
    scale: f64,
}
impl CardText {
    fn new(ui: &Ui, node: &PlanNode, actual: Option<&ActualMetrics>, id: usize) -> Self {
        let operation = bounded(&node.operation, 320);
        let object = bounded(&node.object, 256);
        let label = format!("Plan operation {}: {} · {}", id + 1, operation, object);
        // Three attribute lines in both modes keep the 260×100 card geometry.
        let fields = match actual {
            Some(actual) => {
                let metric = |text: &Option<String>| value(text.as_deref().unwrap_or_default());
                let time = |text: &Option<String>| match text {
                    _ if actual.never_executed => "Not executed".to_owned(),
                    Some(text) => format!("{} ms", bounded(text, 64)),
                    None => "N/A".to_owned(),
                };
                [
                    operation,
                    object,
                    format!(
                        "Actual Rows: {} · Loops: {}",
                        metric(&actual.rows),
                        metric(&actual.loops)
                    ),
                    format!("Actual Time: {}", time(&actual.total_ms)),
                    format!("Startup Time: {}", time(&actual.startup_ms)),
                ]
            }
            None => [
                operation,
                object,
                format!("Rows: {}", value(&node.rows)),
                format!("Startup Cost: {}", value(&node.startup_cost)),
                format!("Total Cost: {}", value(&node.total_cost)),
            ],
        };
        let base = fields
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                let font = FontId::proportional(if index == 0 { 14.0 } else { 12.0 });
                let mut job = egui::text::LayoutJob::simple(text, font, theme::TEXT, 240.0);
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                ui.fonts_mut(|fonts| fonts.layout_job(job))
            })
            .collect();
        Self {
            label,
            base,
            scaled: Vec::new(),
            scale: f64::NAN,
        }
    }
    fn at_scale(&mut self, scale: f64) -> &[Arc<egui::Galley>] {
        if self.scale != scale {
            self.scale = scale;
            self.scaled.clear();
            self.scaled.extend(self.base.iter().map(|galley| {
                if scale == 1.0 {
                    return Arc::clone(galley);
                }
                // Scaling a TextShape copies its mesh. Do it only when zoom changes,
                // then share the transformed galley during idle and panning frames.
                let mut shape =
                    egui::epaint::TextShape::new(Pos2::ZERO, Arc::clone(galley), theme::TEXT);
                shape.transform(egui::emath::TSTransform::from_scaling(scale as f32));
                shape.galley
            }));
        }
        &self.scaled
    }
}

pub(super) struct DiagramState {
    camera: Camera,
    fitted: bool,
    attributes: bool,
    query: Option<ViewRequest>,
    visible: VisibleDiagram,
    cards: HashMap<usize, CardText>,
    cluster_labels: HashMap<ClusterKey, String>,
    font_key: Option<(f32, egui::FontId, f32)>,
    press: Option<Press>,
    zoom_label: String,
    label_scale: f64,
    #[cfg(test)]
    queries: usize,
    #[cfg(test)]
    painted: usize,
}
impl Default for DiagramState {
    fn default() -> Self {
        Self {
            camera: Default::default(),
            fitted: false,
            attributes: true,
            query: None,
            visible: Default::default(),
            cards: HashMap::new(),
            cluster_labels: HashMap::new(),
            font_key: None,
            press: None,
            zoom_label: "100%".into(),
            label_scale: 1.0,
            #[cfg(test)]
            queries: 0,
            #[cfg(test)]
            painted: 0,
        }
    }
}
impl DiagramState {
    pub(super) fn show(
        &mut self,
        ui: &mut Ui,
        nodes: &[PlanNode],
        actual: Option<&[ActualMetrics]>,
        layout: &DiagramLayout,
        selected: &mut Option<usize>,
    ) {
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            for (label, intent) in [
                ("Zoom In", CameraAction::In),
                ("Zoom Out", CameraAction::Out),
                ("Actual Size", CameraAction::Actual),
                ("Fit Content", CameraAction::Fit),
            ] {
                if ui.button(label).clicked() {
                    action = Some(intent);
                }
            }
            ui.label(&self.zoom_label);
            ui.checkbox(&mut self.attributes, "Attributes");
        });
        ui.add(
            egui::Label::new(
                egui::RichText::new(if self.visible.overview {
                    "Overview at this zoom. Zoom in to inspect operations."
                } else {
                    if actual.is_some() {
                        "Actual execution hierarchy · Drag to pan"
                    } else {
                        "Estimated plan hierarchy · Drag to pan"
                    }
                })
                .small()
                .color(theme::TEXT_MUTED),
            )
            .truncate(),
        );
        let (canvas, _) = ui.allocate_exact_size(
            ui.available_size().max(egui::vec2(1.0, 1.0)),
            Sense::hover(),
        );
        let focus = ui.id().with("diagram_canvas");
        let response = ui.interact(canvas, focus, Sense::click_and_drag());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Query Plan Diagram")
        });
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
        let painter = ui
            .painter()
            .with_clip_rect(ui.clip_rect().intersect(canvas));
        painter.rect_filled(canvas, 0.0, theme::BG_EDITOR);
        if !self.fitted {
            self.camera.fit(layout.bounds(), canvas);
            self.fitted = true;
        }
        if let Some(action) = action {
            match action {
                CameraAction::In => {
                    self.camera
                        .zoom(self.camera.scale * 1.25, canvas.center(), canvas)
                }
                CameraAction::Out => {
                    self.camera
                        .zoom(self.camera.scale / 1.25, canvas.center(), canvas)
                }
                CameraAction::Actual => self.camera.zoom(1.0, canvas.center(), canvas),
                CameraAction::Fit => self.camera.fit(layout.bounds(), canvas),
            }
        }
        let mut new_selection = None;
        if ui.memory(|memory| memory.has_focus(focus)) && !ui.ctx().any_popup_open() {
            for key in [
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::ArrowUp,
                Key::ArrowDown,
                Key::Home,
                Key::End,
            ] {
                if ui.input_mut(|input| input.consume_key(Modifiers::NONE, key)) {
                    let old = selected.unwrap_or(0);
                    let next = match key {
                        Key::ArrowLeft => layout.parent(old).unwrap_or(old),
                        Key::ArrowRight => layout.first_child(old).unwrap_or(old),
                        Key::ArrowUp => old.saturating_sub(1),
                        Key::ArrowDown => (old + 1).min(layout.len().saturating_sub(1)),
                        Key::End => layout.len().saturating_sub(1),
                        _ => 0,
                    };
                    new_selection = Some(next);
                    if let Some(rect) = layout.node_rect(next) {
                        self.camera.reveal(rect, canvas, self.visible.overview);
                    }
                }
            }
        }
        let pointer = ui.input(|input| input.pointer.interact_pos());
        // Cards share the canvas layer, but cover its widget response. Test the
        // clipped layer rectangle so dragging and pinch zoom also work on cards.
        if ui.is_enabled() && ui.rect_contains_pointer(canvas) && !ui.ctx().any_popup_open() {
            ui.input_mut(|input| {
                if input.pointer.primary_pressed()
                    && let Some(point) = pointer
                {
                    self.press = Some(Press {
                        point,
                        camera: self.camera,
                        dragged: false,
                    });
                }
                let mut zoom = 1.0;
                input.events.retain(|event| {
                    if let egui::Event::Zoom(factor) = event {
                        zoom *= *factor as f64;
                        false
                    } else {
                        true
                    }
                });
                if zoom != 1.0 {
                    self.camera.zoom(
                        self.camera.scale * zoom,
                        pointer.unwrap_or(canvas.center()),
                        canvas,
                    );
                }
                self.camera.pan(input.smooth_scroll_delta);
                input.smooth_scroll_delta = Vec2::ZERO;
            });
        }
        let mut click = None;
        let mut dragged = false;
        if let Some(press) = &mut self.press {
            if let Some(point) = pointer {
                let delta = point - press.point;
                if delta.length_sq() > 16.0 {
                    press.dragged = true;
                }
                if press.dragged {
                    dragged = true;
                    self.camera = press.camera;
                    self.camera.pan(delta);
                }
                if ui.input(|input| input.pointer.primary_released())
                    && !press.dragged
                    && canvas.contains(point)
                {
                    click = Some(point);
                }
            }
            if !ui.input(|input| input.pointer.primary_down()) {
                self.press = None;
            }
        }
        self.refresh_query(layout, canvas);
        if let Some(point) = click {
            let world = self.camera.world(point, canvas);
            if self.visible.overview {
                if let Some(cluster) = self
                    .visible
                    .clusters
                    .iter()
                    .find(|cluster| cluster.bounds.contains(world))
                {
                    self.camera.fit(cluster.bounds, canvas);
                    self.refresh_query(layout, canvas);
                }
            } else {
                new_selection = layout.hit_test(world);
            }
            response.request_focus();
        }
        if let Some(id) = new_selection
            && *selected != Some(id)
        {
            *selected = Some(id);
            ui.ctx().request_repaint();
        }
        if self.label_scale != self.camera.scale {
            self.label_scale = self.camera.scale;
            self.zoom_label = if self.camera.scale < 0.01 {
                "<1%".into()
            } else {
                format!("{:.0}%", self.camera.scale * 100.0)
            };
            ui.ctx().request_repaint();
        }
        // A sampled glyph catches font replacement even if the FontId stayed equal.
        let body = egui::TextStyle::Body.resolve(ui.style());
        let glyph = ui.fonts_mut(|fonts| fonts.glyph_width(&body, 'M'));
        let font_key = (ui.ctx().pixels_per_point(), body, glyph);
        if self.font_key.as_ref() != Some(&font_key) {
            self.cards.clear();
            self.font_key = Some(font_key);
        }
        #[cfg(test)]
        {
            self.painted = 0;
        }
        let world_view = self.camera.viewport(canvas);
        for segment in &self.visible.segments {
            if let Some((from, to)) = clip_segment(segment.from, segment.to, world_view) {
                let a = self.camera.screen(from, canvas);
                let b = self.camera.screen(to, canvas);
                painter.line_segment([a, b], Stroke::new(1.0, theme::TEXT_MUTED));
                if segment.child.is_some() && world_view.contains(segment.to) {
                    let direction = (b - a).normalized();
                    if direction.is_finite() && a.distance(b) > 5.0 {
                        let normal = egui::vec2(-direction.y, direction.x);
                        painter.line_segment(
                            [b, b - direction * 6.0 + normal * 3.0],
                            Stroke::new(1.0, theme::TEXT_MUTED),
                        );
                        painter.line_segment(
                            [b, b - direction * 6.0 - normal * 3.0],
                            Stroke::new(1.0, theme::TEXT_MUTED),
                        );
                    }
                }
                #[cfg(test)]
                {
                    self.painted += 1;
                }
            }
        }
        let mut activated_cluster = None;
        for cluster in &self.visible.clusters {
            if let Some(rect) = screen_clipped_rect(self.camera, cluster.bounds, world_view, canvas)
            {
                let rect =
                    Rect::from_center_size(rect.center(), rect.size().max(egui::vec2(2.0, 2.0)));
                painter.rect_filled(rect, 1.0, Color32::from_rgb(58, 76, 95));
                let response = ui.interact(
                    rect,
                    ui.id().with(("plan_cluster", cluster.key)),
                    Sense::click(),
                );
                let label = &self.cluster_labels[&cluster.key];
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label)
                });
                if response.clicked() && !dragged && click.is_none() {
                    activated_cluster = Some(cluster.bounds);
                }
                response.on_hover_text(label);
                #[cfg(test)]
                {
                    self.painted += 1;
                }
            }
        }
        for &id in &self.visible.nodes {
            let Some(world_rect) = layout.node_rect(id) else {
                continue;
            };
            let rect = Rect::from_min_max(
                self.camera.screen(world_rect.min, canvas),
                self.camera.screen(world_rect.max, canvas),
            );
            painter.rect_filled(rect, 2.0, Color32::from_rgb(47, 50, 57));
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(
                    if *selected == Some(id) { 2.0 } else { 1.0 },
                    if *selected == Some(id) {
                        theme::ACCENT
                    } else {
                        theme::TEXT_MUTED
                    },
                ),
                egui::StrokeKind::Inside,
            );
            let text = self.cards.entry(id).or_insert_with(|| {
                CardText::new(ui, &nodes[id], actual.and_then(|actual| actual.get(id)), id)
            });
            let response = ui.interact(rect, ui.id().with(("plan_operation", id)), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    true,
                    *selected == Some(id),
                    &text.label,
                )
            });
            // AccessKit/keyboard activation has no pointer press to hit-test.
            if response.clicked() && !dragged {
                *selected = Some(id);
                ui.memory_mut(|memory| memory.request_focus(focus));
                ui.ctx().request_repaint();
            }
            let count = if self.attributes { 5 } else { 2 };
            let card_painter =
                painter.with_clip_rect(painter.clip_rect().intersect(rect.shrink(3.0)));
            for (row, galley) in text
                .at_scale(self.camera.scale)
                .iter()
                .take(count)
                .enumerate()
            {
                let y = match row {
                    0 => 8.0,
                    1 => 28.0,
                    _ => 48.0 + (row - 2) as f32 * 16.0,
                };
                card_painter.galley(
                    rect.min + egui::vec2(10.0, y) * self.camera.scale as f32,
                    Arc::clone(galley),
                    theme::TEXT,
                );
            }
            #[cfg(test)]
            {
                self.painted += 1;
            }
        }
        if self.visible.overview
            && let Some(rect) = selected.and_then(|id| layout.node_rect(id))
            && let Some(rect) = screen_clipped_rect(self.camera, rect, world_view, canvas)
        {
            painter.rect_stroke(
                Rect::from_center_size(rect.center(), rect.size().max(egui::vec2(3.0, 3.0))),
                0.0,
                Stroke::new(1.5, theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(bounds) = activated_cluster {
            self.camera.fit(bounds, canvas);
            self.refresh_query(layout, canvas);
            ui.memory_mut(|memory| memory.request_focus(focus));
            ui.ctx().request_repaint();
        }
    }

    fn refresh_query(&mut self, layout: &DiagramLayout, canvas: Rect) {
        let request = ViewRequest {
            viewport: self.camera.viewport(canvas),
            scale: self.camera.scale,
            budget: QueryBudget::default(),
        };
        if self.query == Some(request) {
            return;
        }
        self.query = Some(request);
        layout.query_into(request, &mut self.visible);
        self.visible.nodes.sort_unstable();
        self.cards
            .retain(|id, _| self.visible.nodes.binary_search(id).is_ok());
        self.cluster_labels.clear();
        self.cluster_labels
            .extend(self.visible.clusters.iter().map(|cluster| {
                (
                    cluster.key,
                    format!(
                        "Plan overview: {} operations. Click to zoom into this group.",
                        cluster.count
                    ),
                )
            }));
        #[cfg(test)]
        {
            self.queries += 1;
        }
    }
}

fn screen_clipped_rect(
    camera: Camera,
    rect: WorldRect,
    viewport: WorldRect,
    canvas: Rect,
) -> Option<Rect> {
    if !rect.intersects(viewport) {
        return None;
    }
    Some(Rect::from_min_max(
        camera.screen(
            WorldPoint {
                x: rect.min.x.max(viewport.min.x),
                y: rect.min.y.max(viewport.min.y),
            },
            canvas,
        ),
        camera.screen(
            WorldPoint {
                x: rect.max.x.min(viewport.max.x),
                y: rect.max.y.min(viewport.max.y),
            },
            canvas,
        ),
    ))
}
fn clip_segment(
    from: WorldPoint,
    to: WorldPoint,
    rect: WorldRect,
) -> Option<(WorldPoint, WorldPoint)> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let (mut start, mut end) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, from.x - rect.min.x),
        (dx, rect.max.x - from.x),
        (-dy, from.y - rect.min.y),
        (dy, rect.max.y - from.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let ratio = q / p;
            if p < 0.0 {
                start = start.max(ratio);
            } else {
                end = end.min(ratio);
            }
            if start > end {
                return None;
            }
        }
    }
    Some((
        WorldPoint {
            x: from.x + start * dx,
            y: from.y + start * dy,
        },
        WorldPoint {
            x: from.x + end * dx,
            y: from.y + end * dy,
        },
    ))
}
fn bounded(text: &str, limit: usize) -> String {
    let end = text.floor_char_boundary(text.len().min(limit));
    let mut result = text[..end].replace(['\n', '\r'], "↵");
    if end < text.len() {
        result.push('…');
    }
    result
}
fn value(text: &str) -> String {
    if text.is_empty() {
        "N/A".into()
    } else {
        bounded(text, 64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    struct Scene {
        state: DiagramState,
        nodes: Vec<PlanNode>,
        layout: DiagramLayout,
        selected: Option<usize>,
    }

    impl Scene {
        fn new(count: usize, chain: bool) -> Self {
            let nodes: Vec<_> = (0..count)
                .map(|index| PlanNode {
                    operation: format!("Operation {index}"),
                    object: "public.cities".into(),
                    rows: "1250".into(),
                    startup_cost: "0.00".into(),
                    total_cost: "24.50".into(),
                    depth: if chain {
                        index
                    } else {
                        usize::from(index != 0)
                    },
                    subtree_end: if chain || index == 0 {
                        count
                    } else {
                        index + 1
                    },
                    ..Default::default()
                })
                .collect();
            let layout = crate::query_plan::diagram::build(&nodes).unwrap();
            Self {
                state: Default::default(),
                nodes,
                layout,
                selected: Some(0),
            }
        }

        fn show(&mut self, ui: &mut Ui) {
            self.state
                .show(ui, &self.nodes, None, &self.layout, &mut self.selected);
        }
    }

    fn ui_scene(count: usize) -> Harness<'static, Scene> {
        let mut h = Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, scene: &mut Scene| scene.show(ui),
                Scene::new(count, false),
            );
        h.run_steps(4);
        h
    }

    fn pointer(h: &mut Harness<'_, Scene>, at: Pos2, pressed: bool) {
        h.event(egui::Event::PointerMoved(at));
        h.event(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
        h.run_steps(1);
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> WorldRect {
        WorldRect {
            min: WorldPoint { x, y },
            max: WorldPoint {
                x: x + width,
                y: y + height,
            },
        }
    }

    #[test]
    fn analysed_cards_keep_three_attribute_lines_without_misleading_units() {
        let mut h = Harness::new_ui(|ui| {
            let node = PlanNode {
                operation: "Seq Scan".into(),
                rows: "999".into(),
                ..Default::default()
            };
            let lines = |actual: Option<&ActualMetrics>| -> Vec<String> {
                CardText::new(ui, &node, actual, 0)
                    .base
                    .iter()
                    .map(|galley| galley.text().to_owned())
                    .collect()
            };
            let measured = ActualMetrics {
                startup_ms: Some("0.010".into()),
                total_ms: Some("1.5e-3".into()),
                rows: Some("0".into()),
                loops: Some("250".into()),
                never_executed: false,
            };
            assert_eq!(
                lines(Some(&measured))[2..],
                [
                    "Actual Rows: 0 · Loops: 250",
                    "Actual Time: 1.5e-3 ms",
                    "Startup Time: 0.010 ms"
                ]
            );
            assert_eq!(
                lines(Some(&ActualMetrics::default()))[2..],
                [
                    "Actual Rows: N/A · Loops: N/A",
                    "Actual Time: N/A",
                    "Startup Time: N/A"
                ]
            );
            let skipped = ActualMetrics {
                loops: Some("0".into()),
                never_executed: true,
                ..Default::default()
            };
            assert_eq!(lines(Some(&skipped))[3], "Actual Time: Not executed");
            assert_eq!(lines(None)[2], "Rows: 999");
        });
        h.run();
    }

    #[test]
    fn camera_zoom_keeps_pointer_anchor_and_fit_preserves_large_coordinates() {
        let canvas = Rect::from_min_size(egui::pos2(20.0, 60.0), egui::vec2(800.0, 400.0));
        let mut camera = Camera {
            center: WorldPoint {
                x: 33_200_000.0,
                y: 12_400_000.0,
            },
            scale: 1.0,
        };
        let anchor = egui::pos2(233.0, 171.0);
        let before = camera.world(anchor, canvas);
        camera.zoom(2.0, anchor, canvas);
        assert_eq!(camera.world(anchor, canvas), before);
        assert!(camera.screen(before, canvas).distance(anchor) < 0.001);
        camera.pan(egui::vec2(12.0, -8.0));
        assert!((camera.screen(before, canvas) - anchor - egui::vec2(12.0, -8.0)).length() < 0.001);
        let bounds = rect(0.0, 0.0, 33_200_000.0, 12_400_000.0);
        camera.fit(bounds, canvas);
        assert!(camera.viewport(canvas).contains(bounds.min));
        assert!(camera.viewport(canvas).contains(bounds.max));
        let valid = camera;
        camera.zoom(f64::NAN, anchor, canvas);
        camera.zoom(0.0, anchor, canvas);
        assert_eq!(camera, valid);
    }

    #[test]
    fn clipping_keeps_crossing_edges_with_both_endpoints_offscreen() {
        let viewport = rect(10_000_000.0, 30.0, 500.0, 200.0);
        let (a, b) = clip_segment(
            WorldPoint { x: 0.0, y: 80.0 },
            WorldPoint {
                x: 33_200_000.0,
                y: 80.0,
            },
            viewport,
        )
        .unwrap();
        assert!((a.x - viewport.min.x).abs() < 1e-6);
        assert!((b.x - viewport.max.x).abs() < 1e-6);
        assert!(
            clip_segment(
                WorldPoint { x: 0.0, y: 0.0 },
                WorldPoint { x: 1.0, y: 0.0 },
                viewport
            )
            .is_none()
        );
    }

    #[test]
    fn card_click_drag_and_zoom_share_canvas_without_rebuilding_idle_cache() {
        let mut h = ui_scene(4);
        assert!(!h.state().state.visible.overview);
        let label = "Plan operation 3: Operation 2 · public.cities";
        let card = h.get_by_label(label).rect();
        pointer(&mut h, card.center(), true);
        pointer(&mut h, card.center(), false);
        h.run_steps(2);
        assert_eq!(h.state().selected, Some(2));
        let canvas = h.get_by_label("Query Plan Diagram").rect();
        let queries = h.state().state.queries;
        let galley = Arc::as_ptr(&h.state().state.cards[&2].scaled[0]);
        h.run_steps(10);
        assert_eq!(h.state().state.queries, queries);
        assert_eq!(Arc::as_ptr(&h.state().state.cards[&2].scaled[0]), galley);
        h.get_by_label("Attributes").click();
        h.run_steps(3);
        assert_eq!(h.state().state.queries, queries);
        assert_eq!(Arc::as_ptr(&h.state().state.cards[&2].scaled[0]), galley);

        let from = h.get_by_label(label).rect().center();
        let old_camera = h.state().state.camera;
        pointer(&mut h, from, true);
        h.event(egui::Event::PointerMoved(from + egui::vec2(45.0, 20.0)));
        h.run_steps(2);
        pointer(&mut h, from + egui::vec2(45.0, 20.0), false);
        h.run_steps(2);
        assert_eq!(h.state().selected, Some(2));
        assert!(h.state().state.camera.center.x < old_camera.center.x);
        assert_eq!(Arc::as_ptr(&h.state().state.cards[&2].scaled[0]), galley);

        let anchor = h.get_by_label(label).rect().center();
        let world = h.state().state.camera.world(anchor, canvas);
        h.event(egui::Event::PointerMoved(anchor));
        h.event(egui::Event::Zoom(1.25));
        h.run_steps(2);
        let after = h.state().state.camera.world(anchor, canvas);
        assert!((after.x - world.x).abs() < 0.001);
        assert!((after.y - world.y).abs() < 0.001);
    }

    #[test]
    fn keyboard_selection_leaves_overview_and_reveals_an_offscreen_operation() {
        let mut h = ui_scene(5000);
        assert!(h.state().state.visible.overview);
        h.key_press(Key::End);
        h.run_steps(2);
        assert_eq!(h.state().selected, Some(0), "keys require canvas focus");
        let canvas = h.get_by_label("Query Plan Diagram").rect();
        let point = canvas.left_top() + egui::vec2(3.0, 3.0);
        pointer(&mut h, point, true);
        pointer(&mut h, point, false);
        h.key_press(Key::End);
        h.run_steps(3);
        assert_eq!(h.state().selected, Some(4999));
        assert!(!h.state().state.visible.overview);
        let card = h
            .get_by_label("Plan operation 5000: Operation 4999 · public.cities")
            .rect();
        assert!(canvas.contains(card.center()));
        assert!(h.state().state.cards.len() <= QueryBudget::default().max_nodes);
    }

    #[test]
    fn fit_large_plans_has_bounded_cached_primitives_and_no_idle_query() {
        for chain in [false, true] {
            let mut scene = Scene::new(100_000, chain);
            let context = egui::Context::default();
            let draw = |scene: &mut Scene| {
                let mut output = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(
                            Pos2::ZERO,
                            egui::vec2(1100.0, 500.0),
                        )),
                        ..Default::default()
                    },
                    |ui| scene.show(ui),
                );
                output.textures_delta.clear();
            };
            for _ in 0..3 {
                draw(&mut scene);
            }
            let queries = scene.state.queries;
            for _ in 0..3 {
                draw(&mut scene);
            }
            assert_eq!(scene.state.queries, queries);
            assert!(scene.state.visible.overview);
            assert!(scene.state.cards.is_empty());
            assert!(scene.state.painted <= QueryBudget::default().max_clusters);
            assert_eq!(
                scene
                    .state
                    .visible
                    .clusters
                    .iter()
                    .map(|c| c.count)
                    .sum::<usize>(),
                100_000
            );
        }
    }

    #[test]
    fn unicode_card_previews_are_bounded_and_keep_the_original_plan() {
        let mut scene = Scene::new(1, false);
        scene.nodes[0].operation = "Łódź\n".repeat(100_000);
        scene.nodes[0].rows.clear();
        let original_len = scene.nodes[0].operation.len();
        let mut h = Harness::builder()
            .with_size([800.0, 400.0])
            .build_ui_state(|ui, scene: &mut Scene| scene.show(ui), scene);
        h.run_steps(3);
        let text = &h.state().state.cards[&0];
        assert!(text.label.len() < 650);
        assert!(text.label.contains('…'));
        assert!(!text.label.contains('\n'));
        assert_eq!(text.base[2].job.text, "Rows: N/A");
        assert_eq!(h.state().nodes[0].operation.len(), original_len);
    }

    #[test]
    #[ignore = "release CPU diagram UI benchmark; no GPU timing"]
    fn benchmark_diagram_ui_cache() {
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(!cfg!(debug_assertions), "run with --release");
        }
        fn median(values: &mut [std::time::Duration]) -> std::time::Duration {
            values.sort_unstable();
            values[values.len() / 2]
        }
        for chain in [false, true] {
            for count in [500, 5000, 100_000] {
                // Source and worker layout construction are outside UI timings.
                let mut scene = Scene::new(count, chain);
                let context = egui::Context::default();
                let draw = |scene: &mut Scene| {
                    let mut output = context.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(
                                Pos2::ZERO,
                                egui::vec2(1100.0, 500.0),
                            )),
                            ..Default::default()
                        },
                        |ui| scene.show(ui),
                    );
                    output.textures_delta.clear();
                };
                for _ in 0..5 {
                    draw(&mut scene);
                }
                let mut overview = Vec::new();
                let mut idle = Vec::new();
                let mut pan = Vec::new();
                let mut zoom = Vec::new();
                for _ in 0..7 {
                    let start = std::time::Instant::now();
                    for _ in 0..30 {
                        draw(&mut scene);
                    }
                    overview.push(start.elapsed() / 30);
                }
                scene.state.camera.scale = 1.0;
                scene.state.camera.center = scene.layout.node_rect(count / 2).unwrap().center();
                draw(&mut scene);
                let queries = scene.state.queries;
                for _ in 0..7 {
                    let start = std::time::Instant::now();
                    for _ in 0..30 {
                        draw(&mut scene);
                    }
                    idle.push(start.elapsed() / 30);
                }
                assert_eq!(scene.state.queries, queries);
                for _ in 0..7 {
                    scene.state.camera.pan(egui::vec2(-10.0, 2.0));
                    let start = std::time::Instant::now();
                    draw(&mut scene);
                    pan.push(start.elapsed());
                    scene.state.camera.scale *= 1.03;
                    let start = std::time::Instant::now();
                    draw(&mut scene);
                    zoom.push(start.elapsed());
                }
                eprintln!(
                    "diagram_ui shape={} nodes={count} overview_idle={:?} detail_idle={:?} pan={:?} zoom={:?} cards={} primitives={} visited={}",
                    if chain { "chain" } else { "star" },
                    median(&mut overview),
                    median(&mut idle),
                    median(&mut pan),
                    median(&mut zoom),
                    scene.state.cards.len(),
                    scene.state.painted,
                    scene.state.visible.visited
                );
            }
        }
    }
}
