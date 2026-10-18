//! Geometry values for the Geo Viewer: decoding from the formats databases
//! return, the spatial reference (SRID) they carry, and reprojection to WGS84.
//!
//! Supported encodings: WKB and PostGIS EWKB (binary or hex text), the MySQL
//! internal format (SRID + WKB), GeoPackage and SpatiaLite blobs, WKT and
//! EWKT (`SRID=2180;POINT(...)`), and GeoJSON.

use crate::model::{Column, Value};
use std::collections::HashMap;

pub type Xy = [f64; 2];

/// Drawable parts; multi-geometries and collections are flattened.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Point(Xy),
    Line(Vec<Xy>),
    /// Exterior ring first, then holes.
    Polygon(Vec<Vec<Xy>>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub srid: Option<u32>,
    pub shapes: Vec<Shape>,
}

impl Geometry {
    fn coords(&self) -> impl Iterator<Item = &Xy> {
        self.shapes
            .iter()
            .flat_map(|s| -> Box<dyn Iterator<Item = &Xy>> {
                match s {
                    Shape::Point(p) => Box::new(std::iter::once(p)),
                    Shape::Line(l) => Box::new(l.iter()),
                    Shape::Polygon(rings) => Box::new(rings.iter().flatten()),
                }
            })
    }

    /// Transforms every coordinate in place; false when one cannot be transformed.
    fn project(&mut self, f: &mut dyn FnMut(Xy) -> Option<Xy>) -> bool {
        let mut apply = |p: &mut Xy| match f(*p) {
            Some(q) => {
                *p = q;
                true
            }
            None => false,
        };
        self.shapes.iter_mut().all(|s| match s {
            Shape::Point(p) => apply(p),
            Shape::Line(l) => l.iter_mut().all(&mut apply),
            Shape::Polygon(rs) => rs.iter_mut().flatten().all(&mut apply),
        })
    }
}

/// Decodes a cell value; `None` when it holds no recognizable geometry.
pub fn parse_value(v: &Value) -> Option<Geometry> {
    match v {
        Value::Bytes(b) => parse_blob(b),
        Value::Text(t) => parse_text(t),
        _ => None,
    }
}

fn parse_blob(b: &[u8]) -> Option<Geometry> {
    // GeoPackage: "GP", version, flags, srs_id, optional envelope, WKB.
    if b.len() > 8 && &b[..2] == b"GP" {
        let flags = b[3];
        let little = flags & 1 == 1;
        let srid = read_u32(&b[4..8], little)?;
        let envelope = match (flags >> 1) & 0b111 {
            0 => 0,
            1 => 32,
            2 | 3 => 48,
            4 => 64,
            _ => return None,
        };
        let mut g = parse_wkb(b.get(8 + envelope..)?)?;
        g.srid = (srid > 0).then_some(srid).or(g.srid);
        return Some(g);
    }
    // SpatiaLite: 0x00, byte order, srid, MBR (32 bytes), 0x7C, class + WKB body, 0xFE.
    if b.len() > 44
        && b[0] == 0
        && (b[1] == 0 || b[1] == 1)
        && b[38] == 0x7C
        && b[b.len() - 1] == 0xFE
    {
        let little = b[1] == 1;
        let srid = read_u32(&b[2..6], little)?;
        let mut wkb = vec![b[1]];
        wkb.extend_from_slice(&b[39..b.len() - 1]);
        let mut g = parse_wkb(&wkb)?;
        g.srid = Some(srid).filter(|s| *s > 0);
        return Some(g);
    }
    // MySQL internal format: SRID (little endian) followed by WKB.
    if b.len() > 9 && (b[4] == 0 || b[4] == 1) && !(b[0] <= 1 && wkb_type_ok(&b[..5])) {
        let srid = u32::from_le_bytes(b[..4].try_into().ok()?);
        if let Some(mut g) = parse_wkb(&b[4..]) {
            g.srid = (srid > 0).then_some(srid);
            return Some(g);
        }
    }
    parse_wkb(b)
}

/// Whether the first 5 bytes look like a WKB header (byte order + known type).
fn wkb_type_ok(b: &[u8]) -> bool {
    let little = b[0] == 1;
    read_u32(&b[1..5], little).is_some_and(|t| (1..=7).contains(&((t & 0x0FFF_FFFF) % 1000)))
}

