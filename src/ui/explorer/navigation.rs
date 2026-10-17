//! Cached, borrowed-tree navigation for Database Explorer.
//!
//! Rebuild the index after tree structure, expansion, scope, source order, or
//! source labels change. A frame only resolves its visible rows. Row tokens are
//! valid for one index generation; they must never survive a rebuild unchecked.

use crate::model::{DataSource, DbObject, DriverKind, ObjectKind};
use std::collections::HashSet;
use std::ops::Range;

/// Pseudo source ID for data source groups in `expanded` and selection keys.
/// Real IDs are UUIDs or file paths and never contain a control character.
pub(in crate::ui) const GROUP_KEY: &str = "\u{1}group";

/// A stable location until the next index rebuild, without retaining a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::ui) enum SourceSlot {
    Config(usize),
    File(usize),
}

/// Borrowed input assembled from config/files and the corresponding SourceState.
#[derive(Clone, Copy)]
pub(in crate::ui) struct SourceTree<'a> {
    pub source: &'a DataSource,
    /// Group path segments; empty for ungrouped sources and opened files.
    pub group: &'a [String],
    pub slot: SourceSlot,
    pub roots: Option<&'a [DbObject]>,
    pub loading: &'a HashSet<Vec<String>>,
    pub connecting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum RowKind {
    Group,
    Source,
    Object(ObjectKind),
    Loading,
    FilesHeader,
}

#[derive(Clone, Debug)]
pub(in crate::ui) struct VisibleRow {
    pub kind: RowKind,
    /// Actual rendered parent, including synthetic tables/views folders.
    pub parent: Option<usize>,
    pub depth: usize,
    pub expandable: bool,
    pub expanded: bool,
    source: usize,
    /// Index into the group list for group rows.
    group: usize,
    /// Position in the parent's unfiltered children, or in the source roots.
    sibling: usize,
}

impl VisibleRow {
    pub fn selectable(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Group | RowKind::Source | RowKind::Object(_)
        )
    }
}

struct IndexedSource {
    slot: SourceSlot,
    id: String,
    label: String,
}

