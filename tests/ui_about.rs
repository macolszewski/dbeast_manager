//! About DBeast: opens from Find Action, credits the author with a link to the
//! profile, and the music mute choice is saved and kept on the next opening.
//! Tests never open an audio device.

mod common;

use dbeast::config::Config;
use egui::Key;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn open_about(h: &mut H<'_>) {
    h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::A);
    h.run_steps(3);
    h.get_by_role_and_label(Role::TextInput, "Find Action").type_text("About");
    h.run_steps(3);
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert!(h.query_by_label("Maciej Olszewski").is_some(), "About DBeast is open");
}

#[test]
fn about_credits_the_author_and_remembers_mute() {
    let dir = std::env::temp_dir().join(format!("dbeast_about_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.json");
    let config = Config { tile_url: String::new(), files_root: Some(dir.clone()), ..Config::default() };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    open_about(&mut h);
    assert!(h.query_by_label("github.com/macolszewski").is_some(), "link to the author's profile");
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/104-about.png"))
        .unwrap();

    // Music is on by default; muting is saved.
    h.get_by_label_contains("Sound on").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Sound off").is_some());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        if saved["about_muted"] == serde_json::Value::Bool(true) {
            break;
        }
        assert!(Instant::now() < deadline, "mute not saved: {}", saved["about_muted"]);
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }

    // Escape closes the screen; it opens again still muted.
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Maciej Olszewski").is_none(), "Escape closes About");
    open_about(&mut h);
    assert!(h.query_by_label_contains("Sound off").is_some(), "mute is remembered");
    drop(h);
    std::fs::remove_dir_all(dir).ok();
}
