//! Database Graph: objects of one database (tables, partitions, views,
//! materialized views, foreign tables) with their measured or estimated size
//! and the relations recorded in metadata. Drivers fill [`SchemaGraph`] from
//! catalogs only; nothing here reads table contents.
//!
//! The layout is computed once per graph, off the UI thread, and does not
//! depend on the chosen metric, so switching rows and bytes keeps positions.

use serde::{Deserialize, Serialize};

/// Upper bound of objects read in one scan; more are reported, not dropped silently.
pub const MAX_OBJECTS: usize = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Table,
    /// Parent of declarative partitions; it stores no rows itself.
    PartitionedTable,
    Partition,
    View,
    MaterializedView,
    ForeignTable,
    /// Foreign server of foreign tables, without options or user mappings.
    ForeignServer,
}

impl NodeKind {
    pub const ALL: [NodeKind; 7] = [
        NodeKind::Table,
        NodeKind::PartitionedTable,
        NodeKind::Partition,
        NodeKind::View,
        NodeKind::MaterializedView,
        NodeKind::ForeignTable,
        NodeKind::ForeignServer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NodeKind::Table => "Table",
            NodeKind::PartitionedTable => "Partitioned table",
            NodeKind::Partition => "Partition",
            NodeKind::View => "View",
            NodeKind::MaterializedView => "Materialized view",
            NodeKind::ForeignTable => "Foreign table",
            NodeKind::ForeignServer => "Foreign server",
        }
    }

    /// Objects whose data can be opened in a table tab.
    pub fn has_data(self) -> bool {
        !matches!(self, NodeKind::ForeignServer)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricStatus {
    /// Read from the storage at scan time.
    Measured,
    /// A planner statistic or an engine approximation.
    Estimated,
    /// The engine has no value yet (for example, never analyzed).
    Unknown,
    /// The value exists elsewhere but is not readable here (remote data, missing extension).
    Unavailable,
    /// The object does not store data of its own.
    NotApplicable,
}

impl MetricStatus {
    pub fn label(self) -> &'static str {
        match self {
            MetricStatus::Measured => "measured",
            MetricStatus::Estimated => "estimated",
            MetricStatus::Unknown => "unknown",
            MetricStatus::Unavailable => "unavailable",
            MetricStatus::NotApplicable => "not applicable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub value: Option<f64>,
    pub status: MetricStatus,
    /// Where the value comes from, e.g. `pg_class.reltuples`.
    pub source: String,
    /// Why the value is missing or how it was derived.
    pub note: Option<String>,
}

impl Metric {
    pub fn known(value: f64, status: MetricStatus, source: &str) -> Self {
        Self {
            value: Some(value),
            status,
            source: source.into(),
            note: None,
        }
    }

    pub fn missing(status: MetricStatus, source: &str, note: &str) -> Self {
        Self {
            value: None,
            status,
            source: source.into(),
            note: (!note.is_empty()).then(|| note.into()),
        }
    }

    pub fn usable(&self) -> Option<f64> {
        self.value.filter(|v| v.is_finite() && *v >= 0.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub schema: String,
    pub name: String,
    pub kind: NodeKind,
    /// Database Explorer path, used for explicit Open Data and DDL actions.
    pub path: Vec<String>,
    pub rows: Metric,
    /// Table data without indexes (with TOAST where the engine has it).
    pub table_bytes: Metric,
    /// Everything the object occupies, including indexes.
    pub total_bytes: Metric,
    /// Partition parent, by node index.
    pub parent: Option<usize>,
    /// Additional facts, such as the last statistics update or the foreign data wrapper.
    pub facts: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    ForeignKey,
    ViewDependency,
    MaterializedViewDependency,
    ForeignSource,
    Partition,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 5] = [
        EdgeKind::ForeignKey,
        EdgeKind::ViewDependency,
        EdgeKind::MaterializedViewDependency,
        EdgeKind::ForeignSource,
        EdgeKind::Partition,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EdgeKind::ForeignKey => "Foreign key",
            EdgeKind::ViewDependency => "View dependency",
            EdgeKind::MaterializedViewDependency => "Materialized view dependency",
            EdgeKind::ForeignSource => "Foreign source mapping",
            EdgeKind::Partition => "Partition membership",
        }
    }

    /// What the edge does not prove, shown next to it.
    pub fn meaning(self) -> &'static str {
        match self {
            EdgeKind::ForeignKey => "Declared reference between columns; not proof that all data was validated.",
            EdgeKind::ViewDependency => "The view definition reads this object; rows are not shared.",
            EdgeKind::MaterializedViewDependency => "Data was copied from this object at the last refresh, not read on every SELECT.",
            EdgeKind::ForeignSource => "Configuration of the foreign table; not a foreign key and not a check of the remote schema.",
            EdgeKind::Partition => "Membership in a partition tree, separate from foreign keys.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Referencing or dependent object.
    pub from: usize,
    /// Referenced object or source.
    pub to: usize,
    pub kind: EdgeKind,
    pub name: Option<String>,
    /// Column pairs of a foreign key in key order.
    pub columns: Vec<(String, String)>,
    /// Whether the engine reports the constraint as validated, when it tells.
    pub validated: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SchemaGraph {
    pub database: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Limits of this scan: missing catalogs, truncated object lists.
    pub notes: Vec<String>,
    /// More objects exist than [`MAX_OBJECTS`].
    pub truncated: bool,
    pub read_at: String,
}

impl SchemaGraph {
    /// Partitioned parents show the sum of their leaf partitions while the
    /// partitions are collapsed; the parent itself has no storage.
    pub fn aggregate_partitions(&mut self) {
        let children = self.children();
        let parents: Vec<usize> = (0..self.nodes.len())
            .filter(|i| self.nodes[*i].kind == NodeKind::PartitionedTable)
            .collect();
        for parent in parents {
            let leaves = self.leaves_with(parent, &children);
            if leaves.is_empty() {
                continue;
            }
            let count = leaves.len();
            let sum = |pick: fn(&GraphNode) -> &Metric| -> Metric {
                let metrics: Vec<&Metric> = leaves.iter().map(|i| pick(&self.nodes[*i])).collect();
                let unknown = metrics.iter().filter(|m| m.usable().is_none()).count();
                let total: f64 = metrics.iter().filter_map(|m| m.usable()).sum();
                let status = if unknown == count {
                    MetricStatus::Unknown
                } else if metrics.iter().all(|m| m.status == MetricStatus::Measured) {
                    MetricStatus::Measured
                } else {
                    MetricStatus::Estimated
                };
                let mut note = format!("Sum of {count} partitions");
                if unknown > 0 && unknown < count {
                    note.push_str(&format!("; {unknown} without a value are not included"));
                }
                Metric {
                    value: (unknown < count).then_some(total),
                    status,
                    source: pick(&self.nodes[leaves[0]]).source.clone(),
                    note: Some(note),
                }
            };
            let rows = sum(|n| &n.rows);
            let table = sum(|n| &n.table_bytes);
            let total = sum(|n| &n.total_bytes);
            let node = &mut self.nodes[parent];
            node.rows = rows;
            node.table_bytes = table;
            node.total_bytes = total;
        }
    }

    /// Direct partitions of every node, built once.
    fn children(&self) -> Vec<Vec<usize>> {
        let mut children = vec![Vec::new(); self.nodes.len()];
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(parent) = node.parent.filter(|p| *p < self.nodes.len()) {
                children[parent].push(index);
            }
        }
        children
    }

    /// Leaf partitions under a parent, through nested partitioned tables.
    pub fn leaves(&self, parent: usize) -> Vec<usize> {
        self.leaves_with(parent, &self.children())
    }

    fn leaves_with(&self, parent: usize, children: &[Vec<usize>]) -> Vec<usize> {
        let mut result = Vec::new();
        let mut stack = vec![parent];
        let mut visited = 0;
        while let Some(current) = stack.pop() {
            visited += 1;
            if visited > self.nodes.len() + 1 {
                break; // a malformed cycle
            }
            for &index in &children[current] {
                if self.nodes[index].kind == NodeKind::PartitionedTable {
                    stack.push(index);
                } else {
                    result.push(index);
                }
            }
        }
        result
    }

    /// Node index by schema and name, for restoring pinned positions.
    pub fn index_by_name(&self) -> std::collections::HashMap<(&str, &str), usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(index, node)| ((node.schema.as_str(), node.name.as_str()), index))
            .collect()
    }

    /// Top partitioned parent of a partition, for collapsing partition trees.
    pub fn root_parent(&self, mut index: usize) -> usize {
        for _ in 0..self.nodes.len() {
            match self.nodes[index].parent {
                Some(parent) => index = parent,
                None => break,
            }
        }
        index
    }
}

/// Positions in world units, one per node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    pub positions: Vec<[f32; 3]>,
}