/// Index identity prevents delayed UI intents from addressing a different row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) struct RowToken {
    generation: u64,
    pub row: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum TreeIntent {
    Select(RowToken),
    Expand(RowToken),
    Collapse(RowToken),
    Open(RowToken),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum HierarchyCommand {
    Left,
    Right,
    Activate,
    EditData,
}

#[derive(Default)]
pub(in crate::ui) struct VisibleIndex {
    generation: u64,
    rows: Vec<VisibleRow>,
    sources: Vec<IndexedSource>,
    groups: Vec<Vec<String>>,
    selected: Option<usize>,
    files_header: Option<usize>,
}

impl VisibleIndex {
    /// Changes on every rebuild; caches derived from the tree key on it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Visits only expanded, scope-visible nodes. No DbObject or path is cloned.
    /// Hidden selections deliberately become None, never a different object.
    /// `groups` lists every group path (parents before children); within a
    /// group, subgroups come first, then sources in their configured order.
    pub fn rebuild<'a>(
        &mut self,
        sources: impl IntoIterator<Item = SourceTree<'a>>,
        groups: &[Vec<String>],
        expanded: &HashSet<(String, Vec<String>)>,
        selected: Option<(&str, &[String])>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        self.rows.clear();
        self.sources.clear();
        self.groups.clear();
        self.selected = None;
        self.files_header = None;
        // HashSet<(String, Vec<String>)> cannot look up a borrowed tuple directly.
        // Build a borrowed lookup once rather than allocating a key per node.
        let expanded: HashSet<(&str, &[String])> = expanded
            .iter()
            .map(|(id, path)| (id.as_str(), path.as_slice()))
            .collect();
        let trees: Vec<SourceTree<'a>> = sources.into_iter().collect();
        self.add_group_level(&trees, groups, &[], None, 0, &expanded, selected);
        for tree in trees.iter().filter(|tree| matches!(tree.slot, SourceSlot::File(_))) {
            if self.files_header.is_none() {
                self.files_header = Some(self.rows.len());
                self.rows.push(VisibleRow {
                    kind: RowKind::FilesHeader,
                    parent: None,
                    depth: 0,
                    expandable: false,
                    expanded: false,
                    source: usize::MAX,
                    group: usize::MAX,
                    sibling: 0,
                });
            }
            self.add_source(*tree, None, 0, &expanded, selected);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_group_level<'a>(
        &mut self,
        trees: &[SourceTree<'a>],
        groups: &[Vec<String>],
        prefix: &[String],
        parent: Option<usize>,
        depth: usize,
        expanded: &HashSet<(&str, &[String])>,
        selected: Option<(&str, &[String])>,
    ) {
        for group in groups
            .iter()
            .filter(|group| group.len() == prefix.len() + 1 && group.starts_with(prefix))
        {
            let row = self.rows.len();
            let open = expanded.contains(&(GROUP_KEY, group.as_slice()));
            self.groups.push(group.clone());
            self.rows.push(VisibleRow {
                kind: RowKind::Group,
                parent,
                depth,
                expandable: true,
                expanded: open,
                source: usize::MAX,
                group: self.groups.len() - 1,
                sibling: 0,
            });
            if selected == Some((GROUP_KEY, group.as_slice())) {
                self.selected = Some(row);
            }
            if open {
                self.add_group_level(trees, groups, group, Some(row), depth + 1, expanded, selected);
            }
        }
        for tree in trees
            .iter()
            .filter(|tree| matches!(tree.slot, SourceSlot::Config(_)) && tree.group == prefix)
        {
            self.add_source(*tree, parent, depth, expanded, selected);
        }
    }

    fn add_source(
        &mut self,
        tree: SourceTree<'_>,
        parent: Option<usize>,
        depth: usize,
        expanded: &HashSet<(&str, &[String])>,
        selected: Option<(&str, &[String])>,
    ) {
        let source = self.sources.len();
        self.sources.push(IndexedSource {
            slot: tree.slot,
            id: tree.source.id.clone(),
            label: tree.source.display_name(),
        });
        let row = self.rows.len();
        let open = expanded.contains(&(tree.source.id.as_str(), &[][..]));
        self.rows.push(VisibleRow {
            kind: RowKind::Source,
            parent,
            depth,
            expandable: true,
            expanded: open,
            source,
            group: usize::MAX,
            sibling: 0,
        });
        if selected == Some((tree.source.id.as_str(), &[][..])) {
            self.selected = Some(row);
        }
        if open {
            if let Some(roots) = tree.roots {
                for (sibling, node) in roots.iter().enumerate() {
                    if root_visible(tree.source, node) {
                        self.add_node(tree, node, sibling, row, expanded, selected);
                    }
                }
            } else if tree.connecting || tree.loading.contains(&[][..]) {
                self.add_loading(row);
            }
        }
    }

    fn add_node(
        &mut self,
        tree: SourceTree<'_>,
        node: &DbObject,
        sibling: usize,
        parent: usize,
        expanded: &HashSet<(&str, &[String])>,
        selected: Option<(&str, &[String])>,
    ) {
        let row = self.rows.len();
        let open =
            node.expandable && expanded.contains(&(tree.source.id.as_str(), node.path.as_slice()));
        let source = self.rows[parent].source;
        let depth = self.rows[parent].depth + 1;
        self.rows.push(VisibleRow {
            kind: RowKind::Object(node.kind),
            parent: Some(parent),
            depth,
            expandable: node.expandable,
            expanded: open,
            source,
            group: usize::MAX,
            sibling,
        });
        if selected == Some((tree.source.id.as_str(), node.path.as_slice())) {
            self.selected = Some(row);
        }
        if !open {
            return;
        }
        if tree.loading.contains(&node.path) {
            self.add_loading(row);
        }
        if let Some(children) = &node.children {
            for (sibling, child) in children.iter().enumerate() {
                if child_visible(tree.source, node, child) {
                    self.add_node(tree, child, sibling, row, expanded, selected);
                }
            }
        }
    }

    fn add_loading(&mut self, parent: usize) {
        self.rows.push(VisibleRow {
            kind: RowKind::Loading,
            parent: Some(parent),
            depth: self.rows[parent].depth + 1,
            expandable: false,
            expanded: false,
            source: self.rows[parent].source,
            group: usize::MAX,
            sibling: 0,
        });
    }

    pub fn rows(&self) -> &[VisibleRow] {
        &self.rows
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn select(&mut self, token: RowToken) -> bool {
        if self.valid(token) && self.rows[token.row].selectable() {
            self.selected = Some(token.row);
            true
        } else {
            false
        }
    }

    pub fn token(&self, row: usize) -> Option<RowToken> {
        self.rows.get(row).map(|_| RowToken {
            generation: self.generation,
            row,
        })
    }

    pub fn valid(&self, token: RowToken) -> bool {
        token.generation == self.generation && token.row < self.rows.len()
    }

    pub fn source_slot(&self, row: usize) -> Option<SourceSlot> {
        Some(self.sources.get(self.rows.get(row)?.source)?.slot)
    }

    pub fn source_id(&self, row: usize) -> Option<&str> {
        Some(&self.sources.get(self.rows.get(row)?.source)?.id)
    }

    pub fn source_label(&self, row: usize) -> Option<&str> {
        Some(&self.sources.get(self.rows.get(row)?.source)?.label)
    }

    /// Full path of a group row.
    pub fn group_path(&self, row: usize) -> Option<&[String]> {
        let current = self.rows.get(row)?;
        (current.kind == RowKind::Group)
            .then(|| self.groups.get(current.group).map(Vec::as_slice))
            .flatten()
    }

    /// Resolve in O(depth), without allocation or scanning siblings. The caller
    /// must rebuild before passing roots that changed since the previous rebuild.
    pub fn object<'a>(&self, row: usize, roots: &'a [DbObject]) -> Option<&'a DbObject> {
        let current = self.rows.get(row)?;
        if !matches!(current.kind, RowKind::Object(_)) {
            return None;
        }
        let parent = current.parent?;
        let children = match self.rows.get(parent)?.kind {
            RowKind::Source => roots,
            RowKind::Object(_) => self.object(parent, roots)?.children.as_deref()?,
            _ => return None,
        };
        children.get(current.sibling)
    }

    fn hierarchy(&self, row: usize, command: HierarchyCommand) -> Option<TreeIntent> {
        let current = self.rows.get(row)?;
        if !current.selectable() {
            return None;
        }
        let token = self.token(row)?;
        match command {
            HierarchyCommand::Left if current.expanded => Some(TreeIntent::Collapse(token)),
            HierarchyCommand::Left => Some(TreeIntent::Select(self.token(current.parent?)?)),
            HierarchyCommand::Right if current.expandable && !current.expanded => {
                Some(TreeIntent::Expand(token))
            }
            HierarchyCommand::Right => {
                let mut next = row + 1;
                if self
                    .rows
                    .get(next)
                    .is_some_and(|row| row.kind == RowKind::Loading)
                {
                    next += 1;
                }
                if self.rows.get(next)?.parent == Some(row) {
                    Some(TreeIntent::Select(self.token(next)?))
                } else {
                    None
                }
            }
            HierarchyCommand::Activate | HierarchyCommand::EditData
                if matches!(
                    current.kind,
                    RowKind::Object(ObjectKind::Table | ObjectKind::View)
                ) =>
            {
                Some(TreeIntent::Open(token))
            }
            HierarchyCommand::Activate if current.expandable => Some(if current.expanded {
                TreeIntent::Collapse(token)
            } else {
                TreeIntent::Expand(token)
            }),
            _ => None,
        }
    }
}

fn root_visible(source: &DataSource, node: &DbObject) -> bool {
    match (source.driver, node.kind) {
        (DriverKind::Postgres, ObjectKind::Database) => {
            source.shows_database(&node.name, node.detail == crate::drivers::DEFAULT_DATABASE)
        }
        (DriverKind::MySql, ObjectKind::Schema) => source.shows_database(
            &node.name,
            source.database.is_empty() || source.database == node.name,
        ),
        _ => true,
    }
}

fn child_visible(source: &DataSource, parent: &DbObject, child: &DbObject) -> bool {
    source.driver != DriverKind::Postgres
        || parent.kind != ObjectKind::Database
        || source.shows_schema(&parent.name, &child.name)
}

/// A directional move in the currently displayed (possibly filtered) projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum SelectionMove {
    Previous,
    Next,
    First,
    Last,
    PageUp(usize),
    PageDown(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::ui) struct SearchHit {
    pub row: usize,
    /// UTF-8 byte ranges, always on original character boundaries.
    pub highlights: Vec<Range<usize>>,
}

/// Ordered, case-insensitive subsequence search: `fa` matches `film_actor`.
///
/// Matching compares Unicode lowercase expansions of individual original
/// scalars. It does not normalize accents or equate one scalar with several
/// query scalars (`ß` is not `ss`; `İ` is not plain `i`). This preserves complete
/// original-character highlights, with no byte indexing assumptions.
#[derive(Default)]
struct SpeedPattern {
    scalars: Vec<std::char::ToLowercase>,
}

impl SpeedPattern {
    fn new(query: &str) -> Self {
        Self {
            scalars: query.chars().map(char::to_lowercase).collect(),
        }
    }

    fn match_into(&self, text: &str, ranges: &mut Vec<Range<usize>>) -> bool {
        ranges.clear();
        if self.scalars.is_empty() {
            return false;
        }
        let mut matched = 0;
        for (byte, scalar) in text.char_indices() {
            if scalar.to_lowercase().eq(self.scalars[matched].clone()) {
                let end = byte + scalar.len_utf8();
                if let Some(last) = ranges.last_mut()
                    && last.end == byte
                {
                    last.end = end;
                } else {
                    ranges.push(byte..end);
                }
                matched += 1;
                if matched == self.scalars.len() {
                    return true;
                }
            }
        }
        ranges.clear();
        false
    }
}

/// A search projection rebuilt only when query, filtering, or index changes.
/// Accessors and selection moves do not allocate or scan the whole tree.
#[derive(Default)]
pub(in crate::ui) struct SpeedSearch {
    generation: Option<u64>,
    query: String,
    filtering: bool,
    pattern: SpeedPattern,
    rows: Vec<usize>,
    selectable: Vec<usize>,
    // usize::MAX is absent. Option<usize> would double these dense arrays.
    positions: Vec<usize>,
    selectable_positions: Vec<usize>,
    hit_positions: Vec<usize>,
    hits: Vec<SearchHit>,
}

impl SpeedSearch {
    /// `roots` borrows the current tree for a source slot. Missing roots are
    /// valid for disconnected sources; their cached display names still match.
    /// Returns false when no work was needed, without calling the resolver.
    pub fn update<'a>(
        &mut self,
        index: &VisibleIndex,
        query: &str,
        filtering: bool,
        roots: impl Fn(SourceSlot) -> Option<&'a [DbObject]>,
    ) -> bool {
        if self.generation == Some(index.generation)
            && self.query == query
            && self.filtering == filtering
        {
            return false;
        }
        if self.query != query {
            self.query.clear();
            self.query.push_str(query);
            self.pattern = SpeedPattern::new(query);
        }
        self.generation = Some(index.generation);
        self.filtering = filtering;
        self.rows.clear();
        self.selectable.clear();
        self.hits.clear();
        self.positions.clear();
        self.positions.resize(index.rows.len(), usize::MAX);
        self.selectable_positions.clear();
        self.selectable_positions
            .resize(index.rows.len(), usize::MAX);
        self.hit_positions.clear();
        self.hit_positions.resize(index.rows.len(), usize::MAX);
        let mut keep = vec![!filtering || query.is_empty(); index.rows.len()];
        let mut ranges = Vec::with_capacity(self.pattern.scalars.len());
        if !query.is_empty() {
            for (row, current) in index.rows.iter().enumerate() {
                let name = match current.kind {
                    RowKind::Group => index
                        .group_path(row)
                        .and_then(|path| path.last())
                        .map(String::as_str),
                    RowKind::Source => index.source_label(row),
                    RowKind::Object(_) => index
                        .source_slot(row)
                        .and_then(&roots)
                        .and_then(|nodes| index.object(row, nodes))
                        .map(|node| node.name.as_str()),
                    _ => None,
                };
                if name.is_some_and(|name| self.pattern.match_into(name, &mut ranges)) {
                    keep[row] = true;
                    self.hit_positions[row] = self.hits.len();
                    self.hits.push(SearchHit {
                        row,
                        highlights: ranges.clone(),
                    });
                }
            }
        }
        if filtering && !query.is_empty() {
            // One reverse pass retains all ancestors, even with many deep hits.
            for row in (0..index.rows.len()).rev() {
                if keep[row] {
                    if let Some(parent) = index.rows[row].parent {
                        keep[parent] = true;
                    }
                    if matches!(index.source_slot(row), Some(SourceSlot::File(_)))
                        && let Some(header) = index.files_header
                    {
                        keep[header] = true;
                    }
                }
            }
        }
        for (row, current) in index.rows.iter().enumerate() {
            if keep[row] {
                self.positions[row] = self.rows.len();
                self.rows.push(row);
                if current.selectable() {
                    self.selectable_positions[row] = self.selectable.len();
                    self.selectable.push(row);
                }
            }
        }
        true
    }

    pub fn rows(&self) -> &[usize] {
        &self.rows
    }

    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    pub fn highlights(&self, row: usize) -> &[Range<usize>] {
        self.hit_positions
            .get(row)
            .copied()
            .filter(|&position| position != usize::MAX)
            .map_or(&[], |hit| self.hits[hit].highlights.as_slice())
    }

    pub fn display_position(&self, row: usize) -> Option<usize> {
        self.positions
            .get(row)
            .copied()
            .filter(|&position| position != usize::MAX)
    }

    /// Hierarchy navigation must use this projection, so Right never selects a
    /// child hidden by Speed Search Filter. Stale projections cannot act.
    pub fn hierarchy(
        &self,
        index: &VisibleIndex,
        row: usize,
        command: HierarchyCommand,
    ) -> Option<TreeIntent> {
        if self.generation != Some(index.generation) || self.display_position(row).is_none() {
            return None;
        }
        let current = index.rows.get(row)?;
        if command == HierarchyCommand::Right && current.expanded {
            let position = self
                .selectable_positions
                .get(row)
                .copied()
                .filter(|&position| position != usize::MAX)?;
            let child = *self.selectable.get(position + 1)?;
            return (index.rows[child].parent == Some(row))
                .then(|| index.token(child).map(TreeIntent::Select))
                .flatten();
        }
        index.hierarchy(row, command)
    }

    pub fn move_selection(
        &self,
        current: Option<usize>,
        direction: SelectionMove,
    ) -> Option<usize> {
        let last = self.selectable.len().checked_sub(1)?;
        let current = current.and_then(|row| {
            self.selectable_positions
                .get(row)
                .copied()
                .filter(|&position| position != usize::MAX)
        });
        let position = match direction {
            SelectionMove::First => 0,
            SelectionMove::Last => last,
            SelectionMove::Previous => current.map_or(last, |i| i.saturating_sub(1)),
            SelectionMove::Next => current.map_or(0, |i| (i + 1).min(last)),
            SelectionMove::PageUp(rows) => current.map_or(last, |i| i.saturating_sub(rows.max(1))),
            SelectionMove::PageDown(rows) => {
                current.map_or(0, |i| i.saturating_add(rows.max(1)).min(last))
            }
        };
        Some(self.selectable[position])
    }

    /// Move among matches, wrapping at either end. Context ancestors are skipped.
    pub fn next_match(&self, current: Option<usize>, backwards: bool) -> Option<usize> {
        let last = self.hits.len().checked_sub(1)?;
        let position = match current {
            None => {
                if backwards {
                    last
                } else {
                    0
                }
            }
            Some(row) => match self.hits.binary_search_by_key(&row, |hit| hit.row) {
                Ok(position) if backwards => position.checked_sub(1).unwrap_or(last),
                Ok(position) => (position + 1) % self.hits.len(),
                Err(position) if backwards => position.checked_sub(1).unwrap_or(last),
                Err(position) => position % self.hits.len(),
            },
        };
        Some(self.hits[position].row)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) struct RevealToken(u64);

#[derive(Debug)]
pub(in crate::ui) struct RevealRequest {
    pub token: RevealToken,
    pub tab_id: u64,
    pub source_id: String,
    pub path: Vec<String>,
}

/// One action per rebuild/event. No background work is started by this module.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::ui) enum RevealStep {
    Expand(Vec<String>),
    /// Empty path asks the caller to connect/load source roots.
    Load(Vec<String>),
    Select(Vec<String>),
    Waiting,
    HiddenByScope,
    NotFound,
}

