//! Raster slippy map with OpenStreetMap tiles and a geometry overlay, used by
//! the Geo Viewer. Coordinates are WGS84 longitude/latitude, shown in Web
//! Mercator like any web map.
//!
//! Tiles follow the OpenStreetMap tile usage policy: an identifying
//! User-Agent, a disk cache, few parallel downloads and visible attribution.

use super::theme;
use crate::geo::{Feature, Layer, Shape};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

pub const OSM_TILES: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
const TILE_SIZE: f64 = 256.0;
const MAX_ZOOM: f64 = 19.0;
const MAX_DOWNLOADS: usize = 4;
const CACHE_DAYS: u64 = 7;
const USER_AGENT: &str = concat!(
    "DBeast/",
    env!("CARGO_PKG_VERSION"),
    " (desktop database client, Geo Viewer)"
);

type TileKey = (u8, u32, u32);
type TileLoader = Arc<dyn Fn(TileKey) -> Option<egui::ColorImage> + Send + Sync>;

/// Bytes of tile textures kept for tiles not drawn recently. Tiles needed by
/// the current view are always kept, so a large view may go above it.
const TEXTURE_BUDGET: usize = 64 << 20;
/// Remembered failures and downloads in flight, so moving around the map
/// does not grow the cache with entries that hold no texture.
const MAX_EMPTY_ENTRIES: usize = 1024;

enum TileState {
    Loading,
    Ready(egui::TextureHandle, usize),
    Failed,
}

struct Tile {
    state: TileState,
    /// Frame number in which a view last drew or kept this tile.
    used: u64,
}

/// Downloaded tiles as textures, shared by all Geo Viewers. Textures are
/// evicted least recently used first once they exceed the byte budget.
pub struct TileCache {
    url: String,
    load: TileLoader,
    tiles: HashMap<TileKey, Tile>,
    tx: Sender<(TileKey, Option<egui::ColorImage>)>,
    rx: Receiver<(TileKey, Option<egui::ColorImage>)>,
    running: Arc<AtomicUsize>,
    budget: usize,
    texture_bytes: usize,
    frame: u64,
}

impl TileCache {
    /// `url` with `{z}`, `{x}` and `{y}`; empty disables the base map.
    pub fn new(url: String, dir: PathBuf) -> Self {
        let template = url.clone();
        let load: TileLoader = Arc::new(move |key: TileKey| {
            let url = template
                .replace("{z}", &key.0.to_string())
                .replace("{x}", &key.1.to_string())
                .replace("{y}", &key.2.to_string());
            load_tile(&url, &dir.join(format!("{}/{}/{}.png", key.0, key.1, key.2)))
        });
        Self::with_loader(url, load, TEXTURE_BUDGET)
    }

    fn with_loader(url: String, load: TileLoader, budget: usize) -> Self {
        let (tx, rx) = channel();
        Self {
            url,
            load,
            tiles: HashMap::new(),
            tx,
            rx,
            running: Arc::default(),
            budget,
            texture_bytes: 0,
            frame: 0,
        }
    }

    /// Takes finished downloads and evicts old textures, once per frame.
    fn poll(&mut self, ctx: &egui::Context) {
        let frame = ctx.cumulative_frame_nr();
        if frame == self.frame && frame != 0 {
            return;
        }
        self.frame = frame;
        // Tiles drawn in the previous frame by any view are still needed.
        let recent = frame.saturating_sub(1);
        for (key, image) in self.rx.try_iter() {
            let Some(tile) = self.tiles.get_mut(&key) else {
                continue;
            };
            match image {
                // A tile that left the view meanwhile is only on disk until needed again.
                Some(_) if tile.used < recent => {
                    self.tiles.remove(&key);
                }
                Some(img) => {
                    let bytes = img.width() * img.height() * 4;
                    let texture =
                        ctx.load_texture(format!("tile-{key:?}"), img, egui::TextureOptions::LINEAR);
                    tile.state = TileState::Ready(texture, bytes);
                    self.texture_bytes += bytes;
                }
                None => tile.state = TileState::Failed,
            }
        }
        self.evict(recent);
    }

    /// Drops least recently used textures above the budget and old empty
    /// entries, never a tile used since `recent`.
    fn evict(&mut self, recent: u64) {
        let empty = self.tiles.len() - self.texture_count();
        if self.texture_bytes <= self.budget && empty <= MAX_EMPTY_ENTRIES {
            return;
        }
        let mut old: Vec<(u64, TileKey)> = self
            .tiles
            .iter()
            .filter(|(_, t)| t.used < recent && !matches!(t.state, TileState::Loading))
            .map(|(k, t)| (t.used, *k))
            .collect();
        old.sort_unstable();
        let mut empty = empty;
        for (_, key) in old {
            if self.texture_bytes <= self.budget && empty <= MAX_EMPTY_ENTRIES {
                break;
            }
            match self.tiles.get(&key).map(|t| &t.state) {
                Some(TileState::Ready(_, bytes)) if self.texture_bytes > self.budget => {
                    self.texture_bytes -= bytes;
                    self.tiles.remove(&key);
                }
                Some(TileState::Failed) if empty > MAX_EMPTY_ENTRIES => {
                    empty -= 1;
                    self.tiles.remove(&key);
                }
                _ => {}
            }
        }
    }

    fn texture_count(&self) -> usize {
        self.tiles
            .values()
            .filter(|t| matches!(t.state, TileState::Ready(..)))
            .count()
    }

    /// Texture of a tile, starting its download when missing.
    fn get(&mut self, key: TileKey, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if self.url.is_empty() {
            return None;
        }
        let frame = self.frame;
        if !self.tiles.contains_key(&key) && self.running.load(Ordering::Relaxed) < MAX_DOWNLOADS {
            self.tiles.insert(
                key,
                Tile {
                    state: TileState::Loading,
                    used: frame,
                },
            );
            self.running.fetch_add(1, Ordering::Relaxed);
            let (load, tx, running, ctx) = (
                self.load.clone(),
                self.tx.clone(),
                self.running.clone(),
                ctx.clone(),
            );
            std::thread::spawn(move || {
                let image = load(key);
                running.fetch_sub(1, Ordering::Relaxed);
                let _ = tx.send((key, image));
                ctx.request_repaint();
            });
        }
        self.ready(key)
    }

    /// Texture of a tile already loaded, marked as still in use.
    fn ready(&mut self, key: TileKey) -> Option<&egui::TextureHandle> {
        let frame = self.frame;
        let tile = self.tiles.get_mut(&key)?;
        tile.used = frame;
        match &tile.state {
            TileState::Ready(t, _) => Some(t),
            _ => None,
        }
    }
}

/// Tile from the disk cache when fresh, otherwise downloaded and cached.
fn load_tile(url: &str, path: &PathBuf) -> Option<egui::ColorImage> {
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < Duration::from_secs(CACHE_DAYS * 86_400));
    let bytes = if fresh {
        std::fs::read(path).ok()?
    } else {
        let mut response = ureq::get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .ok()?;
        let bytes = response.body_mut().read_to_vec().ok()?;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, &bytes);
        bytes
    };
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

