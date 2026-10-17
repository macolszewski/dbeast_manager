//! Record View shares the table's edits and survives refreshes and restarts.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

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

fn view_action(h: &mut H<'_>, label: &str) {
    if let Some(action) = h.query_by_role_and_label(Role::Button, label) {
        action.click();
    } else {
        h.get_by_label("Options  ⏷").click();
        h.run_steps(3);
        h.query_all_by_label(label)
            .find(|node| matches!(node.accesskit_node().role(), Role::Button | Role::CheckBox))
            .expect("view action in the result Options menu")
            .click();
    }
    h.run_steps(3);
}

fn record_value<'h>(h: &'h H<'_>, value: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(value)
        .max_by(|a, b| a.rect().center().x.total_cmp(&b.rect().center().x))
        .expect("value in Record View")
}

fn saved_result(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap();
    let session: serde_json::Value = serde_json::from_str(&text).unwrap();
    session["tabs"][0]["results"][0].clone()
}

fn assert_transposed(h: &H<'_>) {
    let column = |name| {
        h.get_all_by_label(name)
            .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
            .expect("transposed column name")
            .rect()
    };
    let name = column("Select Column: name");
    let email = column("Select Column: email");
    assert!((name.left() - email.left()).abs() < 1.0);
    assert!(
        name.bottom() < email.bottom(),
        "columns become successive rows"
    );
}

