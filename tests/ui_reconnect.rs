//! A session closed by the server shows as lost in the explorer and the next
//! query reconnects with a notice in the Output log.
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::time::{Duration, Instant};

fn step_until(h: &mut Harness<'_, dbeast::ui::App>, what: &str, done: impl Fn(&Harness<'_, dbeast::ui::App>) -> bool) {
    let started = Instant::now();
    while !done(h) {
        assert!(started.elapsed() < Duration::from_secs(15), "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

#[test]
fn closed_session_is_shown_as_lost_and_reconnects() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: local PostgreSQL is unavailable");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dbeast_reconnect_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    // The console ends its own session, so no other session is affected.
    let config = serde_json::json!({
        "data_sources": [{"id":"local", "name":"Local reconnect test", "driver":"Postgres",
            "host":"127.0.0.1", "port":55432, "user":"dbeast", "password":"dbeast",
            "database":"dbeast", "file":"", "use_tls":false, "read_only":true}],
        "consoles": [{"title":"console", "ds_id":"local",
            "sql":"select pg_terminate_backend(pg_backend_pid());"}],
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
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    step_until(&mut h, "the lost connection", |h| {
        h.query_by_label("connection lost").is_some()
    });
    let shot = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/screenshots/connection-lost.png");
    h.render().unwrap().save(shot).unwrap();

    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    step_until(&mut h, "the reconnect notice", |h| {
        h.query_by_label_contains("reconnected in a new session").is_some()
    });
    drop(h);
    std::fs::remove_dir_all(dir).unwrap();
}