#[derive(Default)]
pub(in crate::ui) struct RevealState {
    sequence: u64,
    pending: Option<RevealRequest>,
}

impl RevealState {
    pub fn begin(&mut self, tab_id: u64, source_id: String, path: Vec<String>) -> RevealToken {
        self.sequence = self.sequence.wrapping_add(1);
        let token = RevealToken(self.sequence);
        self.pending = Some(RevealRequest {
            token,
            tab_id,
            source_id,
            path,
        });
        token
    }

    pub fn pending(&self) -> Option<&RevealRequest> {
        self.pending.as_ref()
    }

    /// Call on Collapse All, active-editor changes, source removal, or user
    /// navigation. Late Children events may update metadata, but cannot reveal.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn finish(&mut self, token: RevealToken) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|request| request.token == token)
        {
            self.pending = None;
            true
        } else {
            false
        }
    }
}

impl RevealRequest {
    /// Plan only for this request's source. Existing scopes are never changed.
    /// Paths come from actual tree ancestry, including synthetic folders.
    /// Invoke after relevant metadata/expansion changes, never every idle frame.
    /// The caller must cancel the request on a failed metadata/connection event:
    /// an absent child list alone cannot distinguish failure from an initial load.
    pub fn step(
        &self,
        tree: SourceTree<'_>,
        expanded: &HashSet<(String, Vec<String>)>,
    ) -> RevealStep {
        if tree.source.id != self.source_id {
            return RevealStep::NotFound;
        }
        if self.path.is_empty() {
            return RevealStep::Select(Vec::new());
        }
        let route = tree
            .roots
            .map(|roots| find_route(tree.source, roots, &self.path));
        if matches!(route, Some(Route::Hidden)) {
            return RevealStep::HiddenByScope;
        }
        if matches!(route, Some(Route::Missing)) {
            return RevealStep::NotFound;
        }
        // This runs on an explicit reveal/event, and probes only its ancestry.
        // A few short owned lookup keys are cheaper than rebuilding a borrowed
        // lookup for every expanded object as the request progresses.
        let is_open = |path: &[String]| expanded.contains(&(self.source_id.clone(), path.to_vec()));
        if !is_open(&[]) {
            return RevealStep::Expand(Vec::new());
        }
        match route {
            None if tree.connecting || tree.loading.contains(&[][..]) => RevealStep::Waiting,
            None => RevealStep::Load(Vec::new()),
            Some(Route::Found(nodes)) => {
                for node in nodes.iter().take(nodes.len().saturating_sub(1)) {
                    if !is_open(&node.path) {
                        return RevealStep::Expand(node.path.clone());
                    }
                }
                RevealStep::Select(self.path.clone())
            }
            Some(Route::Unloaded(nodes)) => {
                for node in &nodes {
                    if !is_open(&node.path) {
                        return RevealStep::Expand(node.path.clone());
                    }
                }
                let node = nodes.last().expect("unloaded route includes its node");
                if tree.loading.contains(&node.path) {
                    RevealStep::Waiting
                } else {
                    RevealStep::Load(node.path.clone())
                }
            }
            Some(Route::Hidden | Route::Missing) => unreachable!(),
        }
    }
}

