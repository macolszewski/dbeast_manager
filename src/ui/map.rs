//! Raster slippy map with OpenStreetMap tiles and a geometry overlay, used by
//! the Geo Viewer. Coordinates are WGS84 longitude/latitude, shown in Web
//! Mercator like any web map.
//!
//! Tiles follow the OpenStreetMap tile usage policy: an identifying
//! User-Agent, a disk cache, few parallel downloads and visible attribution.

use super::theme;
use crate::geo::{Layer, Shape};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

enum Tile {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

/// Downloaded tiles as textures, shared by all Geo Viewers.
pub struct TileCache {
    url: String,
    dir: PathBuf,
    tiles: HashMap<TileKey, Tile>,
    tx: Sender<(TileKey, Option<egui::ColorImage>)>,
    rx: Receiver<(TileKey, Option<egui::ColorImage>)>,
    running: Arc<AtomicUsize>,
}

impl TileCache {
    /// `url` with `{z}`, `{x}` and `{y}`; empty disables the base map.
    pub fn new(url: String, dir: PathBuf) -> Self {
        let (tx, rx) = channel();
        Self {
            url,
            dir,
            tiles: HashMap::new(),
            tx,
            rx,
            running: Arc::default(),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        for (key, image) in self.rx.try_iter() {
            let tile = match image {
                Some(img) => Tile::Ready(ctx.load_texture(
                    format!("tile-{key:?}"),
                    img,
                    egui::TextureOptions::LINEAR,
                )),
                None => Tile::Failed,
            };
            self.tiles.insert(key, tile);
        }
    }

    fn get(&mut self, key: TileKey, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if self.url.is_empty() {
            return None;
        }
        if !self.tiles.contains_key(&key) && self.running.load(Ordering::Relaxed) < MAX_DOWNLOADS {
            self.tiles.insert(key, Tile::Loading);
            self.running.fetch_add(1, Ordering::Relaxed);
            let (url, path, tx, running, ctx) = (
                self.url
                    .replace("{z}", &key.0.to_string())
                    .replace("{x}", &key.1.to_string())
                    .replace("{y}", &key.2.to_string()),
                self.dir.join(format!("{}/{}/{}.png", key.0, key.1, key.2)),
                self.tx.clone(),
                self.running.clone(),
                ctx.clone(),
            );
            std::thread::spawn(move || {
                let image = load_tile(&url, &path);
                running.fetch_sub(1, Ordering::Relaxed);
                let _ = tx.send((key, image));
                ctx.request_repaint();
            });
        }
        match self.tiles.get(&key) {
            Some(Tile::Ready(t)) => Some(t),
            _ => None,
        }
    }

    fn ready(&self, key: TileKey) -> Option<&egui::TextureHandle> {
        match self.tiles.get(&key) {
            Some(Tile::Ready(t)) => Some(t),
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

/// Vertices of all rings, triangle indices into them, and the rings for outlines and hit tests.
type PreparedPolygon = (Vec<[f64; 2]>, Vec<u32>, Vec<Vec<[f64; 2]>>);

/// A feature prepared for drawing: world coordinates and polygon triangles.
pub struct MapFeature {
    pub row: usize,
    points: Vec<[f64; 2]>,
    lines: Vec<Vec<[f64; 2]>>,
    polygons: Vec<PreparedPolygon>,
}

/// Converts a layer to world coordinates once, so drawing a frame only transforms vertices.
pub fn prepare(layer: &Layer) -> Vec<MapFeature> {
    let mut earcut = earcut::Earcut::new();
    layer
        .features
        .iter()
        .map(|f| {
            let mut out = MapFeature {
                row: f.row,
                points: Vec::new(),
                lines: Vec::new(),
                polygons: Vec::new(),
            };
            for s in &f.shapes {
                match s {
                    Shape::Point(p) => out.points.push(world(*p)),
                    Shape::Line(l) => out.lines.push(l.iter().map(|p| world(*p)).collect()),
                    Shape::Polygon(rings) => {
                        let rings: Vec<Vec<[f64; 2]>> = rings
                            .iter()
                            .map(|r| r.iter().map(|p| world(*p)).collect())
                            .collect();
                        let mut holes = Vec::new();
                        let mut vertices = Vec::new();
                        for (i, r) in rings.iter().enumerate() {
                            if i > 0 {
                                holes.push(vertices.len() as u32);
                            }
                            vertices.extend(r.iter().copied());
                        }
                        let mut triangles: Vec<u32> = Vec::new();
                        earcut.earcut(vertices.iter().copied(), &holes, &mut triangles);
                        out.polygons.push((vertices, triangles, rings));
                    }
                }
            }
            out
        })
        .collect()
}

/// World bounding box of all features.
pub fn bounds(features: &[MapFeature]) -> Option<([f64; 2], [f64; 2])> {
    let mut all = features.iter().flat_map(|f| {
        f.points
            .iter()
            .chain(f.lines.iter().flatten())
            .chain(f.polygons.iter().flat_map(|p| p.0.iter()))
    });
    let first = *all.next()?;
    Some(all.fold((first, first), |(lo, hi), p| {
        (
            [lo[0].min(p[0]), lo[1].min(p[1])],
            [hi[0].max(p[0]), hi[1].max(p[1])],
        )
    }))
}

pub struct MapOutput {
    pub clicked: Option<usize>,
}

const FILL: Color32 = Color32::from_rgba_premultiplied(26, 58, 120, 90);
const LINE: Color32 = Color32::from_rgb(0x35, 0x74, 0xF0);
const HIGHLIGHT: Color32 = Color32::from_rgb(0xF2, 0x9D, 0x2E);

/// Draws the map and features; `selected` is the row to highlight.
pub fn show(
    ui: &mut Ui,
    view: &mut MapView,
    tiles: &mut TileCache,
    features: &[MapFeature],
    selected: Option<usize>,
    fit: bool,
    tooltip: &dyn Fn(&mut Ui, usize),
) -> MapOutput {
    let ctx = ui.ctx().clone();
    tiles.poll(&ctx);
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Map"));
    if fit && let Some((min, max)) = bounds(features) {
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

    // Overlay, with hit testing for hover and click.
    let pointer = resp.hover_pos();
    let mut hovered = None;
    let to = |w: [f64; 2]| view.screen_pos(rect, w);
    for (i, f) in features.iter().enumerate() {
        let hit = pointer.is_some_and(|p| feature_hit(f, p, &to));
        if hit {
            hovered = Some(i);
        }
    }
    for (i, f) in features.iter().enumerate() {
        let accent = hovered == Some(i) || selected == Some(f.row);
        draw_feature(&painter, f, &to, accent);
    }
    // Hovered and selected features on top.
    for (i, f) in features.iter().enumerate() {
        if hovered == Some(i) || selected == Some(f.row) {
            draw_feature(&painter, f, &to, true);
        }
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

fn draw_feature(
    painter: &egui::Painter,
    f: &MapFeature,
    to: &dyn Fn([f64; 2]) -> Pos2,
    accent: bool,
) {
    let line = if accent { HIGHLIGHT } else { LINE };
    for (vertices, triangles, rings) in &f.polygons {
        let mut mesh = egui::Mesh::default();
        let fill = if accent {
            Color32::from_rgba_premultiplied(120, 70, 10, 110)
        } else {
            FILL
        };
        for v in vertices {
            mesh.colored_vertex(to(*v), fill);
        }
        mesh.indices = triangles.clone();
        painter.add(egui::Shape::mesh(mesh));
        for r in rings {
            painter.add(egui::Shape::closed_line(
                r.iter().map(|p| to(*p)).collect(),
                Stroke::new(1.5, line),
            ));
        }
    }
    for l in &f.lines {
        painter.add(egui::Shape::line(
            l.iter().map(|p| to(*p)).collect(),
            Stroke::new(if accent { 3.5 } else { 2.5 }, line),
        ));
    }
    for p in &f.points {
        let c = to(*p);
        painter.circle(
            c,
            if accent { 6.5 } else { 5.0 },
            line,
            Stroke::new(1.5, Color32::WHITE),
        );
    }
}

fn feature_hit(f: &MapFeature, p: Pos2, to: &dyn Fn([f64; 2]) -> Pos2) -> bool {
    if f.points.iter().any(|w| to(*w).distance(p) <= 7.0) {
        return true;
    }
    let near_line = |pts: &[[f64; 2]]| {
        pts.windows(2)
            .any(|s| segment_distance(p, to(s[0]), to(s[1])) <= 5.0)
    };
    if f.lines.iter().any(|l| near_line(l)) {
        return true;
    }
    f.polygons.iter().any(|(_, _, rings)| {
        let inside =
            |r: &Vec<[f64; 2]>| point_in_ring(p, &r.iter().map(|w| to(*w)).collect::<Vec<_>>());
        rings.first().is_some_and(inside) && !rings.iter().skip(1).any(inside)
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

fn point_in_ring(p: Pos2, ring: &[Pos2]) -> bool {
    let mut inside = false;
    let mut j = ring.len().wrapping_sub(1);
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
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
        let ring = [
            pos2(0.0, 0.0),
            pos2(10.0, 0.0),
            pos2(10.0, 10.0),
            pos2(0.0, 10.0),
        ];
        assert!(point_in_ring(pos2(5.0, 5.0), &ring));
        assert!(!point_in_ring(pos2(15.0, 5.0), &ring));
        assert!(segment_distance(pos2(5.0, 3.0), pos2(0.0, 0.0), pos2(10.0, 0.0)) == 3.0);
    }
}
