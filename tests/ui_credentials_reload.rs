//! Recovery from a stale settings save, using only a temporary config and memory vault.
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable};
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn has_value(h: &H<'_>, value: &str) -> bool {
    h.get_all_by_role(Role::TextInput)
        .any(|node| node.value().as_deref() == Some(value))
}
fn replace_value(h: &mut H<'_>, old: &str, new: &str) {
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.value().as_deref() == Some(old))
        .unwrap()
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .unwrap()
        .type_text(new);
    h.run_steps(2);
}
fn until(h: &mut H<'_>, ready: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready(h) {
        assert!(
            Instant::now() < deadline,
            "settings recovery did not finish"
        );
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(2);
}

#[test]
fn stale_save_exposes_reload_failed_reload_keeps_draft_and_success_rebases_apply() {
    let dir = std::env::temp_dir().join(format!("dbeast-reload-ui-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let mut config = dbeast::config::Config::default();
    let mut source = dbeast::model::DataSource::new(dbeast::model::DriverKind::Postgres);
    source.name = "Saved source".into();
    source.host = "127.0.0.1".into();
    source.port = 1;
    source.password_storage = dbeast::model::PasswordStorage::ForSession;
    config.data_sources.push(source);
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    // SAFETY: this binary has exactly one test and uses only synthetic fixture data.
    unsafe {
        std::env::set_var("DBEAST_CONFIG", &path);
    }
    let mut h = Harness::builder()
        .with_size([1280.0, 720.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    h.get_all_by_label("⚙").next().unwrap().click();
    h.run_steps(3);
    replace_value(&mut h, "Saved source", "Unsaved draft");
    config.data_sources[0].name = "Changed elsewhere".into();
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    h.get_by_label("Apply").click();
    until(&mut h, |h| {
        h.query_by_label("Reload settings and discard changes")
            .is_some()
    });
    let shots = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(shots.join("90-password-reload.png"))
        .unwrap();
    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 500.0));
    for label in ["Reload settings and discard changes", "Apply", "Cancel"] {
        assert!(viewport.contains_rect(h.get_by_label(label).rect()));
    }
    h.render()
        .unwrap()
        .save(shots.join("91-password-reload-small.png"))
        .unwrap();
    std::fs::write(&path, b"broken synthetic config").unwrap();
    h.get_by_label("Reload settings and discard changes")
        .click();
    h.run_steps(2);
    until(&mut h, |h| {
        h.query_by_label("Reload settings and discard changes")
            .is_some()
    });
    assert!(has_value(&h, "Unsaved draft"));
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    h.get_by_label("Reload settings and discard changes")
        .click();
    until(&mut h, |h| has_value(h, "Changed elsewhere"));
    assert!(!has_value(&h, "Unsaved draft"));
    assert!(
        h.query_by_label("Reload settings and discard changes")
            .is_none()
    );
    replace_value(&mut h, "Changed elsewhere", "Final source");
    h.get_by_label("Apply").click();
    until(&mut h, |_| {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["data_sources"]
            [0]["name"]
            == "Final source"
    });
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    drop(h);
    std::fs::remove_dir_all(dir).unwrap();
}
