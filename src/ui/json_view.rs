//! JSON values in the value panel: a collapsible tree, or formatted text with
//! syntax colors.

use super::theme;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Id, RichText, Ui};
use serde_json::Value as Json;

const KEY: Color32 = Color32::from_rgb(0xC7, 0x7D, 0xBB);
const STRING: Color32 = theme::SQL_STRING;
const NUMBER: Color32 = theme::SQL_NUMBER;
const LITERAL: Color32 = theme::SQL_KEYWORD;
const PUNCT: Color32 = theme::TEXT_MUTED;

/// Parses text that holds a JSON object or array; scalars are shown as plain text.
pub fn parse(text: &str) -> Option<Json> {
    let t = text.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// Collapsible tree; the first two levels start open.
pub fn tree(ui: &mut Ui, value: &Json, id: Id) {
    node(ui, None, value, id, 0);
}

fn scalar(v: &Json) -> RichText {
    let (text, color) = match v {
        Json::String(s) => (format!("\"{s}\""), STRING),
        Json::Number(n) => (n.to_string(), NUMBER),
        Json::Bool(b) => (b.to_string(), LITERAL),
        Json::Null => ("null".into(), LITERAL),
        _ => (String::new(), PUNCT),
    };
    RichText::new(text).monospace().color(color)
}

fn key_label(ui: &mut Ui, key: Option<&str>) {
    if let Some(k) = key {
        ui.label(RichText::new(format!("\"{k}\"")).monospace().color(KEY));
        ui.label(RichText::new(":").monospace().color(PUNCT));
    }
}

fn node(ui: &mut Ui, key: Option<&str>, v: &Json, id: Id, depth: usize) {
    let children: Vec<(String, &Json)> = match v {
        Json::Object(m) => m.iter().map(|(k, v)| (k.clone(), v)).collect(),
        Json::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                key_label(ui, key);
                ui.add(egui::Label::new(scalar(v)).wrap());
            });
            return;
        }
    };
    let (open, close) = if v.is_object() {
        ("{", "}")
    } else {
        ("[", "]")
    };
    if children.is_empty() {
        ui.horizontal(|ui| {
            key_label(ui, key);
            ui.label(
                RichText::new(format!("{open}{close}"))
                    .monospace()
                    .color(PUNCT),
            );
        });
        return;
    }
    let state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, depth < 2);
    let is_open = state.is_open();
    state
        .show_header(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            key_label(ui, key);
            let summary = if is_open {
                open.to_string()
            } else {
                format!("{open}{}{close}", children.len())
            };
            ui.label(RichText::new(summary).monospace().color(PUNCT));
        })
        .body(|ui| {
            for (k, child) in &children {
                let child_key = v.is_object().then_some(k.as_str());
                node(ui, child_key, child, id.with(k), depth + 1);
            }
        });
}

/// Pretty-printed JSON with syntax colors.
pub fn highlighted(value: &Json) -> LayoutJob {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    let mut job = LayoutJob::default();
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
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                // A string followed by a colon is a key.
                let is_key = text[i..].trim_start().starts_with(':');
                push(&text[start..i], if is_key { KEY } else { STRING });
            }
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
    job
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
}