/// Distance the layout aims for between neighbours, in world units.
pub const SPACING: f32 = 120.0;

/// Forces of the layout, as multipliers of the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutParams {
    /// Rest length of relations.
    pub link_distance: f32,
    /// How strongly related objects pull each other.
    pub attraction: f32,
    /// How strongly all objects push each other apart.
    pub repulsion: f32,
    /// How strongly objects keep to their schema's cluster.
    pub grouping: f32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            link_distance: 1.0,
            attraction: 1.0,
            repulsion: 1.0,
            grouping: 1.0,
        }
    }
}

/// Force-directed layout with schema clusters. Deterministic for the same
/// graph and parameters, so a rescan of an unchanged database keeps its shape.
/// `pinned` positions are kept as they are.
pub fn layout(graph: &SchemaGraph, flat: bool, pinned: &[(usize, [f32; 3])], params: &LayoutParams) -> Layout {
    layout_cancellable(graph, flat, pinned, params, &|| false).unwrap_or_default()
}

/// [`layout`] that checks `cancelled` once per iteration and gives up when it
/// returns true, so a superseded request does not keep a core busy.
pub fn layout_cancellable(
    graph: &SchemaGraph,
    flat: bool,
    pinned: &[(usize, [f32; 3])],
    params: &LayoutParams,
    cancelled: &dyn Fn() -> bool,
) -> Option<Layout> {
    let params = LayoutParams {
        link_distance: params.link_distance.clamp(0.2, 5.0),
        attraction: params.attraction.clamp(0.0, 5.0),
        repulsion: params.repulsion.clamp(0.1, 5.0),
        grouping: params.grouping.clamp(0.0, 5.0),
    };
    let n = graph.nodes.len();
    if n == 0 {
        return Some(Layout::default());
    }
    let dims = if flat { 2 } else { 3 };
    // Schema clusters: placed apart first, so the forces refine rather than invent structure.
    let mut schemas: Vec<&str> = graph.nodes.iter().map(|node| node.schema.as_str()).collect();
    schemas.sort_unstable();
    schemas.dedup();
    let cluster_of: Vec<usize> = graph
        .nodes
        .iter()
        .map(|node| schemas.binary_search(&node.schema.as_str()).unwrap_or(0))
        .collect();
    let mut counts = vec![0usize; schemas.len()];
    for cluster in &cluster_of {
        counts[*cluster] += 1;
    }
    let spread = |count: usize| SPACING * params.repulsion.sqrt() * (count as f32).powf(1.0 / dims as f32).max(1.0);
    let ring = if schemas.len() > 1 {
        let largest = counts.iter().copied().max().unwrap_or(1);
        spread(largest) * 1.2 + spread(n) * 0.6
    } else {
        0.0
    };
    let centers: Vec<[f32; 3]> = (0..schemas.len())
        .map(|i| scale(direction(i, schemas.len(), flat), ring))
        .collect();
    let mut seed = graph
        .nodes
        .iter()
        .fold(0x9e37_79b9_7f4a_7c15u64, |hash, node| fnv(hash, &node.schema) ^ fnv(hash, &node.name).rotate_left(7));
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
    };
    let mut positions: Vec<[f32; 3]> = (0..n)
        .map(|i| {
            let center = centers[cluster_of[i]];
            let radius = spread(counts[cluster_of[i]]) * 0.6;
            let z = if flat { 0.0 } else { random() * radius };
            [center[0] + random() * radius, center[1] + random() * radius, center[2] + z]
        })
        .collect();
    let mut fixed = vec![false; n];
    for (index, position) in pinned {
        if *index < n {
            positions[*index] = if flat { [position[0], position[1], 0.0] } else { *position };
            fixed[*index] = true;
        }
    }
    let edges: Vec<(usize, usize, f32)> = graph
        .edges
        .iter()
        .filter(|edge| edge.from < n && edge.to < n && edge.from != edge.to)
        .map(|edge| {
            // Partitions stay close to their parent; other relations at the usual distance.
            let length = if edge.kind == EdgeKind::Partition { 0.6 } else { 1.0 };
            (edge.from, edge.to, length * SPACING * params.link_distance)
        })
        .collect();
    let iterations = if n > 2_000 { 120 } else if n > 500 { 200 } else { 300 };
    let mut temperature = spread(n) * 0.25;
    // Stronger repulsion reaches farther, so the neighbour grid grows with it.
    let reach = SPACING * params.repulsion.sqrt();
    // Beyond one and a half spacings the push is weak next to the springs.
    let cell = reach * 1.5;
    let (cell_squared, reach_squared) = (cell * cell, reach * reach);
    // Buckets are kept between iterations; only their contents are cleared.
    let mut grid: std::collections::HashMap<(i32, i32, i32), Vec<usize>, CellHash> = std::collections::HashMap::default();
    let mut force = vec![[0.0f32; 3]; n];
    for iteration in 0..iterations {
        if cancelled() {
            return None;
        }
        if iteration % 16 == 0 {
            grid.retain(|_, bucket| !bucket.is_empty());
        }
        for bucket in grid.values_mut() {
            bucket.clear();
        }
        let key = |p: &[f32; 3]| ((p[0] / cell).floor() as i32, (p[1] / cell).floor() as i32, (p[2] / cell).floor() as i32);
        for (index, position) in positions.iter().enumerate() {
            grid.entry(key(position)).or_default().push(index);
        }
        for f in force.iter_mut() {
            *f = [0.0; 3];
        }
        // Repulsion from nearby nodes only: a uniform grid keeps it linear in practice.
        let z_range = if flat { 0..=0 } else { -1..=1 };
        for (index, position) in positions.iter().enumerate() {
            let (cx, cy, cz) = key(position);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in z_range.clone() {
                        let Some(bucket) = grid.get(&(cx + dx, cy + dy, cz + dz)) else {
                            continue;
                        };
                        for &other in bucket {
                            if other == index {
                                continue;
                            }
                            // reach² / d along the unit direction is reach² / d² along delta: no square root.
                            let delta = sub(*position, positions[other]);
                            let squared = delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2];
                            if squared > cell_squared {
                                continue;
                            }
                            let (push, squared) = if squared < 1e-4 {
                                ([random(), random(), if flat { 0.0 } else { random() }], 1e-4)
                            } else {
                                (delta, squared)
                            };
                            force[index] = add(force[index], scale(push, reach_squared / squared));
                        }
                    }
                }
            }
        }
        for &(a, b, rest) in &edges {
            let delta = sub(positions[b], positions[a]);
            let distance = length(delta).max(0.01);
            let magnitude = (distance - rest) * distance / SPACING * 0.5 * params.attraction;
            let pull = scale(delta, magnitude / distance);
            force[a] = add(force[a], pull);
            force[b] = sub(force[b], pull);
        }
        for index in 0..n {
            // A weak pull to the schema center keeps clusters readable.
            let center = centers[cluster_of[index]];
            let to_center = sub(center, positions[index]);
            force[index] = add(force[index], scale(to_center, 0.02 * params.grouping));
        }
        for index in 0..n {
            if fixed[index] {
                continue;
            }
            let mut step = force[index];
            if flat {
                step[2] = 0.0;
            }
            let magnitude = length(step);
            if magnitude > temperature {
                step = scale(step, temperature / magnitude);
            }
            positions[index] = add(positions[index], step);
        }
        temperature = (temperature * 0.97).max(SPACING * 0.02);
    }
    Some(Layout { positions })
}

