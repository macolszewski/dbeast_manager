//! Exact-value column filters compose with page search and staged grid edits.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

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

fn set_field(h: &mut H<'_>, label: &str, text: &str) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    if text.is_empty() {
        h.key_press(Key::Backspace);
    } else {
        h.get_by_role_and_label(Role::TextInput, label)
            .type_text(text);
    }
    h.run_steps(3);
}

fn focused_editor<'h>(h: &'h H<'_>) -> egui_kittest::Node<'h> {
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("focused cell editor")
}

fn toolbar_button<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    let toolbar_y = h.get_by_label("Transpose").rect().center().y;
    h.get_all_by_label(label)
        .find(|node| (node.rect().center().y - toolbar_y).abs() < 5.0)
        .expect("result toolbar button")
}

fn select_only(h: &mut H<'_>, column: &str, value: &str) {
    h.get_by_label(&format!("Local Filter: {column}")).click();
    h.run_steps(3);
    set_field(h, "Search Filter Values", "");
    h.get_by_label("All Values").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::CheckBox, "Value").click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::CheckBox, &format!("Filter value: \"{value}\""))
        .click();
    h.run_steps(3);
    h.key_press(Key::Escape);
    h.run_steps(3);
}

fn clear_filters(h: &mut H<'_>) {
    h.get_by_label("Options  ⏷").click();
    h.run_steps(3);
    h.get_by_label("Clear Local Filter For All Columns").click();
    h.run_steps(3);
}

