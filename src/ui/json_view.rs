//! JSON values in the value panel: a collapsible tree, or formatted text with
//! syntax colors. Both are built once per value and drawn only where visible.

use super::theme;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, RichText, Ui};
use serde_json::Value as Json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

const KEY: Color32 = Color32::from_rgb(0xC7, 0x7D, 0xBB);
const STRING: Color32 = theme::SQL_STRING;
const NUMBER: Color32 = theme::SQL_NUMBER;
const LITERAL: Color32 = theme::SQL_KEYWORD;
const PUNCT: Color32 = theme::TEXT_MUTED;

/// Longest key or scalar a tree row lays out; the full value stays in Text mode and Copy.
const ROW_CHARS: usize = 1024;
/// Longest piece of a line laid out as one row of a long text.
const SEGMENT_BYTES: usize = 1024;

/// Parses text that holds a JSON object or array; scalars are shown as plain text.
#[cfg(test)]
fn parse(text: &str) -> Option<Json> {
    if !looks_like_json(text) {
        return None;
    }
    serde_json::from_str(text).ok()
}

pub fn looks_like_json(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('{') || t.starts_with('[')
}

enum Kind {
    Container { object: bool, len: usize },
    Scalar { text: Box<str>, color: Color32 },
}

enum Key {
    None,
    Name(Box<str>),
    Item,
}

struct Node {
    depth: u32,
    /// Index of the first node after this node's subtree, in preorder.
    next: u32,
    key: Key,
    kind: Kind,
}

/// A parsed JSON value flattened in preorder, with the rows its open nodes show.
pub struct JsonTree {
    nodes: Vec<Node>,
    /// Nodes whose open state differs from the default (the first two levels open).
    toggled: HashSet<u32>,
    rows: Vec<u32>,
}

