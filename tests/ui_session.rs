//! The working session is restored on the next start: the same
//! tabs, console text and fetched rows, without connecting to the server.

use egui::Modifiers;
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

fn expand(h: &mut H<'_>, label: &str, depth: usize) {
    let r = row(h, label).rect();
    let pos = egui::pos2(r.left() + depth as f32 * 16.0 + 9.0, r.center().y);
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

fn app<'a>() -> H<'a> {
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    h
}

fn config(port: u16) -> String {
    serde_json::json!({"data_sources": [{"id": "pg", "name": "", "driver": "Postgres", "host": "localhost", "port": port,
        "user": "dbeast", "password": "dbeast", "database": "dbeast", "file": "", "use_tls": false, "read_only": false}],
        "consoles": [{"title": "console", "ds_id": "pg", "sql": "select 'kept text' as note"}],
        "tile_url": ""})
    .to_string()
}

#[test]
fn tabs_and_data_survive_a_restart() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: PostgreSQL test container is not running");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dbeast_session_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, config(55432)).unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };

    // First run: open a table and run the console.
    let mut h = app();
    double_click(&mut h, "dbeast@localhost", 10.0);
    wait_until(&mut h, "schemas", |h| h.query_by_label("shop").is_some());
    expand(&mut h, "shop", 2);
    wait_until(&mut h, "tables folder", |h| h.query_by_label("tables").is_some());
    expand(&mut h, "tables", 3);
    wait_until(&mut h, "customers", |h| h.query_by_label("customers").is_some());
    double_click(&mut h, "customers", 20.0);
    wait_until(&mut h, "customers data", |h| {
        h.query_by_label("user10@example.com").is_some()
    });
    let session = dir.join("session.json");
    let results = dir.join("session-results");
    wait_until(&mut h, "session written", |_| {
        // Rows are kept next to session.json, one file per result.
        session.exists()
            && std::fs::read_dir(&results).is_ok_and(|files| {
                files.flatten().any(|file| {
                    std::fs::read_to_string(file.path())
                        .is_ok_and(|s| s.contains("user10@example.com"))
                })
            })
    });
    drop(h);

    // Second run against an unreachable server: everything comes from the session.
    std::fs::write(&config_path, config(1)).unwrap();
    let mut h = app();
    // The session is read in the background, then restored at once.
    wait_until(&mut h, "session restored", |h| {
        h.query_by_label("user10@example.com").is_some()
    });
    assert!(
        h.query_all_by_label_contains("customers").count() >= 2,
        "table tab restored next to the tree row"
    );
    assert!(h.query_by_label("user10@example.com").is_some(), "rows restored");
    assert!(h.query_by_label("tables").is_some(), "explorer tree restored");
    assert!(
        h.query_by_label("connecting…").is_none(),
        "nothing connects on start"
    );
    shot(&mut h, "26-restored-session");
    h.get_all_by_label_contains("console")
        .next()
        .expect("console tab")
        .click();
    h.run_steps(3);
    let text = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .value()
        .unwrap_or_default();
    assert!(text.contains("kept text"), "{text}");

    drop(h);

    // With the server back, the restored tree loads further on demand.
    std::fs::write(&config_path, config(55432)).unwrap();
    let mut h = app();
    wait_until(&mut h, "session restored", |h| h.query_by_label("orders").is_some());
    expand(&mut h, "orders", 4);
    wait_until(&mut h, "folders of orders", |h| {
        h.query_by_label("indexes").is_some()
    });

    std::fs::remove_dir_all(dir).ok();
}
