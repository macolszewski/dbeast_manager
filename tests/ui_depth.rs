//! Depth View on a restored, synthetic log result. No database is opened.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;

type H<'a> = Harness<'a, dbeast::ui::App>;

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

/// Incident-style rows: users in order, events every 40 s, delivery in batches
/// and one reversed and one missing processing time.
fn logs() -> QueryResult {
    let time = |seconds: i64| {
        let minutes = seconds / 60;
        Value::Text(format!("2026-10-15 10:{:02}:{:02}", minutes % 60, seconds % 60))
    };
    let users = ["Ada", "Bartek", "Celina", "Dawid", "Ewa"];
    let rows = (0..30)
        .map(|index: i64| {
            let occurred = index * 40;
            let received = (occurred / 300 + 1) * 300; // batch delivery every 5 minutes
            let processed = match index {
                7 => Value::Null,
                12 => time(received - 30), // processed before it was received
                _ => time(received + 2 + index % 5),
            };
            vec![
                Value::Int(1000 + index),
                Value::Text(users[index as usize % users.len()].into()),
                Value::Text(format!("request-{}", 900 + index / 3)),
                time(occurred),
                time(received),
                processed,
                Value::Text(if index % 9 == 0 { "ERROR" } else { "INFO" }.into()),
            ]
        })
        .collect();
    QueryResult {
        columns: [
            ("log_id", "int4"),
            ("user_name", "text"),
            ("request_id", "text"),
            ("occurred_at", "timestamp"),
            ("received_at", "timestamp"),
            ("processed_at", "timestamp"),
            ("level", "text"),
        ]
        .iter()
        .map(|(name, ty)| Column {
            name: (*name).into(),
            type_name: (*ty).into(),
        })
        .collect(),
        rows,
        ..Default::default()
    }
}

#[test]
fn depth_view_projects_rows_on_one_time_axis_without_running_sql() {
    let directory = std::env::temp_dir().join(format!("dbeast_depth_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "depth-fixture".into();
    source.name = "Logs".into();
    source.file = directory
        .join("never-opened.sqlite")
        .to_string_lossy()
        .into_owned();
    let config = Config {
        data_sources: vec![source],
        tile_url: String::new(),
        files_root: Some(directory.clone()),
        ..Config::default()
    };
    std::fs::write(
        directory.join("config.json"),
        common::legacy_config(&config).unwrap(),
    )
    .unwrap();
    let session = serde_json::json!({"active": 0, "tabs": [{
        "title": "logs", "ds_id": "depth-fixture",
        "kind": {"Table": {"schema": null, "table": "logs", "filter": "", "order": "user_name"}},
        "view": 0, "show_value_editor": false,
        "results": [{"title": "logs", "sql": "SELECT * FROM logs", "page": Page::first(500),
            "data": logs(), "show_value_editor": false}]
    }]});
    std::fs::write(directory.join("session.json"), session.to_string()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", directory.join("config.json")) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    // Select a row in the table; Depth View keeps the same selection.
    h.query_all_by_label("Celina")
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .unwrap()
        .click();
    h.run_steps(3);
    click(&mut h, "Depth View");
    assert!(h.query_by_label_contains("30 of 30 rows in the window").is_some());
    click(&mut h, "+ Time Column");
    click(&mut h, "+ Time Column");
    // Three projections per row, minus the one missing processing time.
    assert!(h.query_by_label_contains("89 projections").is_some());
    assert!(h.query_by_label_contains("1 row without a value").is_some());
    // The details panel shows exact, signed deltas of the selected row.
    assert!(h.query_by_label("Row 3").is_some());
    assert!(h.query_by_label_contains("received_at − occurred_at: +3 min 40 s").is_some());
    h.render()
        .unwrap()
        .save(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/screenshots")
                .join("93-depth-view.png"),
        )
        .unwrap();

    // Hovering the histogram moves a time wall through the scene.
    let histogram = h.get_by_label("Time histogram").rect();
    h.event(egui::Event::PointerMoved(egui::pos2(histogram.left() + histogram.width() * 0.25, histogram.center().y)));
    h.run_steps(30);
    assert!(h.query_by_label_contains("drag or click to move the window").is_some(), "the hovered time is shown");
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/93-depth-view-wall.png"))
        .unwrap();
    h.event(egui::Event::PointerMoved(egui::pos2(10.0, 10.0)));
    h.run_steps(3);

    // The camera orbits and zooms without touching the data or the selection.
    let canvas = egui::pos2(600.0, 650.0);
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    h.event(egui::Event::PointerMoved(canvas));
    h.event(button(canvas, true));
    h.run_steps(1);
    for step in 1..=6 {
        h.event(egui::Event::PointerMoved(canvas + egui::vec2(step as f32 * 25.0, step as f32 * 8.0)));
        h.run_steps(1);
    }
    h.event(button(canvas + egui::vec2(150.0, 48.0), false));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 360.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    h.run_steps(10);
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/93-depth-view-orbit.png"))
        .unwrap();
    assert!(h.query_by_label("Row 3").is_some());
    assert!(h.query_by_label_contains("30 of 30 rows in the window").is_some());

    // The edge of the details panel widens it, so long values fit.
    let before = h.get_by_label("Row 3").rect().left();
    let edge = h.get_by_label("Resize details").rect().center();
    h.event(egui::Event::PointerMoved(edge));
    h.event(button(edge, true));
    h.run_steps(1);
    for step in 1..=6 {
        h.event(egui::Event::PointerMoved(edge - egui::vec2(step as f32 * 30.0, 0.0)));
        h.run_steps(1);
    }
    h.event(button(edge - egui::vec2(180.0, 0.0), false));
    h.run_steps(3);
    let after = h.get_by_label("Row 3").rect().left();
    assert!(before - after > 150.0, "the panel grows to the left: {before} -> {after}");

    // Narrowing the window filters projections locally; Fit restores the range.
    click(&mut h, "Narrow");
    assert!(h.query_by_label_contains("30 of 30 rows in the window").is_none());
    click(&mut h, "Fit");
    assert!(h.query_by_label_contains("30 of 30 rows in the window").is_some());

    // Back to the table: the grid and its rows are untouched.
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Depth View")
        .click();
    h.run_steps(3);
    assert!(h.query_all_by_label("Celina").next().is_some());
    assert!(!directory.join("never-opened.sqlite").exists());
    drop(h);
    std::fs::remove_dir_all(directory).unwrap();
}