/// Position on the Web Mercator world square, both axes 0..1.
pub fn world(lonlat: [f64; 2]) -> [f64; 2] {
    let lat = lonlat[1].clamp(-85.051_128, 85.051_128).to_radians();
    [
        (lonlat[0] + 180.0) / 360.0,
        (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0,
    ]
}

#[derive(Clone, Copy, Debug)]
pub struct MapView {
    /// Center on the world square.
    pub center: [f64; 2],
    pub zoom: f64,
}

impl Default for MapView {
    fn default() -> Self {
        Self {
            center: world([19.0, 52.0]),
            zoom: 5.0,
        }
    }
}

impl MapView {
    fn scale(&self) -> f64 {
        TILE_SIZE * 2f64.powf(self.zoom)
    }

    fn screen_pos(&self, rect: Rect, w: [f64; 2]) -> Pos2 {
        let s = self.scale();
        rect.center()
            + vec2(
                ((w[0] - self.center[0]) * s) as f32,
                ((w[1] - self.center[1]) * s) as f32,
            )
    }

    fn world_pos(&self, rect: Rect, p: Pos2) -> [f64; 2] {
        let s = self.scale();
        let d = p - rect.center();
        [
            self.center[0] + d.x as f64 / s,
            self.center[1] + d.y as f64 / s,
        ]
    }

    /// Centers and zooms so that the world box fits the rect with a margin.
    pub fn fit(&mut self, rect: Rect, min: [f64; 2], max: [f64; 2]) {
        self.center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
        let span = ((max[0] - min[0]) / rect.width().max(1.0) as f64)
            .max((max[1] - min[1]) / rect.height().max(1.0) as f64);
        self.zoom = if span <= 0.0 {
            16.0
        } else {
            (1.0 / (span * 1.25 * TILE_SIZE)).log2().clamp(1.0, 18.0)
        };
    }
}

/// World bounding box: minimum and maximum corner.
type WorldBox = ([f64; 2], [f64; 2]);

/// A polygon as a slice of its feature's coordinates.
struct MapPolygon {
    /// First vertex of the exterior ring in `MapFeature::coords`.
    start: u32,
    /// End of each ring in `MapFeature::coords`, exterior ring first.
    ring_ends: Vec<u32>,
    /// Triangles, as indices relative to `start`.
    triangles: Vec<u32>,
}

/// A feature prepared for drawing. Every coordinate is stored once in world
/// space; lines and polygon rings are ranges of it.
pub struct MapFeature {
    pub row: usize,
    bounds: WorldBox,
    /// Points, then lines, then polygon rings.
    coords: Vec<[f64; 2]>,
    points: u32,
    /// End of each line in `coords`.
    line_ends: Vec<u32>,
    polygons: Vec<MapPolygon>,
    /// Bounds of each run of [`BLOCK`] coordinates, so drawing and picking
    /// skip whole runs far from the view or the pointer.
    blocks: Vec<WorldBox>,
}

/// Coordinates per entry of [`MapFeature::blocks`].
const BLOCK: usize = 256;

impl MapFeature {
    fn points(&self) -> &[[f64; 2]] {
        &self.coords[..self.points as usize]
    }

    fn lines(&self) -> impl Iterator<Item = &[[f64; 2]]> {
        let mut start = self.points as usize;
        self.line_ends.iter().map(move |&end| {
            let line = &self.coords[start..end as usize];
            start = end as usize;
            line
        })
    }

    /// Index in `coords` of the first coordinate of `part`, a line or a ring.
    fn offset(&self, part: &[[f64; 2]]) -> usize {
        (part.as_ptr() as usize - self.coords.as_ptr() as usize) / std::mem::size_of::<[f64; 2]>()
    }

    fn rings<'a>(&'a self, p: &'a MapPolygon) -> impl Iterator<Item = &'a [[f64; 2]]> {
        let mut start = p.start as usize;
        p.ring_ends.iter().map(move |&end| {
            let ring = &self.coords[start..end as usize];
            start = end as usize;
            ring
        })
    }

    /// Bounding box on screen, grown by `margin` for strokes and hit tolerance.
    fn screen_bounds(&self, view: &MapView, rect: Rect, margin: f32) -> Rect {
        Rect::from_min_max(
            view.screen_pos(rect, self.bounds.0),
            view.screen_pos(rect, self.bounds.1),
        )
        .expand(margin)
    }
}

/// Features of a geometry column ready to draw, with their common bounds.
#[derive(Default)]
pub struct MapLayer {
    features: Vec<MapFeature>,
    bounds: Option<WorldBox>,
    /// Shapes of the last drawn view, reused while the view and the selected
    /// row stay the same; hovering does not change them.
    drawn: std::cell::RefCell<Option<DrawnView>>,
}

/// What `DrawnView` was built for: the view, the map's rect and the selected row.
#[derive(Clone, Copy, PartialEq)]
struct ViewKey {
    center: [u64; 2],
    zoom: u64,
    rect: [u32; 4],
    selected: Option<usize>,
}

impl ViewKey {
    fn of(view: &MapView, rect: Rect, selected: Option<usize>) -> Self {
        Self {
            center: view.center.map(f64::to_bits),
            zoom: view.zoom.to_bits(),
            rect: [rect.min.x, rect.min.y, rect.max.x, rect.max.y].map(f32::to_bits),
            selected,
        }
    }
}

struct DrawnView {
    key: ViewKey,
    /// Features in view other than the selected row.
    base: Vec<egui::Shape>,
    /// The selected row's features, drawn once on top.
    selected: Vec<egui::Shape>,
    features: usize,
    /// Screen coordinates computed to build the shapes.
    #[cfg(test)]
    transformed: usize,
}

impl MapLayer {
    pub fn len(&self) -> usize {
        self.features.len()
    }
}

/// Converts features to world coordinates and triangulates polygons once, so
/// drawing a frame only transforms visible vertices. Each source feature is
/// released as soon as it is converted. `None` when cancelled.
pub fn prepare(features: Vec<Feature>, cancelled: &dyn Fn() -> bool) -> Option<MapLayer> {
    let mut earcut = earcut::Earcut::new();
    let mut out = MapLayer {
        features: Vec::with_capacity(features.len()),
        bounds: None,
        drawn: Default::default(),
    };
    for f in features {
        if cancelled() {
            return None;
        }
        let count = f
            .shapes
            .iter()
            .map(|s| match s {
                Shape::Point(_) => 1,
                Shape::Line(l) => l.len(),
                Shape::Polygon(rings) => rings.iter().map(Vec::len).sum(),
            })
            .sum();
        let mut coords = Vec::with_capacity(count);
        coords.extend(f.shapes.iter().filter_map(|s| match s {
            Shape::Point(p) => Some(world(*p)),
            _ => None,
        }));
        let points = coords.len() as u32;
        let mut line_ends = Vec::new();
        for s in &f.shapes {
            if let Shape::Line(l) = s {
                coords.extend(l.iter().map(|p| world(*p)));
                line_ends.push(coords.len() as u32);
            }
        }
        let mut polygons = Vec::new();
        for s in &f.shapes {
            let Shape::Polygon(rings) = s else { continue };
            let start = coords.len();
            let mut ring_ends = Vec::with_capacity(rings.len());
            let mut holes = Vec::with_capacity(rings.len().saturating_sub(1));
            for (i, r) in rings.iter().enumerate() {
                if i > 0 {
                    holes.push((coords.len() - start) as u32);
                }
                coords.extend(r.iter().map(|p| world(*p)));
                ring_ends.push(coords.len() as u32);
            }
            let mut triangles: Vec<u32> = Vec::new();
            earcut.earcut(coords[start..].iter().copied(), &holes, &mut triangles);
            triangles.shrink_to_fit();
            polygons.push(MapPolygon {
                start: start as u32,
                ring_ends,
                triangles,
            });
        }
        let Some(bounds) = box_of(&coords) else {
            continue;
        };
        out.bounds = Some(match out.bounds {
            None => bounds,
            Some((lo, hi)) => (
                [lo[0].min(bounds.0[0]), lo[1].min(bounds.0[1])],
                [hi[0].max(bounds.1[0]), hi[1].max(bounds.1[1])],
            ),
        });
        let blocks = coords.chunks(BLOCK).filter_map(box_of).collect();
        out.features.push(MapFeature {
            row: f.row,
            bounds,
            coords,
            points,
            line_ends,
            polygons,
            blocks,
        });
    }
    Some(out)
}

fn box_of(coords: &[[f64; 2]]) -> Option<WorldBox> {
    let (first, rest) = coords.split_first()?;
    Some(rest.iter().fold((*first, *first), |(lo, hi), p| {
        (
            [lo[0].min(p[0]), lo[1].min(p[1])],
            [hi[0].max(p[0]), hi[1].max(p[1])],
        )
    }))
}

/// Layer key: geometry column and SRID override.
pub type LayerKey = (usize, Option<u32>);

/// Geometry columns up to this many bytes are prepared within the frame.
const SYNC_PREPARE_BYTES: usize = 256 << 10;

/// A geometry column prepared for the map: decoding results and features.
pub struct PreparedLayer {
    /// SRID, its source, skipped rows and errors; `features` is left empty.
    pub info: Layer,
    pub map: MapLayer,
}

struct PendingLayer {
    key: LayerKey,
    generation: u64,
    cancel: Arc<AtomicBool>,
    rx: Receiver<(u64, Option<PreparedLayer>)>,
}

impl Drop for PendingLayer {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Prepares geometry layers of one result. Large columns are decoded,
/// reprojected and triangulated on a worker thread; a newer request cancels
/// the older one, and only the newest generation is published.
pub struct LayerLoader {
    generation: u64,
    current: Option<(LayerKey, PreparedLayer)>,
    pending: Option<PendingLayer>,
    sync_bytes: usize,
}

impl Default for LayerLoader {
    fn default() -> Self {
        Self {
            generation: 0,
            current: None,
            pending: None,
            sync_bytes: SYNC_PREPARE_BYTES,
        }
    }
}

fn prepare_layer(
    rows: &[Vec<crate::model::Value>],
    (column, srid): LayerKey,
    cancelled: &dyn Fn() -> bool,
) -> Option<PreparedLayer> {
    let mut info = crate::geo::build_layer_cancellable(rows, column, srid, cancelled)?;
    let map = prepare(std::mem::take(&mut info.features), cancelled)?;
    Some(PreparedLayer { info, map })
}

impl LayerLoader {
    /// Drops the prepared layer and cancels a preparation in progress.
    pub fn reset(&mut self) {
        self.current = None;
        self.pending = None;
    }

    /// The layer for `key`, or `None` while it is being prepared. The flag is
    /// true in the frame the layer becomes available.
    pub fn get(
        &mut self,
        data: &Arc<crate::model::QueryResult>,
        key: LayerKey,
        ctx: &egui::Context,
    ) -> (Option<&PreparedLayer>, bool) {
        let mut fresh = false;
        if let Some(p) = &self.pending {
            match p.rx.try_recv() {
                Ok((generation, Some(layer))) if generation == p.generation && p.key == key => {
                    self.current = Some((key, layer));
                    self.pending = None;
                    fresh = true;
                }
                Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let have = self.current.as_ref().is_some_and(|(k, _)| *k == key);
        let waiting = self.pending.as_ref().is_some_and(|p| p.key == key);
        if !have && !waiting {
            self.current = None;
            self.generation += 1;
            let bytes: usize = data
                .rows
                .iter()
                .map(|r| match r.get(key.0) {
                    Some(crate::model::Value::Text(t)) => t.len(),
                    Some(crate::model::Value::Bytes(b)) => b.len(),
                    _ => 0,
                })
                .sum();
            if bytes <= self.sync_bytes {
                self.pending = None;
                self.current = prepare_layer(&data.rows, key, &|| false).map(|l| (key, l));
                fresh = true;
            } else {
                let cancel = Arc::new(AtomicBool::new(false));
                let (tx, rx) = channel();
                let generation = self.generation;
                // Replacing the pending request cancels its worker.
                self.pending = Some(PendingLayer {
                    key,
                    generation,
                    cancel: cancel.clone(),
                    rx,
                });
                let (data, ctx) = (data.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let layer =
                        prepare_layer(&data.rows, key, &|| cancel.load(Ordering::Relaxed));
                    if !cancel.load(Ordering::Relaxed) {
                        let _ = tx.send((generation, layer));
                        ctx.request_repaint();
                    }
                });
            }
        }
        (self.current.as_ref().map(|(_, l)| l), fresh)
    }
}

pub struct MapOutput {
    pub clicked: Option<usize>,
    /// Features drawn in this frame; the others were outside the view.
    #[cfg_attr(not(test), expect(dead_code, reason = "checked by culling tests"))]
    pub drawn: usize,
}

const FILL: Color32 = Color32::from_rgba_premultiplied(26, 58, 120, 90);
const LINE: Color32 = Color32::from_rgb(0x35, 0x74, 0xF0);
const HIGHLIGHT: Color32 = Color32::from_rgb(0xF2, 0x9D, 0x2E);

/// Draws the map and features; `selected` is the row to highlight.
pub fn show(
    ui: &mut Ui,
    view: &mut MapView,
    tiles: &mut TileCache,
    layer: &MapLayer,
    selected: Option<usize>,
    fit: bool,
    tooltip: &dyn Fn(&mut Ui, usize),
) -> MapOutput {
    let ctx = ui.ctx().clone();
    tiles.poll(&ctx);
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Map"));
    let features = &layer.features[..];
    if fit && let Some((min, max)) = layer.bounds {
        view.fit(rect, min, max);
    }

    // Pan by dragging, zoom with the wheel or pinch around the pointer.
    if resp.dragged() {
        let d = resp.drag_delta();
        let s = view.scale();
        view.center[0] -= d.x as f64 / s;
        view.center[1] -= d.y as f64 / s;
    }
    if resp.hovered() {
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = (scroll as f64 / 200.0) + (pinch as f64).log2();
        if factor != 0.0
            && let Some(p) = resp.hover_pos()
        {
            let before = view.world_pos(rect, p);
            view.zoom = (view.zoom + factor).clamp(1.0, MAX_ZOOM);
            let after = view.world_pos(rect, p);
            view.center = [
                view.center[0] + before[0] - after[0],
                view.center[1] + before[1] - after[1],
            ];
        }
    }
    if resp.double_clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let target = view.world_pos(rect, p);
        view.zoom = (view.zoom + 1.0).min(MAX_ZOOM);
        view.center = target;
    }
    view.center[1] = view.center[1].clamp(0.0, 1.0);

    let painter = ui.painter_at(rect);
    // Without base tiles the map is a dark canvas like the rest of the IDE.
    let light = !tiles.url.is_empty();
    if light {
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0xAA, 0xD3, 0xDF));
        draw_tiles(&painter, rect, view, tiles, &ctx);
    } else {
        painter.rect_filled(rect, 0.0, theme::BG_EDITOR);
        draw_grid(&painter, rect);
    }

    // Overlay, with hit testing for hover and click. Features whose bounds
    // miss the view are neither tested nor drawn.
    let pointer = resp.hover_pos();
    let mut hovered = None;
    let to = |w: [f64; 2]| view.screen_pos(rect, w);
    if let Some(p) = pointer {
        let pw = view.world_pos(rect, p);
        let world_px = 1.0 / view.scale();
        for (i, f) in features.iter().enumerate() {
            if f.screen_bounds(view, rect, HIT_TOLERANCE).contains(p)
                && feature_hit(f, p, pw, world_px, &to)
            {
                hovered = Some(i);
            }
        }
    }
    let drawn = draw_layer(&painter, layer, view, rect, selected);
    // The hovered feature on top, unless it is already drawn as selected.
    if let Some(i) = hovered
        && selected != Some(features[i].row)
    {
        let mut shapes = Vec::new();
        let clip = WorldRect::of(view, rect, STROKE_MARGIN);
        feature_shapes(&mut shapes, &features[i], &clip, &to, true);
        painter.extend(shapes);
    }

    // Zoom buttons and attribution, as on any OSM map.
    let mut zoom_by = 0.0;
    let buttons = Rect::from_min_size(rect.min + vec2(10.0, 10.0), vec2(28.0, 58.0));
    for (k, (label, delta)) in [("+", 1.0), ("−", -1.0)].into_iter().enumerate() {
        let r = Rect::from_min_size(buttons.min + vec2(0.0, k as f32 * 30.0), vec2(28.0, 28.0));
        let b = ui
            .interact(r, resp.id.with(("zoom", k)), Sense::click())
            .on_hover_text(if delta > 0.0 { "Zoom in" } else { "Zoom out" });
        b.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
        let (fill, hover, border, text) = if light {
            (Color32::WHITE, Color32::from_gray(235), Color32::from_gray(160), Color32::from_gray(40))
        } else {
            (theme::BG_PANEL, theme::BG_HOVER, theme::BORDER_INPUT, theme::TEXT)
        };
        painter.rect_filled(r, 4.0, if b.hovered() { hover } else { fill });
        painter.rect_stroke(r, 4.0, Stroke::new(1.0, border), egui::StrokeKind::Inside);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(17.0),
            text,
        );
        if b.clicked() {
            zoom_by = delta;
        }
    }
    view.zoom = (view.zoom + zoom_by).clamp(1.0, MAX_ZOOM);
    if !tiles.url.is_empty() {
        let text = "© OpenStreetMap contributors";
        let galley = painter.layout_no_wrap(
            text.into(),
            egui::FontId::proportional(11.0),
            Color32::from_gray(40),
        );
        let r = Rect::from_min_size(
            rect.right_bottom() - galley.size() - vec2(8.0, 4.0),
            galley.size() + vec2(8.0, 4.0),
        );
        painter.rect_filled(r, 2.0, Color32::from_white_alpha(200));
        painter.galley(r.min + vec2(4.0, 2.0), galley, Color32::from_gray(40));
        let link = ui
            .interact(r, resp.id.with("attribution"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if link.clicked() {
            ctx.open_url(egui::OpenUrl::new_tab(
                "https://www.openstreetmap.org/copyright",
            ));
        }
    }

    if let Some(i) = hovered {
        resp.clone()
            .on_hover_ui_at_pointer(|ui| tooltip(ui, features[i].row));
    }
    let clicked = (resp.clicked() && zoom_by == 0.0)
        .then_some(hovered)
        .flatten();
    MapOutput {
        clicked: clicked.map(|i| features[i].row),
        drawn,
    }
}

/// Faint grid on the dark canvas, so it reads as a map area and not an empty panel.
fn draw_grid(painter: &egui::Painter, rect: Rect) {
    const STEP: f32 = 48.0;
    let stroke = Stroke::new(1.0, theme::GRID_LINE);
    let mut x = rect.left() + STEP;
    while x < rect.right() {
        painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], stroke);
        x += STEP;
    }
    let mut y = rect.top() + STEP;
    while y < rect.bottom() {
        painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], stroke);
        y += STEP;
    }
}