/// Cheap hash for grid cells; SipHash is needlessly slow for three small integers.
#[derive(Clone, Copy, Default)]
struct CellHash;

impl std::hash::BuildHasher for CellHash {
    type Hasher = CellHasher;
    fn build_hasher(&self) -> CellHasher {
        CellHasher(0)
    }
}

struct CellHasher(u64);

impl std::hash::Hasher for CellHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }
    fn write_i32(&mut self, value: i32) {
        self.write_u64(value as u32 as u64);
    }
    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

fn fnv(mut hash: u64, text: &str) -> u64 {
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Evenly spread directions: a sunflower in 2D, a Fibonacci sphere in 3D.
fn direction(index: usize, count: usize, flat: bool) -> [f32; 3] {
    let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
    let angle = golden * index as f32;
    if flat {
        return [angle.cos(), angle.sin(), 0.0];
    }
    let y = 1.0 - 2.0 * (index as f32 + 0.5) / count.max(1) as f32;
    let radius = (1.0 - y * y).max(0.0).sqrt();
    [angle.cos() * radius, y, angle.sin() * radius]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: [f32; 3], factor: f32) -> [f32; 3] {
    [a[0] * factor, a[1] * factor, a[2] * factor]
}
fn length(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeMetric {
    Rows,
    TableBytes,
    TotalBytes,
}

impl SizeMetric {
    pub fn of(self, node: &GraphNode) -> &Metric {
        match self {
            SizeMetric::Rows => &node.rows,
            SizeMetric::TableBytes => &node.table_bytes,
            SizeMetric::TotalBytes => &node.total_bytes,
        }
    }

    pub fn format(self, value: f64) -> String {
        match self {
            SizeMetric::Rows => format_count(value),
            SizeMetric::TableBytes | SizeMetric::TotalBytes => format_bytes(value),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeScale {
    /// Sphere volume proportional to the value: radius by cube root.
    Volume,
    Logarithmic,
}

/// Node radius in world units for a value, given the largest shown value.
/// Values never map to zero: the smallest visible size is a floor.
pub fn radius(value: f64, max: f64, scale: SizeScale) -> f32 {
    const LARGEST: f32 = SPACING * 0.45;
    const SMALLEST: f32 = SPACING * 0.06;
    if max <= 0.0 || value <= 0.0 {
        return SMALLEST;
    }
    let fraction = match scale {
        SizeScale::Volume => (value / max).cbrt(),
        SizeScale::Logarithmic => (value.ln_1p() / max.ln_1p()).clamp(0.0, 1.0),
    } as f32;
    (LARGEST * fraction).max(SMALLEST)
}

pub fn format_count(value: f64) -> String {
    let value = value.round();
    if value.abs() < 1_000.0 {
        return format!("{value:.0}");
    }
    for (unit, divisor) in [("B", 1e9), ("M", 1e6), ("K", 1e3)] {
        if value.abs() >= divisor {
            return format!("{:.1}{unit}", value / divisor);
        }
    }
    format!("{value:.0}")
}

pub fn format_bytes(value: f64) -> String {
    let mut amount = value;
    for unit in ["B", "kB", "MB", "GB", "TB"] {
        if amount < 1024.0 || unit == "TB" {
            return if unit == "B" { format!("{amount:.0} {unit}") } else { format!("{amount:.1} {unit}") };
        }
        amount /= 1024.0;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(schema: &str, name: &str, kind: NodeKind, rows: Option<f64>) -> GraphNode {
        GraphNode {
            schema: schema.into(),
            name: name.into(),
            kind,
            path: vec![schema.into(), name.into()],
            rows: match rows {
                Some(v) => Metric::known(v, MetricStatus::Estimated, "test"),
                None => Metric::missing(MetricStatus::Unknown, "test", "never analyzed"),
            },
            table_bytes: Metric::known(8192.0, MetricStatus::Measured, "test"),
            total_bytes: Metric::known(16384.0, MetricStatus::Measured, "test"),
            parent: None,
            facts: Vec::new(),
        }
    }

    fn edge(from: usize, to: usize, kind: EdgeKind) -> GraphEdge {
        GraphEdge {
            from,
            to,
            kind,
            name: None,
            columns: Vec::new(),
            validated: None,
        }
    }

    fn sample() -> SchemaGraph {
        let mut nodes = vec![
            node("public", "customers", NodeKind::Table, Some(1_000.0)),
            node("public", "orders", NodeKind::Table, Some(50_000.0)),
            node("public", "events", NodeKind::PartitionedTable, Some(0.0)),
            node("public", "events_2026_01", NodeKind::Partition, Some(300.0)),
            node("public", "events_2026_02", NodeKind::Partition, None),
            node("reports", "order_totals", NodeKind::View, None),
        ];
        nodes[3].parent = Some(2);
        nodes[4].parent = Some(2);
        SchemaGraph {
            database: "shop".into(),
            nodes,
            edges: vec![
                edge(1, 0, EdgeKind::ForeignKey),
                edge(3, 2, EdgeKind::Partition),
                edge(4, 2, EdgeKind::Partition),
                edge(5, 1, EdgeKind::ViewDependency),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn partition_parents_sum_known_leaves_and_say_so() {
        let mut graph = sample();
        graph.aggregate_partitions();
        let parent = &graph.nodes[2];
        assert_eq!(parent.rows.value, Some(300.0));
        assert_eq!(parent.rows.status, MetricStatus::Estimated);
        let note = parent.rows.note.as_deref().unwrap();
        assert!(note.contains("2 partitions") && note.contains("1 without a value"), "{note}");
        assert_eq!(parent.table_bytes.value, Some(16384.0));
        assert_eq!(parent.table_bytes.status, MetricStatus::Measured);
        assert_eq!(graph.root_parent(4), 2);
    }

    #[test]
    fn layout_is_deterministic_keeps_pins_and_separates_nodes() {
        let graph = sample();
        let first = layout(&graph, false, &[], &LayoutParams::default());
        let second = layout(&graph, false, &[], &LayoutParams::default());
        assert_eq!(first, second, "the same graph gives the same layout");
        for a in 0..graph.nodes.len() {
            for b in a + 1..graph.nodes.len() {
                assert!(length(sub(first.positions[a], first.positions[b])) > SPACING * 0.2, "{a} and {b} overlap");
            }
        }
        let pinned = layout(&graph, false, &[(1, [900.0, 0.0, 0.0])], &LayoutParams::default());
        assert_eq!(pinned.positions[1], [900.0, 0.0, 0.0]);
        let flat = layout(&graph, true, &[], &LayoutParams::default());
        assert!(flat.positions.iter().all(|p| p[2] == 0.0));
        // Stronger repulsion and longer relations spread the objects apart.
        let extent = |l: &Layout| {
            let mut max = 0.0f32;
            for a in &l.positions {
                for b in &l.positions {
                    max = max.max(length(sub(*a, *b)));
                }
            }
            max
        };
        let spread = layout(&graph, false, &[], &LayoutParams { repulsion: 3.0, link_distance: 2.5, ..Default::default() });
        assert!(extent(&spread) > extent(&first) * 1.3, "{} vs {}", extent(&spread), extent(&first));
    }

    #[test]
    fn radius_follows_volume_or_logarithm_and_never_vanishes() {
        let max = 1_000_000.0;
        let full = radius(max, max, SizeScale::Volume);
        let eighth = radius(max / 8.0, max, SizeScale::Volume);
        assert!((eighth / full - 0.5).abs() < 0.01, "8x less volume is half the radius");
        assert!(radius(0.0, max, SizeScale::Volume) > 0.0);
        assert!(radius(10.0, max, SizeScale::Logarithmic) > radius(10.0, max, SizeScale::Volume));
        assert_eq!(format_count(1_234_567.0), "1.2M");
        assert_eq!(format_bytes(8192.0), "8.0 kB");
    }

    #[test]
    fn large_graphs_lay_out_in_reasonable_time() {
        let mut graph = SchemaGraph::default();
        for index in 0..2_000 {
            graph.nodes.push(node(&format!("s{}", index % 7), &format!("t{index}"), NodeKind::Table, Some(index as f64)));
            if index > 0 {
                graph.edges.push(edge(index, index / 3, EdgeKind::ForeignKey));
            }
        }
        let started = std::time::Instant::now();
        let result = layout(&graph, false, &[], &LayoutParams::default());
        assert_eq!(result.positions.len(), 2_000);
        assert!(result.positions.iter().all(|p| p.iter().all(|v| v.is_finite())));
        // Debug builds are slow; this only guards against quadratic regressions.
        assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
    }
}

#[cfg(test)]
mod benchmarks {
    use super::*;

    /// `cargo test --release --lib schema_graph::benchmarks -- --ignored --nocapture`
    #[test]
    #[ignore = "release measurement of the layout for 100, 1000 and 5000 objects"]
    fn layout_cost() {
        for (count, density) in [(100, 1), (1_000, 2), (5_000, 3)] {
            let mut graph = SchemaGraph::default();
            for index in 0..count {
                graph.nodes.push(GraphNode {
                    schema: format!("s{}", index % 12),
                    name: format!("t{index}"),
                    kind: NodeKind::Table,
                    path: Vec::new(),
                    rows: Metric::known(index as f64, MetricStatus::Estimated, ""),
                    table_bytes: Metric::known(index as f64, MetricStatus::Measured, ""),
                    total_bytes: Metric::known(index as f64, MetricStatus::Measured, ""),
                    parent: None,
                    facts: Vec::new(),
                });
                for k in 1..=density {
                    if index >= k * 7 {
                        graph.edges.push(GraphEdge {
                            from: index,
                            to: (index * 31 + k * 17) % index,
                            kind: EdgeKind::ForeignKey,
                            name: None,
                            columns: Vec::new(),
                            validated: None,
                        });
                    }
                }
            }
            let started = std::time::Instant::now();
            let space = layout(&graph, false, &[], &LayoutParams::default());
            let space_time = started.elapsed();
            let started = std::time::Instant::now();
            let _flat = layout(&graph, true, &[], &LayoutParams::default());
            println!(
                "{count} objects, {} edges: 3D layout {:?}, 2D layout {:?}",
                graph.edges.len(),
                space_time,
                started.elapsed()
            );
            assert_eq!(space.positions.len(), count);
        }
    }
}