fn read_u32(b: &[u8], little: bool) -> Option<u32> {
    let a: [u8; 4] = b.get(..4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(a)
    } else {
        u32::from_be_bytes(a)
    })
}

/// WKB, ISO WKB with Z/M, or PostGIS EWKB (SRID and dimension flags).
pub fn parse_wkb(b: &[u8]) -> Option<Geometry> {
    let mut r = WkbReader { b, pos: 0 };
    let mut srid = None;
    let shapes = r.geometry(&mut srid, 0)?;
    (r.pos == b.len()).then_some(Geometry { srid, shapes })
}

struct WkbReader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl WkbReader<'_> {
    fn u32(&mut self, little: bool) -> Option<u32> {
        let v = read_u32(self.b.get(self.pos..)?, little)?;
        self.pos += 4;
        Some(v)
    }

    fn f64(&mut self, little: bool) -> Option<f64> {
        let a: [u8; 8] = self.b.get(self.pos..self.pos + 8)?.try_into().ok()?;
        self.pos += 8;
        Some(if little {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        })
    }

    fn point(&mut self, little: bool, dims: usize) -> Option<Xy> {
        let x = self.f64(little)?;
        let y = self.f64(little)?;
        for _ in 2..dims {
            self.f64(little)?;
        }
        Some([x, y])
    }

    fn points(&mut self, little: bool, dims: usize) -> Option<Vec<Xy>> {
        let n = self.u32(little)? as usize;
        // A count larger than the remaining bytes means this is not WKB.
        if n > self.b.len() / 16 + 1 {
            return None;
        }
        (0..n).map(|_| self.point(little, dims)).collect()
    }

    fn geometry(&mut self, srid: &mut Option<u32>, depth: usize) -> Option<Vec<Shape>> {
        if depth > 32 {
            return None;
        }
        let little = match *self.b.get(self.pos)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        self.pos += 1;
        let raw = self.u32(little)?;
        let (mut z, mut m) = (raw & 0x8000_0000 != 0, raw & 0x4000_0000 != 0);
        if raw & 0x2000_0000 != 0 {
            *srid = Some(self.u32(little)?);
        }
        let iso = raw & 0x0FFF_FFFF;
        let kind = iso % 1000;
        match iso / 1000 {
            0 => {}
            1 => z = true,
            2 => m = true,
            3 => (z, m) = (true, true),
            _ => return None,
        }
        let dims = 2 + z as usize + m as usize;
        Some(match kind {
            1 => {
                let p = self.point(little, dims)?;
                // POINT EMPTY is written as NaN coordinates.
                if p[0].is_nan() {
                    Vec::new()
                } else {
                    vec![Shape::Point(p)]
                }
            }
            2 => vec![Shape::Line(self.points(little, dims)?)],
            3 => {
                let rings = self.u32(little)?;
                vec![Shape::Polygon(
                    (0..rings)
                        .map(|_| self.points(little, dims))
                        .collect::<Option<_>>()?,
                )]
            }
            4..=7 => {
                let n = self.u32(little)?;
                let mut out = Vec::new();
                for _ in 0..n {
                    out.extend(self.geometry(srid, depth + 1)?);
                }
                out
            }
            _ => return None,
        })
    }
}

fn parse_text(t: &str) -> Option<Geometry> {
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('{') {
        return parse_geojson(&serde_json::from_str(t).ok()?);
    }
    // Hex-encoded (E)WKB, as PostGIS prints geometry::text.
    if t.len() >= 18
        && t.len().is_multiple_of(2)
        && (t.starts_with("00") || t.starts_with("01"))
        && t.bytes().all(|c| c.is_ascii_hexdigit())
    {
        let bytes: Option<Vec<u8>> = (0..t.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok())
            .collect();
        return parse_wkb(&bytes?);
    }
    parse_wkt(t)
}

/// WKT and EWKT, including Z/M/ZM and EMPTY.
pub fn parse_wkt(t: &str) -> Option<Geometry> {
    let (srid, body) = match t.split_once(';') {
        Some((head, rest)) if head.trim().to_ascii_uppercase().starts_with("SRID=") => {
            (head.trim()[5..].trim().parse().ok(), rest)
        }
        _ => (None, t),
    };
    let mut p = WktParser {
        s: body.as_bytes(),
        pos: 0,
    };
    let shapes = p.geometry()?;
    p.skip_ws();
    (p.pos == p.s.len()).then_some(Geometry { srid, shapes })
}

