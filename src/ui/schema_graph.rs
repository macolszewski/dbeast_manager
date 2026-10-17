//! Database Graph window: objects of one database as a 3D or 2D graph, sized
//! by estimated rows or storage, with typed relations from metadata. The scan
//! and the layout run in the backend; this view only filters, projects and
//! draws. Pointing at or rotating the graph never reads table contents.

use super::scene3d::{Camera, Preset, Vec3, View};
use super::theme;
use crate::backend::{GraphLayoutRequest, SchemaGraphLoaded};
use crate::model::{DbObject, ObjectKind};
use crate::schema_graph::{
    EdgeKind, GraphNode, Layout, LayoutParams, MetricStatus, NodeKind, SPACING, SchemaGraph,
    SizeMetric, SizeScale, radius,
};
use egui::{Align2, Color32, FontId, Id, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

const DETAILS_WIDTH: f32 = 300.0;
/// Labels of searched objects and neighbours drawn at most; the rest keep their outline.
const MAX_IMPORTANT_LABELS: usize = 200;
/// Relations listed in the details panel.
const MAX_LISTED_RELATIONS: usize = 300;
/// World radius of nodes without a value or without data of their own.
const MARKER_RADIUS: f32 = SPACING * 0.1;

/// What the window asks the application to do after drawing.
pub(super) enum GraphIntent {
    Load {
        ds_id: String,
        database: String,
        schemas: Option<Vec<String>>,
        request: GraphLayoutRequest,
        request_id: u64,
    },
    /// The window closed or the scan was cancelled: stop layouts in progress.
    Cancel,
    Relayout {
        graph: Arc<SchemaGraph>,
        flat: bool,
        pins: Vec<(usize, [f32; 3])>,
        params: LayoutParams,
        request_id: u64,
    },
    OpenData {
        ds_id: String,
        node: DbObject,
    },
    Ddl {
        ds_id: String,
        node: DbObject,
    },
    SaveLayout {
        key: String,
        layout: SavedGraphLayout,
    },
    ResetLayout {
        key: String,
    },
}

/// Saved view of one database's graph: settings and pinned nodes, never data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SavedGraphLayout {
    pub flat: bool,
    pub metric: Option<SizeMetric>,
    pub scale: Option<SizeScale>,
    pub pinned_space: Vec<(String, String, [f32; 3])>,
    pub pinned_flat: Vec<(String, String, [f32; 3])>,
    pub params: Option<LayoutParams>,
    pub display: Option<DisplayOptions>,
}

/// How the graph is drawn; changes apply at once, without a new layout.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct DisplayOptions {
    /// Distances between objects, scaled around the center of the graph.
    pub spread: f32,
    pub node_size: f32,
    /// Labels drawn without being selected, searched or hovered.
    pub labels: usize,
    pub edge_opacity: f32,
    pub depth_fade: bool,
    pub arrows: bool,
    /// Slow rotation around the vertical axis; repaints only while on.
    pub turntable: bool,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self {
            spread: 1.0,
            node_size: 1.0,
            labels: 80,
            edge_opacity: 0.45,
            depth_fade: true,
            arrows: true,
            turntable: false,
        }
    }
}

pub(super) fn layout_key(ds_id: &str, database: &str) -> String {
    format!("{ds_id}\u{1f}{database}")
}

#[derive(Default)]
pub(super) struct GraphWindow {
    view: Option<GraphView>,
    next_request: u64,
}

struct Loaded {
    graph: Arc<SchemaGraph>,
    space: Layout,
    flat: Layout,
    pinned_space: HashMap<usize, [f32; 3]>,
    pinned_flat: HashMap<usize, [f32; 3]>,
    /// Sorted schema names, for the Schemas filter.
    schemas: Arc<[String]>,
    /// Relations of the selected object (outgoing first), cached per selection.
    relations: Option<(usize, Vec<(bool, usize)>)>,
}

struct GraphView {
    ds_id: String,
    title: String,
    database: String,
    schemas: Option<Vec<String>>,
    request_id: u64,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    loaded: Option<Loaded>,
    saved: SavedGraphLayout,
    flat: bool,
    metric: SizeMetric,
    scale: SizeScale,
    hidden_kinds: HashSet<NodeKind>,
    hidden_edges: HashSet<EdgeKind>,
    hidden_schemas: BTreeSet<String>,
    expand_partitions: bool,
    search: String,
    focus: bool,
    selected: Option<usize>,
    context_node: Option<usize>,
    camera: Option<Camera>,
    camera_moved: bool,
    dragging: Option<usize>,
    /// Pending layout request of the 3D (0) and the 2D (1) layout.
    relayout: [Option<u64>; 2],
    /// Changes whenever a layout position changes, so projections are redone.
    positions_version: u64,
    /// What the automatic camera was framed for.
    framed: Option<(usize, u64, bool, u32, u32, Rect)>,
    projection: Option<Arc<Projection>>,
    projection_generation: u64,
    frame: Option<Frame>,
    /// The layout was computed with older forces and is redone when shown.
    stale: [bool; 2],
    params: LayoutParams,
    display: DisplayOptions,
    scene: Option<(SceneKey, Arc<Scene>)>,
}

/// Everything the scene depends on except the camera.
#[derive(Clone, PartialEq)]
struct SceneKey {
    graph: usize,
    metric: SizeMetric,
    scale: SizeScale,
    hidden_kinds: Vec<NodeKind>,
    hidden_edges: Vec<EdgeKind>,
    hidden_schemas: Vec<String>,
    expand_partitions: bool,
    focus: Option<usize>,
    search: String,
}

#[derive(Clone, Copy, PartialEq)]
struct ProjectionKey {
    camera: [u32; 6],
    rect: Rect,
    scene: usize,
    positions: u64,
    flat: bool,
    spread: u32,
    node_size: u32,
}

/// Shown nodes on screen, far to near, plus a lookup by node index.
struct Projection {
    key: ProjectionKey,
    generation: u64,
    ordered: Vec<(usize, Pos2, f32, f32)>,
    lookup: Vec<Option<(Pos2, f32, f32)>>,
}

#[derive(Clone, Copy, PartialEq)]
struct FrameKey {
    projection: u64,
    hovered: Option<usize>,
    selected: Option<usize>,
    display: DisplayOptions,
    positions: u64,
    font_texture: [usize; 2],
    pixels_per_point: u32,
}

/// The tessellated picture of one projection with its accessible labels.
struct Frame {
    key: FrameKey,
    mesh: Arc<egui::Mesh>,
    labels: Vec<(Rect, usize, String)>,
}

/// Filtered nodes and edges with their sizes, rebuilt only when the key changes.
struct Scene {
    visible: Vec<usize>,
    shown: Vec<bool>,
    /// World radius per node index.
    radius: Vec<f32>,
    /// How a node is drawn: sized by a value, or a marker for a missing one.
    sized: Vec<bool>,
    /// Edges between shown nodes, after collapsing partitions; index into graph edges.
    edges: Vec<(usize, usize, EdgeKind, usize)>,
    matches: HashSet<usize>,
    max: f64,
    /// Labels name the schema too when objects come from several schemas.
    several_schemas: bool,
}

impl GraphWindow {
    /// Opens the window for a database and asks for a scan.
    pub(super) fn open(
        &mut self,
        ds_id: String,
        title: String,
        database: String,
        schemas: Option<Vec<String>>,
        saved: SavedGraphLayout,
    ) -> GraphIntent {
        self.next_request += 1;
        let request_id = self.next_request;
        let params = saved.params.unwrap_or_default();
        let request = GraphLayoutRequest {
            flat: saved.flat,
            pins: if saved.flat { saved.pinned_flat.clone() } else { saved.pinned_space.clone() },
            params,
        };
        let display = saved.display.unwrap_or_default();
        self.view = Some(GraphView {
            ds_id: ds_id.clone(),
            title,
            database: database.clone(),
            schemas: schemas.clone(),
            request_id,
            loading: true,
            error: None,
            notice: None,
            loaded: None,
            flat: saved.flat,
            metric: saved.metric.unwrap_or(SizeMetric::Rows),
            scale: saved.scale.unwrap_or(SizeScale::Volume),
            saved,
            hidden_kinds: HashSet::new(),
            hidden_edges: HashSet::new(),
            hidden_schemas: BTreeSet::new(),
            expand_partitions: false,
            search: String::new(),
            focus: false,
            selected: None,
            context_node: None,
            camera: None,
            camera_moved: false,
            dragging: None,
            relayout: [None, None],
            positions_version: 0,
            framed: None,
            projection: None,
            projection_generation: 0,
            frame: None,
            stale: [false, false],
            params,
            display,
            scene: None,
        });
        GraphIntent::Load {
            ds_id,
            database,
            schemas,
            request,
            request_id,
        }
    }