impl JsonTree {
    /// Returns `None` when `cancel` was set while the tree was being built.
    pub fn build(value: &Json, toggled: HashSet<u32>, cancel: &AtomicBool) -> Option<Self> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut stack = vec![(Key::None, value, 0u32)];
        // Preorder walk; `next` is patched when a subtree is closed.
        let mut open: Vec<usize> = Vec::new();
        while let Some((key, v, depth)) = stack.pop() {
            if nodes.len().is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let index = nodes.len();
            while let Some(&parent) = open.last() {
                if nodes[parent].depth < depth {
                    break;
                }
                nodes[parent].next = index as u32;
                open.pop();
            }
            let kind = match v {
                Json::Object(map) => Kind::Container {
                    object: true,
                    len: map.len(),
                },
                Json::Array(items) => Kind::Container {
                    object: false,
                    len: items.len(),
                },
                scalar => {
                    let (text, color) = scalar_text(scalar);
                    Kind::Scalar {
                        text: text.into_boxed_str(),
                        color,
                    }
                }
            };
            nodes.push(Node {
                depth,
                next: 0,
                key,
                kind,
            });
            match v {
                Json::Object(map) => {
                    open.push(index);
                    for (k, child) in map.iter().rev() {
                        let name = crate::model::cell_preview(k, ROW_CHARS);
                        stack.push((Key::Name(name.into_boxed_str()), child, depth + 1));
                    }
                }
                Json::Array(items) => {
                    open.push(index);
                    for child in items.iter().rev() {
                        stack.push((Key::Item, child, depth + 1));
                    }
                }
                _ => nodes[index].next = index as u32 + 1,
            }
        }
        let end = nodes.len() as u32;
        for index in open {
            nodes[index].next = end;
        }
        let mut tree = Self {
            nodes,
            toggled,
            rows: Vec::new(),
        };
        tree.refresh_rows();
        Some(tree)
    }

    pub fn toggled(&self) -> &HashSet<u32> {
        &self.toggled
    }

    fn is_open(&self, index: u32) -> bool {
        (self.nodes[index as usize].depth < 2) != self.toggled.contains(&index)
    }

    /// Visible rows in order; skipping a closed subtree costs nothing.
    fn refresh_rows(&mut self) {
        self.rows.clear();
        let mut index = 0u32;
        let end = self.nodes.len() as u32;
        while index < end {
            self.rows.push(index);
            let node = &self.nodes[index as usize];
            index = match node.kind {
                Kind::Container { len, .. } if len > 0 && self.is_open(index) => index + 1,
                _ => node.next,
            };
        }
    }

    #[cfg(test)]
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Draws visible rows; returns whether a node was opened or closed.
    pub fn show(&mut self, ui: &mut Ui) -> bool {
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace).max(16.0);
        let mut toggle = None;
        egui::ScrollArea::both()
            .id_salt("value_viewer")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, self.rows.len(), |ui, range| {
                for &index in &self.rows[range] {
                    if self.row(ui, index, row_height) {
                        toggle = Some(index);
                    }
                }
            });
        if let Some(index) = toggle {
            if !self.toggled.remove(&index) {
                self.toggled.insert(index);
            }
            self.refresh_rows();
        }
        toggle.is_some()
    }

    fn row(&self, ui: &mut Ui, index: u32, row_height: f32) -> bool {
        let node = &self.nodes[index as usize];
        let mut clicked = false;
        ui.horizontal(|ui| {
            ui.set_height(row_height);
            ui.spacing_mut().item_spacing.x = 4.0;
            let indent = ui.spacing().indent;
            ui.add_space(node.depth as f32 * indent);
            let has_children = matches!(node.kind, Kind::Container { len, .. } if len > 0);
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(indent - 4.0, row_height), egui::Sense::click());
            if has_children {
                let open = self.is_open(index);
                egui::collapsing_header::paint_default_icon(
                    ui,
                    if open { 1.0 } else { 0.0 },
                    &response.clone().with_new_rect(rect),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::CollapsingHeader,
                        true,
                        if open { "Collapse" } else { "Expand" },
                    )
                });
                clicked = response.clicked();
            }
            if let Key::Name(name) = &node.key {
                ui.label(RichText::new(format!("\"{name}\"")).monospace().color(KEY));
                ui.label(RichText::new(":").monospace().color(PUNCT));
            }
            match &node.kind {
                Kind::Scalar { text, color } => {
                    ui.add(
                        egui::Label::new(RichText::new(&**text).monospace().color(*color))
                            .truncate(),
                    );
                }
                Kind::Container { object, len } => {
                    let (open, close) = if *object { ("{", "}") } else { ("[", "]") };
                    let summary = if *len == 0 {
                        format!("{open}{close}")
                    } else if self.is_open(index) {
                        open.to_string()
                    } else {
                        format!("{open}{len}{close}")
                    };
                    let label = ui.add(
                        egui::Label::new(RichText::new(summary).monospace().color(PUNCT))
                            .sense(egui::Sense::click()),
                    );
                    clicked |= has_children && label.clicked();
                }
            }
        });
        clicked
    }
}

fn scalar_text(v: &Json) -> (String, Color32) {
    match v {
        Json::String(s) => (
            format!("\"{}\"", crate::model::cell_preview(s, ROW_CHARS)),
            STRING,
        ),
        Json::Number(n) => (crate::model::cell_preview(&n.to_string(), ROW_CHARS), NUMBER),
        Json::Bool(b) => (b.to_string(), LITERAL),
        Json::Null => ("null".into(), LITERAL),
        _ => (String::new(), PUNCT),
    }
}

/// Line starts of a long text, with long lines split into pieces of about
/// `SEGMENT_BYTES`, so only the rows on screen are laid out.
pub struct TextLines {
    /// Byte offset of each row; the top bit marks a row that starts inside a
    /// JSON string, which only matters for formatted JSON.
    starts: Vec<u64>,
    len: usize,
}

const IN_STRING: u64 = 1 << 63;

