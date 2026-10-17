//! Explain selects source SQL and keeps plans separate from editable results.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, QueryResult, Value};
use egui::accesskit::{Role, Toggled};
use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;
const ORIGINAL: &str = "SELECT id,name FROM customers ORDER BY id";
const SCRIPT: &str = "-- Łódź 😀; this semicolon stays inside a comment\nWITH chosen AS (\n    SELECT id FROM orders WHERE customer_id=1\n)\nSELECT id FROM chosen UNION ALL SELECT id FROM orders WHERE customer_id=2;\nSELECT name FROM customers WHERE name <> 'a;b Żółw';";

fn app() -> (H<'static>, egui::Context) {
    let mut context = None;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| {
            context = Some(cc.egui_ctx.clone());
            dbeast::ui::App::new_for_test(cc)
        });
    h.run_steps(3);
    (h, context.unwrap())
}

fn key(h: &mut H<'_>, modifiers: Modifiers, key: Key) {
    h.key_press_modifiers(modifiers, key);
    h.run_steps(3);
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn shot(h: &mut H<'_>, name: &str) {
    h.render()
        .unwrap()
        .save(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/screenshots")
                .join(name),
        )
        .unwrap();
}

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        if Instant::now() >= deadline {
            let nodes: Vec<_> = h
                .query_all_by_label_contains("")
                .map(|node| {
                    (
                        node.accesskit_node().role(),
                        node.accesskit_node().label(),
                        node.value(),
                        node.accesskit_node().description(),
                    )
                })
                .collect();
            shot(h, "query-plan-timeout.png");
            panic!("timed out waiting for {what}; nodes: {nodes:#?}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn editor<'h>(h: &'h H<'_>) -> egui_kittest::Node<'h> {
    h.get_all_by_role(Role::MultilineTextInput)
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("SQL editor above Services")
}

fn set_sql(h: &mut H<'_>, text: &str) {
    editor(h).focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::A);
    editor(h).type_text(text);
    h.run_steps(3);
}

fn set_selection(context: &egui::Context, editor: egui::Id, range: Range<usize>) {
    let mut state = egui::text_edit::TextEditState::load(context, editor).unwrap();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(range.start),
        CCursor::new(range.end),
    )));
    state.store(context, editor);
    context.memory_mut(|memory| memory.request_focus(editor));
}

fn selection(context: &egui::Context, editor: egui::Id) -> Range<usize> {
    let cursor = egui::text_edit::TextEditState::load(context, editor)
        .unwrap()
        .cursor
        .char_range()
        .unwrap();
    cursor.primary.index.0.min(cursor.secondary.index.0)
        ..cursor.primary.index.0.max(cursor.secondary.index.0)
}

fn open_action(h: &mut H<'_>, name: &str) {
    key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .type_text(name);
    h.run_steps(3);
}

fn palette_action<'h>(h: &'h H<'_>, name: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(name)
        .find(|node| node.accesskit_node().toggled().is_some())
        .expect("selectable action in Find Action")
}

fn run_action(h: &mut H<'_>, name: &str) {
    open_action(h, name);
    assert!(!palette_action(h, name).accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Enter);
}

fn operations(h: &H<'_>) -> usize {
    h.query_all_by_label_contains("Plan operation: ").count()
}

fn has_operation(h: &H<'_>, text: &str) -> bool {
    h.query_all_by_label_contains("Plan operation: ")
        .any(|node| {
            node.accesskit_node()
                .label()
                .is_some_and(|label| label.contains(text))
        })
}

fn copy_plan(h: &mut H<'_>) -> String {
    h.get_by_label("Copy Original Query Plan").click();
    let mut copied = None;
    for _ in 0..3 {
        h.run_steps(1);
        for command in &h.output().platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                copied = Some(text.clone());
            }
        }
    }
    copied.expect("complete native plan in clipboard output")
}

fn label_text(node: &egui_kittest::Node<'_>) -> Option<String> {
    node.value().or_else(|| node.accesskit_node().label())
}