fn draw_tiles(
    painter: &egui::Painter,
    rect: Rect,
    view: &MapView,
    tiles: &mut TileCache,
    ctx: &egui::Context,
) {
    let z = view.zoom.round().clamp(0.0, MAX_ZOOM) as u8;
    let n = 1u32 << z;
    let tl = view.world_pos(rect, rect.min);
    let br = view.world_pos(rect, rect.max);
    let range = |a: f64, b: f64| {
        (
            (a * n as f64).floor().max(0.0) as u32,
            ((b * n as f64).floor() as u32).min(n - 1),
        )
    };
    let (x0, x1) = range(tl[0], br[0]);
    let (y0, y1) = range(tl[1], br[1]);
    if tl[0] > 1.0 || br[0] < 0.0 || x1 < x0 || y1 < y0 {
        return;
    }
    // Loaded tiles just outside the view stay cached, so small pans do not reload them.
    for y in y0.saturating_sub(1)..=(y1 + 1).min(n - 1) {
        for x in x0.saturating_sub(1)..=(x1 + 1).min(n - 1) {
            if !(x0..=x1).contains(&x) || !(y0..=y1).contains(&y) {
                tiles.ready((z, x, y));
            }
        }
    }
    for y in y0..=y1 {
        for x in x0..=x1 {
            let w0 = [x as f64 / n as f64, y as f64 / n as f64];
            let w1 = [(x + 1) as f64 / n as f64, (y + 1) as f64 / n as f64];
            let r = Rect::from_min_max(view.screen_pos(rect, w0), view.screen_pos(rect, w1));
            if let Some(t) = tiles.get((z, x, y), ctx) {
                painter.image(
                    t.id(),
                    r,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                continue;
            }
            // Until the tile arrives, stretch a loaded tile from a lower zoom level.
            for up in 1..=z.min(6) {
                let (pz, px, py) = (z - up, x >> up, y >> up);
                if let Some(t) = tiles.ready((pz, px, py)) {
                    let k = (1u32 << up) as f32;
                    let (fx, fy) = ((x - (px << up)) as f32 / k, (y - (py << up)) as f32 / k);
                    let uv = Rect::from_min_size(pos2(fx, fy), vec2(1.0 / k, 1.0 / k));
                    painter.image(t.id(), r, uv, Color32::WHITE);
                    break;
                }
            }
        }
    }
}

/// Pixels around a feature where the pointer still hits it.
const HIT_TOLERANCE: f32 = 7.0;
/// Pixels a feature's outline or point marker reaches beyond its bounds.
const STROKE_MARGIN: f32 = 8.0;

/// The view in world coordinates, grown by a margin in pixels.
#[derive(Clone, Copy)]
struct WorldRect {
    min: [f64; 2],
    max: [f64; 2],
}

impl WorldRect {
    fn of(view: &MapView, rect: Rect, margin: f32) -> Self {
        Self {
            min: view.world_pos(rect, rect.min - vec2(margin, margin)),
            max: view.world_pos(rect, rect.max + vec2(margin, margin)),
        }
    }

    fn contains_box(&self, b: &WorldBox) -> bool {
        b.0[0] >= self.min[0]
            && b.0[1] >= self.min[1]
            && b.1[0] <= self.max[0]
            && b.1[1] <= self.max[1]
    }

    /// Whether the box spanned by `a` and `b` meets the view; never misses a
    /// segment that crosses it.
    fn meets(&self, a: [f64; 2], b: [f64; 2]) -> bool {
        a[0].max(b[0]) >= self.min[0]
            && a[0].min(b[0]) <= self.max[0]
            && a[1].max(b[1]) >= self.min[1]
            && a[1].min(b[1]) <= self.max[1]
    }
}

/// Draws the layer for this view. Shapes are rebuilt only when the view or the
/// selected row changes. Returns the number of features in view.
fn draw_layer(
    painter: &egui::Painter,
    layer: &MapLayer,
    view: &MapView,
    rect: Rect,
    selected: Option<usize>,
) -> usize {
    let key = ViewKey::of(view, rect, selected);
    let mut drawn = layer.drawn.borrow_mut();
    if drawn.as_ref().is_none_or(|d| d.key != key) {
        *drawn = Some(build_view(layer, view, rect, key));
    }
    let drawn = drawn.as_ref().expect("built above");
    painter.extend(drawn.base.iter().cloned());
    painter.extend(drawn.selected.iter().cloned());
    drawn.features
}

fn build_view(layer: &MapLayer, view: &MapView, rect: Rect, key: ViewKey) -> DrawnView {
    let clip = WorldRect::of(view, rect, STROKE_MARGIN);
    let visible = rect.expand(STROKE_MARGIN);
    let (mut base, mut selected, mut features) = (Vec::new(), Vec::new(), 0);
    #[cfg(test)]
    let transformed = std::cell::Cell::new(0);
    let to = |w: [f64; 2]| {
        #[cfg(test)]
        transformed.set(transformed.get() + 1);
        view.screen_pos(rect, w)
    };
    for f in &layer.features {
        if !f.screen_bounds(view, rect, 0.0).intersects(visible) {
            continue;
        }
        features += 1;
        if key.selected == Some(f.row) {
            feature_shapes(&mut selected, f, &clip, &to, true);
        } else {
            feature_shapes(&mut base, f, &clip, &to, false);
        }
    }
    DrawnView {
        key,
        base,
        selected,
        features,
        #[cfg(test)]
        transformed: transformed.get(),
    }
}

/// Shapes of one feature. Of a feature partly in view only the parts that
/// reach it are kept: line and ring segments and fill triangles outside the
/// view are skipped before their vertices are transformed.
fn feature_shapes(
    out: &mut Vec<egui::Shape>,
    f: &MapFeature,
    clip: &WorldRect,
    to: &dyn Fn([f64; 2]) -> Pos2,
    accent: bool,
) {
    let inside = clip.contains_box(&f.bounds);
    let near: Vec<bool> = if inside {
        Vec::new()
    } else {
        f.blocks.iter().map(|b| clip.meets(b.0, b.1)).collect()
    };
    let line = if accent { HIGHLIGHT } else { LINE };
    for polygon in &f.polygons {
        let fill = if accent {
            Color32::from_rgba_premultiplied(120, 70, 10, 110)
        } else {
            FILL
        };
        let end = polygon.ring_ends.last().copied().unwrap_or(polygon.start) as usize;
        let vertices = &f.coords[polygon.start as usize..end];
        let mut mesh = egui::Mesh::default();
        if inside {
            mesh.vertices.reserve_exact(vertices.len());
            for v in vertices {
                mesh.colored_vertex(to(*v), fill);
            }
            mesh.indices.clone_from(&polygon.triangles);
        } else {
            // Only triangles that reach the view, with their vertices renumbered.
            let mut index = vec![u32::MAX; vertices.len()];
            for t in polygon.triangles.as_chunks::<3>().0 {
                let [a, b, c] = t.map(|i| vertices[i as usize]);
                let lo = [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1])];
                let hi = [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1])];
                if !clip.meets(lo, hi) {
                    continue;
                }
                for &i in t {
                    let slot = &mut index[i as usize];
                    if *slot == u32::MAX {
                        *slot = mesh.vertices.len() as u32;
                        mesh.colored_vertex(to(vertices[i as usize]), fill);
                    }
                    mesh.indices.push(*slot);
                }
            }
        }
        if !mesh.indices.is_empty() {
            out.push(egui::Shape::mesh(mesh));
        }
        for r in f.rings(polygon) {
            let part = Part { pts: r, start: f.offset(r), near: &near };
            polyline_shapes(out, part, true, inside, clip, to, Stroke::new(1.5, line));
        }
    }
    let width = if accent { 3.5 } else { 2.5 };
    for l in f.lines() {
        let part = Part { pts: l, start: f.offset(l), near: &near };
        polyline_shapes(out, part, false, inside, clip, to, Stroke::new(width, line));
    }
    let radius = if accent { 6.5 } else { 5.0 };
    for p in f.points() {
        if inside || clip.meets(*p, *p) {
            out.push(egui::Shape::Circle(egui::epaint::CircleShape {
                center: to(*p),
                radius,
                fill: line,
                stroke: Stroke::new(1.5, Color32::WHITE),
            }));
        }
    }
}