    pub(super) fn loaded(&mut self, request_id: u64, result: Result<Box<SchemaGraphLoaded>, String>) {
        let Some(view) = self.view.as_mut().filter(|view| view.request_id == request_id && view.loading) else {
            return;
        };
        view.loading = false;
        match result {
            Ok(loaded) => {
                let SchemaGraphLoaded { graph, layout, flat: laid_out_flat } = *loaded;
                let names = graph.index_by_name();
                let resolve = |pins: &[(String, String, [f32; 3])]| -> HashMap<usize, [f32; 3]> {
                    pins.iter()
                        .filter_map(|(schema, name, position)| names.get(&(schema.as_str(), name.as_str())).map(|i| (*i, *position)))
                        .collect()
                };
                let pinned_space = resolve(&view.saved.pinned_space);
                let pinned_flat = resolve(&view.saved.pinned_flat);
                drop(names);
                // Only the shown mode was laid out; the other one waits until shown.
                let (space, flat) = if laid_out_flat {
                    (Layout::default(), layout)
                } else {
                    (layout, Layout::default())
                };
                view.error = None;
                view.relayout = [None, None];
                view.positions_version += 1;
                view.stale = [!laid_out_flat, laid_out_flat];
                view.selected = None;
                view.camera = None;
                view.camera_moved = false;
                view.scene = None;
                let schemas: BTreeSet<&str> = graph.nodes.iter().map(|n| n.schema.as_str()).collect();
                let schemas: Arc<[String]> = schemas.into_iter().map(str::to_owned).collect();
                view.loaded = Some(Loaded {
                    graph,
                    space,
                    flat,
                    pinned_space,
                    pinned_flat,
                    schemas,
                    relations: None,
                });
            }
            Err(error) => view.error = Some(error),
        }
    }

    pub(super) fn relaid(&mut self, request_id: u64, flat: bool, layout: Layout) {
        let Some(view) = self.view.as_mut().filter(|view| view.relayout[flat as usize] == Some(request_id)) else {
            return;
        };
        view.relayout[flat as usize] = None;
        if let Some(loaded) = view.loaded.as_mut()
            && layout.positions.len() == loaded.graph.nodes.len()
        {
            if flat {
                loaded.flat = layout;
            } else {
                loaded.space = layout;
            }
            view.positions_version += 1;
            if flat == view.flat {
                view.camera_moved = false;
            }
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> Vec<GraphIntent> {
        let mut intents = Vec::new();
        let Some(view) = self.view.as_mut() else {
            return intents;
        };
        let screen = ctx.content_rect();
        let mut open = true;
        let width = (screen.width() - 40.0).max(480.0);
        let height = (screen.height() - 80.0).max(320.0);
        theme::show_window(ctx, format!("Database Graph · {}", view.title), &mut open, |window| window
            .id(Id::new("schema_graph_window"))
            
            .collapsible(false)
            .default_pos(egui::pos2(screen.left() + 20.0, screen.top() + 40.0))
            .default_size(egui::vec2(width, height))
            .min_size(egui::vec2(480.0, 320.0))
            .max_size(screen.size() - egui::vec2(16.0, 32.0)), |ui| {
                view.ui(ui, &mut self.next_request, &mut intents);
            });
        if !open {
            self.view = None;
            intents.push(GraphIntent::Cancel);
        }
        intents
    }
}

fn kind_color(kind: NodeKind) -> Color32 {
    match kind {
        NodeKind::Table => Color32::from_rgb(0x4A, 0x8C, 0xF0),
        NodeKind::PartitionedTable => Color32::from_rgb(0x2A, 0xB0, 0xA8),
        NodeKind::Partition => Color32::from_rgb(0x6C, 0xC8, 0xC0),
        NodeKind::View => Color32::from_rgb(0xB0, 0x7C, 0xE0),
        NodeKind::MaterializedView => Color32::from_rgb(0xE0, 0x9A, 0x4A),
        NodeKind::ForeignTable => Color32::from_rgb(0x6E, 0xB8, 0x5A),
        NodeKind::ForeignServer => Color32::from_rgb(0x9A, 0x9E, 0xA6),
    }
}

/// Letter inside a node, so kinds are not told apart by color alone.
fn kind_letter(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Table => "T",
        NodeKind::PartitionedTable => "P",
        NodeKind::Partition => "p",
        NodeKind::View => "V",
        NodeKind::MaterializedView => "M",
        NodeKind::ForeignTable => "F",
        NodeKind::ForeignServer => "S",
    }
}

fn edge_color(kind: EdgeKind) -> Color32 {
    match kind {
        EdgeKind::ForeignKey => Color32::from_rgb(0x8F, 0xA8, 0xD8),
        EdgeKind::ViewDependency => Color32::from_rgb(0xB0, 0x7C, 0xE0),
        EdgeKind::MaterializedViewDependency => Color32::from_rgb(0xE0, 0x9A, 0x4A),
        EdgeKind::ForeignSource => Color32::from_rgb(0x6E, 0xB8, 0x5A),
        EdgeKind::Partition => Color32::from_rgb(0x6C, 0x70, 0x78),
    }
}

/// Dash and gap of an edge kind; a solid line has no gap.
fn edge_dash(kind: EdgeKind) -> (f32, f32) {
    match kind {
        EdgeKind::ForeignKey => (1.0, 0.0),
        EdgeKind::ViewDependency => (6.0, 4.0),
        EdgeKind::MaterializedViewDependency => (2.0, 3.0),
        EdgeKind::ForeignSource => (9.0, 3.0),
        EdgeKind::Partition => (1.0, 0.0),
    }
}

fn line(shapes: &mut Vec<egui::Shape>, from: Pos2, to: Pos2, kind: EdgeKind, stroke: Stroke) {
    let (dash, gap) = edge_dash(kind);
    if gap == 0.0 {
        shapes.push(egui::Shape::line_segment([from, to], stroke));
    } else {
        shapes.extend(egui::Shape::dashed_line(&[from, to], stroke, dash, gap));
    }
}

fn metric_label(metric: SizeMetric) -> &'static str {
    match metric {
        SizeMetric::Rows => "Estimated rows",
        SizeMetric::TableBytes => "Table data",
        SizeMetric::TotalBytes => "Total with indexes",
    }
}

impl GraphView {
    fn pins(&self, flat: bool) -> Vec<(usize, [f32; 3])> {
        self.loaded.as_ref().map_or_else(Vec::new, |loaded| {
            let pins = if flat { &loaded.pinned_flat } else { &loaded.pinned_space };
            let mut pins: Vec<_> = pins.iter().map(|(i, p)| (*i, *p)).collect();
            pins.sort_by_key(|(i, _)| *i);
            pins
        })
    }

    fn named_pins(&self, flat: bool) -> Vec<(String, String, [f32; 3])> {
        let Some(loaded) = &self.loaded else {
            return Vec::new();
        };
        self.pins(flat)
            .into_iter()
            .map(|(i, p)| (loaded.graph.nodes[i].schema.clone(), loaded.graph.nodes[i].name.clone(), p))
            .collect()
    }

    fn relayout(&mut self, next_request: &mut u64, flat: bool) -> Option<GraphIntent> {
        let graph = self.loaded.as_ref()?.graph.clone();
        *next_request += 1;
        self.relayout[flat as usize] = Some(*next_request);
        self.stale[flat as usize] = false;
        Some(GraphIntent::Relayout {
            graph,
            flat,
            pins: self.pins(flat),
            params: self.params,
            request_id: *next_request,
        })
    }

