//! Import Data from File dialog: a new table with inferred types, then the
//! same file mapped into an existing table. Uses a local SQLite file.

mod common;

use dbeast::config::Config;
use dbeast::model::{DataSource, DriverKind};
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

fn count(database: &Path, table: &str) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
        .unwrap_or(-1)
}

#[test]
fn import_dialog_creates_and_fills_tables() {
    let dir = std::env::temp_dir().join(format!("dbeast_import_ui_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("crm.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("CREATE TABLE contacts (id INTEGER PRIMARY KEY, full_name TEXT, city TEXT);")
        .unwrap();
    let file = dir.join("new people.csv");
    std::fs::write(&file, "id;name;born;score\n1;Ada;1815-12-10;9.5\n2;Olek;1990-01-02;7\n3;Lena;2001-03-04;8.25\n").unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "import-fixture".into();
    source.name = "Local CRM".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config { data_sources: vec![source], tile_url: String::new(), ..Config::default() };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(5);

    // New table: separator detected, types inferred, name from the file.
    h.state_mut().import_file_for_test("import-fixture", None, file.clone());
    wait_until(&mut h, "the preview", |h| h.query_by_label("Lena").is_some());
    assert!(h.get_all_by_role(Role::ComboBox).any(|n| n.value().as_deref() == Some("Semicolon")));
    assert!(h.get_all_by_role(Role::TextInput).any(|n| n.value().as_deref() == Some("new_people")));
    assert!(h.get_all_by_role(Role::TextInput).any(|n| n.value().as_deref() == Some("REAL")), "score is a number");
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/100-import-data.png"))
        .unwrap();
    h.get_by_label("Import").click();
    h.run_steps(3);
    wait_until(&mut h, "the import", |_| count(&database, "new_people") == 3);
    wait_until(&mut h, "the outcome", |h| h.query_by_label("Done").is_some());
    let born: String = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row("SELECT born FROM new_people WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(born, "1815-12-10");

    // Existing table: columns map by name, the rest is chosen by hand.
    h.state_mut().import_file_for_test("import-fixture", Some("contacts"), file.clone());
    wait_until(&mut h, "the mapping", |h| h.query_by_label("full_name").is_some() && h.query_by_label("Lena").is_some());
    let skipped: Vec<_> = h
        .get_all_by_role(Role::ComboBox)
        .filter(|n| n.value().as_deref() == Some("<skip>"))
        .collect();
    assert_eq!(skipped.len(), 2, "full_name and city have no column of the same name");
    skipped[0].click();
    h.run_steps(3);
    h.get_all_by_label("name").last().unwrap().click();
    h.run_steps(3);
    h.get_by_label("Import").click();
    h.run_steps(3);
    wait_until(&mut h, "the second import", |_| count(&database, "contacts") == 3);
    let name: String = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row("SELECT full_name FROM contacts WHERE id = 2", [], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "Olek");
    drop(h);
    std::fs::remove_dir_all(dir).ok();
}
