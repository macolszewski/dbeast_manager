//! Linear forest layout and bounded, allocation-reusing viewport queries.
//! Geometry is prepared once by the query-plan worker, without fonts or egui.

use super::PlanNode;
use std::{fmt, mem::size_of, ops::Range};

pub const MAX_DIAGRAM_NODES: usize = 100_000;
pub type NodeId = usize;
const NONE: u32 = u32::MAX;
const METRICS: DiagramMetrics = DiagramMetrics {
    card_width: 260.0,
    card_height: 100.0,
    column_gap: 72.0,
    leaf_gap: 24.0,
};
const STEP_X: f64 = METRICS.card_width + METRICS.column_gap;
const STEP_Y: f64 = METRICS.card_height + METRICS.leaf_gap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldRect {
    pub min: WorldPoint,
    pub max: WorldPoint,
}
impl WorldRect {
    pub fn width(self) -> f64 {
        self.max.x - self.min.x
    }
    pub fn height(self) -> f64 {
        self.max.y - self.min.y
    }
    pub fn center(self) -> WorldPoint {
        WorldPoint {
            x: self.min.x + self.width() * 0.5,
            y: self.min.y + self.height() * 0.5,
        }
    }
    /// Boundaries are inclusive, including zero-length segments.
    pub fn contains(self, point: WorldPoint) -> bool {
        point.x >= self.min.x
            && point.x <= self.max.x
            && point.y >= self.min.y
            && point.y <= self.max.y
    }
    /// Touching boundaries intersect.
    pub fn intersects(self, other: Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
    }
    fn finite(self) -> bool {
        [self.min.x, self.min.y, self.max.x, self.max.y]
            .into_iter()
            .all(f64::is_finite)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiagramMetrics {
    pub card_width: f64,
    pub card_height: f64,
    pub column_gap: f64,
    pub leaf_gap: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagramError {
    Empty,
    TooManyNodes,
    InvalidHierarchy { index: usize },
}
impl fmt::Display for DiagramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("No operations available. Open Raw to inspect the original plan."),
            Self::TooManyNodes => f.write_str("Diagram supports up to 100000 operations. Use Operations Tree or Raw for this plan."),
            Self::InvalidHierarchy { index } => write!(f, "Invalid hierarchy at operation {}. Use Operations Tree or Raw.", index + 1),
        }
    }
}
impl std::error::Error for DiagramError {}

#[derive(Clone, Copy, Debug)]
struct NodeGeometry {
    y: f64,
    depth: u32,
    parent: u32,
}
#[derive(Clone, Copy, Debug, Default)]
struct Branch {
    parent: u32,
    first: u32,
    last: u32,
}
#[derive(Clone, Copy, Debug)]
struct ColumnBounds {
    min_y: f64,
    max_y: f64,
    count: usize,
}
impl ColumnBounds {
    const EMPTY: Self = Self {
        min_y: f64::INFINITY,
        max_y: f64::NEG_INFINITY,
        count: 0,
    };
    fn union(self, other: Self) -> Self {
        Self {
            min_y: self.min_y.min(other.min_y),
            max_y: self.max_y.max(other.max_y),
            count: self.count + other.count,
        }
    }
}

#[derive(Debug)]
pub struct DiagramLayout {
    nodes: Vec<NodeGeometry>,
    depth_offsets: Vec<usize>,
    depth_nodes: Vec<u32>,
    branch_offsets: Vec<usize>,
    branches: Vec<Branch>,
    column_tree: Vec<ColumnBounds>,
    column_base: usize,
    bounds: WorldRect,
}