    fn ui(&mut self, ui: &mut Ui, next_request: &mut u64, intents: &mut Vec<GraphIntent>) {
        self.toolbar(ui, next_request, intents);
        self.status(ui);
        if self.loaded.is_none() {
            if let Some(error) = &self.error {
                ui.label(RichText::new(error).color(theme::RED));
            } else if self.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading catalogs and statistics…");
                });
            }
            ui.allocate_space(ui.available_size());
            return;
        }
        // A mode shown for the first time is laid out in the background first.
        let laid_out = self.loaded.as_ref().is_some_and(|loaded| {
            let layout = if self.flat { &loaded.flat } else { &loaded.space };
            layout.positions.len() == loaded.graph.nodes.len()
        });
        if !laid_out {
            if self.relayout[self.flat as usize].is_none()
                && let Some(intent) = self.relayout(next_request, self.flat)
            {
                intents.push(intent);
            }
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Laying out the graph…");
            });
            ui.allocate_space(ui.available_size());
            return;
        }
        let scene = self.scene();
        let available = ui.available_rect_before_wrap();
        let details = self.selected.is_some() && available.width() > DETAILS_WIDTH * 2.0;
        let canvas = if details {
            Rect::from_min_max(available.min, egui::pos2(available.max.x - DETAILS_WIDTH - 8.0, available.max.y))
        } else {
            available
        };
        self.canvas(ui, canvas, &scene, intents);
        if details && let Some(selected) = self.selected {
            let rect = Rect::from_min_max(egui::pos2(available.max.x - DETAILS_WIDTH, available.min.y), available.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                self.details(ui, selected, &scene, intents);
            });
        }
        ui.allocate_rect(available, Sense::hover());
    }

    fn toolbar(&mut self, ui: &mut Ui, next_request: &mut u64, intents: &mut Vec<GraphIntent>) {
        ui.horizontal_wrapped(|ui| {
            theme::flat_widgets(ui);
            ui.label(RichText::new("Size:").color(theme::TEXT_MUTED));
            for (metric, label) in [(SizeMetric::Rows, "Estimated rows"), (SizeMetric::TotalBytes, "Storage size")] {
                let active = match metric {
                    SizeMetric::Rows => self.metric == SizeMetric::Rows,
                    _ => self.metric != SizeMetric::Rows,
                };
                if ui.selectable_label(active, label).clicked() && !active {
                    self.metric = metric;
                }
            }
            if self.metric != SizeMetric::Rows {
                egui::ComboBox::from_id_salt("graph_storage")
                    .selected_text(metric_label(self.metric))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.metric, SizeMetric::TableBytes, "Table data")
                            .on_hover_text("Without indexes; PostgreSQL includes TOAST");
                        ui.selectable_value(&mut self.metric, SizeMetric::TotalBytes, "Total with indexes")
                            .on_hover_text("Everything the object occupies, including indexes and TOAST");
                    });
            }
            egui::ComboBox::from_id_salt("graph_scale")
                .selected_text(match self.scale {
                    SizeScale::Volume => "Volume",
                    SizeScale::Logarithmic => "Logarithmic",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.scale, SizeScale::Volume, "Volume")
                        .on_hover_text("Sphere volume proportional to the value");
                    ui.selectable_value(&mut self.scale, SizeScale::Logarithmic, "Logarithmic")
                        .on_hover_text("Radius by the logarithm of the value");
                });
            ui.separator();
            for (flat, label) in [(false, "3D"), (true, "2D")] {
                if ui.selectable_label(self.flat == flat, label).clicked() && self.flat != flat {
                    self.flat = flat;
                    self.camera = None;
                    self.camera_moved = false;
                    if self.stale[flat as usize]
                        && let Some(intent) = self.relayout(next_request, flat)
                    {
                        intents.push(intent);
                    }
                }
            }
            ui.separator();
            let search_label = ui.label(RichText::new("Search:").color(theme::TEXT_MUTED));
            let search = ui
                .add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Name or schema.name")
                        .desired_width(150.0),
                )
                .labelled_by(search_label.id);
            // Enter selects the first match by name, which also works without a pointer.
            if search.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && let Some((_, scene)) = &self.scene
            {
                let graph = &self.loaded.as_ref().expect("scene of a loaded graph").graph;
                self.selected = scene
                    .matches
                    .iter()
                    .copied()
                    .min_by(|a, b| graph.nodes[*a].name.cmp(&graph.nodes[*b].name));
            }
            self.filters(ui);
            self.view_options(ui, next_request, intents);
            let focus_enabled = self.selected.is_some();
            ui.add_enabled_ui(focus_enabled, |ui| {
                ui.toggle_value(&mut self.focus, "Neighbors Only")
                    .on_hover_text("Show the selected object and its direct relations");
            });
            ui.separator();
            if ui.button("Fit View").clicked() {
                self.camera = None;
                self.camera_moved = false;
            }
            // Layout commands share one menu, as the toolbar otherwise wraps.
            ui.add_enabled_ui(self.loaded.is_some(), |ui| {
                ui.menu_button("Layout", |ui| {
                    if ui
                        .add_enabled(self.loaded.is_some() && self.relayout[self.flat as usize].is_none(), egui::Button::new("Re-layout"))
                        .on_hover_text("Arrange the graph again around pinned objects")
                        .clicked()
                        && let Some(intent) = self.relayout(next_request, self.flat)
                    {
                        intents.push(intent);
                    }
                    if ui
                        .add_enabled(self.loaded.is_some(), egui::Button::new("Save Layout"))
                        .on_hover_text("Remember pinned objects, size, scale and 2D/3D for this database")
                        .clicked()
                    {
                        let layout = SavedGraphLayout {
                            flat: self.flat,
                            metric: Some(self.metric),
                            scale: Some(self.scale),
                            pinned_space: self.named_pins(false),
                            pinned_flat: self.named_pins(true),
                            params: Some(self.params),
                            display: Some(self.display),
                        };
                        self.saved = layout.clone();
                        self.notice = Some("Layout saved".into());
                        intents.push(GraphIntent::SaveLayout {
                            key: layout_key(&self.ds_id, &self.database),
                            layout,
                        });
                    }
                    if ui
                        .add_enabled(self.loaded.is_some(), egui::Button::new("Reset Layout"))
                        .on_hover_text("Unpin all objects and forget the saved layout")
                        .clicked()
                    {
                        if let Some(loaded) = self.loaded.as_mut() {
                            loaded.pinned_space.clear();
                            loaded.pinned_flat.clear();
                        }
                        self.positions_version += 1;
                        self.saved = SavedGraphLayout {
                            params: Some(self.params),
                            display: Some(self.display),
                            ..Default::default()
                        };
                        intents.push(GraphIntent::ResetLayout {
                            key: layout_key(&self.ds_id, &self.database),
                        });
                        for flat in [false, true] {
                            if let Some(intent) = self.relayout(next_request, flat) {
                                intents.push(intent);
                            }
                        }
                        self.notice = Some("Layout reset".into());
                    }
                });
            });
            if ui
                .add_enabled(!self.loading, egui::Button::new("Rescan"))
                .on_hover_text("Read catalogs and statistics again; no ANALYZE is run")
                .clicked()
            {
                *next_request += 1;
                self.request_id = *next_request;
                self.loading = true;
                self.error = None;
                intents.push(GraphIntent::Load {
                    ds_id: self.ds_id.clone(),
                    database: self.database.clone(),
                    schemas: self.schemas.clone(),
                    request: GraphLayoutRequest {
                        flat: self.flat,
                        pins: self.named_pins(self.flat),
                        params: self.params,
                    },
                    request_id: self.request_id,
                });
                self.saved.pinned_space = self.named_pins(false);
                self.saved.pinned_flat = self.named_pins(true);
            }
            if self.loading && ui.button("Cancel").clicked() {
                // A late answer is ignored; catalog queries already sent finish on the server.
                self.loading = false;
                self.request_id = 0;
                self.notice = Some("Scan cancelled".into());
                intents.push(GraphIntent::Cancel);
            }
        });
    }

    fn filters(&mut self, ui: &mut Ui) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let graph = loaded.graph.clone();
        ui.menu_button("Objects", |ui| {
            for kind in NodeKind::ALL {
                let count = graph.nodes.iter().filter(|n| n.kind == kind).count();
                if count == 0 {
                    continue;
                }
                let mut shown = !self.hidden_kinds.contains(&kind);
                let label = RichText::new(format!("{} {} ({count})", kind_letter(kind), kind.label())).color(kind_color(kind));
                if ui.checkbox(&mut shown, label).changed() {
                    if shown {
                        self.hidden_kinds.remove(&kind);
                    } else {
                        self.hidden_kinds.insert(kind);
                    }
                }
            }
            if graph.nodes.iter().any(|n| n.kind == NodeKind::Partition) {
                ui.separator();
                ui.checkbox(&mut self.expand_partitions, "Expand partitions")
                    .on_hover_text("Collapsed, a partitioned table shows the sum of its partitions");
            }
        });
        ui.menu_button("Relations", |ui| {
            for kind in EdgeKind::ALL {
                let count = graph.edges.iter().filter(|e| e.kind == kind).count();
                if count == 0 {
                    continue;
                }
                let mut shown = !self.hidden_edges.contains(&kind);
                if ui
                    .checkbox(&mut shown, RichText::new(format!("{} ({count})", kind.label())).color(edge_color(kind)))
                    .on_hover_text(kind.meaning())
                    .changed()
                {
                    if shown {
                        self.hidden_edges.remove(&kind);
                    } else {
                        self.hidden_edges.insert(kind);
                    }
                }
            }
        });
        if loaded.schemas.len() > 1 {
            let schemas = Arc::clone(&loaded.schemas);
            ui.menu_button("Schemas", |ui| {
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for schema in schemas.iter() {
                        let schema = schema.as_str();
                        let mut shown = !self.hidden_schemas.contains(schema);
                        if ui.checkbox(&mut shown, schema).changed() {
                            if shown {
                                self.hidden_schemas.remove(schema);
                            } else {
                                self.hidden_schemas.insert(schema.to_owned());
                            }
                        }
                    }
                });
            });
        }
    }

    fn view_options(&mut self, ui: &mut Ui, next_request: &mut u64, intents: &mut Vec<GraphIntent>) {
        // Settings stay open while sliders and checkboxes change; a click outside closes them.
        let config = egui::containers::menu::MenuConfig::new()
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        egui::containers::menu::MenuButton::new("View").config(config).ui(ui, |ui| {
            ui.set_min_width(300.0);
            ui.spacing_mut().slider_width = 150.0;
            // The menu background hides the default rail; show it and its filled part.
            ui.visuals_mut().widgets.inactive.bg_fill = theme::BORDER_INPUT;
            ui.visuals_mut().slider_trailing_fill = true;
            ui.visuals_mut().selection.bg_fill = theme::ACCENT;
            ui.label(RichText::new("Layout forces").strong());
            ui.label(RichText::new("Recomputed in the background when a slider is released").small().color(theme::TEXT_MUTED));
            let mut changed = false;
            let mut slider = |ui: &mut Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, label: &str, hint: &str| {
                let response = ui
                    .add(egui::Slider::new(value, range).text(label).fixed_decimals(1).suffix("×"))
                    .on_hover_text(hint);
                // Only a finished change asks for a new layout, not every drag step.
                if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    changed = true;
                }
            };
            slider(ui, &mut self.params.link_distance, 0.3..=4.0, "Link distance", "Rest length of relations");
            slider(ui, &mut self.params.attraction, 0.0..=4.0, "Attraction", "How strongly related objects pull each other");
            slider(ui, &mut self.params.repulsion, 0.2..=4.0, "Repulsion", "How strongly all objects push each other apart");
            slider(ui, &mut self.params.grouping, 0.0..=4.0, "Schema grouping", "How strongly objects keep to their schema");
            if changed && self.loaded.is_some() {
                self.stale = [true, true];
                if let Some(intent) = self.relayout(next_request, self.flat) {
                    intents.push(intent);
                }
            }
            ui.separator();
            ui.label(RichText::new("Display").strong());
            let display = &mut self.display;
            ui.add(egui::Slider::new(&mut display.spread, 0.3..=4.0).text("Spread").fixed_decimals(1).suffix("×"))
                .on_hover_text("Move objects apart or together at once, keeping the layout");
            ui.add(egui::Slider::new(&mut display.node_size, 0.2..=3.0).text("Node size").fixed_decimals(1).suffix("×"));
            ui.add(egui::Slider::new(&mut display.labels, 0..=400).text("Labels"))
                .on_hover_text("Names drawn besides selected, searched and hovered objects, largest first");
            ui.add(egui::Slider::new(&mut display.edge_opacity, 0.05..=1.0).text("Relation opacity").fixed_decimals(2));
            ui.checkbox(&mut display.depth_fade, "Fade with depth");
            ui.checkbox(&mut display.arrows, "Arrows on foreign keys");
            ui.add_enabled(!self.flat, egui::Checkbox::new(&mut display.turntable, "Turntable"))
                .on_hover_text("Rotate slowly; repaints continuously only while on");
            ui.separator();
            if ui.button("Reset View Options").clicked() {
                let forces_changed = self.params != LayoutParams::default();
                self.params = LayoutParams::default();
                self.display = DisplayOptions::default();
                if forces_changed && self.loaded.is_some() {
                    self.stale = [true, true];
                    if let Some(intent) = self.relayout(next_request, self.flat) {
                        intents.push(intent);
                    }
                }
            }
        });
    }

    fn status(&mut self, ui: &mut Ui) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let graph = &loaded.graph;
        ui.horizontal_wrapped(|ui| {
            let muted = |text: String| RichText::new(text).small().color(theme::TEXT_MUTED);
            ui.label(muted(format!(
                "{} objects · {} relations · {} · read {} from catalogs and statistics, no table scans",
                graph.nodes.len(),
                graph.edges.len(),
                graph.database,
                graph.read_at
            )));
            if self.loading {
                ui.spinner();
            }
            if graph.truncated {
                ui.label(RichText::new("Partial: object limit reached").small().color(theme::SEARCH_MARKER));
            }
            for note in &graph.notes {
                ui.label(muted(note.clone()));
            }
            if let Some(error) = &self.error {
                ui.label(RichText::new(error).small().color(theme::RED));
            }
            if let Some(notice) = &self.notice {
                ui.label(muted(notice.clone()));
            }
        });
    }

    fn scene(&mut self) -> Arc<Scene> {
        let loaded = self.loaded.as_ref().expect("scene of a loaded graph");
        let mut key = SceneKey {
            graph: Arc::as_ptr(&loaded.graph) as usize,
            metric: self.metric,
            scale: self.scale,
            hidden_kinds: self.hidden_kinds.iter().copied().collect(),
            hidden_edges: self.hidden_edges.iter().copied().collect(),
            hidden_schemas: self.hidden_schemas.iter().cloned().collect(),
            expand_partitions: self.expand_partitions,
            focus: self.focus.then_some(self.selected).flatten(),
            search: self.search.trim().to_lowercase(),
        };
        key.hidden_kinds.sort_by_key(|k| *k as u8);
        key.hidden_edges.sort_by_key(|k| *k as u8);
        if let Some((cached, scene)) = &self.scene
            && *cached == key
        {
            return scene.clone();
        }
        let scene = Arc::new(build_scene(&loaded.graph, &key));
        self.scene = Some((key, scene.clone()));
        scene
    }

    /// Positions as drawn: the layout spread around its center.
    fn positions(&self) -> Vec<[f32; 3]> {
        let loaded = self.loaded.as_ref().expect("positions of a loaded graph");
        let raw = if self.flat { &loaded.flat.positions } else { &loaded.space.positions };
        let spread = self.display.spread;
        if (spread - 1.0).abs() < f32::EPSILON || raw.is_empty() {
            return raw.clone();
        }
        let count = raw.len() as f32;
        let center = raw.iter().fold([0.0; 3], |c, p| [c[0] + p[0] / count, c[1] + p[1] / count, c[2] + p[2] / count]);
        raw.iter()
            .map(|p| {
                [
                    center[0] + (p[0] - center[0]) * spread,
                    center[1] + (p[1] - center[1]) * spread,
                    center[2] + (p[2] - center[2]) * spread,
                ]
            })
            .collect()
    }

    fn canvas(&mut self, ui: &mut Ui, rect: Rect, scene: &Scene, intents: &mut Vec<GraphIntent>) {
        let response = ui.interact(rect, ui.id().with("schema_graph_canvas"), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, theme::BG_EDITOR);
        let graph = self.loaded.as_ref().unwrap().graph.clone();
        if scene.visible.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "No objects match the filters.", FontId::proportional(13.0), theme::TEXT_MUTED);
            return;
        }
        let scene_id = scene as *const Scene as usize;
        let preset = if self.flat { Preset::Front } else { Preset::Oblique };
        let framing = (scene_id, self.positions_version, self.flat, self.display.spread.to_bits(), self.display.node_size.to_bits(), rect);
        if self.camera.is_none() || (!self.camera_moved && self.framed != Some(framing)) {
            let positions = self.positions();
            // Frame the spheres, not only their centers, so large nodes stay inside.
            let points: Vec<Vec3> = scene
                .visible
                .iter()
                .flat_map(|i| {
                    let [x, y, z] = positions[*i];
                    let r = scene.radius[*i] * self.display.node_size * 1.3;
                    [
                        Vec3::new(x - r, y, z),
                        Vec3::new(x + r, y, z),
                        Vec3::new(x, y - r, z),
                        // Room for the label under the node.
                        Vec3::new(x, y + r * 1.6, z),
                        Vec3::new(x, y, z - r),
                        Vec3::new(x, y, z + r),
                    ]
                })
                .collect();
            // The legend takes the bottom of the canvas.
            let frame = Rect::from_min_max(rect.min + Vec2::splat(16.0), rect.max - Vec2::new(16.0, 64.0));
            self.camera = Some(Camera::framing_into(preset, &points, rect, frame));
            self.framed = Some(framing);
        }
        let mut camera = self.camera.unwrap();
        // Turning only while the window has focus; otherwise the scene stands still.
        if self.display.turntable && !self.flat && self.dragging.is_none() && ui.ctx().input(|i| i.focused) {
            let dt = ui.input(|i| i.stable_dt).min(0.1);
            camera.yaw += dt * 0.25;
            self.camera_moved = true;
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
        }
        let view = camera.view();
        self.project(scene, scene_id, &camera, &view, rect);
        let projection = self.projection.clone().expect("projected above");
        // Labels on top of the canvas take the hover, so read the pointer directly.
        let pointer = ui.rect_contains_pointer(rect).then(|| ui.input(|i| i.pointer.latest_pos())).flatten();
        let hit = |pointer: Pos2| {
            projection
                .ordered
                .iter()
                .rev()
                .find(|(_, pos, _, r)| pos.distance(pointer) <= r.max(6.0))
                .map(|(index, ..)| *index)
        };
        let hovered = pointer.and_then(hit);
        // Dragging a node pins it; dragging the background moves the camera.
        // The node is the one under the press, not where the drag threshold was crossed.
        if response.drag_started_by(egui::PointerButton::Primary) && !ui.input(|i| i.modifiers.shift) {
            self.dragging = ui.input(|i| i.pointer.press_origin()).and_then(hit);
        }
        if response.drag_stopped() {
            self.dragging = None;
        }
        if let Some(index) = self.dragging
            && response.dragged_by(egui::PointerButton::Primary)
            && let Some((_, depth, _)) = projection.lookup.get(index).copied().flatten()
        {
            // Drawn positions are spread; the layout keeps unspread ones.
            let delta = view.screen_delta_to_world(response.drag_delta(), depth, rect).scale(1.0 / self.display.spread.max(0.05));
            let loaded = self.loaded.as_mut().unwrap();
            let (layout, pins) = if self.flat {
                (&mut loaded.flat, &mut loaded.pinned_flat)
            } else {
                (&mut loaded.space, &mut loaded.pinned_space)
            };
            let p = &mut layout.positions[index];
            *p = [p[0] + delta.x, p[1] + delta.y, if self.flat { 0.0 } else { p[2] + delta.z }];
            pins.insert(index, *p);
            self.positions_version += 1;
            self.camera_moved = true;
        } else {
            let pan = response.dragged_by(egui::PointerButton::Secondary)
                || response.dragged_by(egui::PointerButton::Middle)
                || (response.dragged_by(egui::PointerButton::Primary) && (self.flat || ui.input(|i| i.modifiers.shift)));
            if pan {
                let delta = response.drag_delta();
                camera.pan(delta.x, delta.y, rect);
                self.camera_moved = true;
            } else if response.dragged_by(egui::PointerButton::Primary) {
                let delta = response.drag_delta();
                camera.orbit(delta.x, delta.y);
                self.camera_moved = true;
            }
        }
        if response.hovered() {
            let wheel = ui.input(|i| i.smooth_scroll_delta.y + i.zoom_delta().ln() * 400.0);
            if wheel != 0.0 {
                camera.zoom(wheel);
                self.camera_moved = true;
            }
        }
        self.camera = Some(camera);
        if response.clicked() {
            self.selected = hovered;
            if hovered.is_none() {
                self.focus = false;
            }
        }
        if response.double_clicked()
            && let Some(index) = hovered
            && let Some(node) = explorer_node(&graph.nodes[index])
        {
            intents.push(GraphIntent::OpenData { ds_id: self.ds_id.clone(), node });
        }
        if response.secondary_clicked() {
            self.context_node = hovered;
            if hovered.is_some() {
                self.selected = hovered;
            }
        }
        let context = self.context_node;
        response.context_menu(|ui| {
            let Some(index) = context else {
                ui.close();
                return;
            };
            self.node_actions(ui, index, &graph, intents);
        });
        // The tessellated picture is reused while nothing it shows has changed,
        // so typing elsewhere in the application does not redraw the graph.
        let key = FrameKey {
            projection: projection.generation,
            hovered,
            selected: self.selected,
            display: self.display,
            positions: self.positions_version,
            font_texture: ui.ctx().fonts(|f| f.font_image_size()),
            pixels_per_point: ui.ctx().pixels_per_point().to_bits(),
        };
        if self.frame.as_ref().is_none_or(|frame| frame.key != key) {
            let frame = self.build_frame(ui.ctx(), &painter, &graph, scene, &projection, key);
            self.frame = Some(frame);
        }
        let frame = self.frame.as_ref().unwrap();
        painter.add(egui::Shape::Mesh(Arc::clone(&frame.mesh)));
        for (label, index, name) in &frame.labels {
            // Labelled objects are exposed to accessibility; hover sense keeps canvas drags working.
            ui.interact(*label, ui.id().with(("graph_label", *index)), Sense::hover())
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, name));
        }
        if let Some(index) = hovered
            && self.dragging.is_none()
        {
            let node = &graph.nodes[index];
            let metric = self.metric.of(node);
            let value = metric.usable().map_or_else(|| metric.status.label().to_owned(), |v| format!("{} ({})", self.metric.format(v), metric.status.label()));
            response.on_hover_text_at_pointer(format!(
                "{}.{}\n{}\n{}: {}",
                node.schema,
                node.name,
                node.kind.label(),
                metric_label(self.metric),
                value
            ));
        }
        self.legend(&painter, rect, scene);
    }

    /// Screen positions of shown nodes, recomputed only when the camera, the
    /// layout, the filters or the canvas change.
    fn project(&mut self, scene: &Scene, scene_id: usize, camera: &Camera, view: &View, rect: Rect) {
        let key = ProjectionKey {
            camera: [
                camera.yaw.to_bits(),
                camera.pitch.to_bits(),
                camera.distance.to_bits(),
                camera.target.x.to_bits(),
                camera.target.y.to_bits(),
                camera.target.z.to_bits(),
            ],
            rect,
            scene: scene_id,
            positions: self.positions_version,
            flat: self.flat,
            spread: self.display.spread.to_bits(),
            node_size: self.display.node_size.to_bits(),
        };
        if self.projection.as_ref().is_some_and(|p| p.key == key) {
            return;
        }
        let positions = self.positions();
        let mut lookup = self
            .projection
            .take()
            .and_then(|p| Arc::try_unwrap(p).ok())
            .map(|p| p.lookup)
            .unwrap_or_default();
        lookup.clear();
        lookup.resize(positions.len(), None);
        let mut ordered = Vec::with_capacity(scene.visible.len());
        for &index in &scene.visible {
            let p = positions[index];
            if let Some(point) = view.project(Vec3::new(p[0], p[1], p[2]), rect) {
                let r = (scene.radius[index] * self.display.node_size * view.pixels_per_unit(point.depth, rect)).max(2.5);
                ordered.push((index, point.pos, point.depth, r));
                lookup[index] = Some((point.pos, point.depth, r));
            }
        }
        // Painted far to near.
        ordered.sort_unstable_by(|a, b| b.2.total_cmp(&a.2));
        self.projection_generation += 1;
        self.projection = Some(Arc::new(Projection {
            key,
            generation: self.projection_generation,
            ordered,
            lookup,
        }));
    }

    /// Edges, nodes and labels as one tessellated mesh.
    fn build_frame(&self, ctx: &egui::Context, painter: &egui::Painter, graph: &SchemaGraph, scene: &Scene, projection: &Projection, key: FrameKey) -> Frame {
        let mut shapes: Vec<egui::Shape> = Vec::with_capacity(scene.edges.len() + projection.ordered.len() * 3);
        let (near, far) = projection.ordered.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p.2), hi.max(p.2)));
        let depth_fade = self.display.depth_fade;
        let fade = |depth: f32| if !depth_fade || far - near < 1.0 { 1.0 } else { 1.0 - 0.55 * (depth - near) / (far - near) };
        let (selected, hovered) = (key.selected, key.hovered);
        let touches = |a: usize, b: usize, node: Option<usize>| node.is_some_and(|n| n == a || n == b);
        for &(from, to, kind, _) in &scene.edges {
            let (Some(a), Some(b)) = (projection.lookup[from], projection.lookup[to]) else {
                continue;
            };
            let focus = touches(from, to, selected) || touches(from, to, hovered);
            let alpha = if focus { 1.0 } else { self.display.edge_opacity * fade((a.1 + b.1) * 0.5) };
            let width = if focus { 2.0 } else if kind == EdgeKind::Partition { 0.8 } else { 1.2 };
            let stroke = Stroke::new(width, edge_color(kind).gamma_multiply(alpha));
            // Stop at the node outlines so arrows stay visible.
            let direction = (b.0 - a.0).normalized();
            let start = a.0 + direction * a.2;
            let end = b.0 - direction * b.2;
            if (end - start).dot(direction) <= 0.0 {
                continue;
            }
            line(&mut shapes, start, end, kind, stroke);
            if self.display.arrows && kind == EdgeKind::ForeignKey && (end - start).length() > 14.0 {
                let normal = Vec2::new(-direction.y, direction.x);
                let size = if focus { 8.0 } else { 6.0 };
                shapes.push(egui::Shape::convex_polygon(
                    vec![end, end - direction * size + normal * size * 0.45, end - direction * size - normal * size * 0.45],
                    stroke.color,
                    Stroke::NONE,
                ));
            }
        }
        let neighbors: HashSet<usize> = selected
            .map(|s| {
                scene
                    .edges
                    .iter()
                    .filter_map(|(a, b, ..)| if *a == s { Some(*b) } else if *b == s { Some(*a) } else { None })
                    .collect()
            })
            .unwrap_or_default();
        let pinned = {
            let loaded = self.loaded.as_ref().unwrap();
            if self.flat { &loaded.pinned_flat } else { &loaded.pinned_space }
        };
        for &(index, pos, depth, r) in &projection.ordered {
            let node = &graph.nodes[index];
            let color = kind_color(node.kind).gamma_multiply(fade(depth));
            let emphasized = Some(index) == selected || Some(index) == hovered;
            let outline = Stroke::new(if emphasized { 2.5 } else { 1.0 }, if emphasized { theme::TEXT } else { color });
            if !scene.sized[index] {
                draw_marker(&mut shapes, painter, node, pos, r, color, outline);
            } else {
                // A lit sphere: base disc, darker rim and a highlight towards the top left.
                shapes.push(egui::Shape::circle_filled(pos, r, color.gamma_multiply(0.75)));
                shapes.push(egui::Shape::circle_filled(pos - Vec2::splat(r * 0.18), r * 0.78, color));
                if r >= 6.0 {
                    shapes.push(egui::Shape::circle_filled(pos - Vec2::splat(r * 0.38), r * 0.25, Color32::from_white_alpha(40)));
                }
                shapes.push(egui::Shape::circle_stroke(pos, r, outline));
                if node.kind == NodeKind::PartitionedTable {
                    shapes.push(egui::Shape::circle_stroke(pos, r + 3.0, Stroke::new(1.0, color)));
                }
            }
            if scene.matches.contains(&index) {
                shapes.push(egui::Shape::circle_stroke(pos, r + 4.0, Stroke::new(2.0, theme::SEARCH_MARKER)));
            }
            if pinned.contains_key(&index) {
                shapes.push(egui::Shape::circle_filled(pos + Vec2::new(r * 0.7, -r * 0.7), 3.0, theme::TEXT));
            }
            if r >= 9.0 {
                let galley = painter.layout_no_wrap(kind_letter(node.kind).to_owned(), FontId::monospace((r * 0.7).min(14.0).round()), theme::BG_EDITOR);
                shapes.push(egui::Shape::galley(pos - galley.size() * 0.5, galley, theme::BG_EDITOR));
            }
        }
        // Labels: important ones first (capped), then the largest nodes that fit.
        let priority = |index: usize| -> u8 {
            if Some(index) == selected || Some(index) == hovered {
                0
            } else if scene.matches.contains(&index) || neighbors.contains(&index) {
                1
            } else {
                2
            }
        };
        let mut order: Vec<(u8, usize, Pos2, f32)> = projection
            .ordered
            .iter()
            .map(|&(index, pos, _, r)| (priority(index), index, pos, r))
            .collect();
        order.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.3.total_cmp(&a.3)));
        let mut placed: Vec<Rect> = Vec::new();
        let mut labels = Vec::new();
        let (mut plain, mut important_count) = (0, 0);
        for (rank, index, pos, r) in order {
            let important = rank < 2;
            if important {
                if rank == 1 && important_count >= MAX_IMPORTANT_LABELS {
                    continue;
                }
                important_count += 1;
            } else if plain >= self.display.labels || (r < 7.0 && scene.sized[index]) {
                // Markers are small by design; their names still matter.
                if plain >= self.display.labels {
                    break;
                }
                continue;
            }
            let node = &graph.nodes[index];
            let text = if self.hidden_schemas.is_empty() && scene.several_schemas {
                format!("{}.{}", node.schema, node.name)
            } else {
                node.name.clone()
            };
            let galley = painter.layout_no_wrap(text, FontId::proportional(if important { 12.5 } else { 11.0 }), theme::TEXT);
            let at = pos + Vec2::new(-galley.size().x * 0.5, r + 3.0);
            let label = Rect::from_min_size(at, galley.size()).expand(2.0);
            if !important && placed.iter().any(|p| p.intersects(label)) {
                continue;
            }
            placed.push(label);
            if !important {
                plain += 1;
            }
            shapes.push(egui::Shape::rect_filled(label, 3.0, theme::BG_EDITOR.gamma_multiply(0.85)));
            labels.push((label, index, galley.text().to_owned()));
            shapes.push(egui::Shape::galley(at, galley, theme::TEXT));
        }
        let options = ctx.tessellation_options(|o| *o);
        let mut tessellator = egui::epaint::Tessellator::new(ctx.pixels_per_point(), options, key.font_texture, Vec::new());
        let mut mesh = egui::Mesh::default();
        for shape in shapes {
            tessellator.tessellate_shape(shape, &mut mesh);
        }
        Frame {
            key,
            mesh: Arc::new(mesh),
            labels,
        }
    }

    fn legend(&self, painter: &egui::Painter, rect: Rect, scene: &Scene) {
        let mut lines = vec![format!(
            "Size: {} · {}{}",
            metric_label(self.metric),
            match self.scale {
                SizeScale::Volume => "volume proportional to the value",
                SizeScale::Logarithmic => "logarithmic radius",
            },
            if scene.max > 0.0 { format!(" · largest {}", self.metric.format(scene.max)) } else { String::new() }
        )];
        lines.push("Dashed ring with ?: no value (unknown or unavailable), not zero · ◆ no data of its own".into());
        lines.push(if self.flat {
            "Drag: pan · Wheel: zoom · Drag an object: pin it · Double-click: Open Data".into()
        } else {
            "Drag: rotate · Shift or right drag: pan · Wheel: zoom · Drag an object: pin it · Double-click: Open Data".into()
        });
        let mut y = rect.bottom() - 8.0;
        for text in lines.iter().rev() {
            let galley = painter.layout_no_wrap(text.clone(), FontId::proportional(11.0), theme::TEXT_MUTED);
            y -= galley.size().y + 2.0;
            let at = egui::pos2(rect.left() + 8.0, y);
            painter.rect_filled(Rect::from_min_size(at, galley.size()).expand(2.0), 2.0, theme::BG_EDITOR.gamma_multiply(0.9));
            painter.galley(at, galley, theme::TEXT_MUTED);
        }
    }

    fn node_actions(&mut self, ui: &mut Ui, index: usize, graph: &SchemaGraph, intents: &mut Vec<GraphIntent>) {
        let node = &graph.nodes[index];
        let explorer = explorer_node(node);
        if ui.add_enabled(explorer.is_some(), egui::Button::new("Open Data")).clicked()
            && let Some(object) = explorer.clone()
        {
            intents.push(GraphIntent::OpenData { ds_id: self.ds_id.clone(), node: object });
            ui.close();
        }
        if ui.add_enabled(explorer.is_some(), egui::Button::new("Generate DDL to Query Console")).clicked()
            && let Some(object) = explorer
        {
            intents.push(GraphIntent::Ddl { ds_id: self.ds_id.clone(), node: object });
            ui.close();
        }
        let loaded = self.loaded.as_mut().unwrap();
        let pins = if self.flat { &mut loaded.pinned_flat } else { &mut loaded.pinned_space };
        let pinned = pins.contains_key(&index);
        if ui.button(if pinned { "Unpin" } else { "Pin" }).clicked() {
            self.positions_version += 1;
            if pinned {
                pins.remove(&index);
            } else {
                let layout = if self.flat { &loaded.flat } else { &loaded.space };
                pins.insert(index, layout.positions[index]);
            }
            ui.close();
        }
        if ui.button("Neighbors Only").clicked() {
            self.selected = Some(index);
            self.focus = true;
            ui.close();
        }
    }

    fn details(&mut self, ui: &mut Ui, index: usize, scene: &Scene, intents: &mut Vec<GraphIntent>) {
        let graph = self.loaded.as_ref().unwrap().graph.clone();
        let Some(node) = graph.nodes.get(index) else {
            return;
        };
        egui::ScrollArea::vertical().id_salt("schema_graph_details").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&node.name).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tool_button(ui, "×", "Close details").clicked() {
                        self.selected = None;
                        self.focus = false;
                    }
                });
            });
            ui.label(RichText::new(format!("{} · {}", node.kind.label(), node.schema)).color(kind_color(node.kind)));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                theme::flat_widgets(ui);
                self.node_actions(ui, index, &graph, intents);
            });
            ui.separator();
            egui::Grid::new("schema_graph_metrics").num_columns(2).spacing([8.0, 4.0]).striped(true).show(ui, |ui| {
                for metric in [SizeMetric::Rows, SizeMetric::TableBytes, SizeMetric::TotalBytes] {
                    let value = metric.of(node);
                    ui.label(RichText::new(metric_label(metric)).color(theme::TEXT_MUTED));
                    let text = match value.usable() {
                        Some(v) => format!("{} · {}", metric.format(v), value.status.label()),
                        None => value.status.label().to_owned(),
                    };
                    let mut hover = String::new();
                    if !value.source.is_empty() {
                        hover.push_str(&format!("Source: {}", value.source));
                    }
                    if let Some(note) = &value.note {
                        if !hover.is_empty() {
                            hover.push('\n');
                        }
                        hover.push_str(note);
                    }
                    let label = ui.label(RichText::new(text).monospace());
                    if !hover.is_empty() {
                        label.on_hover_text(hover);
                    }
                    ui.end_row();
                    if let Some(note) = &value.note
                        && matches!(value.status, MetricStatus::Unknown | MetricStatus::Unavailable)
                    {
                        ui.label("");
                        ui.label(RichText::new(note).small().color(theme::TEXT_MUTED));
                        ui.end_row();
                    }
                }
                for (key, value) in &node.facts {
                    ui.label(RichText::new(key).color(theme::TEXT_MUTED));
                    ui.label(RichText::new(value).monospace());
                    ui.end_row();
                }
                ui.label(RichText::new("Read at").color(theme::TEXT_MUTED));
                ui.label(RichText::new(&graph.read_at).monospace())
                    .on_hover_text("When the catalogs were read; statistics may be older");
                ui.end_row();
            });
            ui.label(
                RichText::new("Statistics describe the object, not the rows visible to you after policies or filters.")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
            ui.separator();
            let relations = {
                let loaded = self.loaded.as_mut().unwrap();
                if loaded.relations.as_ref().is_none_or(|(cached, _)| *cached != index) {
                    let mut relations: Vec<(bool, usize)> = graph
                        .edges
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.from == index || e.to == index)
                        .map(|(i, e)| (e.from == index, i))
                        .collect();
                    relations.sort_by_key(|(outgoing, i)| (!outgoing, graph.edges[*i].kind as u8));
                    loaded.relations = Some((index, relations));
                }
                loaded.relations.as_ref().map(|(_, r)| r.clone()).unwrap_or_default()
            };
            ui.label(RichText::new(format!("Relations ({})", relations.len())).strong());
            if relations.is_empty() {
                ui.label(RichText::new("None recorded in metadata. That does not prove there is no business relation.").small().color(theme::TEXT_MUTED));
            }
            // A hub can have thousands of relations; the first ones are listed.
            if relations.len() > MAX_LISTED_RELATIONS {
                ui.label(
                    RichText::new(format!("The first {MAX_LISTED_RELATIONS} are listed; Neighbors Only shows all in the graph."))
                        .small()
                        .color(theme::TEXT_MUTED),
                );
            }
            for &(outgoing, edge_index) in relations.iter().take(MAX_LISTED_RELATIONS) {
                let edge = &graph.edges[edge_index];
                let other = if outgoing { edge.to } else { edge.from };
                let other_node = &graph.nodes[other];
                let arrow = if outgoing { "→" } else { "←" };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(edge.kind.label()).small().color(edge_color(edge.kind)))
                        .on_hover_text(edge.kind.meaning());
                    let shown = scene.shown.get(other).copied().unwrap_or(false);
                    if ui
                        .link(format!("{arrow} {}.{}", other_node.schema, other_node.name))
                        .on_hover_text(if shown { "Select" } else { "Select (hidden by filters)" })
                        .clicked()
                    {
                        self.selected = Some(other);
                    }
                });
                if !edge.columns.is_empty() {
                    let columns: Vec<String> = edge.columns.iter().map(|(a, b)| format!("{a} → {b}")).collect();
                    let name = edge.name.as_deref().map(|n| format!("{n}: ")).unwrap_or_default();
                    ui.label(RichText::new(format!("{name}{}", columns.join(", "))).small().monospace().color(theme::TEXT_MUTED));
                }
                if edge.validated == Some(false) {
                    ui.label(RichText::new("Not validated (NOT VALID)").small().color(theme::SEARCH_MARKER));
                }
            }
        });
    }
}

