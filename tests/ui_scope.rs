//! Several databases under one PostgreSQL data source: the chooser, schema
//! filtering, and tables and consoles in a non-default database.

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        if Instant::now() > deadline {
            shot(h, "timeout");
            panic!("timed out waiting for {what}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn row<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(label)
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
        .or_else(|| h.get_all_by_label(label).next())
        .unwrap_or_else(|| panic!("no {label}"))
}

fn click_at(h: &mut H<'_>, pos: egui::Pos2) {
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    h.run_steps(2);
}

/// Clicks a tree row's expand arrow at the given depth.
fn expand(h: &mut H<'_>, label: &str, depth: usize) {
    let r = row(h, label).rect();
    click_at(
        h,
        egui::pos2(r.left() + depth as f32 * 16.0 + 9.0, r.center().y),
    );
}

fn double_click(h: &mut H<'_>, label: &str, t: f64) {
    h.input_mut().time = Some(t);
    row(h, label).click();
    h.step();
    h.input_mut().time = Some(t + 0.1);
    row(h, label).click();
    h.step();
    h.input_mut().time = Some(t + 5.0);
    h.run_steps(2);
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

#[test]
fn choose_databases_and_schemas() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: PostgreSQL test container is not running");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dbeast_scope_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        serde_json::json!({"data_sources": [{"id": "pg", "name": "", "driver": "Postgres", "host": "localhost", "port": 55432,
            "user": "dbeast", "password": "dbeast", "database": "dbeast", "file": "", "use_tls": false, "read_only": false}],
            "tile_url": ""})
        .to_string(),
    )
    .unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    // Only the default database is shown at first; the counter tells there are more.
    double_click(&mut h, "dbeast@localhost", 10.0);
    wait_until(&mut h, "default database", |h| {
        h.query_by_label("shop").is_some()
    });
    assert!(h.query_by_label("dbeast_archive").is_none());
    assert!(h.query_by_label("dbeast").is_some());

    // The chooser lists every database the account may connect to.
    row(&h, "dbeast@localhost").click_secondary();
    h.run_steps(2);
    h.get_by_label("Databases and Schemas…").click();
    h.run_steps(3);
    assert!(h.query_by_label("(Default database)").is_some());
    h.get_all_by_role(egui::accesskit::Role::CheckBox)
        .find(|n| n.accesskit_node().label().as_deref() == Some("dbeast_archive"))
        .expect("dbeast_archive checkbox")
        .click();
    h.run_steps(3);
    assert!(
        h.get_all_by_label("dbeast_archive").count() >= 2,
        "now also in the tree"
    );

    // Schemas of a database can be hidden from the chooser.
    let toggle = h
        .get_all_by_role(egui::accesskit::Role::CheckBox)
        .find(|n| n.accesskit_node().label().as_deref() == Some("dbeast_archive"))
        .unwrap()
        .rect();
    click_at(&mut h, egui::pos2(toggle.left() - 10.0, toggle.center().y));
    wait_until(&mut h, "schemas in chooser", |h| {
        h.get_all_by_role(egui::accesskit::Role::CheckBox)
            .any(|n| n.accesskit_node().label().as_deref() == Some("public"))
    });
    h.get_all_by_role(egui::accesskit::Role::CheckBox)
        .find(|n| n.accesskit_node().label().as_deref() == Some("public"))
        .unwrap()
        .click();
    h.run_steps(3);
    shot(&mut h, "22-database-chooser");
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.contains("\"dbeast_archive\"") && saved.contains("hidden_schemas"),
        "{saved}"
    );

    // Close the chooser and browse the second database.
    h.key_press(Key::Escape);
    h.run_steps(2);
    let close = h
        .get_all_by_role(egui::accesskit::Role::Button)
        .find(|n| n.accesskit_node().label().as_deref() == Some("Close window"));
    if let Some(c) = close {
        c.click();
        h.run_steps(2);
    }
    expand(&mut h, "dbeast_archive", 1);
    wait_until(&mut h, "archive schema", |h| {
        h.query_by_label("archive").is_some()
    });
    let public_rows = h
        .get_all_by_label("public")
        .filter(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
        .count();
    assert_eq!(
        public_rows, 1,
        "public of dbeast only; the hidden one of dbeast_archive is not in the tree"
    );
    expand(&mut h, "archive", 2);
    wait_until(&mut h, "tables folder", |h| {
        h.query_all_by_label("tables").count() >= 1
    });
    let tables_rows: Vec<egui::Rect> = h.get_all_by_label("tables").map(|n| n.rect()).collect();
    let last = *tables_rows.last().unwrap();
    click_at(
        &mut h,
        egui::pos2(last.left() + 3.0 * 16.0 + 9.0, last.center().y),
    );
    wait_until(&mut h, "invoices", |h| {
        h.query_by_label("invoices").is_some()
    });
    double_click(&mut h, "invoices", 20.0);
    wait_until(&mut h, "invoices data", |h| {
        h.query_by_label("1-2 of 2").is_some()
    });
    shot(&mut h, "23-second-database");

    // A console opened on that database runs there.
    row(&h, "dbeast_archive").click_secondary();
    h.run_steps(2);
    h.get_by_label("New ▶ Query Console").click();
    h.run_steps(3);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    h.run_steps(1);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text("select sum(total) as total from archive.invoices");
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    wait_until(&mut h, "console result", |h| {
        h.query_by_label("350.50").is_some()
    });
    assert!(
        h.get_all_by_role(egui::accesskit::Role::ComboBox)
            .any(|n| n.value().as_deref() == Some("dbeast_archive")),
        "console shows its database"
    );
    shot(&mut h, "24-console-in-database");

    std::fs::remove_dir_all(dir).ok();
}
