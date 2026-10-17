//! Stop releases the console and renders an explicit uncertain-outcome message.
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::time::{Duration, Instant};

#[test]
fn stop_releases_console_and_shows_recovery_message() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: local PostgreSQL is unavailable");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dbeast_stop_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let config = serde_json::json!({
        "data_sources": [{"id":"local", "name":"Local recovery test", "driver":"Postgres",
            "host":"127.0.0.1", "port":55432, "user":"dbeast", "password":"dbeast",
            "database":"dbeast", "file":"", "use_tls":false, "read_only":true}],
        "consoles": [{"title":"console", "ds_id":"local", "sql":"select pg_sleep(30);"}],
        "max_rows":500
    });
    std::fs::write(&path, config.to_string()).unwrap();
    // SAFETY: this binary contains only this test, before any UI workers start.
    unsafe {
        std::env::set_var("DBEAST_CONFIG", &path);
    }
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    h.run_steps(3);
    assert!(!h.get_by_label("■").accesskit_node().is_disabled());
    // Give the local SELECT time to start; no user database is configured here.
    std::thread::sleep(Duration::from_millis(150));
    h.get_by_label("■").click();
    let started = Instant::now();
    loop {
        h.step();
        if !h.get_by_label("▶").accesskit_node().is_disabled() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Stop left console busy"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(3);
    assert!(
        h.query_by_label_contains("database outcome is unknown")
            .is_some()
    );
    let shot = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/screenshots/connection-recovery.png");
    h.render().unwrap().save(shot).unwrap();
    drop(h);
    std::fs::remove_dir_all(dir).unwrap();
}