#[test]
fn record_edit_paging_and_view_preferences_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("dbeast_record_ui_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("contacts.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT, email TEXT, city TEXT, notes TEXT);
         INSERT INTO contacts VALUES
           (1, 'Ada Nowak', 'ada@example.test', 'Kraków', 'Primary contact'),
           (2, 'Olek Lis', 'olek@example.test', 'Gdańsk', 'Weekly report'),
           (3, 'Lena Wójcik', 'lena@example.test', 'Poznań', 'New customer');",
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "record-fixture".into();
    source.name = "Local CRM".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        max_rows: 2,
        tile_url: String::new(),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let data = QueryResult {
        columns: [
            ("id", "INTEGER"),
            ("name", "TEXT"),
            ("email", "TEXT"),
            ("city", "TEXT"),
            ("notes", "TEXT"),
        ]
        .into_iter()
        .map(|(name, type_name)| Column {
            name: name.into(),
            type_name: type_name.into(),
        })
        .collect(),
        rows: [
            (
                1,
                ["Ada Nowak", "ada@example.test", "Kraków", "Primary contact"],
            ),
            (
                2,
                ["Olek Lis", "olek@example.test", "Gdańsk", "Weekly report"],
            ),
        ]
        .into_iter()
        .map(|(id, values)| {
            std::iter::once(Value::Int(id))
                .chain(values.map(|text| Value::Text(text.into())))
                .collect()
        })
        .collect(),
        truncated: true,
        elapsed: Duration::from_millis(1),
        ..QueryResult::default()
    };
    let session_path = dir.join("session.json");
    let session = serde_json::json!({
        "tabs": [{
            "title": "contacts",
            "ds_id": "record-fixture",
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
    std::fs::write(&session_path, serde_json::to_string(&session).unwrap()).unwrap();
    // SAFETY: this is the only test in this binary, before starting the app.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };

    let mut h = app([1440.0, 900.0]);
    assert!(h.query_by_label("1-2 of 2+").is_some());
    assert!(
        h.query_by_role_and_label(Role::Label, "Record View")
            .is_none()
    );
    view_action(&mut h, "Record View");
    assert!(
        h.query_by_label("Select a cell to view its record")
            .is_some()
    );
    h.get_by_label("Ada Nowak").click();
    h.run_steps(3);
    // The grid, Record View and read-only Value Editor share the selected value.
    assert_eq!(h.get_all_by_label("Ada Nowak").count(), 3);
    assert!(h.query_by_label_contains("name · ").is_some());

    // Closing the bottom value panel keeps the separate Record View open.
    h.get_by_label("Close Value Editor").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("name · ").is_none());
    h.get_by_role_and_label(Role::Label, "Record View");

    record_value(&h, "Ada Nowak").click();
    h.run_steps(2);
    // Restored rows are read-only; the first edit attempt reloads the page.
    h.key_press(Key::Enter);
    wait_until(&mut h, "refreshed restored table", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    record_value(&h, "Ada Nowak").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_all_by_role(egui::accesskit::Role::TextInput)
        .find(|n| n.is_focused())
        .expect("Record View cell editor")
        .type_text("Ada Kowalska");
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(h.get_all_by_label("Ada Kowalska").count(), 2);
    h.get_by_label("DML").click();
    h.run_steps(3);
    assert!(
        h.query_by_label_contains("-- parameters: 1 = 'Ada Kowalska', 2 = 1")
            .is_some()
    );
    let stored_name = || {
        db.query_row("SELECT name FROM contacts WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
    };
    assert_eq!(stored_name(), "Ada Nowak", "Enter only stages the edit");
    shot(&mut h, "28-record-view.png");

    view_action(&mut h, "Transpose");
    shot(&mut h, "30-record-view-transposed.png");
    assert_transposed(&h);
    assert_eq!(h.get_all_by_label("Ada Kowalska").count(), 2);
    h.get_by_label("Submit").click();
    wait_until(&mut h, "submitted record edit", |h| {
        h.query_by_label("DML").is_none() && stored_name() == "Ada Kowalska"
    });
    h.get_by_role_and_label(Role::Label, "Record View");

    // Paging replaces the result data, but keeps both view preferences.
    let toolbar_y = h.get_by_label("Options  ⏷").rect().center().y;
    h.get_all_by_label("▶")
        .find(|n| (n.rect().center().y - toolbar_y).abs() < 5.0)
        .expect("next page button")
        .click();
    wait_until(&mut h, "second page", |h| {
        h.query_by_label("3-3 of 3").is_some()
    });
    h.get_by_label("Lena Wójcik").click();
    h.run_steps(3);
    assert_eq!(h.get_all_by_label("Lena Wójcik").count(), 2);
    assert!(h.query_by_label_contains("name · ").is_none());
    wait_until(&mut h, "saved view preferences", |_| {
        let result = saved_result(&session_path);
        result["page"]["skip"] == 2
            && result["record_view"] == true
            && result["show_value_editor"] == false
            && result["transposed"] == true
    });
    drop(h);

    let mut h = app([1280.0, 720.0]);
    assert!(h.query_by_label("3-3 of 3").is_some());
    h.get_by_role_and_label(Role::Label, "Record View");
    assert_transposed(&h);
    h.get_by_label("Lena Wójcik").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("name · ").is_none());
    let value = record_value(&h, "Lena Wójcik").rect();
    assert!(value.right() <= 1280.0 && value.bottom() <= 720.0);
    shot(&mut h, "29-record-view-narrow.png");

    // The restored second page is read again from the server (LIMIT/OFFSET)
    // before the first edit, and its edit is submitted to the right row.
    record_value(&h, "Lena Wójcik").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    wait_until(&mut h, "refreshed second page", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    assert!(h.query_by_label("3-3 of 3").is_some());
    record_value(&h, "Lena Wójcik").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_all_by_role(egui::accesskit::Role::TextInput)
        .find(|n| n.is_focused())
        .expect("Record View cell editor")
        .type_text("Lena Nowak");
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("Submit").click();
    let stored = |id: i64| {
        db.query_row("SELECT name FROM contacts WHERE id = ?1", [id], |row| row.get::<_, String>(0))
            .unwrap()
    };
    wait_until(&mut h, "submitted second page edit", |_| stored(3) == "Lena Nowak");
    assert_eq!(stored(1), "Ada Kowalska");
    assert_eq!(stored(2), "Olek Lis");

    drop(h);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