fn diagram_cards(h: &H<'_>) -> Vec<(usize, String, egui::Rect)> {
    h.query_all_by_role(Role::Button)
        .filter_map(|node| {
            let label = node.accesskit_node().label()?;
            let (index, _) = label.strip_prefix("Plan operation ")?.split_once(':')?;
            Some((index.parse().ok()?, label.to_owned(), node.rect()))
        })
        .collect()
}

fn diagram_card<'h>(h: &'h H<'_>, index: usize) -> egui_kittest::Node<'h> {
    let prefix = format!("Plan operation {index}:");
    h.query_all_by_role(Role::Button)
        .find(|node| {
            node.accesskit_node()
                .label()
                .is_some_and(|label| label.starts_with(&prefix))
        })
        .unwrap_or_else(|| panic!("diagram card {index} must be visible: {h:#?}"))
}

fn selected_diagram_card(h: &H<'_>) -> usize {
    let selected: Vec<_> = diagram_cards(h)
        .into_iter()
        .filter(|(index, _, _)| {
            diagram_card(h, *index).accesskit_node().toggled() == Some(Toggled::True)
        })
        .collect();
    assert_eq!(selected.len(), 1, "one selected operation in the diagram");
    selected[0].0
}

fn assert_property(h: &H<'_>, key: &str, value: &str) {
    let right = h.get_by_label("Query Plan Diagram").rect().right();
    let key_y = h
        .query_all_by_role(Role::Label)
        .find(|node| label_text(node).as_deref() == Some(key) && node.rect().left() >= right)
        .unwrap_or_else(|| panic!("visible Details property {key:?}: {h:#?}"))
        .rect()
        .center()
        .y;
    assert!(
        h.query_all_by_role(Role::Label).any(|node| {
            label_text(&node).as_deref() == Some(value)
                && node.rect().left() >= right
                && (node.rect().center().y - key_y).abs() < 4.0
        }),
        "Details must show {key}={value} for the selected operation"
    );
}

