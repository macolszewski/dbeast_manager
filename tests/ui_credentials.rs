//! Password storage drafts, cancellation and asynchronous Apply, using an isolated memory vault.
use egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};
use std::time::{Duration, Instant};

#[test]
fn password_storage_apply_cancel_and_small_window() {
    let dir = std::env::temp_dir().join(format!("dbeast-password-ui-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        serde_json::json!({"data_sources": [{
            "id":"test-source", "name":"Password storage", "driver":"Postgres",
            "host":"127.0.0.1", "port":1, "user":"test", "password":"synthetic-ui-password",
            "database":"test", "file":"", "use_tls":false, "read_only":false
        }]})
        .to_string(),
    )
    .unwrap();
    // SAFETY: this binary has exactly one test; all credentials are synthetic.
    unsafe {
        std::env::set_var("DBEAST_CONFIG", &path);
    }
    let mut h = Harness::builder()
        .with_size([1280.0, 720.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    let initial = std::fs::read(&path).unwrap();
    h.get_all_by_label("⚙").next().unwrap().click();
    h.run_steps(3);
    assert!(h.query_by_label("Save:").is_some());
    let shots = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(shots.join("80-password-storage.png"))
        .unwrap();
    h.get_all_by_role(Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("Forever"))
        .unwrap()
        .click();
    h.run_steps(2);
    h.get_by_label("Until restart").click();
    h.run_steps(2);
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    h.get_all_by_label("⚙").next().unwrap().click();
    h.run_steps(2);
    assert!(
        h.get_all_by_role(Role::ComboBox)
            .any(|n| n.value().as_deref() == Some("Forever"))
    );
    h.get_all_by_role(Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("Forever"))
        .unwrap()
        .click();
    h.run_steps(2);
    h.get_by_label("Until restart").click();
    h.run_steps(2);
    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(shots.join("81-password-storage-small.png"))
        .unwrap();
    for label in ["OK", "Cancel", "Apply"] {
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 500.0))
                .contains_rect(h.get_by_label(label).rect())
        );
    }
    h.get_by_label("Apply").click();
    h.run_steps(2);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if disk["data_sources"][0]["password_storage"] == "ForSession"
            && h.query_by_label("Saving passwords and settings…").is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline, "Apply did not finish");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(3);
    let disk = std::fs::read_to_string(&path).unwrap();
    assert!(!disk.contains("synthetic-ui-password"));
    assert!(!disk.contains("credential_ref"));
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    drop(h);
    std::fs::remove_dir_all(dir).unwrap();
}