struct WktParser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl WktParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn word(&mut self) -> String {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.pos]).to_ascii_uppercase()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.s.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Option<f64> {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.s.len()
            && matches!(
                self.s[self.pos],
                b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E'
            )
        {
            self.pos += 1;
        }
        std::str::from_utf8(&self.s[start..self.pos])
            .ok()?
            .parse()
            .ok()
    }

    /// `x y [z [m]]`
    fn point(&mut self) -> Option<Xy> {
        let x = self.number()?;
        let y = self.number()?;
        loop {
            self.skip_ws();
            match self.s.get(self.pos) {
                Some(b',' | b')') => return Some([x, y]),
                _ => {
                    self.number()?;
                }
            }
        }
    }

    /// `(p, p, ...)`; points of MULTIPOINT may also be wrapped in parentheses.
    fn points(&mut self) -> Option<Vec<Xy>> {
        if !self.eat(b'(') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let p = if self.eat(b'(') {
                let p = self.point()?;
                self.eat(b')').then_some(p)?
            } else {
                self.point()?
            };
            out.push(p);
            if !self.eat(b',') {
                break;
            }
        }
        self.eat(b')').then_some(out)
    }

    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        if !self.eat(b'(') {
            return None;
        }
        let mut out = vec![item(self)?];
        while self.eat(b',') {
            out.push(item(self)?);
        }
        self.eat(b')').then_some(out)
    }

    fn geometry(&mut self) -> Option<Vec<Shape>> {
        let kind = self.word();
        // Dimension markers: POINT Z, POINT ZM, POINTM.
        let save = self.pos;
        let dims = self.word();
        if !matches!(dims.as_str(), "Z" | "M" | "ZM") {
            self.pos = save;
        }
        let save = self.pos;
        if self.word() == "EMPTY" {
            return Some(Vec::new());
        }
        self.pos = save;
        let kind = kind.trim_end_matches(['Z', 'M']).to_string();
        Some(match kind.as_str() {
            "POINT" => {
                if !self.eat(b'(') {
                    return None;
                }
                let p = self.point()?;
                self.eat(b')').then_some(vec![Shape::Point(p)])?
            }
            "LINESTRING" => vec![Shape::Line(self.points()?)],
            "POLYGON" => vec![Shape::Polygon(self.list(|p| p.points())?)],
            "MULTIPOINT" => self.points()?.into_iter().map(Shape::Point).collect(),
            "MULTILINESTRING" => self
                .list(|p| p.points())?
                .into_iter()
                .map(Shape::Line)
                .collect(),
            "MULTIPOLYGON" => self
                .list(|p| p.list(|q| q.points()))?
                .into_iter()
                .map(Shape::Polygon)
                .collect(),
            "GEOMETRYCOLLECTION" => self.list(|p| p.geometry())?.into_iter().flatten().collect(),
            _ => return None,
        })
    }
}

/// GeoJSON geometry, Feature or FeatureCollection; GeoJSON is always WGS84.
pub fn parse_geojson(v: &serde_json::Value) -> Option<Geometry> {
    fn xy(v: &serde_json::Value) -> Option<Xy> {
        Some([v.get(0)?.as_f64()?, v.get(1)?.as_f64()?])
    }
    fn line(v: &serde_json::Value) -> Option<Vec<Xy>> {
        v.as_array()?.iter().map(xy).collect()
    }
    fn polygon(v: &serde_json::Value) -> Option<Vec<Vec<Xy>>> {
        v.as_array()?.iter().map(line).collect()
    }
    fn shapes(v: &serde_json::Value) -> Option<Vec<Shape>> {
        let c = &v["coordinates"];
        Some(match v["type"].as_str()? {
            "Point" => vec![Shape::Point(xy(c)?)],
            "LineString" => vec![Shape::Line(line(c)?)],
            "Polygon" => vec![Shape::Polygon(polygon(c)?)],
            "MultiPoint" => line(c)?.into_iter().map(Shape::Point).collect(),
            "MultiLineString" => c
                .as_array()?
                .iter()
                .map(line)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(Shape::Line)
                .collect(),
            "MultiPolygon" => c
                .as_array()?
                .iter()
                .map(polygon)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(Shape::Polygon)
                .collect(),
            "GeometryCollection" => v["geometries"]
                .as_array()?
                .iter()
                .map(shapes)
                .collect::<Option<Vec<_>>>()?
                .concat(),
            "Feature" => shapes(&v["geometry"])?,
            "FeatureCollection" => v["features"]
                .as_array()?
                .iter()
                .map(shapes)
                .collect::<Option<Vec<_>>>()?
                .concat(),
            _ => return None,
        })
    }
    Some(Geometry {
        srid: Some(4326),
        shapes: shapes(v)?,
    })
}