fn drag_diagram(h: &mut H<'_>, from: egui::Pos2, delta: egui::Vec2) {
    h.event(egui::Event::PointerMoved(from));
    h.run_steps(1);
    h.event(egui::Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(1);
    for step in 1..=4 {
        h.event(egui::Event::PointerMoved(
            from + delta * (step as f32 / 4.0),
        ));
        h.run_steps(1);
    }
    h.event(egui::Event::PointerButton {
        pos: from + delta,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(3);
}

fn loaded_plan_count(h: &mut H<'_>) -> usize {
    click(h, "Output");
    let count = h
        .query_all_by_role(Role::Label)
        .filter(|node| label_text(node).is_some_and(|text| text.contains("query plan loaded in ")))
        .count();
    click(h, "Query Plan");
    count
}

fn verify_diagram(
    h: &mut H<'_>,
    raw: &str,
    context: &egui::Context,
    editor_id: egui::Id,
    caret: usize,
) {
    // Normalize the small captured native result offline to identify exact node
    // indices/SQLite properties; no additional Explain or connection is needed.
    let mut lines = raw.lines();
    let columns = lines
        .next()
        .unwrap()
        .split('\t')
        .map(|name| Column {
            name: name.into(),
            type_name: String::new(),
        })
        .collect();
    let rows = lines
        .map(|line| {
            let fields: Vec<_> = line.splitn(4, '\t').collect();
            vec![
                Value::Int(fields[0].parse().unwrap()),
                Value::Int(fields[1].parse().unwrap()),
                Value::Int(fields[2].parse().unwrap()),
                Value::Text(fields[3].into()),
            ]
        })
        .collect();
    let expected = dbeast::query_plan::parse(
        DriverKind::Sqlite,
        SCRIPT.into(),
        QueryResult {
            columns,
            rows,
            ..QueryResult::default()
        },
    );
    let selected = expected
        .nodes
        .iter()
        .rposition(|node| node.depth > 0)
        .expect("a non-root operation")
        + 1;
    let operation = &expected.nodes[selected - 1];
    let expected_id = &operation
        .details
        .iter()
        .find(|(key, _)| key == "id")
        .unwrap()
        .1;
    let loaded_before = loaded_plan_count(h);
    assert_eq!(loaded_before, 1);
    click(h, "Collapse All");
    assert!(operations(h) < expected.nodes.len());
    click(h, "Show Diagram");
    assert_eq!(
        h.get_by_label("Show Diagram").accesskit_node().toggled(),
        Some(Toggled::True)
    );
    assert_eq!(
        diagram_cards(h).len(),
        expected.nodes.len(),
        "first Fit must include the full small plan despite collapsed Tree"
    );
    diagram_card(h, selected).click();
    h.run_steps(3);
    assert_eq!(selected_diagram_card(h), selected);
    assert_property(h, "Operation", &operation.operation);
    assert_property(h, "id", expected_id);
    assert_eq!(copy_plan(h), raw);

    let canvas = h.get_by_label("Query Plan Diagram").rect();
    let (anchor, _, original_rect) = diagram_cards(h)
        .into_iter()
        .min_by(|a, b| {
            a.2.center()
                .distance(canvas.center())
                .total_cmp(&b.2.center().distance(canvas.center()))
        })
        .unwrap();
    click(h, "Zoom In");
    let zoomed = diagram_card(h, anchor).rect();
    assert!(
        (zoomed.width() / original_rect.width() - 1.25).abs() < 0.03,
        "Zoom In scales cards by the documented factor"
    );
    click(h, "Zoom Out");
    assert!((diagram_card(h, anchor).rect().width() - original_rect.width()).abs() < 1.0);
    click(h, "Actual Size");
    assert!(
        diagram_cards(h)
            .iter()
            .all(|(_, _, rect)| (rect.width() - 260.0).abs() < 1.0)
    );
    assert!(
        !diagram_cards(h).is_empty(),
        "Actual Size keeps the canvas populated"
    );
    click(h, "Fit Content");
    let canvas = h.get_by_label("Query Plan Diagram").rect();
    assert_eq!(diagram_cards(h).len(), expected.nodes.len());
    assert!(
        diagram_cards(h)
            .iter()
            .all(|(_, _, rect)| canvas.expand(1.0).contains_rect(*rect))
    );

    let before = diagram_card(h, anchor).rect();
    let delta = egui::vec2(24.0, 14.0);
    let from = canvas.left_top() + egui::vec2(4.0, 4.0);
    assert!(
        diagram_cards(h)
            .iter()
            .all(|(_, _, rect)| !rect.contains(from)),
        "pan begins on blank canvas"
    );
    drag_diagram(h, from, delta);
    let moved = diagram_card(h, anchor).rect();
    assert!(
        (moved.center() - before.center() - delta).length() < 2.0,
        "drag translates the camera without relayout"
    );
    assert_eq!(selected_diagram_card(h), selected);
    h.run_steps(5);
    assert!(
        (diagram_card(h, anchor).rect().center() - moved.center()).length() < 1.0,
        "pan stops when the pointer is released"
    );

    // Tree reveals a selected descendant even though it was previously collapsed.
    click(h, "Operations Tree");
    let tree_selection: Vec<_> = h
        .query_all_by_label_contains("Plan operation: ")
        .filter(|node| node.accesskit_node().toggled() == Some(Toggled::True))
        .map(|node| node.accesskit_node().label().unwrap().to_owned())
        .collect();
    assert_eq!(
        tree_selection,
        [format!("Plan operation: {}", operation.operation)]
    );
    click(h, "Raw");
    assert_eq!(copy_plan(h), raw);
    click(h, "Show Diagram");
    assert_eq!(selected_diagram_card(h), selected);
    assert!(
        (diagram_card(h, anchor).rect().center() - moved.center()).length() < 1.0,
        "returning to Diagram retains its camera"
    );
    assert_property(h, "id", expected_id);

    let attributes = h.get_by_label("Attributes").accesskit_node().toggled();
    let before_attributes = diagram_card(h, anchor).rect();
    click(h, "Attributes");
    assert_ne!(
        h.get_by_label("Attributes").accesskit_node().toggled(),
        attributes
    );
    assert_eq!(
        diagram_card(h, anchor).rect(),
        before_attributes,
        "Attributes changes content without moving cards"
    );
    click(h, "Attributes");
    click(h, "Fit Content");
    shot(h, "70-query-plan-diagram.png");

    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(4);
    click(h, "Fit Content");
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 500.0));
    for label in [
        "Show Diagram",
        "Operations Tree",
        "Raw",
        "Copy Original Query Plan",
        "Fit Content",
        "Zoom In",
        "Zoom Out",
        "Actual Size",
        "Attributes",
        "Details",
    ] {
        assert!(
            screen.contains_rect(h.get_by_label(label).rect()),
            "{label} remains reachable in the short window"
        );
    }
    let canvas = h.get_by_label("Query Plan Diagram").rect();
    assert!(canvas.width() > 40.0 && canvas.height() > 24.0);
    assert!(
        diagram_cards(h)
            .iter()
            .all(|(_, _, rect)| canvas.expand(1.0).contains_rect(*rect))
    );
    shot(h, "71-query-plan-diagram-narrow.png");
    click(h, "Details");
    assert!(h.get_by_label("Query Plan Diagram").rect().width() > canvas.width());
    click(h, "Details");
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    click(h, "Fit Content");
    diagram_card(h, selected).click();
    h.run_steps(2);
    key(h, Modifiers::NONE, Key::Home);
    assert_eq!(selected_diagram_card(h), 1);
    key(h, Modifiers::NONE, Key::End);
    assert_eq!(selected_diagram_card(h), expected.nodes.len());
    assert_eq!(copy_plan(h), raw);
    assert_eq!(editor(h).value().as_deref(), Some(SCRIPT));
    assert_eq!(selection(context, editor_id), caret..caret);
    assert_eq!(
        loaded_plan_count(h),
        loaded_before,
        "diagram gestures and view switches do not execute Explain again"
    );
    click(h, "Operations Tree");
}

fn session(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn explain_selected_sql_keeps_results_and_displays_an_ephemeral_plan() {
    let dir = std::env::temp_dir().join(format!("dbeast_plan_ui_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("plans.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
         CREATE TABLE customers(id INTEGER PRIMARY KEY,name TEXT NOT NULL);
         CREATE TABLE orders(id INTEGER PRIMARY KEY,customer_id INTEGER NOT NULL,
             FOREIGN KEY(customer_id) REFERENCES customers(id));
         CREATE INDEX orders_customer ON orders(customer_id);
         INSERT INTO customers VALUES(1,'Ada Żółw'),(2,'Olek Lis');
         INSERT INTO orders VALUES(11,1),(12,1),(13,2);",
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "plan-fixture".into();
    source.name = "Query plans".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        files_root: Some(dir.clone()),
        tile_url: String::new(),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    let session_path = dir.join("session.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    std::fs::write(
        &session_path,
        serde_json::json!({"active":0,"tabs":[{
            "title":"Plans","ds_id":"plan-fixture","kind":{"Console":{"sql":ORIGINAL}},
            "show_value_editor":false,"results":[]
        }]})
        .to_string(),
    )
    .unwrap();
    // SAFETY: this binary has one test, and no worker exists before this point.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let (mut h, context) = app();
    editor(&h).focus();
    h.run_steps(1);
    let editor_id = context.memory(|memory| memory.focused()).unwrap();
    key(&mut h, Modifiers::COMMAND, Key::Enter);
    wait_until(&mut h, "editable ordinary result", |h| {
        h.query_by_label("Ada Żółw").is_some() && h.query_by_label("Editable result").is_some()
    });
    let result_tab = h
        .query_all_by_label_contains("⊞ ")
        .find_map(|node| node.accesskit_node().label())
        .expect("ordinary result tab label");

    set_sql(&mut h, SCRIPT);
    let caret = SCRIPT[..SCRIPT.find("UNION ALL").unwrap()].chars().count();
    set_selection(&context, editor_id, caret..caret);
    h.run_steps(2);
    run_action(&mut h, "Explain Plan");
    wait_until(&mut h, "compound current-statement plan", |h| {
        h.query_by_label("Query Plan").is_some() && has_operation(h, "orders")
    });
    assert_eq!(editor(&h).value().as_deref(), Some(SCRIPT));
    assert_eq!(selection(&context, editor_id), caret..caret);
    assert!(h.query_by_label("Submit").is_none());
    assert!(
        h.query_by_label("Options  ⏷").is_none(),
        "a plan has no grid pagination"
    );
    h.get_by_label("Property");
    h.get_by_label("Value");
    let raw = copy_plan(&mut h);
    assert!(raw.starts_with("id\tparent\tnotused\tdetail"));
    assert!(raw.contains("orders") && !raw.contains("customers"));
    let expanded = operations(&h);
    assert!(
        expanded > 1,
        "compound query supplies a hierarchical SQLite plan"
    );
    click(&mut h, "Collapse All");
    assert!(operations(&h) < expanded);
    assert_eq!(copy_plan(&mut h), raw);
    click(&mut h, "Expand All");
    assert_eq!(operations(&h), expanded);
    h.query_all_by_label_contains("Plan operation: ")
        .next()
        .unwrap()
        .click();
    h.run_steps(2);
    key(&mut h, Modifiers::NONE, Key::ArrowDown);
    assert_eq!(
        h.query_all_by_label_contains("Plan operation: ")
            .filter(|node| node.accesskit_node().toggled() == Some(Toggled::True))
            .count(),
        1
    );
    shot(&mut h, "61-query-plan.png");
    click(&mut h, "Raw");
    assert_eq!(copy_plan(&mut h), raw);
    assert!(operations(&h) == 0);
    shot(&mut h, "62-query-plan-raw.png");
    click(&mut h, "Operations Tree");

    verify_diagram(&mut h, &raw, &context, editor_id, caret);

    // A selected statement after a folded Unicode prefix must use source offsets.
    editor(&h).focus();
    h.run_steps(1);
    run_action(&mut h, "Fold All");
    let visible = editor(&h).value().unwrap();
    assert!(
        visible
            .chars()
            .any(|c| dbeast::folding::placeholder_index(c).is_some())
    );
    let start = visible[..visible.rfind("SELECT name").unwrap()]
        .chars()
        .count();
    let selected = start..visible.chars().count();
    set_selection(&context, editor_id, selected.clone());
    h.run_steps(2);
    run_action(&mut h, "Explain Plan");
    wait_until(&mut h, "selected statement after folded prefix", |h| {
        has_operation(h, "customers")
    });
    assert_eq!(editor(&h).value().as_deref(), Some(visible.as_str()));
    assert_eq!(selection(&context, editor_id), selected);
    let selected_raw = copy_plan(&mut h);
    assert!(selected_raw.contains("customers") && !selected_raw.contains("orders"));

    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    for label in [
        "Query Plan",
        "Operations Tree",
        "Raw",
        "Copy Original Query Plan",
    ] {
        let rect = h.get_by_label(label).rect();
        assert!(
            rect.left() >= 0.0
                && rect.right() <= 1100.0
                && rect.top() >= 0.0
                && rect.bottom() <= 500.0,
            "{label} must remain inside the small window: {rect:?}"
        );
    }
    assert!(has_operation(&h, "customers"));
    shot(&mut h, "63-query-plan-narrow.png");
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    editor(&h).focus();
    h.run_steps(1);
    run_action(&mut h, "Unfold All");
    assert_eq!(editor(&h).value().as_deref(), Some(SCRIPT));
    set_selection(&context, editor_id, 0..SCRIPT.chars().count());
    h.run_steps(2);
    run_action(&mut h, "Explain Plan");
    h.get_by_label("Select one statement to explain.");
    assert_eq!(
        copy_plan(&mut h),
        selected_raw,
        "invalid selection keeps the previous plan"
    );

    set_sql(&mut h, "SELECT missing_column FROM customers");
    run_action(&mut h, "Explain Plan");
    wait_until(&mut h, "plan error in Output", |h| {
        h.query_by_label_contains("no such column: missing_column")
            .is_some()
    });
    click(&mut h, "Query Plan");
    assert_eq!(
        copy_plan(&mut h),
        selected_raw,
        "server errors keep the previous plan"
    );
    click(&mut h, &result_tab);
    h.get_by_label("Ada Żółw");
    h.get_by_label("Olek Lis");
    assert_eq!(
        db.query_row("SELECT name FROM customers WHERE id=1", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "Ada Żółw"
    );

    // Discovering Explain must honor an inline buffer before it is submitted.
    h.get_by_label("Ada Żółw").click();
    h.run_steps(2);
    key(&mut h, Modifiers::NONE, Key::Enter);
    key(&mut h, Modifiers::COMMAND, Key::A);
    h.query_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("active inline value editor")
        .type_text("Pending name");
    h.run_steps(2);
    open_action(&mut h, "Explain Plan");
    assert!(
        palette_action(&h, "Explain Plan")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("Submit or revert pending grid changes before explaining a statement");
    key(&mut h, Modifiers::NONE, Key::Escape);
    click(&mut h, "Revert");
    h.get_by_label("Ada Żółw");

    // Related rows explain the captured query with its typed parameters.
    h.get_by_label("Ada Żółw").click();
    h.run_steps(2);
    key(&mut h, Modifiers::NONE, Key::F4);
    wait_until(&mut h, "incoming relationship", |h| {
        h.query_by_role_and_label(Role::Button, "Referencing Rows: main.orders · FK 0")
            .is_some()
    });
    click(&mut h, "Referencing Rows: main.orders · FK 0");
    wait_until(&mut h, "bound result", |h| {
        h.query_by_label("Related rows · bound parameters")
            .is_some()
            && h.query_by_label("11").is_some()
    });
    editor(&h).focus();
    h.run_steps(1);
    run_action(&mut h, "Explain Plan");
    wait_until(&mut h, "related query plan", |h| {
        h.query_by_label("Operations Tree").is_some()
    });
    shot(&mut h, "78-related-query-plan.png");
    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    shot(&mut h, "79-related-query-plan-narrow.png");
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    click(&mut h, "QL Plans");
    set_sql(&mut h, ORIGINAL);
    click(&mut h, "Query Plan");
    wait_until(
        &mut h,
        "session with ordinary results and ephemeral plan",
        |_| {
            let saved = session(&session_path);
            let active = saved["active"].as_u64().unwrap() as usize;
            let tab = &saved["tabs"][active];
            tab["title"] == "Plans"
                && tab["kind"]["Console"]["sql"] == ORIGINAL
                && tab["view"].is_null()
                && tab["results"]
                    .as_array()
                    .is_some_and(|results| results.len() == 1)
                && tab["log"].as_array().is_some_and(|log| {
                    log.iter().any(|line| {
                        line["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("missing_column"))
                    })
                })
        },
    );
    let saved = session(&session_path);
    assert!(
        saved["tabs"][saved["active"].as_u64().unwrap() as usize]
            .get("plan")
            .is_none()
    );
    drop(h);
    let (mut restored, _) = app();
    assert_eq!(editor(&restored).value().as_deref(), Some(ORIGINAL));
    assert!(restored.query_by_label("Query Plan").is_none());
    click(&mut restored, &result_tab);
    restored.get_by_label("Ada Żółw");
    restored.get_by_label("Olek Lis");
    drop(restored);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
