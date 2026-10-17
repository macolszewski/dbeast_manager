//! Find on Current Page searches fetched cells and staged edits without querying.

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

fn query(h: &mut H<'_>, text: &str) {
    h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
        .type_text(text);
    h.run_steps(3);
}

fn focused_editor<'h>(h: &'h H<'_>) -> egui_kittest::Node<'h> {
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("focused cell editor")
}

fn copied_value(h: &mut H<'_>) -> String {
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
        .expect("copied grid value")
}

fn toolbar_button<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    let toolbar_y = h.get_by_label("Transpose").rect().center().y;
    h.get_all_by_label(label)
        .find(|node| (node.rect().center().y - toolbar_y).abs() < 5.0)
        .expect("result toolbar button")
}

#[test]
fn grid_find_filters_navigates_and_includes_pending_edits() {
    let dir = std::env::temp_dir().join(format!("dbeast_grid_find_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("contacts.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT, city TEXT, notes TEXT);
         INSERT INTO contacts VALUES
           (1, 'Ada Nowak', 'Łódź', 'Local contact'),
           (2, 'Olek Lis', 'ŁÓDŹ', 'Weekly report'),
           (3, 'Lena Wójcik', 'Warszawa', 'Remote contact'),
           (4, 'Jan Kowal', 'Łódź', 'New customer');",
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "grid-find-fixture".into();
    source.name = "Local CRM".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        max_rows: 3,
        tile_url: String::new(),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let data = QueryResult {
        columns: [
            ("id", "INTEGER"),
            ("name", "TEXT"),
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
            (1, ["Ada Nowak", "Łódź", "Local contact"]),
            (2, ["Olek Lis", "ŁÓDŹ", "Weekly report"]),
            (3, ["Lena Wójcik", "Warszawa", "Remote contact"]),
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
    const CONSOLE_SQL: &str = "SELECT 'Panel isolation' AS note;";
    let console_data = QueryResult {
        columns: vec![Column {
            name: "note".into(),
            type_name: "TEXT".into(),
        }],
        rows: vec![vec![Value::Text("Panel isolation".into())]],
        ..QueryResult::default()
    };
    let session = serde_json::json!({
        "tabs": [{
            "title": "contacts",
            "ds_id": "grid-find-fixture",
            "kind": {"Table": {
                "schema": null, "table": "contacts", "filter": "", "order": "id",
                "primary_key": ["id"]
            }},
            "view": 0,
            "results": [{
                "title": "contacts", "sql": "SELECT * FROM contacts ORDER BY id",
                "page": Page::first(3), "data": data, "show_value_editor": false
            }]
        }, {
            "title": "scratch",
            "ds_id": "grid-find-fixture",
            "kind": {"Console": {"sql": CONSOLE_SQL}},
            "view": 0,
            "results": [{
                "title": "Result 1", "sql": CONSOLE_SQL,
                "page": Page::first(3), "data": console_data,
                "show_value_editor": false
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
    h.get_by_label("1-3 of 3+");

    // Find starts from the selected cell and Enter navigates without editing it.
    h.get_by_label("Łódź").click();
    h.run_steps(3);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
            .value()
            .as_deref(),
        Some("Łódź")
    );
    h.get_by_label("1 of 2 cells");
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("2 of 2 cells");
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
            .is_focused()
    );
    assert!(h.query_by_label("DML").is_none());
    h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
    h.run_steps(3);
    h.get_by_label("1 of 2 cells");
    h.key_press_modifiers(Modifiers::COMMAND, Key::G);
    h.run_steps(3);
    h.get_by_label("2 of 2 cells");
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    h.run_steps(3);
    h.get_by_label("1 of 2 cells");

    // Matching is Unicode aware, with independently controlled case and words.
    query(&mut h, "łódź");
    h.get_by_label_contains("of 2 cells");
    h.get_by_label("Match Case").click();
    h.run_steps(3);
    h.get_by_label("No matches on this page");
    h.get_by_label("Match Case").click();
    h.run_steps(3);
    query(&mut h, "Łód");
    h.get_by_label("Words").click();
    h.run_steps(3);
    h.get_by_label("No matches on this page");
    h.get_by_label("Words").click();
    h.run_steps(3);
    query(&mut h, "Łódź");

    // Filtering affects this page only and composes with local sorting/Transpose.
    h.get_by_label("Filter rows").click();
    h.run_steps(3);
    assert!(h.query_by_label("Lena Wójcik").is_none());
    h.get_by_label("Ada Nowak");
    h.get_by_label("Olek Lis");
    h.get_by_label("Sort by name").click();
    h.run_steps(3);
    h.get_by_label("Sort by name").click();
    h.run_steps(3);
    assert!(h.get_by_label("Olek Lis").rect().top() < h.get_by_label("Ada Nowak").rect().top());
    shot(&mut h, "34-grid-find.png");
    h.set_size(egui::vec2(1100.0, 720.0));
    h.run_steps(3);
    for node in [
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page"),
        h.get_by_label("Close Grid Find"),
        h.get_by_label("Filter rows"),
    ] {
        let rect = node.rect();
        assert!(
            rect.left() >= 0.0
                && rect.top() >= 0.0
                && rect.right() <= 1100.0
                && rect.bottom() <= 720.0,
            "search control must remain inside the narrow viewport: {rect:?}"
        );
    }
    shot(&mut h, "37-grid-find-narrow.png");
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    h.get_by_label("Transpose").click();
    h.run_steps(3);
    assert!(h.query_by_label("Lena Wójcik").is_none());
    assert!(h.get_by_label("Olek Lis").rect().left() < h.get_by_label("Ada Nowak").rect().left());
    h.get_by_label("Next Matching Cell").focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::G);
    h.run_steps(3);
    h.get_by_label_contains("of 2 cells");
    shot(&mut h, "35-grid-find-transposed.png");
    h.get_by_label("Transpose").click();
    h.run_steps(3);

    // Escape restores all rows and grid keyboard ownership.
    query(&mut h, "Łódź");
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Close Grid Find").is_none());
    h.get_by_label("Lena Wójcik");
    h.key_press(Key::ArrowRight);
    h.run_steps(3);
    assert!(matches!(
        copied_value(&mut h).as_str(),
        "Local contact" | "Weekly report"
    ));

    // A pending edit is searchable; its original fetched value stops matching.
    // Restored rows are read-only; the first edit attempt reloads the page.
    h.get_by_label("Łódź").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    wait_until(&mut h, "refreshed restored table", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    h.get_by_label("Łódź").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    focused_editor(&h).type_text("Żory");
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("Żory");
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    h.get_by_label("1 of 1 cells");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
            .value()
            .as_deref(),
        Some("Żory")
    );
    assert!(h.query_by_label("Olek Lis").is_none());
    query(&mut h, "Łódź");
    h.get_by_label("1 of 1 cells");
    assert!(h.query_by_label("Ada Nowak").is_none());
    h.get_by_label("Olek Lis");
    let stored_city: String = db
        .query_row("SELECT city FROM contacts WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored_city, "Łódź", "search does not submit staged changes");

    // Added rows remain editable under a filter even before they contain a hit.
    toolbar_button(&h, "+").click();
    h.run_steps(3);
    focused_editor(&h).type_text("99");
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("99");
    assert!(h.get_all_by_label("<default>").count() >= 3);
    h.get_by_label_contains("of 1 cells");
    h.get_by_label("Revert").click();
    h.run_steps(3);
    h.get_by_label_contains("of 2 cells");
    assert!(h.query_by_label("99").is_none());
    h.get_by_label("Ada Nowak");

    // Filtering can hide an active cell editor; its typed buffer must be staged.
    h.get_by_label("Filter rows").click();
    h.run_steps(3);
    h.get_by_label("Warszawa").click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    focused_editor(&h).type_text("Kielce");
    h.run_steps(3);
    assert_eq!(focused_editor(&h).value().as_deref(), Some("Kielce"));
    h.get_by_label("Filter rows").click();
    h.run_steps(3);
    assert!(h.query_by_label("Lena Wójcik").is_none());
    query(&mut h, "Kielce");
    h.get_by_label("1 of 1 cells");
    h.get_by_label("Lena Wójcik");
    h.get_by_label("Kielce");
    let stored_city: String = db
        .query_row("SELECT city FROM contacts WHERE id = 3", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored_city, "Warszawa");
    h.get_by_label("Revert").click();
    h.run_steps(3);
    query(&mut h, "Łódź");
    h.get_by_label_contains("of 2 cells");

    // Fetching a new page retains the query and rebuilds its cell index.
    toolbar_button(&h, "▶").click();
    wait_until(&mut h, "second page", |h| {
        h.query_by_label("4-4 of 4").is_some()
    });
    h.get_by_label("Jan Kowal");
    h.get_by_label("1 of 1 cells");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
            .value()
            .as_deref(),
        Some("Łódź")
    );
    query(&mut h, "not in this page");
    h.get_by_label("No matches on this page");
    assert!(h.query_by_label("Jan Kowal").is_none());
    assert!(
        h.get_by_label("Next Matching Cell")
            .accesskit_node()
            .is_disabled()
    );
    shot(&mut h, "36-grid-find-empty.png");
    h.key_press(Key::Escape);
    h.run_steps(3);
    h.get_by_label("Jan Kowal");
    h.key_press(Key::ArrowRight);
    h.run_steps(3);
    assert_eq!(copied_value(&mut h), "4");
    let count: i64 = db
        .query_row("SELECT count(*) FROM contacts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 4);

    // Console and grid searches can coexist; shortcuts affect the focused panel.
    h.get_by_label("QL scratch").click();
    h.run_steps(3);
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    h.get_by_role_and_label(Role::TextInput, "Find")
        .type_text("Panel");
    h.run_steps(3);
    h.get_by_label("Close Find");
    h.get_by_label("Panel isolation").click();
    h.run_steps(3);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    h.get_by_label("Close Find");
    h.get_by_label("Close Grid Find");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
            .value()
            .as_deref(),
        Some("Panel isolation")
    );
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Close Grid Find").is_none());
    h.get_by_label("Close Find");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Find")
            .value()
            .as_deref(),
        Some("Panel")
    );
    h.get_by_role_and_label(Role::TextInput, "Find").focus();
    h.run_steps(1);
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Close Find").is_none());
    assert!(h.get_by_role(Role::MultilineTextInput).is_focused());
    assert_eq!(
        h.get_by_role(Role::MultilineTextInput).value().as_deref(),
        Some(CONSOLE_SQL)
    );

    drop(h);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