enum Route<'a> {
    Found(Vec<&'a DbObject>),
    Unloaded(Vec<&'a DbObject>),
    Hidden,
    Missing,
}

fn find_route<'a>(source: &DataSource, roots: &'a [DbObject], target: &[String]) -> Route<'a> {
    fn visit<'a>(source: &DataSource, node: &'a DbObject, target: &[String]) -> Route<'a> {
        if node.path == target {
            return Route::Found(vec![node]);
        }
        if node.kind != ObjectKind::Folder && !target.starts_with(&node.path) {
            return Route::Missing;
        }
        let Some(children) = &node.children else {
            return if node.expandable && node.kind != ObjectKind::Folder {
                Route::Unloaded(vec![node])
            } else {
                Route::Missing
            };
        };
        for child in children {
            if !child_visible(source, node, child) {
                if target.starts_with(&child.path) {
                    return Route::Hidden;
                }
                continue;
            }
            match visit(source, child, target) {
                Route::Found(mut route) => {
                    route.insert(0, node);
                    return Route::Found(route);
                }
                Route::Unloaded(mut route) => {
                    route.insert(0, node);
                    return Route::Unloaded(route);
                }
                Route::Hidden => return Route::Hidden,
                Route::Missing => {}
            }
        }
        Route::Missing
    }
    for root in roots {
        if !root_visible(source, root) {
            if target.starts_with(&root.path) {
                return Route::Hidden;
            }
            continue;
        }
        match visit(source, root, target) {
            Route::Missing => {}
            route => return route,
        }
    }
    Route::Missing
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        source: DataSource,
        roots: Vec<DbObject>,
        loading: HashSet<Vec<String>>,
        expanded: HashSet<(String, Vec<String>)>,
    }

    impl Fixture {
        fn sqlite() -> Self {
            let mut source = DataSource::new(DriverKind::Sqlite);
            source.id = "sqlite".into();
            source.name = "Library".into();
            let mut schema = DbObject::new(ObjectKind::Schema, "main", &[]);
            let tables = ["film_actor", "country", "ŻÓŁĆ", "東京"]
                .map(|name| DbObject::new(ObjectKind::Table, name, &schema.path));
            schema.children = Some(vec![DbObject::folder(
                "tables",
                &schema.path,
                tables.into(),
            )]);
            Self {
                source,
                roots: vec![schema],
                loading: HashSet::new(),
                expanded: HashSet::new(),
            }
        }

        fn tree(&self) -> SourceTree<'_> {
            SourceTree {
                group: &[],
                source: &self.source,
                slot: SourceSlot::Config(0),
                roots: Some(&self.roots),
                loading: &self.loading,
                connecting: false,
            }
        }

        fn open_all(&mut self) {
            self.expanded.insert((self.source.id.clone(), Vec::new()));
            fn add(
                nodes: &[DbObject],
                source: &str,
                expanded: &mut HashSet<(String, Vec<String>)>,
            ) {
                for node in nodes {
                    if node.expandable {
                        expanded.insert((source.into(), node.path.clone()));
                    }
                    if let Some(children) = &node.children {
                        add(children, source, expanded);
                    }
                }
            }
            add(&self.roots, &self.source.id, &mut self.expanded);
        }

        fn index(&self) -> VisibleIndex {
            let mut index = VisibleIndex::default();
            index.rebuild([self.tree()], &[], &self.expanded, None);
            index
        }

        fn search(&self, index: &VisibleIndex, query: &str, filtering: bool) -> SpeedSearch {
            let mut search = SpeedSearch::default();
            search.update(index, query, filtering, |_| Some(&self.roots));
            search
        }
    }

    #[test]
    fn nested_groups_hide_collapsed_sources_and_navigate_like_folders() {
        let mut fixture = Fixture::sqlite();
        let mut other = DataSource::new(DriverKind::Sqlite);
        other.id = "other".into();
        other.name = "Loose".into();
        let group = vec!["Clients".to_string(), "Acme".to_string()];
        let groups = vec![vec!["Clients".to_string()], group.clone()];
        let rebuild = |fixture: &Fixture, other: &DataSource, selected: Option<(&str, &[String])>| {
            let mut index = VisibleIndex::default();
            let empty = HashSet::new();
            index.rebuild(
                [
                    SourceTree {
                        group: &group,
                        ..fixture.tree()
                    },
                    SourceTree {
                        source: other,
                        group: &[],
                        slot: SourceSlot::Config(1),
                        roots: None,
                        loading: &empty,
                        connecting: false,
                    },
                ],
                &groups,
                &fixture.expanded,
                selected,
            );
            index
        };
        // Collapsed: only the top group and the ungrouped source are visible.
        let index = rebuild(&fixture, &other, None);
        let kinds: Vec<_> = index.rows().iter().map(|row| row.kind).collect();
        assert_eq!(kinds, [RowKind::Group, RowKind::Source]);
        assert_eq!(index.group_path(0), Some(&groups[0][..]));
        assert_eq!(index.source_label(1), Some("Loose"));

        fixture.expanded.insert((GROUP_KEY.into(), groups[0].clone()));
        fixture.expanded.insert((GROUP_KEY.into(), group.clone()));
        let index = rebuild(&fixture, &other, Some((GROUP_KEY, &group)));
        let kinds: Vec<_> = index.rows().iter().map(|row| (row.kind, row.depth)).collect();
        assert_eq!(
            kinds,
            [(RowKind::Group, 0), (RowKind::Group, 1), (RowKind::Source, 2), (RowKind::Source, 0)]
        );
        assert_eq!(index.selected(), Some(1));
        // Left from a grouped source selects its group, as with any parent.
        assert_eq!(
            index.hierarchy(2, HierarchyCommand::Left),
            Some(TreeIntent::Select(index.token(1).unwrap()))
        );
        assert_eq!(
            index.hierarchy(1, HierarchyCommand::Left),
            Some(TreeIntent::Collapse(index.token(1).unwrap()))
        );
        // Speed Search matches group names and keeps ancestors when filtering.
        let search = fixture.search(&index, "acme", true);
        assert_eq!(search.hits()[0].row, 1);
        assert_eq!(search.rows(), &[0, 1]);
        let search = fixture.search(&index, "library", true);
        assert_eq!(search.rows(), &[0, 1, 2]);
    }

    #[test]
    fn actual_parent_keeps_synthetic_folder_and_borrowed_identity() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        let mut index = fixture.index();
        assert_eq!(index.rows().len(), 7);
        assert_eq!(index.rows()[3].parent, Some(2));
        assert_eq!(index.rows()[2].parent, Some(1));
        let table = &fixture.roots[0].children.as_ref().unwrap()[0]
            .children
            .as_ref()
            .unwrap()[0];
        assert!(std::ptr::eq(
            index.object(3, &fixture.roots).unwrap(),
            table
        ));
        assert_eq!(table.path, ["main", "film_actor"]);
        // Closing the table first makes Left choose its folder, not the schema.
        index.rows[3].expanded = false;
        assert_eq!(
            index.hierarchy(3, HierarchyCommand::Left),
            Some(TreeIntent::Select(index.token(2).unwrap()))
        );
        assert_eq!(
            index.hierarchy(2, HierarchyCommand::Right),
            Some(TreeIntent::Select(index.token(3).unwrap()))
        );
        assert_eq!(
            index.hierarchy(3, HierarchyCommand::Activate),
            Some(TreeIntent::Open(index.token(3).unwrap()))
        );
        assert_eq!(index.hierarchy(2, HierarchyCommand::EditData), None);
    }

    #[test]
    fn stale_tokens_rejected_and_hidden_selection_does_not_move_to_other_object() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        let mut index = fixture.index();
        let old = index.token(3).unwrap();
        assert!(index.select(old));
        fixture.expanded.clear();
        index.rebuild(
            [fixture.tree()], &[],
            &fixture.expanded,
            Some((&fixture.source.id, &["main".into(), "film_actor".into()])),
        );
        assert_eq!(index.rows().len(), 1);
        assert_eq!(index.selected(), None);
        assert!(!index.select(old));
        assert!(!index.valid(old));
        assert_eq!(
            index.hierarchy(0, HierarchyCommand::Right),
            Some(TreeIntent::Expand(index.token(0).unwrap()))
        );
        assert_eq!(index.hierarchy(0, HierarchyCommand::Left), None);
    }

    #[test]
    fn source_selection_and_reordering_resolve_by_slot_after_rebuild() {
        let fixture = Fixture::sqlite();
        let mut index = VisibleIndex::default();
        let tree = SourceTree {
            slot: SourceSlot::Config(9),
            ..fixture.tree()
        };
        index.rebuild([tree], &[], &fixture.expanded, Some((&fixture.source.id, &[])));
        assert_eq!(index.selected(), Some(0));
        assert_eq!(index.source_slot(0), Some(SourceSlot::Config(9)));
        assert_eq!(index.source_id(0), Some("sqlite"));
        assert_eq!(index.source_label(0), Some("Library"));
        assert_eq!(index.object(0, &fixture.roots).map(|n| &n.path), None);
    }

    #[test]
    fn loading_and_file_header_are_nonselectable() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        fixture.loading.insert(fixture.roots[0].path.clone());
        let mut index = VisibleIndex::default();
        index.rebuild(
            [SourceTree {
                slot: SourceSlot::File(0),
                ..fixture.tree()
            }], &[],
            &fixture.expanded,
            None,
        );
        assert_eq!(index.rows()[0].kind, RowKind::FilesHeader);
        assert_eq!(index.rows()[3].kind, RowKind::Loading);
        assert!(!index.select(index.token(0).unwrap()));
        assert!(!index.select(index.token(3).unwrap()));
        assert_eq!(
            index.hierarchy(2, HierarchyCommand::Right),
            Some(TreeIntent::Select(index.token(4).unwrap()))
        );
        let search = fixture.search(&index, "", false);
        assert_eq!(search.move_selection(None, SelectionMove::First), Some(1));
        assert_eq!(search.move_selection(Some(2), SelectionMove::Next), Some(4));
        assert_eq!(
            search.move_selection(Some(4), SelectionMove::Previous),
            Some(2)
        );
    }

    #[test]
    fn postgres_scope_and_original_sibling_indexes_are_preserved() {
        let mut fixture = Fixture::sqlite();
        fixture.source.driver = DriverKind::Postgres;
        let mut hidden = DbObject::new(ObjectKind::Database, "other", &[]);
        hidden.children = Some(Vec::new());
        let mut shown = DbObject::new(ObjectKind::Database, "current", &[])
            .with_detail(crate::drivers::DEFAULT_DATABASE);
        shown.children = Some(vec![
            DbObject::new(ObjectKind::Schema, "private", &shown.path),
            DbObject::new(ObjectKind::Schema, "public", &shown.path),
        ]);
        fixture
            .source
            .scope
            .hidden_schemas
            .insert("current".into(), vec!["private".into()]);
        fixture.roots = vec![hidden, shown];
        fixture.open_all();
        let index = fixture.index();
        assert_eq!(index.rows().len(), 3);
        assert_eq!(index.object(1, &fixture.roots).unwrap().name, "current");
        assert_eq!(index.object(2, &fixture.roots).unwrap().name, "public");
    }

    #[test]
    fn mysql_default_and_explicit_database_scopes_match_explorer() {
        let mut fixture = Fixture::sqlite();
        fixture.source.driver = DriverKind::MySql;
        fixture.source.database = "default".into();
        fixture.source.scope.databases = vec!["extra".into()];
        fixture.roots = ["hidden", "extra", "default"]
            .map(|name| DbObject::new(ObjectKind::Schema, name, &[]))
            .into();
        fixture.open_all();
        let index = fixture.index();
        assert_eq!(index.rows().len(), 3);
        assert_eq!(index.object(1, &fixture.roots).unwrap().name, "extra");
        assert_eq!(index.object(2, &fixture.roots).unwrap().name, "default");
    }

    #[test]
    fn speed_search_only_expanded_nodes_and_ancestors_stay_when_filtering() {
        let mut fixture = Fixture::sqlite();
        let index = fixture.index();
        assert!(fixture.search(&index, "fa", false).hits().is_empty());
        fixture.open_all();
        let index = fixture.index();
        let search = fixture.search(&index, "fa", true);
        assert_eq!(
            search.hits(),
            &[SearchHit {
                row: 3,
                highlights: vec![0..1, 5..6]
            }]
        );
        assert_eq!(search.rows(), &[0, 1, 2, 3]);
        assert_eq!(search.display_position(3), Some(3));
        assert_eq!(search.display_position(4), None);
        assert_eq!(search.highlights(3), &[0..1, 5..6]);
        assert!(search.highlights(4).is_empty());
        assert!(fixture.search(&index, "zzzzz", true).rows().is_empty());
        assert_eq!(
            fixture.search(&index, "", true).rows().len(),
            index.rows().len()
        );
    }

    #[test]
    fn matching_file_retains_heading_and_repeated_query_does_no_work() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        let mut index = VisibleIndex::default();
        index.rebuild(
            [SourceTree {
                slot: SourceSlot::File(0),
                ..fixture.tree()
            }], &[],
            &fixture.expanded,
            None,
        );
        let mut search = fixture.search(&index, "country", true);
        assert_eq!(search.rows(), &[0, 1, 2, 3, 5]);
        let rows_ptr = search.rows().as_ptr();
        assert!(!search.update(&index, "country", true, |_| panic!(
            "idle must not resolve nodes"
        )));
        assert_eq!(rows_ptr, search.rows().as_ptr());
        assert!(search.update(&index, "country", false, |_| Some(&fixture.roots)));
        assert_eq!(search.rows().len(), index.rows().len());
    }

    #[test]
    fn filtered_hierarchy_never_selects_hidden_or_stale_children() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        let mut index = fixture.index();
        let search = fixture.search(&index, "country", true);
        assert_eq!(
            search.hierarchy(&index, 2, HierarchyCommand::Right),
            Some(TreeIntent::Select(index.token(4).unwrap()))
        );
        assert_eq!(
            search.hierarchy(&index, 3, HierarchyCommand::Activate),
            None
        );
        // An expanded matching source has no displayed children if none match.
        let search_source = fixture.search(&index, "Library", true);
        assert_eq!(
            search_source.hierarchy(&index, 0, HierarchyCommand::Right),
            None
        );
        index.rebuild([fixture.tree()], &[], &fixture.expanded, None);
        assert_eq!(search.hierarchy(&index, 2, HierarchyCommand::Right), None);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // Each expected value is a list of highlight ranges.
    fn unicode_highlights_are_complete_original_scalars() {
        for (text, query, expected) in [
            ("ŻÓŁĆ", "żó", vec![0..4]),
            ("film_東京", "f東", vec![0..1, 5..8]),
            ("İstanbul", "İs", vec![0..3]),
            ("Σσ", "σσ", vec![0..4]),
            ("a🦀b", "🦀", vec![1..5]),
        ] {
            let mut ranges = Vec::new();
            assert!(
                SpeedPattern::new(query).match_into(text, &mut ranges),
                "{text} / {query}"
            );
            assert_eq!(ranges, expected);
            for range in ranges {
                assert!(text.get(range).is_some());
            }
        }
        for (text, query) in [
            ("Straße", "ss"),
            ("İ", "i"),
            ("é", "e"),
            ("σ", "ς"),
            ("anything", ""),
        ] {
            assert!(
                !SpeedPattern::new(query).match_into(text, &mut vec![]),
                "{text} / {query}"
            );
        }
    }

    #[test]
    fn page_navigation_clamps_and_match_navigation_wraps() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        let index = fixture.index();
        let search = fixture.search(&index, "a", false);
        assert_eq!(
            search.move_selection(Some(0), SelectionMove::Previous),
            Some(0)
        );
        assert_eq!(search.move_selection(Some(6), SelectionMove::Next), Some(6));
        assert_eq!(
            search.move_selection(Some(6), SelectionMove::PageDown(usize::MAX)),
            Some(6)
        );
        assert_eq!(
            search.move_selection(Some(6), SelectionMove::PageUp(usize::MAX)),
            Some(0)
        );
        assert_eq!(search.move_selection(None, SelectionMove::Last), Some(6));
        assert_eq!(search.next_match(Some(3), false), Some(0));
        assert_eq!(search.next_match(Some(0), true), Some(3));
        assert_eq!(search.next_match(Some(5), true), Some(3));
        assert_eq!(search.next_match(Some(5), false), Some(0));
        let empty = fixture.search(&index, "missing", true);
        assert_eq!(empty.next_match(None, false), None);
        assert_eq!(empty.move_selection(None, SelectionMove::Next), None);
    }

    #[test]
    fn reveal_follows_real_folder_ancestry_one_step_at_a_time() {
        let mut fixture = Fixture::sqlite();
        let mut state = RevealState::default();
        let token = state.begin(
            42,
            fixture.source.id.clone(),
            vec!["main".into(), "film_actor".into()],
        );
        let request = state.pending().unwrap();
        assert_eq!(request.tab_id, 42);
        for path in [
            vec![],
            vec!["main".into()],
            vec!["main".into(), "\0tables".into()],
        ] {
            assert_eq!(
                request.step(fixture.tree(), &fixture.expanded),
                RevealStep::Expand(path.clone())
            );
            fixture.expanded.insert((fixture.source.id.clone(), path));
        }
        assert_eq!(
            request.step(fixture.tree(), &fixture.expanded),
            RevealStep::Select(vec!["main".into(), "film_actor".into()])
        );
        assert!(state.finish(token));
        assert!(state.pending().is_none());
    }

    #[test]
    fn reveal_waits_for_load_and_cancel_rejects_late_completion() {
        let mut fixture = Fixture::sqlite();
        fixture.roots[0].children = None;
        fixture.open_all();
        let mut state = RevealState::default();
        let first = state.begin(
            1,
            fixture.source.id.clone(),
            vec!["main".into(), "film_actor".into()],
        );
        assert_eq!(
            state
                .pending()
                .unwrap()
                .step(fixture.tree(), &fixture.expanded),
            RevealStep::Load(vec!["main".into()])
        );
        fixture.loading.insert(vec!["main".into()]);
        assert_eq!(
            state
                .pending()
                .unwrap()
                .step(fixture.tree(), &fixture.expanded),
            RevealStep::Waiting
        );
        state.cancel();
        assert!(!state.finish(first));
        let second = state.begin(2, fixture.source.id.clone(), vec![]);
        assert_ne!(first, second);
        assert!(!state.finish(first));
        assert_eq!(
            state
                .pending()
                .unwrap()
                .step(fixture.tree(), &fixture.expanded),
            RevealStep::Select(vec![])
        );
        assert!(state.finish(second));
    }

    #[test]
    fn reveal_respects_scope_without_expanding_or_loading_hidden_nodes() {
        let mut fixture = Fixture::sqlite();
        fixture.source.driver = DriverKind::Postgres;
        let mut database = DbObject::new(ObjectKind::Database, "db", &[])
            .with_detail(crate::drivers::DEFAULT_DATABASE);
        database.children = Some(vec![DbObject::new(
            ObjectKind::Schema,
            "hidden",
            &database.path,
        )]);
        fixture
            .source
            .scope
            .hidden_schemas
            .insert("db".into(), vec!["hidden".into()]);
        fixture.roots = vec![database];
        let mut state = RevealState::default();
        state.begin(
            1,
            fixture.source.id.clone(),
            vec!["db".into(), "hidden".into(), "table".into()],
        );
        assert_eq!(
            state
                .pending()
                .unwrap()
                .step(fixture.tree(), &fixture.expanded),
            RevealStep::HiddenByScope
        );
        fixture.roots[0].detail.clear();
        assert_eq!(
            state
                .pending()
                .unwrap()
                .step(fixture.tree(), &fixture.expanded),
            RevealStep::HiddenByScope
        );
    }

    #[test]
    fn disconnected_reveal_connects_once_and_missing_objects_do_not_load_neighbors() {
        let mut fixture = Fixture::sqlite();
        fixture.open_all();
        let mut state = RevealState::default();
        state.begin(
            1,
            fixture.source.id.clone(),
            vec!["main".into(), "missing".into()],
        );
        let request = state.pending().unwrap();
        assert_eq!(
            request.step(
                SourceTree {
                    roots: None,
                    ..fixture.tree()
                },
                &fixture.expanded
            ),
            RevealStep::Load(vec![])
        );
        assert_eq!(
            request.step(
                SourceTree {
                    roots: None,
                    connecting: true,
                    ..fixture.tree()
                },
                &fixture.expanded
            ),
            RevealStep::Waiting
        );
        assert_eq!(
            request.step(fixture.tree(), &fixture.expanded),
            RevealStep::NotFound
        );
        assert!(fixture.loading.is_empty());
    }

    #[test]
    fn empty_index_and_projection_are_safe() {
        let mut index = VisibleIndex::default();
        index.rebuild([], &[], &HashSet::new(), None);
        assert!(index.rows().is_empty());
        assert_eq!(index.token(0), None);
        assert_eq!(index.hierarchy(0, HierarchyCommand::Right), None);
        let mut search = SpeedSearch::default();
        assert!(search.update(&index, "a", true, |_| None));
        assert!(search.rows().is_empty());
        assert_eq!(search.move_selection(None, SelectionMove::First), None);
    }

    #[test]
    fn duplicate_names_keep_source_schema_and_folder_identity() {
        let mut fixture = Fixture::sqlite();
        let mut attached = DbObject::new(ObjectKind::Schema, "attached", &[]);
        attached.children = Some(vec![DbObject::folder(
            "views",
            &attached.path,
            vec![DbObject::new(
                ObjectKind::View,
                "film_actor",
                &attached.path,
            )],
        )]);
        fixture.roots.push(attached);
        fixture.open_all();
        let mut other_source = fixture.source.clone();
        other_source.id = "another".into();
        let path = ["attached".into(), "film_actor".into()];
        let mut index = VisibleIndex::default();
        index.rebuild(
            [
                SourceTree {
                    source: &other_source,
                    slot: SourceSlot::Config(0),
                    ..fixture.tree()
                },
                SourceTree {
                    slot: SourceSlot::Config(1),
                    ..fixture.tree()
                },
            ], &[],
            &fixture.expanded,
            Some((&fixture.source.id, &path)),
        );
        let row = index.selected().unwrap();
        assert_eq!(index.source_id(row), Some("sqlite"));
        assert_eq!(index.source_slot(row), Some(SourceSlot::Config(1)));
        let object = index.object(row, &fixture.roots).unwrap();
        assert_eq!(object.kind, ObjectKind::View);
        assert_eq!(object.path, path);
        let parent = index.rows[row].parent.unwrap();
        assert_eq!(index.object(parent, &fixture.roots).unwrap().name, "views");
        let search = fixture.search(&index, "fa", true);
        assert_eq!(search.hits().len(), 2);
        assert!(
            search
                .hits()
                .iter()
                .all(|hit| index.source_id(hit.row) == Some("sqlite"))
        );

        let mut reveal = RevealState::default();
        reveal.begin(10, fixture.source.id.clone(), path.to_vec());
        fixture.expanded.remove(&(
            fixture.source.id.clone(),
            vec!["attached".into(), "\0views".into()],
        ));
        assert_eq!(
            reveal
                .pending()
                .unwrap()
                .step(fixture.tree(), &fixture.expanded),
            RevealStep::Expand(vec!["attached".into(), "\0views".into()])
        );
    }

    #[test]
    fn scope_change_rebuild_drops_matches_and_selection_from_hidden_schemas() {
        let mut fixture = Fixture::sqlite();
        fixture.source.driver = DriverKind::Postgres;
        let mut database = DbObject::new(ObjectKind::Database, "db", &[])
            .with_detail(crate::drivers::DEFAULT_DATABASE);
        let mut schema = DbObject::new(ObjectKind::Schema, "secret", &database.path);
        let object = DbObject::new(ObjectKind::Table, "Żółć_東京", &schema.path);
        let target = object.path.clone();
        schema.children = Some(vec![DbObject::folder("tables", &schema.path, vec![object])]);
        database.children = Some(vec![schema]);
        fixture.roots = vec![database];
        fixture.open_all();
        let mut index = fixture.index();
        let mut search = fixture.search(&index, "ż東", true);
        assert_eq!(search.hits().len(), 1);
        let token = index.token(search.hits()[0].row).unwrap();
        index.select(token);
        fixture
            .source
            .scope
            .hidden_schemas
            .insert("db".into(), vec!["secret".into()]);
        index.rebuild(
            [fixture.tree()], &[],
            &fixture.expanded,
            Some((&fixture.source.id, &target)),
        );
        assert!(search.update(&index, "ż東", true, |_| Some(&fixture.roots)));
        assert!(search.hits().is_empty());
        assert!(search.rows().is_empty());
        assert_eq!(index.selected(), None);
        assert!(!index.valid(token));
    }

    #[test]
    #[ignore = "release benchmark: cargo test --release --lib explorer::navigation::tests::benchmark -- --ignored --nocapture"]
    fn benchmark() {
        use std::hint::black_box;
        use std::time::Duration;
        use std::time::Instant;

        const SAMPLES: usize = 7;
        const IDLE_ITERATIONS: usize = 10_000;
        fn median(samples: &mut [Duration]) -> Duration {
            samples.sort_unstable();
            samples[samples.len() / 2]
        }
        for size in [500, 5_000, 100_000] {
            let mut fixture = Fixture::sqlite();
            let parent = fixture.roots[0].path.clone();
            // Deterministic mixed ASCII/Unicode labels. Only the source, schema
            // and folder are expanded, as when browsing a large table list.
            fixture.roots[0].children = Some(vec![DbObject::folder(
                "tables",
                &parent,
                (0..size)
                    .map(|i| {
                        let label = match i % 4 {
                            0 => format!("film_actor_{i:06}"),
                            1 => format!("country_{i:06}"),
                            2 => format!("Żółć_東京_{i:06}"),
                            _ => format!("archive_{i:06}"),
                        };
                        DbObject::new(ObjectKind::Table, label, &parent)
                    })
                    .collect(),
            )]);
            for path in [
                vec![],
                parent.clone(),
                vec!["main".into(), "\0tables".into()],
            ] {
                fixture.expanded.insert((fixture.source.id.clone(), path));
            }
            let mut index = fixture.index(); // Warm allocations before measurement.
            let mut search = fixture.search(&index, "ż東9", true);
            let mut rebuild = [Duration::ZERO; SAMPLES];
            let mut ascii = [Duration::ZERO; SAMPLES];
            let mut unicode = [Duration::ZERO; SAMPLES];
            let mut ascii_hits = 0;
            for sample in 0..SAMPLES {
                let start = Instant::now();
                index.rebuild([fixture.tree()], &[], &fixture.expanded, None);
                rebuild[sample] = start.elapsed();
                let start = Instant::now();
                assert!(search.update(&index, "fa9", true, |_| Some(&fixture.roots)));
                ascii[sample] = start.elapsed();
                ascii_hits = search.hits().len();
                let start = Instant::now();
                assert!(search.update(&index, "ż東9", true, |_| Some(&fixture.roots)));
                unicode[sample] = start.elapsed();
            }
            let start = Instant::now();
            for _ in 0..IDLE_ITERATIONS {
                black_box(search.update(&index, "ż東9", true, |_| panic!("idle resolution")));
                black_box(search.move_selection(Some(3), SelectionMove::PageDown(30)));
            }
            println!(
                "explorer {size} table nodes ({SAMPLES}-sample warm medians): rebuild={:.3}ms ascii_query={:.3}ms unicode_query={:.3}ms cached={:.3}us/iteration hits={ascii_hits}/{}",
                median(&mut rebuild).as_secs_f64() * 1000.0,
                median(&mut ascii).as_secs_f64() * 1000.0,
                median(&mut unicode).as_secs_f64() * 1000.0,
                start.elapsed().as_secs_f64() * 1_000_000.0 / IDLE_ITERATIONS as f64,
                search.hits().len()
            );
        }
    }
}
