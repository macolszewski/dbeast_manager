//! Export Data dialog: a result to one file and a schema to one file per
//! table, run in the Run tool window. Uses a local SQLite file.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
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

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

#[test]
fn export_dialog_writes_a_result_and_a_schema() {
    let dir = std::env::temp_dir().join(format!("dbeast_export_ui_{}", uuid::Uuid::new_v4()));
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let database = dir.join("crm.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch(
            "CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO contacts VALUES (1, 'Ada Nowak'), (2, 'Olek Lis'), (3, 'Lena Wójcik');
             CREATE TABLE cities (name TEXT);
             INSERT INTO cities VALUES ('Kraków');",
        )
        .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "export-fixture".into();
    source.name = "Local CRM".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        tile_url: String::new(),
        transfer_dir: Some(out.clone()),
        ..Config::default()
    };
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
        truncated: true,
        elapsed: Duration::from_millis(1),
        ..QueryResult::default()
    };
    let session = serde_json::json!({
        "tabs": [{
            "title": "contacts",
            "ds_id": "export-fixture",
            "kind": {"Table": {
                "schema": null, "table": "contacts", "filter": "", "order": "id",
                "primary_key": ["id"]
            }},
            "view": 0,
            "results": [{
                "title": "contacts", "sql": "SELECT * FROM contacts ORDER BY id",
                "page": Page::first(2), "data": data
            }]
        }]
    });
    std::fs::write(dir.join("session.json"), serde_json::to_string(&session).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(5);

    // The whole result, not only the fetched page, goes to the file.
    click(&mut h, "Export  ⏷");
    click(&mut h, "Export Data…");
    assert!(h.query_by_label("Preview").is_some());
    h.get_all_by_role(Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("CSV"))
        .expect("format combo")
        .click();
    h.run_steps(3);
    click(&mut h, "JSON");
    let expected = out.join("contacts.json");
    assert!(
        h.get_all_by_role(Role::TextInput)
            .any(|n| n.value().as_deref() == Some(expected.to_string_lossy().as_ref())),
        "the file name follows the format"
    );
    assert!(h.query_by_label_contains("\"name\": \"Ada Nowak\"").is_some(), "preview in JSON");
    shot(&mut h, "98-export-data.png");
    click(&mut h, "Export");
    wait_until(&mut h, "the exported file", |_| expected.exists());
    let exported: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&expected).unwrap()).unwrap();
    assert_eq!(exported.as_array().unwrap().len(), 3, "all rows, beyond the fetched page");
    wait_until(&mut h, "the Run window outcome", |h| {
        h.query_by_label("Done").is_some() && h.query_all_by_label_contains("3 rows exported to 1 file").next().is_some()
    });
    shot(&mut h, "99-run-window.png");

    // A schema exports each chosen table to its own file.
    // The tree row, not the breadcrumbs or the data source switcher.
    h.get_all_by_label("Local CRM")
        .find(|n| n.rect().left() < 200.0 && n.rect().top() > 60.0 && n.rect().top() < 300.0)
        .expect("data source in Database Explorer")
        .click_secondary();
    h.run_steps(3);
    click(&mut h, "Import/Export ⏵");
    click(&mut h, "Export Data to Files…");
    wait_until(&mut h, "the table list", |h| h.query_by_label("cities").is_some());
    assert!(h.query_by_label("2 of 2").is_some());
    click(&mut h, "Export");
    wait_until(&mut h, "both files", |_| out.join("cities.json").exists() && out.join("contacts.json").exists());
    assert_eq!(std::fs::read_to_string(out.join("cities.json")).unwrap().matches("Kraków").count(), 1);
    drop(h);
    std::fs::remove_dir_all(dir).ok();
}
