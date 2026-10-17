//! Appearance: grid row height, stripes and font sizes preview live, Cancel
//! restores them and OK saves them. Uses a restored SQLite result, no server.

mod common;

use dbeast::config::{Config, RowDensity};
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn row_pitch(h: &H<'_>) -> f32 {
    let center = |text| h.get_by_label(text).rect().center().y;
    center("Olek Lis") - center("Ada Nowak")
}

fn open_appearance(h: &mut H<'_>) {
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    h.run_steps(3);
    h.get_by_role_and_label(Role::TextInput, "Find Action").type_text("Appearance");
    h.run_steps(3);
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert!(h.query_by_label("Data Editor").is_some(), "Appearance dialog is open");
}

fn choose_density(h: &mut H<'_>, from: &str, to: &str) {
    h.get_all_by_role(Role::ComboBox)
        .find(|n| n.value().as_deref() == Some(from))
        .expect("row height combo")
        .click();
    h.run_steps(3);
    h.get_by_label(to).click();
    h.run_steps(3);
}

#[test]
fn appearance_previews_cancels_and_saves() {
    let dir = std::env::temp_dir().join(format!("dbeast_appearance_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("contacts.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT);")
        .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "appearance-fixture".into();
    source.name = "Local CRM".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config { data_sources: vec![source], tile_url: String::new(), ..Config::default() };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let data = QueryResult {
        columns: [("id", "INTEGER"), ("name", "TEXT")]
            .into_iter()
            .map(|(name, type_name)| Column { name: name.into(), type_name: type_name.into() })
            .collect(),
        rows: vec![
            vec![Value::Int(1), Value::Text("Ada Nowak".into())],
            vec![Value::Int(2), Value::Text("Olek Lis".into())],
        ],
        elapsed: Duration::from_millis(1),
        ..QueryResult::default()
    };
    let session = serde_json::json!({
        "tabs": [{
            "title": "contacts",
            "ds_id": "appearance-fixture",
            "kind": {"Table": {
                "schema": null, "table": "contacts", "filter": "", "order": "id",
                "primary_key": ["id"]
            }},
            "view": 0,
            "results": [{
                "title": "contacts", "sql": "SELECT * FROM contacts ORDER BY id",
                "page": Page::first(500), "data": data
            }]
        }]
    });
    std::fs::write(dir.join("session.json"), serde_json::to_string(&session).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };

    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(5);
    // The pitch includes the table's item spacing; densities change it by their padding.
    let default = row_pitch(&h);

    // Changes show at once and Cancel takes them back.
    open_appearance(&mut h);
    choose_density(&mut h, "Default", "Comfortable");
    assert!((row_pitch(&h) - (default + 4.0)).abs() < 0.5, "comfortable preview: {}", row_pitch(&h));
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.query_by_label("Data Editor").is_none());
    assert!((row_pitch(&h) - default).abs() < 0.5, "cancel restores: {}", row_pitch(&h));

    // OK keeps and saves them.
    open_appearance(&mut h);
    choose_density(&mut h, "Default", "Compact");
    h.get_by_label("Striped rows").click();
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/97-appearance.png"))
        .unwrap();
    h.get_by_label("OK").click();
    h.run_steps(3);
    assert!((row_pitch(&h) - (default - 2.0)).abs() < 0.5, "compact rows: {}", row_pitch(&h));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let appearance: Option<dbeast::config::Appearance> =
            serde_json::from_value(saved["appearance"].clone()).ok();
        if appearance
            .as_ref()
            .is_some_and(|a| a.row_density == RowDensity::Compact && !a.striped_rows)
        {
            break;
        }
        assert!(Instant::now() < deadline, "appearance not saved: {}", saved["appearance"]);
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(h);
    std::fs::remove_dir_all(dir).ok();
}