impl TextLines {
    /// Returns `None` when `cancel` was set while indexing.
    pub fn index(text: &str, json: bool, cancel: &AtomicBool) -> Option<Self> {
        let bytes = text.as_bytes();
        let mut starts = vec![0];
        let (mut in_string, mut escaped) = (false, false);
        let mut row_start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if i.is_multiple_of(1 << 20) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let b = bytes[i];
            if json {
                if escaped {
                    escaped = false;
                } else if in_string && b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_string = !in_string;
                }
            }
            i += 1;
            let split = b == b'\n'
                || (i - row_start >= SEGMENT_BYTES && text.is_char_boundary(i) && !escaped);
            if split && i < bytes.len() {
                row_start = i;
                starts.push(i as u64 | if in_string { IN_STRING } else { 0 });
            }
        }
        Some(Self {
            starts,
            len: text.len(),
        })
    }

    pub fn row_count(&self) -> usize {
        self.starts.len()
    }

    fn row<'a>(&self, text: &'a str, row: usize) -> (&'a str, bool) {
        let start = (self.starts[row] & !IN_STRING) as usize;
        let end = self
            .starts
            .get(row + 1)
            .map_or(self.len, |next| (next & !IN_STRING) as usize);
        let piece = text[start..end].strip_suffix('\n').unwrap_or(&text[start..end]);
        (piece, self.starts[row] & IN_STRING != 0)
    }

    /// Draws only the rows in view; `json` colors them like `highlighted`.
    pub fn show(&self, ui: &mut Ui, text: &str, json: bool) {
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::both()
            .id_salt("value_viewer")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, self.row_count(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for row in range {
                    let (piece, in_string) = self.row(text, row);
                    let label = if json {
                        let mut job = LayoutJob::default();
                        highlight_into(&mut job, piece, in_string);
                        egui::Label::new(job)
                    } else {
                        egui::Label::new(RichText::new(piece).monospace())
                    };
                    ui.add(label.extend().selectable(true));
                }
            });
    }
}

/// Pretty-printed JSON with syntax colors.
#[cfg(test)]
fn highlighted(value: &Json) -> LayoutJob {
    highlighted_text(&serde_json::to_string_pretty(value).unwrap_or_default())
}

pub fn highlighted_text(text: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    highlight_into(&mut job, text, false);
    job
}