/// Column types that hold geometries, by database type name.
fn is_geometry_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    matches!(
        t.as_str(),
        "geometry"
            | "geography"
            | "point"
            | "linestring"
            | "polygon"
            | "multipoint"
            | "multilinestring"
            | "multipolygon"
            | "geometrycollection"
            | "geomcollection"
    )
}

/// Columns that hold geometries: by type, or because their first non-null
/// values decode as geometries.
/// Columns holding geometries: a geometry type, or sampled values that all
/// parse. Checks every candidate fully; see [`detect_geometry_columns`] for
/// the bounded check the UI makes.
pub fn geometry_columns(columns: &[Column], rows: &[Vec<Value>]) -> Vec<usize> {
    let detection = detect_geometry_columns(columns, rows);
    let mut found = detection.columns;
    found.extend(confirm_geometry_columns(
        &detection.undecided,
        rows,
        &std::sync::atomic::AtomicBool::new(false),
    ));
    found.sort_unstable();
    found
}

/// Rows looked at per column, NULLs included.
const SAMPLE_ROWS: usize = 1000;
/// Non-NULL values that must all parse for a column without a geometry type.
const SAMPLE_VALUES: usize = 20;
/// Bytes of values parsed by [`detect_geometry_columns`] for all columns
/// together; columns beyond it, or with larger values, are left undecided.
const QUICK_BYTES: usize = 256 * 1024;

pub struct Detection {
    /// Geometry columns, by type or by small sampled values.
    pub columns: Vec<usize>,
    /// Columns whose sample is too large to parse here; confirm them with
    /// [`confirm_geometry_columns`], away from the UI thread.
    pub undecided: Vec<usize>,
}

fn sample(rows: &[Vec<Value>], column: usize) -> impl Iterator<Item = &Value> {
    rows.iter()
        .take(SAMPLE_ROWS)
        .map(move |row| &row[column])
        .filter(|value| **value != Value::Null)
        .take(SAMPLE_VALUES)
}

fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Text(text) => text.len(),
        Value::Bytes(bytes) => bytes.len(),
        _ => 0,
    }
}

/// Values that cannot be geometries, found without parsing: JSON in which no
/// GeoJSON member appears (every GeoJSON geometry, Feature and collection has
/// "coordinates" or "geometries" somewhere), numbers and booleans. Text longer
/// than `scan` bytes is not scanned and counts as possible.
fn cannot_be_geometry(value: &Value, scan: usize) -> bool {
    match value {
        Value::Text(text) => {
            let text = text.trim_start();
            text.starts_with('{')
                && text.len() <= scan
                && !text.contains("\"coordinates\"")
                && !text.contains("\"geometries\"")
        }
        Value::Bytes(_) => false,
        _ => true,
    }
}

/// A bounded first check of which columns hold geometries: at most
/// [`SAMPLE_ROWS`] rows per column and [`QUICK_BYTES`] of parsed values for
/// the whole result. Its cost does not depend on the size of large cells.
pub fn detect_geometry_columns(columns: &[Column], rows: &[Vec<Value>]) -> Detection {
    let mut detection = Detection {
        columns: Vec::new(),
        undecided: Vec::new(),
    };
    let mut budget = QUICK_BYTES;
    for (c, column) in columns.iter().enumerate() {
        if is_geometry_type(&column.type_name) {
            detection.columns.push(c);
            continue;
        }
        let values: Vec<&Value> = sample(rows, c).collect();
        if values.is_empty() || values.iter().any(|value| cannot_be_geometry(value, QUICK_BYTES)) {
            continue;
        }
        let bytes: usize = values.iter().map(|value| value_bytes(value)).sum();
        if bytes > budget {
            detection.undecided.push(c);
            continue;
        }
        budget -= bytes;
        if values
            .iter()
            .all(|value| parse_value(value).is_some_and(|g| !g.shapes.is_empty()))
        {
            detection.columns.push(c);
        }
    }
    detection
}