#[test]
fn local_filters_compose_with_search_editing_and_paging() {
    let dir = std::env::temp_dir().join(format!("dbeast_local_filter_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("contacts.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT, city TEXT, status TEXT);
         INSERT INTO contacts VALUES
           (1, 'Ada Nowak', 'Łódź', 'Active'),
           (2, 'Olek Lis', 'Kraków', 'Active'),
           (3, 'Lena Wójcik', 'Łódź', 'Pending'),
           (4, 'Jan Kowal', 'Poznań', 'Pending'),
           (5, 'Maria Zając', 'Warszawa', 'Active'),
           (6, 'Tomek Mazur', 'Gdańsk', 'Pending');",
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "local-filter-fixture".into();
    source.name = "Local CRM".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        max_rows: 4,
        tile_url: String::new(),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let data = QueryResult {
        columns: [
            ("id", "integer"),
            ("name", "text"),
            ("city", "text"),
            ("status", "text"),
        ]
        .into_iter()
        .map(|(name, type_name)| Column {
            name: name.into(),
            type_name: type_name.into(),
        })
        .collect(),
        rows: [
            (1, ["Ada Nowak", "Łódź", "Active"]),
            (2, ["Olek Lis", "Kraków", "Active"]),
            (3, ["Lena Wójcik", "Łódź", "Pending"]),
            (4, ["Jan Kowal", "Poznań", "Pending"]),
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
    let session = serde_json::json!({
        "tabs": [{
            "title": "contacts", "ds_id": "local-filter-fixture",
            "kind": {"Table": {
                "schema": null, "table": "contacts", "filter": "", "order": "id",
                "primary_key": ["id"]
            }},
            "view": 0,
            "results": [{
                "title": "contacts", "sql": "SELECT * FROM contacts ORDER BY id",
                "page": Page::first(4), "data": data, "show_value_editor": false
            }]
        }]
    });
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_string(&session).unwrap(),
    )
    .unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    h.get_by_label("1-4 of 4+");
    h.get_by_label("Enable Local Filter").click();
    h.run_steps(3);

    // Search the popup's choices, then select one exact city value.
    h.get_by_label("Local Filter: city").click();
    h.run_steps(3);
    h.get_by_label("Local Filter For 'city'");
    h.get_by_role_and_label(Role::CheckBox, "Value").click();
    h.run_steps(3);
    set_field(&mut h, "Search Filter Values", "łód");
    assert!(h.query_by_label("Filter value: \"Kraków\"").is_none());
    h.get_by_label("Filter value: \"Łódź\"").click();
    h.run_steps(3);
    shot(&mut h, "38-local-filter.png");
    h.key_press(Key::Escape);
    h.run_steps(3);
    h.get_by_label("Ada Nowak");
    h.get_by_label("Lena Wójcik");
    assert!(h.query_by_label("Olek Lis").is_none());
    assert!(h.query_by_label("Jan Kowal").is_none());

    // Column conditions combine with AND and survive temporarily disabling them.
    select_only(&mut h, "status", "Active");
    h.get_by_label("Ada Nowak");
    assert!(h.query_by_label("Lena Wójcik").is_none());
    h.get_by_label("Enable Local Filter").click();
    h.run_steps(3);
    for name in ["Ada Nowak", "Olek Lis", "Lena Wójcik", "Jan Kowal"] {
        h.get_by_label(name);
    }
    h.get_by_label("Enable Local Filter").click();
    h.run_steps(3);
    h.get_by_label("Ada Nowak");
    assert!(h.query_by_label("Lena Wójcik").is_none());
    clear_filters(&mut h);
    h.get_by_label("Olek Lis");
    h.get_by_label("Lena Wójcik");

    // Find counts only cells allowed by the column filters.
    select_only(&mut h, "city", "Łódź");
    h.get_all_by_label("Ada Nowak")
        .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .unwrap()
        .click();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    set_field(&mut h, "Find on Current Page", "Kraków");
    h.get_by_label("No matches on this page");
    h.get_by_label("Filter rows").click();
    h.run_steps(3);
    assert!(h.query_by_label("Ada Nowak").is_none());
    set_field(&mut h, "Find on Current Page", "Łódź");
    h.get_by_label_contains("of 2 cells");
    h.get_by_label("Ada Nowak");
    h.get_by_label("Lena Wójcik");
    shot(&mut h, "39-local-filter-combined.png");

    // Both grid orientations and Record View use the same filtered row order.
    h.get_by_label("Sort by name").click();
    h.run_steps(3);
    h.get_by_label("Sort by name").click();
    h.run_steps(3);
    assert!(h.get_by_label("Lena Wójcik").rect().top() < h.get_by_label("Ada Nowak").rect().top());
    h.get_by_label("Transpose").click();
    h.run_steps(3);
    assert!(
        h.get_by_label("Lena Wójcik").rect().left() < h.get_by_label("Ada Nowak").rect().left()
    );
    h.get_by_label("Record View").click();
    h.run_steps(3);
    h.get_all_by_label("Ada Nowak")
        .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .unwrap()
        .click();
    h.run_steps(3);
    assert_eq!(h.get_all_by_label("Ada Nowak").count(), 2);
    toolbar_button(&h, "Record View").click();
    h.run_steps(3);
    h.get_by_label("Transpose").click();
    h.run_steps(3);

    h.set_size(egui::vec2(1100.0, 720.0));
    h.run_steps(3);
    h.get_by_label("Local Filter: city").click();
    h.run_steps(3);
    for node in [
        h.get_by_role_and_label(Role::TextInput, "Search Filter Values"),
        h.get_by_label("All Values"),
        h.get_by_label("Local Filter For 'city'"),
    ] {
        let rect = node.rect();
        assert!(
            rect.left() >= 0.0
                && rect.top() >= 0.0
                && rect.right() <= 1100.0
                && rect.bottom() <= 720.0,
            "local-filter control must remain in the narrow viewport: {rect:?}"
        );
    }
    shot(&mut h, "40-local-filter-narrow.png");
    h.key_press(Key::Escape);
    h.run_steps(3);
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    h.get_by_label("Close Grid Find").click();
    h.run_steps(3);
    clear_filters(&mut h);

    // A popup that hides an active editor preserves the pending value.
    // Restored rows are read-only; the first edit attempt reloads the page.
    h.get_by_label("Kraków").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    wait_until(&mut h, "refreshed restored table", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    h.get_by_label("Kraków").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    focused_editor(&h).type_text("Gdańsk");
    h.run_steps(3);
    select_only(&mut h, "city", "Łódź");
    assert!(h.query_by_label("Olek Lis").is_none());
    select_only(&mut h, "city", "Gdańsk");
    h.get_by_label("Olek Lis");
    h.get_by_label("Gdańsk");
    let stored_city: String = db
        .query_row("SELECT city FROM contacts WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        stored_city, "Kraków",
        "filtering must not submit cell edits"
    );

    toolbar_button(&h, "+").click();
    h.run_steps(3);
    focused_editor(&h).type_text("99");
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("99");
    assert!(h.get_all_by_label("<default>").count() >= 3);
    h.get_by_label("Revert").click();
    h.run_steps(3);
    assert!(h.query_by_label("99").is_none());
    assert!(h.query_by_label("Olek Lis").is_none());

    // Page replacement retains conditions even when selected values are absent.
    select_only(&mut h, "city", "Łódź");
    toolbar_button(&h, "▶").click();
    wait_until(&mut h, "second page", |h| {
        h.query_by_label("5-6 of 6").is_some()
    });
    assert!(h.query_by_label("Maria Zając").is_none());
    assert!(h.query_by_label("Tomek Mazur").is_none());
    clear_filters(&mut h);
    h.get_by_label("Maria Zając");
    h.get_by_label("Tomek Mazur");
    let count: i64 = db
        .query_row("SELECT count(*) FROM contacts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 6);

    drop(h);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