/// Colors `text`, a piece of formatted JSON that may start inside a string.
fn highlight_into(job: &mut LayoutJob, text: &str, starts_in_string: bool) {
    let font = theme::mono(13.0);
    let mut push = |s: &str, color: Color32| {
        job.append(
            s,
            0.0,
            TextFormat {
                font_id: font.clone(),
                color,
                ..Default::default()
            },
        );
    };
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = starts_in_string;
    while i < bytes.len() {
        let start = i;
        if in_string || bytes[i] == b'"' {
            if !in_string {
                i += 1;
            }
            in_string = false;
            while i < bytes.len() && bytes[i] != b'"' {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(bytes.len());
            // A string followed by a colon is a key.
            let is_key = text[i..].trim_start().starts_with(':');
            push(&text[start..i], if is_key { KEY } else { STRING });
            continue;
        }
        match bytes[i] {
            b'-' | b'0'..=b'9' => {
                while i < bytes.len()
                    && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    i += 1;
                }
                push(&text[start..i], NUMBER);
            }
            b't' | b'f' | b'n' => {
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                push(&text[start..i], LITERAL);
            }
            _ => {
                let len = text[i..].chars().next().map_or(1, char::len_utf8);
                i += len;
                push(&text[start..i], PUNCT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_and_colors_keys() {
        let v = parse(r#"{"z": 1, "a": [true, null, "x"]}"#).unwrap();
        let job = highlighted(&v);
        assert!(
            job.text.find("\"z\"").unwrap() < job.text.find("\"a\"").unwrap(),
            "original key order"
        );
        let color_of = |needle: &str| {
            let at = job.text.find(needle).unwrap();
            job.sections
                .iter()
                .find(|s| s.byte_range.start.0 <= at && at < s.byte_range.end.0)
                .unwrap()
                .format
                .color
        };
        assert_eq!(color_of("\"z\""), KEY);
        assert_eq!(color_of("\"x\""), STRING);
        assert_eq!(color_of("true"), LITERAL);
        assert_eq!(color_of("1"), NUMBER);
        assert!(parse("42").is_none() && parse("not json").is_none());
    }

    fn tree(text: &str) -> JsonTree {
        JsonTree::build(&parse(text).unwrap(), HashSet::new(), &AtomicBool::new(false)).unwrap()
    }

    fn row_keys(tree: &JsonTree) -> Vec<String> {
        tree.rows
            .iter()
            .map(|&i| match &tree.nodes[i as usize].key {
                Key::None => "$".into(),
                Key::Name(name) => name.to_string(),
                Key::Item => "[]".into(),
            })
            .collect()
    }

    #[test]
    fn tree_rows_follow_key_order_and_open_state() {
        let mut t = tree(r#"{"z": {"deep": {"x": 1}}, "a": [1, 2], "e": {}}"#);
        // Two levels open by default; "deep" is at depth 2 and starts closed.
        assert_eq!(row_keys(&t), ["$", "z", "deep", "a", "[]", "[]", "e"]);
        let deep = t.rows[2];
        t.toggled.insert(deep);
        t.refresh_rows();
        assert_eq!(row_keys(&t), ["$", "z", "deep", "x", "a", "[]", "[]", "e"]);
        let root = t.rows[0];
        t.toggled.insert(root);
        t.refresh_rows();
        assert_eq!(row_keys(&t), ["$"]);
    }

    #[test]
    fn closed_subtrees_cost_no_rows() {
        let wide: String = format!(
            "[{}]",
            (0..50_000)
                .map(|i| format!("{{\"id\": {i}, \"tags\": [1, 2, 3]}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut t = tree(&wide);
        // Root and its 50k objects are open, the tag arrays are closed.
        assert_eq!(t.row_count(), 1 + 50_000 * 3);
        let root = t.rows[0];
        t.toggled.insert(root);
        t.refresh_rows();
        assert_eq!(t.row_count(), 1);
    }

    #[test]
    fn building_stops_when_cancelled() {
        let v = parse("[1, 2, 3]").unwrap();
        assert!(JsonTree::build(&v, HashSet::new(), &AtomicBool::new(true)).is_none());
    }

    #[test]
    fn text_rows_split_long_lines_on_char_boundaries() {
        let text = format!("short\n{}\nend", "ż".repeat(SEGMENT_BYTES));
        let lines = TextLines::index(&text, false, &AtomicBool::new(false)).unwrap();
        let rows: Vec<&str> = (0..lines.row_count()).map(|r| lines.row(&text, r).0).collect();
        assert_eq!(rows.first(), Some(&"short"));
        assert_eq!(rows.last(), Some(&"end"));
        assert_eq!(rows.concat(), text.replace('\n', ""));
        assert!(rows.iter().all(|row| row.len() <= SEGMENT_BYTES + 4));
    }

    #[test]
    fn rows_of_formatted_json_know_when_they_start_inside_a_string() {
        let long = "x\\\"".repeat(SEGMENT_BYTES);
        let text = serde_json::to_string_pretty(
            &serde_json::json!({"long": long.clone(), "after": 1}),
        )
        .unwrap();
        let lines = TextLines::index(&text, true, &AtomicBool::new(false)).unwrap();
        let continued = (0..lines.row_count())
            .map(|r| lines.row(&text, r))
            .find(|(_, in_string)| *in_string)
            .expect("a row continues the long string");
        let mut job = LayoutJob::default();
        highlight_into(&mut job, continued.0, true);
        assert_eq!(job.sections[0].format.color, STRING);
        let after = (0..lines.row_count())
            .map(|r| lines.row(&text, r))
            .find(|(piece, _)| piece.contains("\"after\""))
            .unwrap();
        assert!(!after.1);
    }
}