/// A line or ring of a feature, with which of the feature's blocks reach the view.
struct Part<'a> {
    pts: &'a [[f64; 2]],
    /// Index of `pts[0]` in the feature's coordinates.
    start: usize,
    near: &'a [bool],
}

impl Part<'_> {
    /// Whether no segment starting at local index `i`'s block can reach the
    /// view; such a segment ends at most in the next block.
    fn far(&self, i: usize) -> bool {
        let block = (self.start + i) / BLOCK;
        !self.near.get(block).copied().unwrap_or(false)
            && !self.near.get(block + 1).copied().unwrap_or(false)
    }

    /// Local index of the first coordinate of the block after `i`'s.
    fn next_block(&self, i: usize) -> usize {
        ((self.start + i) / BLOCK + 1) * BLOCK - self.start
    }
}

/// A line or ring as one path when the feature is in view, otherwise as the
/// runs of consecutive segments that reach the view.
fn polyline_shapes(
    out: &mut Vec<egui::Shape>,
    part: Part<'_>,
    closed: bool,
    inside: bool,
    clip: &WorldRect,
    to: &dyn Fn([f64; 2]) -> Pos2,
    stroke: Stroke,
) {
    let pts = part.pts;
    if inside {
        let points = pts.iter().map(|p| to(*p)).collect();
        out.push(if closed {
            egui::Shape::closed_line(points, stroke)
        } else {
            egui::Shape::line(points, stroke)
        });
        return;
    }
    let n = pts.len();
    if n < 2 {
        return;
    }
    let mut run: Vec<Pos2> = Vec::new();
    let mut i = 0;
    while i + 1 < n {
        if part.far(i) {
            if !run.is_empty() {
                out.push(egui::Shape::line(std::mem::take(&mut run), stroke));
            }
            i = part.next_block(i);
            continue;
        }
        let (a, b) = (pts[i], pts[i + 1]);
        if clip.meets(a, b) {
            if run.is_empty() {
                run.push(to(a));
            }
            run.push(to(b));
        } else if !run.is_empty() {
            out.push(egui::Shape::line(std::mem::take(&mut run), stroke));
        }
        i += 1;
    }
    // A ring's closing segment joins the run that ends at its last point.
    if closed && clip.meets(pts[n - 1], pts[0]) {
        if run.is_empty() {
            run.push(to(pts[n - 1]));
        }
        run.push(to(pts[0]));
    }
    if !run.is_empty() {
        out.push(egui::Shape::line(run, stroke));
    }
}