/// Builds a deterministic leaf-slot layout in O(N), without recursive traversal.
/// Siblings and separate roots have disjoint leaf intervals. Therefore cards
/// and branch intervals at the same depth are already ordered vertically.
pub fn build(nodes: &[PlanNode]) -> Result<DiagramLayout, DiagramError> {
    if nodes.is_empty() {
        return Err(DiagramError::Empty);
    }
    if nodes.len() > MAX_DIAGRAM_NODES {
        return Err(DiagramError::TooManyNodes);
    }
    let count = nodes.len();
    let mut geometry = Vec::with_capacity(count);
    let mut stack: Vec<usize> = Vec::new();
    let mut last_child = vec![NONE; count];
    let mut leaves = 0usize;
    let mut max_depth = 0;
    for (index, node) in nodes.iter().enumerate() {
        while stack.last().is_some_and(|&p| nodes[p].subtree_end == index) {
            stack.pop();
        }
        let parent = stack.last().copied();
        if node.subtree_end <= index
            || node.subtree_end > count
            || node.depth != stack.len()
            || parent.is_some_and(|p| node.subtree_end > nodes[p].subtree_end)
        {
            return Err(DiagramError::InvalidHierarchy { index });
        }
        if let Some(parent) = parent {
            last_child[parent] = index as u32;
        }
        geometry.push(NodeGeometry {
            y: 0.0,
            depth: node.depth as u32,
            parent: parent.map_or(NONE, |p| p as u32),
        });
        leaves += usize::from(node.subtree_end == index + 1);
        max_depth = max_depth.max(node.depth);
        stack.push(index);
    }
    let mut first_leaf = vec![0u32; count];
    let mut last_leaf = vec![0u32; count];
    let mut remaining = leaves;
    for index in (0..count).rev() {
        if nodes[index].subtree_end == index + 1 {
            remaining -= 1;
            first_leaf[index] = remaining as u32;
            last_leaf[index] = remaining as u32;
        } else {
            first_leaf[index] = first_leaf[index + 1];
            last_leaf[index] = last_leaf[nodes[index].subtree_end - 1];
        }
        geometry[index].y = (first_leaf[index] + last_leaf[index]) as f64 * STEP_Y * 0.5;
    }
    let depths = max_depth + 1;
    let mut node_counts = vec![0usize; depths];
    let mut branch_counts = vec![0usize; depths];
    for (index, node) in geometry.iter().enumerate() {
        node_counts[node.depth as usize] += 1;
        branch_counts[node.depth as usize] += usize::from(last_child[index] != NONE);
    }
    fn offsets(counts: &[usize]) -> Vec<usize> {
        let mut result = Vec::with_capacity(counts.len() + 1);
        result.push(0);
        for &count in counts {
            result.push(result.last().unwrap() + count);
        }
        result
    }
    let depth_offsets = offsets(&node_counts);
    let branch_offsets = offsets(&branch_counts);
    let mut depth_nodes = vec![0; count];
    let mut branches = vec![Branch::default(); *branch_offsets.last().unwrap()];
    let mut next_node = depth_offsets[..depths].to_vec();
    let mut next_branch = branch_offsets[..depths].to_vec();
    for (index, node) in geometry.iter().enumerate() {
        let depth = node.depth as usize;
        depth_nodes[next_node[depth]] = index as u32;
        next_node[depth] += 1;
        if last_child[index] != NONE {
            branches[next_branch[depth]] = Branch {
                parent: index as u32,
                first: (index + 1) as u32,
                last: last_child[index],
            };
            next_branch[depth] += 1;
        }
    }
    let column_base = depths.next_power_of_two();
    let mut column_tree = vec![ColumnBounds::EMPTY; column_base * 2];
    for depth in 0..depths {
        let range = depth_offsets[depth]..depth_offsets[depth + 1];
        column_tree[column_base + depth] = ColumnBounds {
            min_y: geometry[depth_nodes[range.start] as usize].y,
            max_y: geometry[depth_nodes[range.end - 1] as usize].y + METRICS.card_height,
            count: range.len(),
        };
    }
    for index in (1..column_base).rev() {
        column_tree[index] = column_tree[index * 2].union(column_tree[index * 2 + 1]);
    }
    Ok(DiagramLayout {
        nodes: geometry,
        depth_offsets,
        depth_nodes,
        branch_offsets,
        branches,
        column_tree,
        column_base,
        bounds: WorldRect {
            min: WorldPoint { x: 0.0, y: 0.0 },
            max: WorldPoint {
                x: max_depth as f64 * STEP_X + METRICS.card_width,
                y: (leaves - 1) as f64 * STEP_Y + METRICS.card_height,
            },
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryBudget {
    pub max_nodes: usize,
    pub max_segments: usize,
    pub max_clusters: usize,
    pub max_visits: usize,
}
impl Default for QueryBudget {
    fn default() -> Self {
        Self {
            max_nodes: 512,
            max_segments: 2048,
            max_clusters: 512,
            max_visits: 4096,
        }
    }
}
impl QueryBudget {
    /// Callers cannot accidentally turn a viewport query into unbounded work.
    fn bounded(self) -> Self {
        Self {
            max_nodes: self.max_nodes.clamp(1, 2048),
            max_segments: self.max_segments.clamp(1, 8192),
            max_clusters: self.max_clusters.clamp(1, 2048),
            max_visits: self.max_visits.clamp(1, 16384),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewRequest {
    pub viewport: WorldRect,
    pub scale: f64,
    pub budget: QueryBudget,
}
#[derive(Clone, Copy, Debug)]
pub struct DiagramSegment {
    pub from: WorldPoint,
    pub to: WorldPoint,
    pub parent: NodeId,
    pub child: Option<NodeId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClusterKey {
    /// Exclusive depth range.
    Columns { start: usize, end: usize },
    /// Exclusive range of positions in the depth CSR index, not node IDs.
    Rows {
        depth: usize,
        start: usize,
        end: usize,
    },
}
#[derive(Clone, Copy, Debug)]
pub struct DiagramCluster {
    pub bounds: WorldRect,
    /// Number of operations represented by the whole group, including any
    /// portion outside the viewport. This is not a visible-operation count.
    pub count: usize,
    pub key: ClusterKey,
}
#[derive(Default, Debug)]
pub struct VisibleDiagram {
    pub nodes: Vec<NodeId>,
    pub segments: Vec<DiagramSegment>,
    pub clusters: Vec<DiagramCluster>,
    pub overview: bool,
    /// Refined index/candidate visits, including binary-search comparisons.
    /// Emitting the bounded remaining frontier takes O(max_clusters) separately.
    pub visited: usize,
    pending: Vec<Group>,
}
#[derive(Clone, Copy, Debug)]
enum Group {
    Columns {
        tree: usize,
        start: usize,
        end: usize,
    },
    Rows {
        depth: usize,
        start: usize,
        end: usize,
    },
}
struct Work {
    visited: usize,
    limit: usize,
}
impl Work {
    fn visit(&mut self) -> bool {
        if self.visited == self.limit {
            false
        } else {
            self.visited += 1;
            true
        }
    }
    fn partition(
        &mut self,
        range: Range<usize>,
        mut before: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let (mut low, mut high) = (range.start, range.end);
        while low < high {
            if !self.visit() {
                return None;
            }
            let mid = low + (high - low) / 2;
            if before(mid) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Some(low)
    }
}

impl DiagramLayout {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn bounds(&self) -> WorldRect {
        self.bounds
    }
    pub fn metrics(&self) -> DiagramMetrics {
        METRICS
    }
    pub fn node_rect(&self, id: NodeId) -> Option<WorldRect> {
        let node = self.nodes.get(id)?;
        let x = node.depth as f64 * STEP_X;
        Some(WorldRect {
            min: WorldPoint { x, y: node.y },
            max: WorldPoint {
                x: x + METRICS.card_width,
                y: node.y + METRICS.card_height,
            },
        })
    }
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes
            .get(id)
            .and_then(|node| (node.parent != NONE).then_some(node.parent as usize))
    }
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        let next = id.checked_add(1)?;
        (self.parent(next) == Some(id)).then_some(next)
    }
    pub fn hit_test(&self, point: WorldPoint) -> Option<NodeId> {
        if !point.x.is_finite() || !point.y.is_finite() || !self.bounds.contains(point) {
            return None;
        }
        let depth = (point.x / STEP_X).floor() as usize;
        if depth >= self.depths() || point.x - depth as f64 * STEP_X > METRICS.card_width {
            return None;
        }
        let ids = &self.depth_nodes[self.depth_range(depth)];
        let position =
            ids.partition_point(|&id| self.nodes[id as usize].y + METRICS.card_height < point.y);
        let id = *ids.get(position)? as usize;
        self.node_rect(id)?.contains(point).then_some(id)
    }
    /// Heap capacities owned by the layout, excluding input strings and Vec
    /// headers. This is an allocation-size estimate, not process RSS.
    pub fn allocated_bytes(&self) -> usize {
        self.nodes.capacity() * size_of::<NodeGeometry>()
            + self.depth_offsets.capacity() * size_of::<usize>()
            + self.depth_nodes.capacity() * size_of::<u32>()
            + self.branch_offsets.capacity() * size_of::<usize>()
            + self.branches.capacity() * size_of::<Branch>()
            + self.column_tree.capacity() * size_of::<ColumnBounds>()
    }
    fn depths(&self) -> usize {
        self.depth_offsets.len() - 1
    }
    fn depth_range(&self, depth: usize) -> Range<usize> {
        self.depth_offsets[depth]..self.depth_offsets[depth + 1]
    }
    fn center_y(&self, id: u32) -> f64 {
        self.nodes[id as usize].y + METRICS.card_height * 0.5
    }

    pub fn query_into(&self, request: ViewRequest, output: &mut VisibleDiagram) {
        output.nodes.clear();
        output.segments.clear();
        output.clusters.clear();
        output.pending.clear();
        output.overview = false;
        output.visited = 0;
        let view = request.viewport;
        if !view.finite()
            || view.width() <= 0.0
            || view.height() <= 0.0
            || !request.scale.is_finite()
            || request.scale <= 0.0
            || !self.bounds.intersects(view)
        {
            return;
        }
        let budget = request.budget.bounded();
        let mut work = Work {
            visited: 0,
            limit: budget.max_visits,
        };
        let small =
            METRICS.card_width * request.scale < 96.0 || METRICS.card_height * request.scale < 32.0;
        if small || !self.exact(view, budget, &mut work, output) {
            output.nodes.clear();
            output.segments.clear();
            output.overview = true;
            self.overview(view, request.scale, budget, &mut work, output);
        }
        output.visited = work.visited;
    }

    fn y_range(
        &self,
        depth: usize,
        min_y: f64,
        max_y: f64,
        centers: bool,
        work: &mut Work,
    ) -> Option<Range<usize>> {
        let range = self.depth_range(depth);
        let bottom = if centers {
            METRICS.card_height * 0.5
        } else {
            METRICS.card_height
        };
        let top = if centers {
            METRICS.card_height * 0.5
        } else {
            0.0
        };
        let start = work.partition(range.clone(), |i| {
            self.nodes[self.depth_nodes[i] as usize].y + bottom < min_y
        })?;
        let end = work.partition(start..range.end, |i| {
            self.nodes[self.depth_nodes[i] as usize].y + top <= max_y
        })?;
        Some(start..end)
    }
    fn exact(
        &self,
        view: WorldRect,
        budget: QueryBudget,
        work: &mut Work,
        out: &mut VisibleDiagram,
    ) -> bool {
        for depth in columns(
            view.min.x,
            view.max.x,
            0.0,
            METRICS.card_width,
            self.depths(),
        ) {
            if !work.visit() {
                return false;
            }
            let Some(range) = self.y_range(depth, view.min.y, view.max.y, false, work) else {
                return false;
            };
            if out.nodes.len() + range.len() > budget.max_nodes {
                return false;
            }
            for position in range {
                if !work.visit() {
                    return false;
                }
                out.nodes.push(self.depth_nodes[position] as usize);
            }
        }
        // Query connection lanes independently: the viewport can contain a bus
        // even when neither its parent nor any child card is visible.
        for depth in columns(
            view.min.x,
            view.max.x,
            METRICS.card_width,
            METRICS.column_gap,
            self.depths().saturating_sub(1),
        ) {
            if !work.visit() {
                return false;
            }
            let left = depth as f64 * STEP_X + METRICS.card_width;
            let bus = left + METRICS.column_gap * 0.5;
            let right = (depth + 1) as f64 * STEP_X;
            let branches = self.branch_offsets[depth]..self.branch_offsets[depth + 1];
            if view.min.x <= bus && view.max.x >= left {
                let Some(start) = work.partition(branches.clone(), |i| {
                    self.center_y(self.branches[i].parent) < view.min.y
                }) else {
                    return false;
                };
                let Some(end) = work.partition(start..branches.end, |i| {
                    self.center_y(self.branches[i].parent) <= view.max.y
                }) else {
                    return false;
                };
                for branch in &self.branches[start..end] {
                    let y = self.center_y(branch.parent);
                    if !emit_segment(
                        out,
                        work,
                        budget,
                        DiagramSegment {
                            from: WorldPoint { x: left, y },
                            to: WorldPoint { x: bus, y },
                            parent: branch.parent as usize,
                            child: None,
                        },
                    ) {
                        return false;
                    }
                }
            }
            if view.min.x <= bus && view.max.x >= bus {
                let Some(start) = work.partition(branches.clone(), |i| {
                    self.center_y(self.branches[i].last) < view.min.y
                }) else {
                    return false;
                };
                let Some(end) = work.partition(start..branches.end, |i| {
                    self.center_y(self.branches[i].first) <= view.max.y
                }) else {
                    return false;
                };
                for branch in &self.branches[start..end] {
                    let first = self.center_y(branch.first);
                    let last = self.center_y(branch.last);
                    if first != last
                        && !emit_segment(
                            out,
                            work,
                            budget,
                            DiagramSegment {
                                from: WorldPoint { x: bus, y: first },
                                to: WorldPoint { x: bus, y: last },
                                parent: branch.parent as usize,
                                child: None,
                            },
                        )
                    {
                        return false;
                    }
                }
            }
            if view.min.x <= right && view.max.x >= bus {
                let Some(range) = self.y_range(depth + 1, view.min.y, view.max.y, true, work)
                else {
                    return false;
                };
                for position in range {
                    let child = self.depth_nodes[position];
                    let y = self.center_y(child);
                    if !emit_segment(
                        out,
                        work,
                        budget,
                        DiagramSegment {
                            from: WorldPoint { x: bus, y },
                            to: WorldPoint { x: right, y },
                            parent: self.nodes[child as usize].parent as usize,
                            child: Some(child as usize),
                        },
                    ) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn cluster(&self, group: Group) -> DiagramCluster {
        match group {
            Group::Columns { tree, start, end } => {
                let value = self.column_tree[tree];
                let end = end.min(self.depths());
                DiagramCluster {
                    bounds: WorldRect {
                        min: WorldPoint {
                            x: start as f64 * STEP_X,
                            y: value.min_y,
                        },
                        max: WorldPoint {
                            x: (end.saturating_sub(1)) as f64 * STEP_X + METRICS.card_width,
                            y: value.max_y,
                        },
                    },
                    count: value.count,
                    key: ClusterKey::Columns { start, end },
                }
            }
            Group::Rows { depth, start, end } => DiagramCluster {
                bounds: WorldRect {
                    min: WorldPoint {
                        x: depth as f64 * STEP_X,
                        y: self.nodes[self.depth_nodes[start] as usize].y,
                    },
                    max: WorldPoint {
                        x: depth as f64 * STEP_X + METRICS.card_width,
                        y: self.nodes[self.depth_nodes[end - 1] as usize].y + METRICS.card_height,
                    },
                },
                count: end - start,
                key: ClusterKey::Rows { depth, start, end },
            },
        }
    }
    fn split(&self, group: Group) -> Option<[Group; 2]> {
        let (depth, start, end) = match group {
            Group::Columns { tree, start, end } if end - start > 1 => {
                let mid = start + (end - start) / 2;
                return Some([
                    Group::Columns {
                        tree: tree * 2,
                        start,
                        end: mid,
                    },
                    Group::Columns {
                        tree: tree * 2 + 1,
                        start: mid,
                        end,
                    },
                ]);
            }
            Group::Columns { start, .. } => (
                start,
                self.depth_offsets[start],
                self.depth_offsets[start + 1],
            ),
            Group::Rows { depth, start, end } => (depth, start, end),
        };
        if end - start <= 1 {
            return None;
        }
        let mid = start + (end - start) / 2;
        Some([
            Group::Rows {
                depth,
                start,
                end: mid,
            },
            Group::Rows {
                depth,
                start: mid,
                end,
            },
        ])
    }
    fn overview(
        &self,
        view: WorldRect,
        scale: f64,
        budget: QueryBudget,
        work: &mut Work,
        out: &mut VisibleDiagram,
    ) {
        let mut pending = std::mem::take(&mut out.pending);
        pending.push(Group::Columns {
            tree: 1,
            start: 0,
            end: self.column_base,
        });
        while let Some(group) = pending.pop() {
            let refine = work.visit();
            let cluster = self.cluster(group);
            if cluster.count == 0 || !cluster.bounds.intersects(view) {
                continue;
            }
            let small =
                cluster.bounds.width() * scale <= 8.0 && cluster.bounds.height() * scale <= 8.0;
            // Keep enough output slots to represent the entire remaining
            // frontier. Exhaustion groups data; it never drops pending nodes.
            if refine
                && !small
                && out.clusters.len() + pending.len() + 2 <= budget.max_clusters
                && let Some(children) = self.split(group)
            {
                pending.push(children[1]);
                pending.push(children[0]);
            } else {
                out.clusters.push(cluster);
            }
        }
        out.pending = pending;
    }
}

fn columns(min: f64, max: f64, offset: f64, width: f64, count: usize) -> Range<usize> {
    let first = ((min - offset - width) / STEP_X).ceil().max(0.0) as usize;
    let end = ((max - offset) / STEP_X).floor() + 1.0;
    first.min(count)..(end.max(0.0) as usize).min(count)
}
fn emit_segment(
    out: &mut VisibleDiagram,
    work: &mut Work,
    budget: QueryBudget,
    segment: DiagramSegment,
) -> bool {
    if out.segments.len() == budget.max_segments || !work.visit() {
        return false;
    }
    out.segments.push(segment);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };

    fn plan(depths: &[usize]) -> Vec<PlanNode> {
        let mut nodes: Vec<_> = depths
            .iter()
            .map(|&depth| PlanNode {
                depth,
                subtree_end: depths.len(),
                ..Default::default()
            })
            .collect();
        let mut stack: Vec<usize> = Vec::new();
        for index in 0..nodes.len() {
            while stack
                .last()
                .is_some_and(|&p| nodes[p].depth >= nodes[index].depth)
            {
                nodes[stack.pop().unwrap()].subtree_end = index;
            }
            stack.push(index);
        }
        nodes
    }
    fn shape(kind: &str, count: usize) -> Vec<PlanNode> {
        let depths = match kind {
            "chain" => (0..count).collect(),
            "star" => (0..count).map(|i| usize::from(i != 0)).collect(),
            "forest" => vec![0; count],
            "balanced" => {
                let mut depths = Vec::with_capacity(count);
                let mut pending = vec![(1usize, 0)];
                while let Some((id, depth)) = pending.pop() {
                    depths.push(depth);
                    if id * 2 < count {
                        pending.push((id * 2 + 1, depth + 1));
                    }
                    if id * 2 <= count {
                        pending.push((id * 2, depth + 1));
                    }
                }
                depths
            }
            _ => unreachable!(),
        };
        plan(&depths)
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
    fn query(
        layout: &DiagramLayout,
        viewport: WorldRect,
        scale: f64,
        budget: QueryBudget,
    ) -> VisibleDiagram {
        let mut out = VisibleDiagram::default();
        layout.query_into(
            ViewRequest {
                viewport,
                scale,
                budget,
            },
            &mut out,
        );
        out
    }
    fn covered(layout: &DiagramLayout, output: &VisibleDiagram) {
        assert!(output.overview);
        assert!(output.nodes.is_empty() && output.segments.is_empty());
        let mut seen = vec![0u8; layout.len()];
        for cluster in &output.clusters {
            let range = match cluster.key {
                ClusterKey::Columns { start, end } => {
                    layout.depth_offsets[start]..layout.depth_offsets[end]
                }
                ClusterKey::Rows { start, end, .. } => start..end,
            };
            assert_eq!(cluster.count, range.len());
            for &id in &layout.depth_nodes[range] {
                seen[id as usize] += 1;
                let node = layout.node_rect(id as usize).unwrap();
                assert!(cluster.bounds.contains(node.min) && cluster.bounds.contains(node.max));
            }
        }
        assert!(seen.into_iter().all(|count| count == 1));
    }
    #[test]
    fn hierarchy_layout_is_deterministic_and_preserves_all_roots() {
        let nodes = plan(&[0, 1, 2, 1, 2, 3, 3, 0, 1]);
        let layout = build(&nodes).unwrap();
        let repeated = build(&nodes).unwrap();
        let parents = [
            None,
            Some(0),
            Some(1),
            Some(0),
            Some(3),
            Some(4),
            Some(4),
            None,
            Some(7),
        ];
        assert_eq!(layout.len(), nodes.len());
        assert!(!layout.is_empty());
        for (i, expected) in parents.into_iter().enumerate() {
            assert_eq!(layout.parent(i), expected);
            let node = layout.node_rect(i).unwrap();
            assert_eq!(Some(node), repeated.node_rect(i));
            assert!(layout.bounds.contains(node.min) && layout.bounds.contains(node.max));
            assert_eq!(layout.hit_test(node.center()), Some(i));
            for j in 0..i {
                assert!(!node.intersects(layout.node_rect(j).unwrap()));
            }
        }
        assert_eq!(layout.first_child(0), Some(1));
        assert_eq!(layout.first_child(4), Some(5));
        assert_eq!(layout.first_child(6), None);
        assert_eq!(layout.first_child(usize::MAX), None);
        assert_eq!(layout.node_rect(nodes.len()), None);
        for depth in 0..layout.depths() {
            assert!(
                layout.depth_nodes[layout.depth_range(depth)]
                    .windows(2)
                    .all(
                        |pair| layout.nodes[pair[0] as usize].y + METRICS.card_height
                            < layout.nodes[pair[1] as usize].y
                    )
            );
        }
    }
    #[test]
    fn rejects_invalid_spans_depths_and_size_without_truncating() {
        assert_eq!(build(&[]).unwrap_err(), DiagramError::Empty);
        let mut invalid = vec![plan(&[1]), plan(&[0, 2]), plan(&[0, 1, 1, 0])];
        invalid[2][1].subtree_end = 4; // Crosses its parent's exclusive end.
        let mut zero = plan(&[0]);
        zero[0].subtree_end = 0;
        invalid.push(zero);
        let mut outside = plan(&[0]);
        outside[0].subtree_end = 2;
        invalid.push(outside);
        let mut premature = plan(&[0, 1]);
        premature[0].subtree_end = 1;
        invalid.push(premature);
        for nodes in invalid {
            assert!(matches!(
                build(&nodes),
                Err(DiagramError::InvalidHierarchy { .. })
            ));
        }
        assert_eq!(
            build(&shape("forest", MAX_DIAGRAM_NODES + 1)).unwrap_err(),
            DiagramError::TooManyNodes
        );
    }
    #[test]
    fn hundred_thousand_chain_and_star_are_finite_and_iterative() {
        for kind in ["chain", "star"] {
            let layout = build(&shape(kind, MAX_DIAGRAM_NODES)).unwrap();
            assert!(layout.bounds.finite());
            assert!(layout.allocated_bytes() > 0 && layout.allocated_bytes() < 32 * 1024 * 1024);
            assert_eq!(
                layout.parent(MAX_DIAGRAM_NODES - 1),
                Some(if kind == "chain" {
                    MAX_DIAGRAM_NODES - 2
                } else {
                    0
                })
            );
            let last = layout.node_rect(MAX_DIAGRAM_NODES - 1).unwrap();
            assert_eq!(layout.hit_test(last.center()), Some(MAX_DIAGRAM_NODES - 1));
            if kind == "chain" {
                assert_eq!(last.min.x, (MAX_DIAGRAM_NODES - 1) as f64 * STEP_X);
                assert_eq!(last.min.y, 0.0);
                assert_eq!(last.max.x - last.min.x, METRICS.card_width);
            }
            for budget in [
                QueryBudget {
                    max_clusters: 1,
                    max_visits: 1,
                    ..Default::default()
                },
                QueryBudget {
                    max_clusters: 7,
                    max_visits: 19,
                    ..Default::default()
                },
                QueryBudget::default(),
            ] {
                let out = query(&layout, layout.bounds, 0.000001, budget);
                covered(&layout, &out);
                assert!(
                    out.clusters.len() <= budget.max_clusters && out.visited <= budget.max_visits
                );
            }
        }
    }
    #[test]
    fn bus_crossing_viewport_is_visible_without_either_endpoint() {
        let layout = build(&shape("star", 101)).unwrap();
        let bus = METRICS.card_width + METRICS.column_gap * 0.5;
        let viewport = rect(bus - 1.0, 105.0, 2.0, 8.0);
        let out = query(&layout, viewport, 1.0, QueryBudget::default());
        assert!(!out.overview);
        assert!(out.nodes.is_empty());
        assert_eq!(out.segments.len(), 1);
        let line = out.segments[0];
        assert_eq!(line.parent, 0);
        assert_eq!(line.child, None);
        assert!(!viewport.contains(line.from) && !viewport.contains(line.to));
        assert!(line.from.y < viewport.min.y && line.to.y > viewport.max.y);
    }
    fn segment_key(segment: &DiagramSegment) -> (NodeId, Option<NodeId>, u64, u64, u64, u64) {
        (
            segment.parent,
            segment.child,
            segment.from.x.to_bits(),
            segment.from.y.to_bits(),
            segment.to.x.to_bits(),
            segment.to.y.to_bits(),
        )
    }
    fn all_segments(layout: &DiagramLayout) -> Vec<DiagramSegment> {
        let mut result = Vec::new();
        for parent in 0..layout.len() {
            let children: Vec<_> = (0..layout.len())
                .filter(|&id| layout.parent(id) == Some(parent))
                .collect();
            if children.is_empty() {
                continue;
            }
            let origin = layout.node_rect(parent).unwrap();
            let bus = origin.max.x + METRICS.column_gap * 0.5;
            let y = origin.center().y;
            result.push(DiagramSegment {
                from: WorldPoint { x: origin.max.x, y },
                to: WorldPoint { x: bus, y },
                parent,
                child: None,
            });
            let first = layout.node_rect(children[0]).unwrap().center().y;
            let last = layout
                .node_rect(*children.last().unwrap())
                .unwrap()
                .center()
                .y;
            if first != last {
                result.push(DiagramSegment {
                    from: WorldPoint { x: bus, y: first },
                    to: WorldPoint { x: bus, y: last },
                    parent,
                    child: None,
                });
            }
            for child in children {
                let target = layout.node_rect(child).unwrap();
                let y = target.center().y;
                result.push(DiagramSegment {
                    from: WorldPoint { x: bus, y },
                    to: WorldPoint { x: target.min.x, y },
                    parent,
                    child: Some(child),
                });
            }
        }
        result
    }
    #[test]
    fn indexed_cards_and_edges_match_brute_force_for_irregular_forests() {
        let mut seed = 42u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 32
        };
        for _ in 0..12 {
            let mut depths = vec![0];
            for _ in 1..85 {
                depths.push(random() as usize % (depths.last().unwrap() + 2).min(9));
            }
            let layout = build(&plan(&depths)).unwrap();
            let lines = all_segments(&layout);
            for _ in 0..40 {
                let x = random() as f64 / u32::MAX as f64 * (layout.bounds.width() + 600.0) - 300.0;
                let y =
                    random() as f64 / u32::MAX as f64 * (layout.bounds.height() + 600.0) - 300.0;
                let viewport = rect(
                    x,
                    y,
                    300.0 + (random() % 900) as f64,
                    200.0 + (random() % 600) as f64,
                );
                let out = query(&layout, viewport, 1.0, QueryBudget::default());
                assert!(!out.overview);
                let mut expected: Vec<_> = (0..layout.len())
                    .filter(|&id| layout.node_rect(id).unwrap().intersects(viewport))
                    .collect();
                let mut actual = out.nodes;
                expected.sort_unstable();
                actual.sort_unstable();
                assert_eq!(actual, expected);
                let mut expected: Vec<_> = lines
                    .iter()
                    .filter(|line| {
                        WorldRect {
                            min: line.from,
                            max: line.to,
                        }
                        .intersects(viewport)
                    })
                    .map(segment_key)
                    .collect();
                let mut actual: Vec<_> = out.segments.iter().map(segment_key).collect();
                expected.sort_unstable();
                actual.sort_unstable();
                assert_eq!(actual, expected);
            }
        }
    }
    #[test]
    fn overview_covers_forest_and_balanced_graph_under_every_small_budget() {
        for kind in ["forest", "balanced"] {
            let layout = build(&shape(kind, 301)).unwrap();
            for clusters in [1, 2, 3, 17, 512] {
                for visits in [1, 2, 13, 4096] {
                    let budget = QueryBudget {
                        max_clusters: clusters,
                        max_visits: visits,
                        ..Default::default()
                    };
                    let out = query(&layout, layout.bounds, 0.001, budget);
                    covered(&layout, &out);
                    assert!(out.clusters.len() <= clusters && out.visited <= visits);
                }
            }
        }
    }
    #[test]
    fn readable_query_overflow_falls_back_without_silently_losing_operations() {
        let layout = build(&shape("star", 301)).unwrap();
        for budget in [
            QueryBudget {
                max_nodes: 1,
                ..Default::default()
            },
            QueryBudget {
                max_segments: 1,
                ..Default::default()
            },
            QueryBudget {
                max_visits: 1,
                ..Default::default()
            },
        ] {
            let out = query(&layout, layout.bounds, 1.0, budget);
            covered(&layout, &out);
            assert!(out.visited <= budget.max_visits);
        }
        let out = query(
            &layout,
            layout.bounds,
            0.001,
            QueryBudget {
                max_nodes: 0,
                max_segments: 0,
                max_clusters: 0,
                max_visits: 0,
            },
        );
        covered(&layout, &out);
        assert_eq!(out.clusters.len(), 1);
        assert_eq!(out.visited, 1);
    }
    #[test]
    fn hit_testing_gaps_and_invalid_viewports_are_safe() {
        let layout = build(&shape("star", 3)).unwrap();
        assert_eq!(
            layout.hit_test(WorldPoint {
                x: STEP_X + 20.0,
                y: 110.0
            }),
            None
        );
        assert_eq!(
            layout.hit_test(WorldPoint {
                x: METRICS.card_width + 1.0,
                y: layout.node_rect(0).unwrap().center().y
            }),
            None
        );
        assert_eq!(
            layout.hit_test(WorldPoint {
                x: f64::NAN,
                y: 0.0
            }),
            None
        );
        let node = layout.node_rect(1).unwrap();
        assert_eq!(layout.hit_test(node.min), Some(1));
        assert_eq!(layout.hit_test(node.max), Some(1));
        let mut out = VisibleDiagram::default();
        for (viewport, scale) in [
            (rect(0.0, 0.0, 0.0, 100.0), 1.0),
            (layout.bounds, f64::NAN),
            (layout.bounds, 0.0),
            (rect(f64::INFINITY, 0.0, 1.0, 1.0), 1.0),
        ] {
            layout.query_into(
                ViewRequest {
                    viewport: layout.bounds,
                    scale: 0.1,
                    budget: QueryBudget::default(),
                },
                &mut out,
            );
            layout.query_into(
                ViewRequest {
                    viewport,
                    scale,
                    budget: QueryBudget::default(),
                },
                &mut out,
            );
            assert!(out.nodes.is_empty() && out.segments.is_empty() && out.clusters.is_empty());
            assert_eq!(out.visited, 0);
        }
    }
    #[test]
    fn stationary_queries_reuse_buffers_and_clear_previous_outputs() {
        let layout = build(&shape("balanced", 511)).unwrap();
        let request = ViewRequest {
            viewport: layout.bounds,
            scale: 0.1,
            budget: QueryBudget::default(),
        };
        let mut output = VisibleDiagram::default();
        layout.query_into(request, &mut output);
        let capacities = (
            output.nodes.capacity(),
            output.segments.capacity(),
            output.clusters.capacity(),
            output.pending.capacity(),
        );
        let counts = (output.clusters.len(), output.visited);
        for _ in 0..20 {
            layout.query_into(request, &mut output);
            assert_eq!(
                capacities,
                (
                    output.nodes.capacity(),
                    output.segments.capacity(),
                    output.clusters.capacity(),
                    output.pending.capacity()
                )
            );
            assert_eq!(counts, (output.clusters.len(), output.visited));
        }
        layout.query_into(
            ViewRequest {
                viewport: rect(-500.0, -500.0, 1.0, 1.0),
                ..request
            },
            &mut output,
        );
        assert!(output.clusters.is_empty());
        assert!(!output.overview);
    }
    #[test]
    #[ignore = "release-only layout and indexed viewport benchmark"]
    fn benchmark_diagram() {
        fn median(mut values: Vec<Duration>) -> Duration {
            values.sort();
            values[values.len() / 2]
        }
        for count in [500, 5000, 100000] {
            for kind in ["chain", "star", "balanced", "forest"] {
                let input = shape(kind, count);
                let mut samples = Vec::new();
                for sample in 0..8 {
                    let start = Instant::now();
                    let layout = black_box(build(black_box(&input)).unwrap());
                    let elapsed = start.elapsed();
                    if sample > 0 {
                        samples.push(elapsed);
                    }
                    assert_eq!(layout.len(), count);
                }
                let layout = build(&input).unwrap();
                println!(
                    "diagram shape={kind} nodes={count} layout_median={:?} allocated_bytes={}",
                    median(samples),
                    layout.allocated_bytes()
                );
                let center = layout.node_rect(count / 2).unwrap().center();
                let fit_scale =
                    (1280.0 / layout.bounds.width()).min(720.0 / layout.bounds.height());
                for (name, viewport, scale) in [
                    (
                        "detail",
                        rect(center.x - 640.0, center.y - 360.0, 1280.0, 720.0),
                        1.0,
                    ),
                    ("fit", layout.bounds, fit_scale),
                ] {
                    let request = ViewRequest {
                        viewport,
                        scale,
                        budget: QueryBudget::default(),
                    };
                    let mut out = VisibleDiagram::default();
                    layout.query_into(request, &mut out);
                    let mut samples = Vec::new();
                    for sample in 0..8 {
                        let start = Instant::now();
                        for _ in 0..1000 {
                            layout.query_into(black_box(request), black_box(&mut out));
                            black_box(&out);
                        }
                        if sample > 0 {
                            samples.push(start.elapsed() / 1000);
                        }
                    }
                    println!(
                        "diagram shape={kind} nodes={count} viewport={name} query_median={:?} cards={} segments={} clusters={} visits={} overview={}",
                        median(samples),
                        out.nodes.len(),
                        out.segments.len(),
                        out.clusters.len(),
                        out.visited,
                        out.overview
                    );
                }
                let mut hits = Vec::new();
                for sample in 0..8 {
                    let start = Instant::now();
                    for _ in 0..10000 {
                        black_box(layout.hit_test(black_box(center)));
                    }
                    if sample > 0 {
                        hits.push(start.elapsed() / 10000);
                    }
                }
                println!(
                    "diagram shape={kind} nodes={count} hit_median={:?}",
                    median(hits)
                );
            }
        }
    }
}