/// The full check of undecided columns; stops early when `cancel` is set.
pub fn confirm_geometry_columns(
    candidates: &[usize],
    rows: &[Vec<Value>],
    cancel: &std::sync::atomic::AtomicBool,
) -> Vec<usize> {
    candidates
        .iter()
        .copied()
        .filter(|&c| {
            // A scan rejects plain JSON before any value is parsed into a tree.
            !sample(rows, c).any(|value| cannot_be_geometry(value, usize::MAX))
                && sample(rows, c).all(|value| {
                    !cancel.load(std::sync::atomic::Ordering::Relaxed)
                        && parse_value(value).is_some_and(|g| !g.shapes.is_empty())
                })
                && !cancel.load(std::sync::atomic::Ordering::Relaxed)
        })
        .collect()
}

/// SRID to assume when the data carries none: WGS84 when every coordinate
/// fits in degrees, otherwise unknown.
pub fn guess_srid<'a>(geoms: impl Iterator<Item = &'a Geometry>) -> Option<u32> {
    let mut any = false;
    for g in geoms {
        for p in g.coords() {
            any = true;
            if p[0].abs() > 180.0 || p[1].abs() > 90.0 {
                return None;
            }
        }
    }
    any.then_some(4326)
}

/// Converts coordinates of one SRID to WGS84 longitude/latitude in degrees.
pub struct Projector {
    src: Option<proj4rs::Proj>,
    dst: proj4rs::Proj,
}

struct P3(f64, f64, f64);

impl proj4rs::transform::Transform for P3 {
    fn transform_coordinates<F: proj4rs::transform::TransformClosure>(
        &mut self,
        f: &mut F,
    ) -> proj4rs::errors::Result<()> {
        let (x, y, z) = f(self.0, self.1, self.2)?;
        (self.0, self.1, self.2) = (x, y, z);
        Ok(())
    }
}

impl Projector {
    pub fn new(srid: u32) -> Result<Self, String> {
        let dst = proj4rs::Proj::from_epsg_code(4326).map_err(|e| e.to_string())?;
        if srid == 4326 {
            return Ok(Self { src: None, dst });
        }
        let code = u16::try_from(srid).map_err(|_| format!("unknown SRID {srid}"))?;
        let src =
            proj4rs::Proj::from_epsg_code(code).map_err(|_| format!("unknown SRID {srid}"))?;
        Ok(Self {
            src: Some(src),
            dst,
        })
    }

    pub fn to_wgs84(&self, p: Xy) -> Option<Xy> {
        let Some(src) = &self.src else {
            return Some(p).filter(|p| p[0].is_finite() && p[1].is_finite());
        };
        // proj4rs works in radians for geographic systems.
        let (x, y) = if src.is_latlong() {
            (p[0].to_radians(), p[1].to_radians())
        } else {
            (p[0], p[1])
        };
        let mut pt = P3(x, y, 0.0);
        proj4rs::transform::transform(src, &self.dst, &mut pt).ok()?;
        let out = [pt.0.to_degrees(), pt.1.to_degrees()];
        (out[0].is_finite() && out[1].is_finite()).then_some(out)
    }
}

/// Shapes of one result row in WGS84, ready to draw.
#[derive(Clone, Debug)]
pub struct Feature {
    pub row: usize,
    pub shapes: Vec<Shape>,
}

/// Outcome of preparing a geometry column for the map.
#[derive(Clone, Debug, Default)]
pub struct Layer {
    pub features: Vec<Feature>,
    /// SRID used: from the data, the override, or the guess.
    pub srid: Option<u32>,
    pub srid_source: &'static str,
    /// Rows whose value is empty or not a geometry.
    pub skipped: usize,
    pub error: Option<String>,
}

/// Decodes and reprojects one column. `srid_override` wins over the data.
pub fn build_layer(rows: &[Vec<Value>], column: usize, srid_override: Option<u32>) -> Layer {
    build_layer_cancellable(rows, column, srid_override, &|| false)
        .expect("never cancelled")
}