/// Hit test without allocating: points and lines by screen distance,
/// polygons by the pointer's world position against the rings. Points and
/// segments far from the pointer are skipped in world space, before they are
/// transformed; `world_px` is the size of a pixel in world units.
fn feature_hit(
    f: &MapFeature,
    p: Pos2,
    pw: [f64; 2],
    world_px: f64,
    to: &dyn Fn([f64; 2]) -> Pos2,
) -> bool {
    let near = |a: [f64; 2], b: [f64; 2], tolerance: f32| {
        let r = tolerance as f64 * world_px;
        a[0].min(b[0]) - r <= pw[0]
            && a[0].max(b[0]) + r >= pw[0]
            && a[1].min(b[1]) - r <= pw[1]
            && a[1].max(b[1]) + r >= pw[1]
    };
    if f
        .points()
        .iter()
        .any(|w| near(*w, *w, HIT_TOLERANCE) && to(*w).distance(p) <= HIT_TOLERANCE)
    {
        return true;
    }
    // Blocks of coordinates far from the pointer are skipped whole.
    let blocks: Vec<bool> = f.blocks.iter().map(|b| near(b.0, b.1, 5.0)).collect();
    let near_line = |pts: &[[f64; 2]]| {
        let part = Part { pts, start: f.offset(pts), near: &blocks };
        let mut i = 0;
        while i + 1 < pts.len() {
            if part.far(i) {
                i = part.next_block(i);
                continue;
            }
            let (a, b) = (pts[i], pts[i + 1]);
            if near(a, b, 5.0) && segment_distance(p, to(a), to(b)) <= 5.0 {
                return true;
            }
            i += 1;
        }
        false
    };
    if f.lines().any(near_line) {
        return true;
    }
    f.polygons.iter().any(|polygon| {
        let mut rings = f.rings(polygon);
        rings.next().is_some_and(|outer| point_in_ring(pw, outer))
            && !rings.any(|hole| point_in_ring(pw, hole))
    })
}

fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab: Vec2 = b - a;
    let t = if ab.length_sq() == 0.0 {
        0.0
    } else {
        ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0)
    };
    p.distance(a + ab * t)
}