/// Nodes drawn without a size: missing values and objects without own data.
fn draw_marker(shapes: &mut Vec<egui::Shape>, painter: &egui::Painter, node: &GraphNode, pos: Pos2, r: f32, color: Color32, outline: Stroke) {
    let no_data = matches!(node.kind, NodeKind::View | NodeKind::ForeignServer | NodeKind::PartitionedTable);
    if no_data {
        let points = vec![pos + Vec2::new(0.0, -r * 1.2), pos + Vec2::new(r * 1.2, 0.0), pos + Vec2::new(0.0, r * 1.2), pos + Vec2::new(-r * 1.2, 0.0)];
        shapes.push(egui::Shape::convex_polygon(points, color.gamma_multiply(0.35), Stroke::new(outline.width.max(1.5), color)));
        if outline.color == theme::TEXT {
            shapes.push(egui::Shape::closed_line(
                vec![pos + Vec2::new(0.0, -r * 1.2 - 2.0), pos + Vec2::new(r * 1.2 + 2.0, 0.0), pos + Vec2::new(0.0, r * 1.2 + 2.0), pos + Vec2::new(-r * 1.2 - 2.0, 0.0)],
                outline,
            ));
        }
    } else {
        let steps = 16;
        let circle: Vec<Pos2> = (0..=steps)
            .map(|i| {
                let angle = i as f32 / steps as f32 * std::f32::consts::TAU;
                pos + Vec2::new(angle.cos(), angle.sin()) * r
            })
            .collect();
        shapes.extend(egui::Shape::dashed_line(&circle, Stroke::new(outline.width.max(1.5), color), 3.0, 2.5));
        if outline.color == theme::TEXT {
            shapes.push(egui::Shape::circle_stroke(pos, r + 2.0, outline));
        }
        let galley = painter.layout_no_wrap("?".to_owned(), FontId::proportional((r * 1.1).clamp(10.0, 16.0).round()), theme::TEXT_MUTED);
        shapes.push(egui::Shape::galley(pos - galley.size() * 0.5, galley, theme::TEXT_MUTED));
    }
}