/// [`build_layer`] that stops with `None` once `cancelled` returns true.
pub fn build_layer_cancellable(
    rows: &[Vec<Value>],
    column: usize,
    srid_override: Option<u32>,
    cancelled: &dyn Fn() -> bool,
) -> Option<Layer> {
    let mut parsed: Vec<(usize, Geometry)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if cancelled() {
            return None;
        }
        if let Some(g) = r
            .get(column)
            .and_then(parse_value)
            .filter(|g| !g.shapes.is_empty())
        {
            parsed.push((i, g));
        }
    }
    let mut layer = Layer {
        skipped: rows.len() - parsed.len(),
        ..Default::default()
    };
    let from_data = parsed.iter().find_map(|(_, g)| g.srid.filter(|s| *s > 0));
    let (srid, source) = match (srid_override, from_data) {
        (Some(s), _) => (Some(s), "set manually"),
        (None, Some(s)) => (Some(s), "from data"),
        (None, None) => (guess_srid(parsed.iter().map(|(_, g)| g)), "assumed"),
    };
    layer.srid = srid;
    layer.srid_source = source;
    let Some(srid) = srid else {
        layer.error = Some(
            "coordinates are not longitude/latitude and the data has no SRID; enter the SRID"
                .into(),
        );
        return Some(layer);
    };
    let mut projectors: HashMap<u32, Result<Projector, String>> = HashMap::new();
    layer.features.reserve(parsed.len());
    for (row, mut g) in parsed {
        if cancelled() {
            return None;
        }
        // A row may carry its own SRID; the override applies to all rows.
        let row_srid = if srid_override.is_some() {
            srid
        } else {
            g.srid.filter(|s| *s > 0).unwrap_or(srid)
        };
        let projector = projectors
            .entry(row_srid)
            .or_insert_with(|| Projector::new(row_srid));
        let p = match projector {
            Ok(p) => p,
            Err(e) => {
                layer.error = Some(e.clone());
                layer.skipped += 1;
                continue;
            }
        };
        // Reprojected in place, so the decoded coordinates are not held twice.
        if g.project(&mut |xy| p.to_wgs84(xy)) {
            layer.features.push(Feature {
                row,
                shapes: g.shapes,
            });
        } else {
            layer.skipped += 1;
        }
    }
    Some(layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkb_point(little: bool, srid: Option<u32>, x: f64, y: f64) -> Vec<u8> {
        let mut b = vec![little as u8];
        let t: u32 = 1 | if srid.is_some() { 0x2000_0000 } else { 0 };
        let put32 = |b: &mut Vec<u8>, v: u32| {
            b.extend(if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            })
        };
        let put64 = |b: &mut Vec<u8>, v: f64| {
            b.extend(if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            })
        };
        put32(&mut b, t);
        if let Some(s) = srid {
            put32(&mut b, s);
        }
        put64(&mut b, x);
        put64(&mut b, y);
        b
    }

    #[test]
    fn wkb_and_ewkb() {
        let g = parse_wkb(&wkb_point(true, None, 21.0, 52.0)).unwrap();
        assert_eq!(
            g,
            Geometry {
                srid: None,
                shapes: vec![Shape::Point([21.0, 52.0])]
            }
        );
        let g = parse_wkb(&wkb_point(false, Some(2180), 637000.0, 486000.0)).unwrap();
        assert_eq!(g.srid, Some(2180));
        let hex: String = wkb_point(true, Some(4326), 1.0, 2.0)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        assert_eq!(parse_value(&Value::Text(hex)).unwrap().srid, Some(4326));
    }

    #[test]
    fn plain_wkb_is_not_taken_for_mysql_format() {
        // Big-endian WKB of a polygon: the 5th byte is 0x00 like a MySQL header would have.
        let wkb = [
            vec![0u8, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 4],
            [0.0f64, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect(),
        ]
        .concat();
        assert!(wkb_type_ok(&wkb[..5]));
        let g = parse_value(&Value::Bytes(wkb)).unwrap();
        assert_eq!((g.srid, g.shapes.len()), (None, 1));
    }

    #[test]
    fn mysql_internal_format() {
        let mut b = 2180u32.to_le_bytes().to_vec();
        b.extend(wkb_point(true, None, 637000.0, 486000.0));
        let g = parse_value(&Value::Bytes(b)).unwrap();
        assert_eq!((g.srid, g.shapes.len()), (Some(2180), 1));
    }

    #[test]
    fn geopackage_blob() {
        let mut b = b"GP".to_vec();
        b.extend([0, 0b0000_0001]); // version, little endian, no envelope
        b.extend(3857u32.to_le_bytes());
        b.extend(wkb_point(true, None, 10.0, 20.0));
        assert_eq!(parse_value(&Value::Bytes(b)).unwrap().srid, Some(3857));
    }

    #[test]
    fn wkt_variants() {
        let g =
            parse_wkt("SRID=2180;POLYGON((0 0, 10 0, 10 10, 0 0),(1 1, 2 1, 2 2, 1 1))").unwrap();
        assert_eq!(g.srid, Some(2180));
        assert!(matches!(&g.shapes[0], Shape::Polygon(r) if r.len() == 2 && r[0].len() == 4));
        assert_eq!(
            parse_wkt("POINT Z (1 2 3)").unwrap().shapes,
            [Shape::Point([1.0, 2.0])]
        );
        assert_eq!(
            parse_wkt("MULTIPOINT ((1 2), (3 4))").unwrap().shapes.len(),
            2
        );
        assert_eq!(
            parse_wkt("GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))")
                .unwrap()
                .shapes
                .len(),
            2
        );
        assert!(parse_wkt("POINT EMPTY").unwrap().shapes.is_empty());
        assert!(parse_wkt("Kraków").is_none());
        assert!(parse_wkt("POINT(1 2) trailing").is_none());
    }

    #[test]
    fn geojson() {
        let g = parse_value(&Value::Text(r#"{"type":"Feature","geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]}}"#.into())).unwrap();
        assert_eq!(g.srid, Some(4326));
        assert!(matches!(&g.shapes[0], Shape::Polygon(_)));
    }

    #[test]
    fn detects_geometry_columns() {
        let cols: Vec<Column> = ["id", "name", "geom", "wkt"]
            .iter()
            .map(|n| Column {
                name: n.to_string(),
                type_name: if *n == "geom" {
                    "geometry".into()
                } else {
                    "text".into()
                },
            })
            .collect();
        let rows = vec![vec![
            Value::Int(1),
            Value::Text("POINT".into()),
            Value::Null,
            Value::Text("POINT(21 52)".into()),
        ]];
        assert_eq!(geometry_columns(&cols, &rows), [2, 3]);
    }

    #[test]
    fn detection_on_the_ui_thread_is_bounded_by_bytes_and_rows() {
        let cols: Vec<Column> = ["json", "geojson", "wkt", "wkb", "empty", "small"]
            .iter()
            .map(|n| Column {
                name: n.to_string(),
                type_name: "text".into(),
            })
            .collect();
        let big_json = format!("{{\"items\": [{}0]}}", "1, ".repeat(1 << 20));
        let big_geojson = format!(
            "{{\"type\": \"LineString\", \"coordinates\": [{}[0, 0]]}}",
            "[21.0, 52.0], ".repeat(40_000)
        );
        let big_wkt = format!("LINESTRING({}0 0)", "21 52, ".repeat(60_000));
        let mut wkb = vec![1u8, 2, 0, 0, 0];
        wkb.extend(100_000u32.to_le_bytes());
        for _ in 0..100_000 {
            wkb.extend(21.0f64.to_le_bytes());
            wkb.extend(52.0f64.to_le_bytes());
        }
        let mut rows: Vec<Vec<Value>> = (0..3)
            .map(|_| {
                vec![
                    Value::Text(big_json.clone()),
                    Value::Text(big_geojson.clone()),
                    Value::Text(big_wkt.clone()),
                    Value::Bytes(wkb.clone()),
                    Value::Null,
                    Value::Text("POINT(1 2)".into()),
                ]
            })
            .collect();
        // Thousands of NULL rows are looked at only up to the row limit.
        rows.extend((0..5000).map(|_| vec![Value::Null; 6]));
        let detection = detect_geometry_columns(&cols, &rows);
        assert_eq!(detection.columns, [5], "small values are decided at once");
        // Large values, plain JSON included, are left to the worker, which
        // rejects plain JSON by a scan before parsing anything.
        assert_eq!(detection.undecided, [0, 1, 2, 3]);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(confirm_geometry_columns(&detection.undecided, &rows, &cancel), [1, 2, 3]);
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(confirm_geometry_columns(&detection.undecided, &rows, &cancel).is_empty());
        assert_eq!(geometry_columns(&cols, &rows), [1, 2, 3, 5]);
        // A column whose values appear only after the sampled rows stays plain.
        let late = vec![vec![Value::Null; 6]; 1000]
            .into_iter()
            .chain([vec![Value::Text("POINT(1 2)".into()); 6]])
            .collect::<Vec<_>>();
        assert!(geometry_columns(&cols, &late).is_empty());
    }

    #[test]
    #[ignore = "benchmark: cargo test --release --lib geometry_detection_cost -- --ignored --nocapture"]
    fn geometry_detection_cost_benchmark() {
        let cols = vec![Column {
            name: "doc".into(),
            type_name: "jsonb".into(),
        }];
        for mib in [1usize, 10] {
            let json = format!("{{\"items\": [{}0]}}", "1, ".repeat((mib << 20) / 3));
            let rows = vec![vec![Value::Text(json)]; 20];
            // The check before: parse every sampled value on the UI thread.
            let start = std::time::Instant::now();
            let old = rows
                .iter()
                .map(|r| &r[0])
                .take(20)
                .all(|v| parse_value(v).is_some_and(|g| !g.shapes.is_empty()));
            let before = start.elapsed();
            let start = std::time::Instant::now();
            let detection = detect_geometry_columns(&cols, &rows);
            let ui = start.elapsed();
            let start = std::time::Instant::now();
            let confirmed = confirm_geometry_columns(
                &detection.undecided,
                &rows,
                &std::sync::atomic::AtomicBool::new(false),
            );
            let worker = start.elapsed();
            assert!(!old && confirmed.is_empty());
            println!("20 x {mib} MiB plain JSON: before {before:?} on the UI thread; after {ui:?} on the UI thread, {worker:?} on the worker (no parse)");
        }
    }

    #[test]
    fn reprojects_to_wgs84() {
        // Web Mercator: 0,0 is 0,0; x = 2 337 709 m is 21° east.
        let p = Projector::new(3857).unwrap();
        let ll = p.to_wgs84([2_337_709.3, 6_840_000.0]).unwrap();
        assert!(
            (ll[0] - 21.0).abs() < 1e-3 && (ll[1] - 52.21).abs() < 0.01,
            "{ll:?}"
        );
        // PL-1992 (EPSG:2180): the false origin 500000, -5300000 lies on 19°E, 0°N.
        let p = Projector::new(2180).unwrap();
        let ll = p.to_wgs84([500_000.0, -5_300_000.0]).unwrap();
        assert!((ll[0] - 19.0).abs() < 1e-6 && ll[1].abs() < 1e-6, "{ll:?}");
        // Warsaw, Palace of Culture: about 21.0067 E, 52.2319 N.
        let ll = p.to_wgs84([637_123.0, 486_626.0]).unwrap();
        assert!(
            (ll[0] - 21.0).abs() < 0.05 && (ll[1] - 52.23).abs() < 0.05,
            "{ll:?}"
        );
        assert!(Projector::new(4326).unwrap().to_wgs84([21.0, 52.0]) == Some([21.0, 52.0]));
        assert!(Projector::new(999_999).is_err());
    }

    #[test]
    fn layer_uses_data_srid_override_or_guess() {
        let rows = vec![
            vec![Value::Text("SRID=3857;POINT(0 0)".into())],
            vec![Value::Null],
            vec![Value::Text("not a geometry".into())],
        ];
        let l = build_layer(&rows, 0, None);
        assert_eq!(
            (l.srid, l.srid_source, l.features.len(), l.skipped),
            (Some(3857), "from data", 1, 2)
        );
        let plain = vec![vec![Value::Text("POINT(21 52)".into())]];
        assert_eq!(build_layer(&plain, 0, None).srid_source, "assumed");
        let projected = vec![vec![Value::Text("POINT(637000 486000)".into())]];
        assert!(build_layer(&projected, 0, None).error.is_some());
        let fixed = build_layer(&projected, 0, Some(2180));
        assert_eq!(
            (fixed.srid_source, fixed.features.len()),
            ("set manually", 1)
        );
    }
}
