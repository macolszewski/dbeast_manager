//! Export with 'pg_dump' dialog: tool detection, the command line and a run
//! in the Run tool window. Needs the PostgreSQL container and pg_dump.

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

#[test]
fn pg_dump_dialog_shows_the_command_and_runs_it() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: local PostgreSQL is unavailable");
        return;
    }
    if dbeast::dump::find_tool(dbeast::dump::Tool::PgDump, "").is_none() {
        eprintln!("skipping: pg_dump is not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dbeast_pg_dump_ui_{}", uuid::Uuid::new_v4()));
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = dir.join("config.json");
    let config = serde_json::json!({
        "data_sources": [{"id":"local", "name":"Shop PG", "driver":"Postgres",
            "host":"127.0.0.1", "port":55432, "user":"dbeast", "password":"dbeast",
            "database":"dbeast", "file":"", "use_tls":false, "read_only":true}],
        "max_rows": 500,
        "transfer_dir": out,
        "tile_url": ""
    });
    std::fs::write(&path, config.to_string()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &path) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(5);
    h.get_all_by_label("Shop PG")
        .find(|n| n.rect().left() < 200.0 && n.rect().top() > 60.0 && n.rect().top() < 300.0)
        .expect("data source in Database Explorer")
        .click_secondary();
    h.run_steps(3);
    click(&mut h, "Import/Export ⏵");
    click(&mut h, "Export with 'pg_dump'…");
    wait_until(&mut h, "the pg_dump version", |h| h.query_by_label_contains("pg_dump (PostgreSQL)").is_some());
    let expected = out.join("dbeast.sql");
    assert!(h.get_all_by_role(Role::TextInput).any(|n| n.value().as_deref() == Some(expected.to_string_lossy().as_ref())));
    let command = h.query_by_label_contains("--format p").expect("command line");
    let text = command.accesskit_node().label().or(command.accesskit_node().value()).unwrap_or_default().to_string();
    assert!(text.contains("--no-password") && !text.contains("PGPASSWORD"), "{text}");
    let row = h.get_by_label("Schemas:").rect().center().y;
    let schemas = |h: &H<'_>| {
        h.get_all_by_role(Role::TextInput)
            .find(|n| (n.rect().center().y - row).abs() < 6.0)
            .expect("schemas field")
            .rect()
            .center()
    };
    let at = schemas(&h);
    h.get_all_by_role(Role::TextInput).find(|n| n.rect().center() == at).unwrap().click();
    h.run_steps(2);
    h.get_all_by_role(Role::TextInput).find(|n| n.rect().center() == at).unwrap().type_text("shop");
    h.run_steps(3);
    assert!(h.query_by_label_contains("--schema=\"shop\"").is_some(), "the command follows the options");
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/101-pg-dump.png"))
        .unwrap();
    click(&mut h, "Run");
    wait_until(&mut h, "the dump", |h| expected.is_file() && h.query_by_label("Done").is_some());
    assert!(std::fs::read_to_string(&expected).unwrap().contains("CREATE TABLE shop.customers"));
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/102-pg-dump-run.png"))
        .unwrap();
    drop(h);
    std::fs::remove_dir_all(dir).ok();
}