/// Explorer object for explicit Open Data and DDL actions.
fn explorer_node(node: &GraphNode) -> Option<DbObject> {
    if !node.kind.has_data() || node.path.is_empty() {
        return None;
    }
    let kind = if matches!(node.kind, NodeKind::View | NodeKind::MaterializedView) {
        ObjectKind::View
    } else {
        ObjectKind::Table
    };
    let parent = &node.path[..node.path.len() - 1];
    Some(DbObject::new(kind, node.name.clone(), parent))
}

fn build_scene(graph: &SchemaGraph, key: &SceneKey) -> Scene {
    let n = graph.nodes.len();
    let hidden_kind: HashSet<NodeKind> = key.hidden_kinds.iter().copied().collect();
    let hidden_edge: HashSet<EdgeKind> = key.hidden_edges.iter().copied().collect();
    let hidden_schema: HashSet<&str> = key.hidden_schemas.iter().map(String::as_str).collect();
    // Collapsed partitions are represented by their top partitioned parent.
    let representative: Vec<usize> = (0..n)
        .map(|i| {
            if !key.expand_partitions && graph.nodes[i].kind == NodeKind::Partition {
                graph.root_parent(i)
            } else {
                i
            }
        })
        .collect();
    let mut shown: Vec<bool> = (0..n)
        .map(|i| {
            let node = &graph.nodes[i];
            representative[i] == i && !hidden_kind.contains(&node.kind) && !hidden_schema.contains(node.schema.as_str())
        })
        .collect();
    let mut edges: Vec<(usize, usize, EdgeKind, usize)> = Vec::new();
    let mut seen: HashSet<(usize, usize, EdgeKind)> = HashSet::new();
    for (index, edge) in graph.edges.iter().enumerate() {
        if hidden_edge.contains(&edge.kind) {
            continue;
        }
        let (from, to) = (representative[edge.from], representative[edge.to]);
        if from == to || !shown[from] || !shown[to] {
            continue;
        }
        if seen.insert((from, to, edge.kind)) {
            edges.push((from, to, edge.kind, index));
        }
    }
    if let Some(focus) = key.focus.filter(|f| *f < n) {
        let mut keep = vec![false; n];
        keep[focus] = true;
        for (from, to, ..) in &edges {
            if *from == focus {
                keep[*to] = true;
            }
            if *to == focus {
                keep[*from] = true;
            }
        }
        for i in 0..n {
            shown[i] = shown[i] && keep[i] || i == focus;
        }
        edges.retain(|(from, to, ..)| shown[*from] && shown[*to]);
    }
    let metric = key.metric;
    // A partitioned parent with expanded partitions has no storage of its own.
    let value = |i: usize| -> Option<f64> {
        let node = &graph.nodes[i];
        if key.expand_partitions && node.kind == NodeKind::PartitionedTable {
            return None;
        }
        metric.of(node).usable()
    };
    let visible: Vec<usize> = (0..n).filter(|i| shown[*i]).collect();
    let max = visible.iter().filter_map(|i| value(*i)).fold(0.0f64, f64::max);
    let mut radii = vec![MARKER_RADIUS; n];
    let mut sized = vec![false; n];
    for &i in &visible {
        if let Some(v) = value(i) {
            radii[i] = radius(v, max, key.scale);
            sized[i] = true;
        }
    }
    let matches: HashSet<usize> = if key.search.is_empty() {
        HashSet::new()
    } else {
        visible
            .iter()
            .copied()
            .filter(|i| {
                let node = &graph.nodes[*i];
                node.name.to_lowercase().contains(&key.search) || format!("{}.{}", node.schema, node.name).to_lowercase().contains(&key.search)
            })
            .collect()
    };
    let several_schemas = graph
        .nodes
        .iter()
        .filter(|n| n.kind != NodeKind::ForeignServer)
        .map(|n| n.schema.as_str())
        .collect::<HashSet<_>>()
        .len()
        > 1;
    Scene {
        visible,
        shown,
        radius: radii,
        sized,
        edges,
        matches,
        max,
        several_schemas,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_graph::{GraphEdge, Metric};

    fn node(schema: &str, name: &str, kind: NodeKind, rows: Option<f64>, parent: Option<usize>) -> GraphNode {
        let metric = |v: Option<f64>| match v {
            Some(v) => Metric::known(v, MetricStatus::Estimated, "test"),
            None => Metric::missing(MetricStatus::Unknown, "test", ""),
        };
        GraphNode {
            schema: schema.into(),
            name: name.into(),
            kind,
            path: vec!["db".into(), schema.into(), name.into()],
            rows: metric(rows),
            table_bytes: metric(rows.map(|r| r * 100.0)),
            total_bytes: metric(rows.map(|r| r * 150.0)),
            parent,
            facts: Vec::new(),
        }
    }

    fn graph() -> SchemaGraph {
        let edge = |from, to, kind| GraphEdge { from, to, kind, name: None, columns: Vec::new(), validated: None };
        let mut graph = SchemaGraph {
            database: "db".into(),
            nodes: vec![
                node("public", "customers", NodeKind::Table, Some(1_000.0), None),
                node("public", "orders", NodeKind::Table, Some(8_000.0), None),
                node("public", "events", NodeKind::PartitionedTable, None, None),
                node("public", "events_1", NodeKind::Partition, Some(10.0), Some(2)),
                node("public", "events_2", NodeKind::Partition, Some(20.0), Some(2)),
                node("audit", "log", NodeKind::Table, None, None),
                node("public", "order_view", NodeKind::View, None, None),
            ],
            edges: vec![
                edge(1, 0, EdgeKind::ForeignKey),
                edge(3, 2, EdgeKind::Partition),
                edge(4, 2, EdgeKind::Partition),
                edge(3, 0, EdgeKind::ForeignKey),
                edge(4, 0, EdgeKind::ForeignKey),
                edge(6, 1, EdgeKind::ViewDependency),
            ],
            ..Default::default()
        };
        graph.aggregate_partitions();
        graph
    }

    fn key(metric: SizeMetric) -> SceneKey {
        SceneKey {
            graph: 0,
            metric,
            scale: SizeScale::Volume,
            hidden_kinds: Vec::new(),
            hidden_edges: Vec::new(),
            hidden_schemas: Vec::new(),
            expand_partitions: false,
            focus: None,
            search: String::new(),
        }
    }

    #[test]
    fn collapsed_partitions_merge_edges_and_parents_carry_the_sum() {
        let graph = graph();
        let scene = build_scene(&graph, &key(SizeMetric::Rows));
        assert!(!scene.shown[3] && !scene.shown[4], "partitions are collapsed");
        let to_customers: Vec<_> = scene.edges.iter().filter(|e| e.0 == 2 && e.1 == 0).collect();
        assert_eq!(to_customers.len(), 1, "two partition keys become one edge of the parent");
        assert!(scene.sized[2], "the parent is sized by the sum");
        assert!(!scene.sized[5], "unknown rows are a marker, not a tiny sphere");
        assert!(!scene.sized[6], "views have no size");
        assert_eq!(scene.max, 8_000.0);

        let mut expanded = key(SizeMetric::Rows);
        expanded.expand_partitions = true;
        let scene = build_scene(&graph, &expanded);
        assert!(scene.shown[3] && scene.shown[4]);
        assert!(!scene.sized[2], "no double counting: the parent has no own size when expanded");
    }

    #[test]
    fn metric_switch_changes_sizes_only_and_filters_hide_objects() {
        let graph = graph();
        let rows = build_scene(&graph, &key(SizeMetric::Rows));
        let bytes = build_scene(&graph, &key(SizeMetric::TotalBytes));
        assert_eq!(rows.visible, bytes.visible);
        assert_eq!(rows.edges, bytes.edges);
        let mut filtered = key(SizeMetric::Rows);
        filtered.hidden_schemas = vec!["audit".into()];
        filtered.hidden_kinds = vec![NodeKind::View];
        filtered.search = "order".into();
        let scene = build_scene(&graph, &filtered);
        assert!(!scene.shown[5] && !scene.shown[6]);
        assert!(scene.matches.contains(&1) && !scene.matches.contains(&0));
        let mut focus = key(SizeMetric::Rows);
        focus.focus = Some(1);
        let scene = build_scene(&graph, &focus);
        let visible: Vec<usize> = scene.visible.clone();
        assert_eq!(visible, vec![0, 1, 6], "orders, the customers it references and the view reading it");
    }

    #[test]
    fn explorer_nodes_follow_driver_paths_and_skip_servers() {
        let table = node("public", "orders", NodeKind::Table, Some(1.0), None);
        let object = explorer_node(&table).unwrap();
        assert_eq!(object.path, ["db", "public", "orders"]);
        assert_eq!(object.kind, ObjectKind::Table);
        let mut server = node("Foreign servers", "remote", NodeKind::ForeignServer, None, None);
        server.path.clear();
        assert!(explorer_node(&server).is_none());
    }
}
