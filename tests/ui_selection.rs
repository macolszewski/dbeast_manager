//! Rectangular grid selections, lossless clipboard data and cached aggregates.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::Role;
use egui::{Key, Modifiers, PointerButton, Pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

const ADA_NOTE: &str = "keep\tŁódź\n\"quoted\"";
const OLEK_NOTE: &str = "drop\r\nsecond";
const LENA_NOTE: &str = "keep żółć";

fn app(size: [f32; 2]) -> H<'static> {
    let mut h = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    h
}

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render().unwrap().save(dir.join(name)).unwrap();
}

fn copy(h: &mut H<'_>) -> String {
    h.event(egui::Event::Copy);
    h.run_steps(1);
    h.output()
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
        .expect("clipboard output for the focused selection")
}

fn assert_tsv(h: &mut H<'_>, expected: &[&[&str]]) {
    let copied = copy(h);
    let records: Vec<Vec<String>> = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .has_headers(false)
        .from_reader(copied.as_bytes())
        .records()
        .map(|record| record.unwrap().iter().map(str::to_owned).collect())
        .collect();
    assert_eq!(records, expected, "lossless visual-order TSV: {copied:?}");
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn extend(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click_modifiers(Modifiers::SHIFT);
    h.run_steps(3);
}

fn drag(h: &mut H<'_>, from: Pos2, to: Pos2) {
    h.event(egui::Event::PointerMoved(from));
    h.run_steps(1);
    h.event(egui::Event::PointerButton {
        pos: from,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(1);
    for step in 1..=5 {
        h.event(egui::Event::PointerMoved(
            from + (to - from) * (step as f32 / 5.0),
        ));
        h.run_steps(1);
    }
    h.event(egui::Event::PointerButton {
        pos: to,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(3);
}

fn focused_editor<'h>(h: &'h H<'_>) -> egui_kittest::Node<'h> {
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("focused cell editor")
}

fn set_query(h: &mut H<'_>, text: &str) {
    h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
        .type_text(text);
    h.run_steps(3);
}

fn toolbar_button<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    let toolbar_y = h.get_by_label("Options  ⏷").rect().center().y;
    h.get_all_by_label(label)
        .find(|node| (node.rect().center().y - toolbar_y).abs() < 5.0)
        .expect("result toolbar button")
}

fn view_action(h: &mut H<'_>, label: &str) {
    if let Some(action) = h.query_by_role_and_label(Role::Button, label) {
        action.click();
    } else {
        click(h, "Options  ⏷");
        h.query_all_by_label(label)
            .find(|node| matches!(node.accesskit_node().role(), Role::Button | Role::CheckBox))
            .expect("view action in the result Options menu")
            .click();
    }
    h.run_steps(3);
}

fn metric(h: &H<'_>, name: &str, value: &str) {
    h.get_by_label(&format!("{name}: {value}"));
}

fn saved_result(path: &Path) -> serde_json::Value {
    let session: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    session["tabs"][0]["results"][0].clone()
}

fn last_visible_drag_record(h: &H<'_>, size: [f32; 2]) -> usize {
    let viewport = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(size[0], size[1]));
    (1..=100)
        .filter(|index| {
            h.query_by_label(&format!("record-{index:03}"))
                .is_some_and(|node| viewport.contains(node.rect().center()))
        })
        .max()
        .expect("at least one visible drag fixture record")
}

fn drag_beyond_viewport(h: &mut H<'_>, transposed: bool, size: [f32; 2]) {
    let initially_visible = last_visible_drag_record(h, size);
    assert!(
        initially_visible < 100,
        "fixture must extend beyond the initial viewport"
    );
    let from = h.get_by_label("record-001").rect().center();
    let edge = if transposed {
        egui::pos2(size[0] - 1.0, from.y)
    } else {
        egui::pos2(from.x, size[1] - 1.0)
    };
    h.event(egui::Event::PointerMoved(from));
    h.run_steps(1);
    h.event(egui::Event::PointerButton {
        pos: from,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(1);
    // Start the drag inside the first cell before holding past the viewport edge.
    h.event(egui::Event::PointerMoved(from + egui::vec2(8.0, 0.0)));
    h.run_steps(2);
    h.event(egui::Event::PointerMoved(edge));
    h.run_steps(150);
    h.event(egui::Event::PointerButton {
        pos: edge,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    h.run_steps(3);

    let copied = copy(h);
    let records: Vec<Vec<String>> = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .has_headers(false)
        .from_reader(copied.as_bytes())
        .records()
        .map(|record| record.unwrap().iter().map(str::to_owned).collect())
        .collect();
    let selected: Vec<&str> = if transposed {
        assert_eq!(records.len(), 1, "horizontal drag stays on the name field");
        records[0].iter().map(String::as_str).collect()
    } else {
        records
            .iter()
            .map(|record| {
                assert_eq!(record.len(), 1, "vertical drag stays in the name column");
                record[0].as_str()
            })
            .collect()
    };
    assert!(
        selected.len() > initially_visible,
        "drag must include initially hidden records: selected {}, initially visible {initially_visible}, transposed {transposed}",
        selected.len()
    );
    for (index, value) in selected.iter().enumerate() {
        assert_eq!(*value, format!("record-{:03}", index + 1));
    }
    assert!(
        last_visible_drag_record(h, size) > initially_visible,
        "drag must also scroll the viewport to reveal the extended selection"
    );
    h.run_steps(30);
    assert_eq!(
        copy(h),
        copied,
        "releasing the pointer stops range extension"
    );
}

#[test]
fn ranges_copy_visual_data_and_aggregates_follow_edits_filters_and_pages() {
    let dir = std::env::temp_dir().join(format!("dbeast_selection_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("balances.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE balances (id INTEGER PRIMARY KEY, name TEXT, amount INTEGER, note TEXT)",
    )
    .unwrap();
    for row in [
        (1, "Ada", 10, ADA_NOTE),
        (2, "Olek", 20, OLEK_NOTE),
        (3, "Lena", 30, LENA_NOTE),
        (4, "Maria", 40, "Page two"),
        (5, "Tomek", 50, "Last record"),
    ] {
        db.execute("INSERT INTO balances VALUES (?1, ?2, ?3, ?4)", row)
            .unwrap();
    }
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "selection-fixture".into();
    source.name = "Local balances".into();
    source.file = database.to_string_lossy().into_owned();
    let config_path = dir.join("config.json");
    let config = Config {
        data_sources: vec![source],
        max_rows: 3,
        tile_url: String::new(),
        ..Config::default()
    };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let data = QueryResult {
        columns: [
            ("id", "integer"),
            ("name", "text"),
            ("amount", "integer"),
            ("note", "text"),
        ]
        .into_iter()
        .map(|(name, type_name)| Column {
            name: name.into(),
            type_name: type_name.into(),
        })
        .collect(),
        rows: [
            (1, "Ada", 10, ADA_NOTE),
            (2, "Olek", 20, OLEK_NOTE),
            (3, "Lena", 30, LENA_NOTE),
        ]
        .into_iter()
        .map(|(id, name, amount, note)| {
            vec![
                Value::Int(id),
                Value::Text(name.into()),
                Value::Int(amount),
                Value::Text(note.into()),
            ]
        })
        .collect(),
        truncated: true,
        elapsed: Duration::from_millis(1),
        ..QueryResult::default()
    };
    let session_path = dir.join("session.json");
    let session = serde_json::json!({
        "tabs": [{
            "title": "balances", "ds_id": "selection-fixture",
            "kind": {"Table": {
                "schema": null, "table": "balances", "filter": "", "order": "id",
                "primary_key": ["id"]
            }},
            "view": 0,
            "results": [{
                "title": "balances", "sql": "SELECT * FROM balances ORDER BY id",
                "page": Page::first(3), "data": data, "show_value_editor": false
            }]
        }]
    });
    std::fs::write(&session_path, serde_json::to_string(&session).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = app([1440.0, 900.0]);
    h.get_by_label("1-3 of 3+");

    // The anchor survives extending, shrinking and returning to one cell.
    click(&mut h, "Ada");
    extend(&mut h, "20");
    assert_tsv(&mut h, &[&["Ada", "10"], &["Olek", "20"]]);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    h.run_steps(3);
    assert_tsv(&mut h, &[&["Ada", "10"], &["Olek", "20"], &["Lena", "30"]]);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowUp);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowLeft);
    h.run_steps(3);
    assert_tsv(&mut h, &[&["Ada"], &["Olek"]]);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowUp);
    h.run_steps(3);
    assert_eq!(copy(&mut h), "Ada");

    let from = h.get_by_label("Ada").rect().center();
    let to = h.get_by_label("30").rect().center();
    drag(&mut h, from, to);
    assert_tsv(&mut h, &[&["Ada", "10"], &["Olek", "20"], &["Lena", "30"]]);
    shot(&mut h, "42-grid-selection.png");
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(3);
    assert_tsv(
        &mut h,
        &[
            &["1", "Ada", "10", ADA_NOTE],
            &["2", "Olek", "20", OLEK_NOTE],
            &["3", "Lena", "30", LENA_NOTE],
        ],
    );

    // Headers select without sorting; the independent sort action changes copy order.
    click(&mut h, "Select Column: name");
    assert_tsv(&mut h, &[&["Ada"], &["Olek"], &["Lena"]]);
    click(&mut h, "Sort by name");
    click(&mut h, "Sort by name");
    click(&mut h, "Select Column: name");
    assert_tsv(&mut h, &[&["Olek"], &["Lena"], &["Ada"]]);
    click(&mut h, "Olek");
    extend(&mut h, "10");
    assert_tsv(&mut h, &[&["Olek", "20"], &["Lena", "30"], &["Ada", "10"]]);
    click(&mut h, "Select Row: 1");
    assert_tsv(&mut h, &[&["2", "Olek", "20", OLEK_NOTE]]);
    click(&mut h, "Sort by name");

    // Text editing owns Cmd+A/C instead of replacing the grid's range.
    // Restored rows are read-only; the first edit attempt reloads the page.
    click(&mut h, "20");
    h.key_press(Key::Enter);
    wait_until(&mut h, "refreshed restored table", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    click(&mut h, "20");
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(1);
    assert_eq!(copy(&mut h), "20");
    focused_editor(&h).type_text("50");
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(copy(&mut h), "50");
    let stored_amount: i64 = db
        .query_row("SELECT amount FROM balances WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored_amount, 20, "selection must not submit staged edits");
    click(&mut h, "Select Column: amount");
    assert_tsv(&mut h, &[&["10"], &["50"], &["30"]]);
    view_action(&mut h, "Aggregate View");
    for (name, value) in [
        ("ROWS", "3"),
        ("COLS", "1"),
        ("COUNT", "3"),
        ("SUM", "90"),
        ("AVG", "30"),
        ("MIN", "10"),
        ("MAX", "50"),
        ("MEDIAN", "30"),
    ] {
        metric(&h, name, value);
    }
    h.run_steps(10);
    metric(&h, "SUM", "90");
    shot(&mut h, "43-aggregate-view.png");

    // Find collapses an old range even when its match is already the active cell.
    click(&mut h, "Select Column: amount");
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    h.get_by_label("1 of 1 cells");
    metric(&h, "COUNT", "1");
    metric(&h, "SUM", "30");
    click(&mut h, "Close Grid Find");
    assert_eq!(copy(&mut h), "30");

    // Only visible rows participate in header selection and aggregation.
    click(&mut h, "50");
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    set_query(&mut h, "keep");
    click(&mut h, "Filter rows");
    assert!(h.query_by_label("Olek").is_none());
    click(&mut h, "Select Column: amount");
    assert_tsv(&mut h, &[&["10"], &["30"]]);
    metric(&h, "COUNT", "2");
    metric(&h, "SUM", "40");
    metric(&h, "AVG", "20");
    set_query(&mut h, "missing-row");
    h.get_by_label("No matches on this page");
    metric(&h, "COUNT", "0");
    click(&mut h, "Close Grid Find");
    click(&mut h, "Select Column: amount");
    metric(&h, "SUM", "90");

    // Transpose changes both copied dimensions and the direction of range navigation.
    view_action(&mut h, "Transpose");
    click(&mut h, "Select Column: amount");
    assert_tsv(&mut h, &[&["10", "50", "30"]]);
    metric(&h, "ROWS", "1");
    metric(&h, "COLS", "3");
    click(&mut h, "10");
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowRight);
    h.run_steps(3);
    assert_tsv(&mut h, &[&["10", "50"]]);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    h.run_steps(3);
    assert_tsv(&mut h, &[&["10", "50"], &[ADA_NOTE, OLEK_NOTE]]);
    click(&mut h, "Select Column: amount");

    h.set_size(egui::vec2(1100.0, 720.0));
    h.run_steps(3);
    shot(&mut h, "44-aggregate-narrow.png");
    for node in [
        h.get_by_label("Close Aggregate View"),
        h.get_by_label("SUM: 90"),
        h.get_by_label("MEDIAN: 30"),
    ] {
        let rect = node.rect();
        assert!(
            rect.left() >= 0.0
                && rect.top() >= 0.0
                && rect.right() <= 1100.0
                && rect.bottom() <= 720.0,
            "aggregate control must remain in the narrow viewport: {rect:?}"
        );
    }
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    view_action(&mut h, "Transpose");
    click(&mut h, "Revert");
    click(&mut h, "Select Column: amount");
    metric(&h, "SUM", "60");

    // Replacing the page clears stale ranges while preserving the open panel.
    toolbar_button(&h, "▶").click();
    wait_until(&mut h, "second page", |h| {
        h.query_by_label("4-5 of 5").is_some()
    });
    h.get_by_label("Close Aggregate View");
    metric(&h, "COUNT", "0");
    click(&mut h, "Select Column: amount");
    assert_tsv(&mut h, &[&["40"], &["50"]]);
    metric(&h, "SUM", "90");
    metric(&h, "AVG", "45");
    wait_until(&mut h, "saved aggregate preference", |_| {
        let result = saved_result(&session_path);
        result["page"]["skip"] == 3 && result["aggregate_view"] == true
    });
    drop(h);

    // Session restore keeps the view preference and rebuilds only after selection.
    let mut h = app([1100.0, 720.0]);
    h.get_by_label("4-5 of 5");
    h.get_by_label("Close Aggregate View");
    metric(&h, "COUNT", "0");
    click(&mut h, "Select Column: amount");
    metric(&h, "SUM", "90");

    // The fixed panel header and scrollable metrics remain usable in a short window.
    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    let panel_body =
        h.get_by_label("Close Aggregate View").rect().center() + egui::vec2(-100.0, 45.0);
    h.event(egui::Event::PointerMoved(panel_body));
    h.run_steps(1);
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -800.0),
        modifiers: Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    h.run_steps(5);
    for node in [
        h.get_by_label("SUM: 90"),
        h.get_by_label("Close Aggregate View"),
    ] {
        let rect = node.rect();
        assert!(
            rect.left() >= 0.0
                && rect.top() >= 0.0
                && rect.right() <= 1100.0
                && rect.bottom() <= 500.0,
            "aggregate control must remain in the short viewport: {rect:?}"
        );
    }
    shot(&mut h, "45-aggregate-short.png");
    click(&mut h, "Close Aggregate View");
    assert!(h.query_by_label("SUM: 90").is_none());
    assert!(h.query_by_label("Close Aggregate View").is_none());

    drop(h);

    // A larger restored page exercises drag scrolling without fetching new rows.
    let drag_data = QueryResult {
        columns: [
            ("id", "integer"),
            ("name", "text"),
            ("amount", "integer"),
            ("note", "text"),
        ]
        .into_iter()
        .map(|(name, type_name)| Column {
            name: name.into(),
            type_name: type_name.into(),
        })
        .collect(),
        rows: (1..=100)
            .map(|index| {
                vec![
                    Value::Int(index),
                    Value::Text(format!("record-{index:03}")),
                    Value::Int(index * 10),
                    Value::Text("Drag fixture".into()),
                ]
            })
            .collect(),
        ..QueryResult::default()
    };
    for transposed in [false, true] {
        let mut drag_session = session.clone();
        let tab = &mut drag_session["tabs"][0];
        tab["transposed"] = transposed.into();
        tab["aggregate_view"] = false.into();
        let result = &mut tab["results"][0];
        result["transposed"] = transposed.into();
        result["aggregate_view"] = false.into();
        result["data"] = serde_json::to_value(&drag_data).unwrap();
        result["page"] = serde_json::to_value(Page::first(100)).unwrap();
        std::fs::write(&session_path, serde_json::to_string(&drag_session).unwrap()).unwrap();

        let size = [1100.0, 600.0];
        let mut h = app(size);
        drag_beyond_viewport(&mut h, transposed, size);
        drop(h);
    }

    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