fn point_in_ring(p: [f64; 2], ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut j = ring.len().wrapping_sub(1);
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[j]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Muted text used in the Geo Viewer header.
pub fn muted(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).color(theme::TEXT_MUTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_world_coordinates() {
        let w = world([0.0, 0.0]);
        assert!((w[0] - 0.5).abs() < 1e-12 && (w[1] - 0.5).abs() < 1e-12);
        let w = world([-180.0, 85.051_128]);
        assert!(w[0].abs() < 1e-12 && w[1].abs() < 1e-6);
    }

    #[test]
    fn fit_zooms_to_the_box() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let mut v = MapView::default();
        let (a, b) = (world([18.0, 53.0]), world([18.1, 53.1]));
        v.fit(rect, [a[0], b[1]], [b[0], a[1]]);
        assert!((10.0..14.0).contains(&v.zoom), "{}", v.zoom);
        let c = v.screen_pos(rect, a);
        assert!(rect.contains(c));
    }

    #[test]
    fn polygon_hit_respects_holes() {
        let ring = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        assert!(point_in_ring([5.0, 5.0], &ring));
        assert!(!point_in_ring([15.0, 5.0], &ring));
        assert!(segment_distance(pos2(5.0, 3.0), pos2(0.0, 0.0), pos2(10.0, 0.0)) == 3.0);
    }

    /// One headless frame; texture uploads are discarded as there is no renderer.
    fn run_frame(ctx: &egui::Context, size: Vec2, mut ui: impl FnMut(&mut Ui)) -> egui::FullOutput {
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                ..Default::default()
            },
            &mut ui,
        );
        out.textures_delta.clear();
        out
    }

    /// Frames of a 1200x800 map until downloads settle; returns the loads made.
    fn settle(
        ctx: &egui::Context,
        view: &mut MapView,
        tiles: &mut TileCache,
        loads: &AtomicUsize,
    ) -> usize {
        let before = loads.load(Ordering::SeqCst);
        let mut idle = 0;
        while idle < 3 {
            let pending = tiles.running.load(Ordering::SeqCst)
                + tiles
                    .tiles
                    .values()
                    .filter(|t| matches!(t.state, TileState::Loading))
                    .count();
            idle = if pending == 0 { idle + 1 } else { 0 };
            run_frame(ctx, vec2(1200.0, 800.0), |ui| {
                show(ui, view, tiles, &MapLayer::default(), None, false, &|_, _| {});
            });
            std::thread::sleep(Duration::from_millis(2));
        }
        loads.load(Ordering::SeqCst) - before
    }

    #[test]
    fn tile_textures_stay_within_budget_and_keep_the_view() {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = loads.clone();
        let load: TileLoader = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Some(egui::ColorImage::filled([256, 256], Color32::GRAY))
        });
        let tile_bytes = 256 * 256 * 4;
        let mut tiles = TileCache::with_loader("fake".into(), load, 40 * tile_bytes);
        let ctx = egui::Context::default();
        let mut view = MapView::default();
        let mut peak = 0;
        // Pan across Europe and zoom in and out repeatedly.
        for step in 0..60 {
            view.zoom = 5.0 + (step % 6) as f64;
            view.center[0] = 0.5 + (step as f64 * 0.37).sin() * 0.05;
            settle(&ctx, &mut view, &mut tiles, &loads);
            let ready: usize = tiles
                .tiles
                .values()
                .filter_map(|t| match t.state {
                    TileState::Ready(_, bytes) => Some(bytes),
                    _ => None,
                })
                .sum();
            assert_eq!(ready, tiles.texture_bytes, "byte accounting");
            peak = peak.max(tiles.texture_count());
            // A settled view does not evict and reload its own tiles.
            assert_eq!(settle(&ctx, &mut view, &mut tiles, &loads), 0, "step {step}");
        }
        // 1200x800 needs up to 6x5 tiles plus a one-tile margin; nothing else grows.
        assert!(peak <= 40 + 8 * 7, "{peak} live textures");
        assert!(tiles.texture_bytes <= 40 * tile_bytes + 8 * 7 * tile_bytes);
        assert!(loads.load(Ordering::SeqCst) > 100, "the scenario evicts and reloads");
    }

    #[test]
    fn late_tile_outside_the_view_is_not_uploaded() {
        let (tx, rx) = channel::<()>();
        let rx = std::sync::Mutex::new(rx);
        let load: TileLoader = Arc::new(move |_| {
            let _ = rx.lock().unwrap().recv();
            Some(egui::ColorImage::filled([256, 256], Color32::GRAY))
        });
        let mut tiles = TileCache::with_loader("fake".into(), load, TEXTURE_BUDGET);
        let ctx = egui::Context::default();
        let frame = |tiles: &mut TileCache, view: &mut MapView| {
            run_frame(&ctx, vec2(300.0, 300.0), |ui| {
                show(ui, view, tiles, &MapLayer::default(), None, false, &|_, _| {});
            });
        };
        let mut view = MapView {
            center: [0.1, 0.1],
            zoom: 10.0,
        };
        frame(&mut tiles, &mut view);
        assert_eq!(tiles.running.load(Ordering::SeqCst), MAX_DOWNLOADS);
        // Far away before the downloads finish.
        view.center = [0.9, 0.9];
        frame(&mut tiles, &mut view);
        frame(&mut tiles, &mut view);
        for _ in 0..MAX_DOWNLOADS * 2 {
            let _ = tx.send(());
        }
        while tiles.running.load(Ordering::SeqCst) > 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(5));
        frame(&mut tiles, &mut view);
        assert_eq!(tiles.texture_count(), 0, "late tiles are dropped, not uploaded");
        assert_eq!(tiles.texture_bytes, 0);
    }

    fn square(x: f64, y: f64, r: f64) -> Vec<[f64; 2]> {
        vec![[x - r, y - r], [x + r, y - r], [x + r, y + r], [x - r, y + r], [x - r, y - r]]
    }

    fn feature(row: usize, shapes: Vec<Shape>) -> Feature {
        Feature { row, shapes }
    }

    #[test]
    fn coordinates_are_stored_once_and_hits_respect_holes() {
        let layer = prepare(
            vec![feature(
                7,
                vec![
                    Shape::Polygon(vec![square(10.0, 10.0, 2.0), square(10.0, 10.0, 1.0)]),
                    Shape::Line(vec![[0.0, 0.0], [1.0, 1.0]]),
                    Shape::Point([5.0, 5.0]),
                ],
            )],
            &|| false,
        )
        .unwrap();
        let f = &layer.features[0];
        assert_eq!(f.coords.len(), 5 + 5 + 2 + 1, "every vertex once");
        assert_eq!(f.points().len(), 1);
        assert_eq!(f.lines().count(), 1);
        assert_eq!(f.rings(&f.polygons[0]).count(), 2);
        assert!(!f.polygons[0].triangles.is_empty());

        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let mut view = MapView::default();
        let (lo, hi) = layer.bounds.unwrap();
        view.fit(rect, lo, hi);
        let to = |w: [f64; 2]| view.screen_pos(rect, w);
        let hit = |lonlat: [f64; 2]| {
            let p = to(world(lonlat));
            feature_hit(f, p, view.world_pos(rect, p), 1.0 / view.scale(), &to)
        };
        assert!(hit([11.5, 10.0]), "inside the ring");
        assert!(!hit([10.0, 10.0]), "inside the hole");
        assert!(hit([5.0, 5.0]), "on the point");
        assert!(hit([0.5, 0.5]), "on the line");
        assert!(!hit([7.0, 3.0]), "empty map");
    }

    fn view_shapes(layer: &MapLayer, view: &MapView, selected: Option<usize>) -> (usize, usize, usize) {
        let ctx = egui::Context::default();
        let mut tiles = TileCache::new(String::new(), std::env::temp_dir());
        let mut view = *view;
        run_frame(&ctx, vec2(800.0, 600.0), |ui| {
            show(ui, &mut view, &mut tiles, layer, selected, false, &|_, _| {});
        });
        let drawn = layer.drawn.borrow();
        let drawn = drawn.as_ref().unwrap();
        (drawn.base.len(), drawn.selected.len(), drawn.transformed)
    }

    #[test]
    fn selected_features_are_drawn_once_on_top() {
        let layer = prepare(
            vec![
                feature(0, vec![Shape::Polygon(vec![square(10.0, 10.0, 1.0)])]),
                feature(1, vec![Shape::Polygon(vec![square(12.0, 10.0, 1.0)])]),
            ],
            &|| false,
        )
        .unwrap();
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let mut view = MapView::default();
        let (lo, hi) = layer.bounds.unwrap();
        view.fit(rect, lo, hi);
        // One fill and one outline per feature, the selected one only on top.
        assert_eq!(view_shapes(&layer, &view, None), (4, 0, 20));
        assert_eq!(view_shapes(&layer, &view, Some(1)), (2, 2, 20));
    }

    #[test]
    fn shapes_are_kept_while_only_the_pointer_moves() {
        let layer = prepare(
            vec![feature(0, vec![Shape::Polygon(vec![square(10.0, 10.0, 1.0)])])],
            &|| false,
        )
        .unwrap();
        let ctx = egui::Context::default();
        let mut tiles = TileCache::new(String::new(), std::env::temp_dir());
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let mut view = MapView::default();
        let (lo, hi) = layer.bounds.unwrap();
        view.fit(rect, lo, hi);
        let built = |layer: &MapLayer| layer.drawn.borrow().as_ref().map(|d| d.base.as_ptr());
        let mut first = None;
        for x in [100.0, 400.0, 401.0, 402.0] {
            let input = egui::RawInput {
                screen_rect: Some(rect),
                events: vec![egui::Event::PointerMoved(pos2(x, 300.0))],
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                show(ui, &mut view, &mut tiles, &layer, None, false, &|_, _| {});
            });
            out.textures_delta.clear();
            let now = built(&layer);
            assert!(first.is_none_or(|first| first == now), "hovering rebuilt the layer");
            first = Some(now);
        }
        view.zoom += 0.5;
        run_frame(&ctx, rect.size(), |ui| {
            show(ui, &mut view, &mut tiles, &layer, None, false, &|_, _| {});
        });
        assert_ne!(Some(built(&layer)), first, "a new view is built again");
    }

    #[test]
    fn long_line_crossing_the_view_transforms_only_the_part_in_view() {
        let line: Vec<[f64; 2]> = (0..200_000)
            .map(|i| [-179.0 + i as f64 * 358.0 / 2e5, if i % 2 == 0 { -1.0 } else { 1.0 }])
            .collect();
        let layer = prepare(vec![feature(0, vec![Shape::Line(line)])], &|| false).unwrap();
        let view = MapView {
            center: world([0.0, 0.0]),
            zoom: 9.0,
        };
        let (base, _, transformed) = view_shapes(&layer, &view, None);
        assert_eq!(base, 1, "the part in view is one path");
        assert!(transformed < 2_000, "{transformed} of 200000 vertices transformed");
        // Picking still finds the line under the pointer.
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let f = &layer.features[0];
        let to = |w: [f64; 2]| view.screen_pos(rect, w);
        let p = to(f.coords[100_000]);
        assert!(feature_hit(f, p, view.world_pos(rect, p), 1.0 / view.scale(), &to));
        let away = p + vec2(0.0, 200.0);
        assert!(!feature_hit(f, away, view.world_pos(rect, away), 1.0 / view.scale(), &to));
    }

    #[test]
    fn polygon_partly_in_view_keeps_holes_and_only_nearby_triangles() {
        // A large square with a hole, viewed close to its hole.
        let layer = prepare(
            vec![feature(
                0,
                vec![Shape::Polygon(vec![square(0.0, 0.0, 40.0), square(0.0, 0.0, 1.0)])],
            )],
            &|| false,
        )
        .unwrap();
        let view = MapView {
            center: world([0.0, 0.0]),
            zoom: 8.0,
        };
        let (base, _, _) = view_shapes(&layer, &view, None);
        let drawn = layer.drawn.borrow();
        let shapes = &drawn.as_ref().unwrap().base;
        let mesh = shapes
            .iter()
            .find_map(|s| match s {
                egui::Shape::Mesh(mesh) => Some(mesh.clone()),
                _ => None,
            })
            .expect("fill");
        assert!(base >= 2, "fill and the hole outline");
        // Every kept triangle reaches the view; none covers the hole's center.
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let hole_center = rect.center();
        for t in mesh.indices.as_chunks::<3>().0 {
            let [a, b, c] = t.map(|i| mesh.vertices[i as usize].pos);
            let area = |p: Pos2, q: Pos2, r: Pos2| (q - p).x * (r - p).y - (q - p).y * (r - p).x;
            let (d1, d2, d3) = (area(hole_center, a, b), area(hole_center, b, c), area(hole_center, c, a));
            let inside = (d1 > 0.0 && d2 > 0.0 && d3 > 0.0) || (d1 < 0.0 && d2 < 0.0 && d3 < 0.0);
            assert!(!inside, "the hole stays empty");
        }
    }

    #[test]
    fn only_features_in_view_are_drawn() {
        let features = (0..400)
            .map(|i| {
                let (x, y) = (-170.0 + (i % 20) as f64 * 17.0, -60.0 + (i / 20) as f64 * 6.0);
                feature(i, vec![Shape::Polygon(vec![square(x, y, 0.5)])])
            })
            .collect();
        let layer = prepare(features, &|| false).unwrap();
        let ctx = egui::Context::default();
        let mut tiles = TileCache::new(String::new(), std::env::temp_dir());
        let mut drawn = |view: &mut MapView| {
            let mut drawn = 0;
            run_frame(&ctx, vec2(800.0, 600.0), |ui| {
                drawn = show(ui, view, &mut tiles, &layer, None, false, &|_, _| {}).drawn;
            });
            drawn
        };
        let mut world_view = MapView {
            center: [0.5, 0.5],
            zoom: 1.0,
        };
        assert_eq!(drawn(&mut world_view), 400);
        let mut close = MapView {
            center: world([-170.0, -60.0]),
            zoom: 9.0,
        };
        assert_eq!(drawn(&mut close), 1);
        let mut empty = MapView {
            center: world([-178.0, 80.0]),
            zoom: 9.0,
        };
        assert_eq!(drawn(&mut empty), 0);
    }

    fn wkt_rows(columns: &[usize]) -> Arc<crate::model::QueryResult> {
        let max = columns.iter().copied().max().unwrap_or(0);
        let rows = (0..max)
            .map(|i| {
                columns
                    .iter()
                    .map(|&n| {
                        if i < n {
                            crate::model::Value::Text(format!("POINT({} {})", i % 80, i % 80))
                        } else {
                            crate::model::Value::Null
                        }
                    })
                    .collect()
            })
            .collect();
        Arc::new(crate::model::QueryResult {
            rows,
            ..Default::default()
        })
    }

    fn wait_for(
        loader: &mut LayerLoader,
        data: &Arc<crate::model::QueryResult>,
        key: LayerKey,
        ctx: &egui::Context,
    ) -> usize {
        for _ in 0..60_000 {
            if let (Some(layer), _) = loader.get(data, key, ctx) {
                return layer.map.len();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("layer was not prepared");
    }

    #[test]
    fn newer_layer_request_wins_over_a_pending_one() {
        let data = wkt_rows(&[300, 200]);
        let ctx = egui::Context::default();
        let mut loader = LayerLoader {
            sync_bytes: 0,
            ..Default::default()
        };
        let (layer, _) = loader.get(&data, (0, None), &ctx);
        assert!(layer.is_none(), "large columns are prepared on a worker");
        let first = loader.pending.as_ref().unwrap().cancel.clone();
        assert!(loader.get(&data, (1, None), &ctx).0.is_none());
        assert!(first.load(Ordering::Relaxed), "the older request is cancelled");
        assert_eq!(wait_for(&mut loader, &data, (1, None), &ctx), 200);
        // A finished older generation is never published for the newer key.
        let (tx, rx) = channel();
        tx.send((0, Some(prepare_layer(&data.rows, (0, None), &|| false).unwrap())))
            .unwrap();
        loader.current = None;
        loader.generation += 1;
        loader.pending = Some(PendingLayer {
            key: (1, None),
            generation: loader.generation,
            cancel: Arc::default(),
            rx,
        });
        let (layer, fresh) = loader.get(&data, (1, None), &ctx);
        assert!(layer.is_none() && !fresh, "stale generation is discarded");
        assert_eq!(wait_for(&mut loader, &data, (1, None), &ctx), 200);

        let small = wkt_rows(&[3]);
        let mut loader = LayerLoader::default();
        let (layer, fresh) = loader.get(&small, (0, None), &ctx);
        assert_eq!(layer.map(|l| l.map.len()), Some(3), "small columns are prepared at once");
        assert!(fresh);
        assert!(!loader.get(&small, (0, None), &ctx).1, "and only once");
    }

    #[test]
    fn reset_cancels_preparation() {
        let data = wkt_rows(&[500]);
        let ctx = egui::Context::default();
        let mut loader = LayerLoader {
            sync_bytes: 0,
            ..Default::default()
        };
        let _ = loader.get(&data, (0, None), &ctx);
        let cancel = loader.pending.as_ref().unwrap().cancel.clone();
        loader.reset();
        assert!(cancel.load(Ordering::Relaxed));
        assert!(loader.current.is_none() && loader.pending.is_none());
    }

    /// Run with `cargo test --release --lib benchmark_geo -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn benchmark_geo() {
        // 40 polygons with a hole, 25k vertices per ring, as WKT.
        let ring = |cx: f64, cy: f64, r: f64, n: usize| {
            let mut s = String::from("(");
            for i in 0..=n {
                let a = (i % n) as f64 / n as f64 * std::f64::consts::TAU;
                s += &format!("{} {},", cx + r * a.cos(), cy + r * a.sin());
            }
            s.pop();
            s + ")"
        };
        let rows: Vec<Vec<crate::model::Value>> = (0..40)
            .map(|i| {
                let (cx, cy) = (-170.0 + i as f64 * 8.0, (i % 10) as f64 * 5.0);
                vec![crate::model::Value::Text(format!(
                    "SRID=4326;POLYGON({},{})",
                    ring(cx, cy, 2.0, 25_000),
                    ring(cx, cy, 1.0, 25_000)
                ))]
            })
            .collect();
        let data = Arc::new(crate::model::QueryResult {
            rows,
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let mut loader = LayerLoader::default();
        let t = std::time::Instant::now();
        let _ = loader.get(&data, (0, None), &ctx);
        eprintln!("UI thread time to start preparation: {:?}", t.elapsed());
        wait_for(&mut loader, &data, (0, None), &ctx);
        eprintln!("preparation on the worker: {:?}", t.elapsed());
        let layer = &loader.current.as_ref().unwrap().1.map;
        let mut tiles = TileCache::new(String::new(), std::env::temp_dir());
        for (name, mut view) in [
            ("zoomed in, 1-2 visible", MapView { center: world([0.0, 2.0]), zoom: 7.0 }),
            ("all visible", MapView { center: [0.5, 0.5], zoom: 2.0 }),
        ] {
            let mut frame = |n: u32| {
                let t = std::time::Instant::now();
                for _ in 0..n {
                    let out = run_frame(&ctx, vec2(1200.0, 800.0), |ui| {
                        show(ui, &mut view, &mut tiles, layer, None, false, &|_, _| {});
                    });
                    let _ = ctx.tessellate(out.shapes, out.pixels_per_point);
                }
                t.elapsed() / n
            };
            frame(2);
            eprintln!("frame ({name}): {:?}", frame(20));
        }
    }

    fn percentiles(mut times: Vec<f64>) -> String {
        times.sort_by(f64::total_cmp);
        let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
        format!("p50={:.3} p95={:.3} p99={:.3} ms", at(0.5), at(0.95), at(0.99))
    }

    /// Run with `cargo test --release --lib benchmark_map_interaction -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn benchmark_map_interaction() {
        let scattered: Vec<Feature> = (0..5000)
            .map(|i| {
                let (x, y) = (-175.0 + (i % 100) as f64 * 3.5, -70.0 + (i / 100) as f64 * 2.8);
                feature(i, vec![Shape::Polygon(vec![square(x, y, 0.4)])])
            })
            .collect();
        // One line of 2 million vertices zigzagging across the whole world.
        let huge = vec![feature(
            0,
            vec![Shape::Line(
                (0..2_000_000)
                    .map(|i| [-179.0 + i as f64 * 358.0 / 2e6, if i % 2 == 0 { -1.0 } else { 1.0 }])
                    .collect(),
            )],
        )];
        let size = vec2(1200.0, 800.0);
        let center = Rect::from_min_size(Pos2::ZERO, size).center();
        let t = std::time::Instant::now();
        let scenarios = [
            (
                "5000 features, few visible",
                prepare(scattered, &|| false).unwrap(),
                MapView { center: world([0.0, 0.0]), zoom: 8.0 },
                Some(2550),
            ),
            (
                "one polygon with a 50k-vertex ring crossing the view",
                prepare(
                    vec![feature(
                        0,
                        vec![Shape::Polygon(vec![(0..50_000)
                            .map(|i| {
                                let a = i as f64 / 5e4 * std::f64::consts::TAU;
                                let r = if i % 2 == 0 { 60.0 } else { 59.9 };
                                [r * a.cos(), r * a.sin()]
                            })
                            .collect()])],
                    )],
                    &|| false,
                )
                .unwrap(),
                MapView { center: world([60.0, 0.0]), zoom: 9.0 },
                Some(0),
            ),
            (
                "one 2M-vertex line crossing the view",
                prepare(huge, &|| false).unwrap(),
                MapView { center: world([0.0, 0.0]), zoom: 9.0 },
                Some(0),
            ),
        ];
        eprintln!("prepared in {:?}", t.elapsed());
        for (name, layer, view, selected) in scenarios {
            let ctx = egui::Context::default();
            let mut tiles = TileCache::new(String::new(), std::env::temp_dir());
            for phase in ["idle", "hover", "pan"] {
                let mut view = view;
                let (mut ui_ms, mut tess_ms) = (Vec::new(), Vec::new());
                for frame in 0..70 {
                    let mut input = egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                        ..Default::default()
                    };
                    if phase != "idle" {
                        input.events.push(egui::Event::PointerMoved(
                            center + vec2((frame % 40) as f32 * 5.0, (frame % 7) as f32),
                        ));
                    }
                    if phase == "pan" {
                        view.center[0] += 1e-6;
                    }
                    let t = std::time::Instant::now();
                    let mut out = ctx.run_ui(input, |ui| {
                        show(ui, &mut view, &mut tiles, &layer, selected, false, &|_, _| {});
                    });
                    let ui_time = t.elapsed().as_secs_f64() * 1000.0;
                    out.textures_delta.clear();
                    let t = std::time::Instant::now();
                    let _ = ctx.tessellate(out.shapes, out.pixels_per_point);
                    let tess_time = t.elapsed().as_secs_f64() * 1000.0;
                    if frame >= 10 {
                        ui_ms.push(ui_time);
                        tess_ms.push(tess_time);
                    }
                }
                eprintln!(
                    "{name}, {phase}: UI {} | tessellation {}",
                    percentiles(ui_ms),
                    percentiles(tess_ms)
                );
            }
        }
    }
}
