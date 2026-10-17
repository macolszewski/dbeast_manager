//! TLS configuration drafts and compact layout with isolated configuration and memory credentials.
use egui_kittest::{Harness, kittest::Queryable};
use std::time::{Duration, Instant};

#[test]
fn tls_settings_apply_cancel_and_small_window() {
    let dir = std::env::temp_dir().join(format!("dbeast-tls-ui-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let mut source = dbeast::model::DataSource::new(dbeast::model::DriverKind::Postgres);
    source.name = "TLS settings".into();
    source.host = "127.0.0.1".into();
    source.port = 1;
    source.password_storage = dbeast::model::PasswordStorage::ForSession;
    std::fs::write(
        &path,
        serde_json::json!({"data_sources": [source]}).to_string(),
    )
    .unwrap();
    // SAFETY: this binary has one test, isolated configuration and synthetic secrets.
    unsafe {
        std::env::set_var("DBEAST_CONFIG", &path);
    }
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    let initial = std::fs::read(&path).unwrap();
    h.get_all_by_label("⚙").next().unwrap().click();
    h.run_steps(3);
    h.get_by_label("SSH/SSL").click();
    h.run_steps(3);
    h.get_by_label("Use SSL").click();
    h.run_steps(3);
    h.get_by_label("CA file:").focus();
    h.run_steps(2);
    h.get_by_label("CA file:").type_text("/tmp/example-ca.pem");
    h.run_steps(2);
    h.get_by_label("Client certificate:").focus();
    h.run_steps(2);
    h.get_by_label("Client certificate:")
        .type_text("/tmp/example-client.p12");
    h.run_steps(2);
    h.get_by_label("PKCS#12 password:").focus();
    h.run_steps(2);
    h.get_by_label("PKCS#12 password:")
        .type_text("synthetic-ui-tls-secret");
    h.get_by_label("PKCS#12 password:").scroll_to_me();
    h.run_steps(3);
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    let shots = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(shots.join("82-tls-settings.png"))
        .unwrap();
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    h.get_all_by_label("⚙").next().unwrap().click();
    h.run_steps(2);
    h.get_by_label("SSH/SSL").click();
    h.run_steps(2);
    h.get_by_label("Use SSL").click();
    h.run_steps(2);
    h.get_by_label("CA file:").focus();
    h.run_steps(2);
    h.get_by_label("CA file:").type_text("/tmp/example-ca.pem");
    h.run_steps(2);
    h.get_by_label("Client key file:").focus();
    h.run_steps(2);
    h.get_by_label("Client key file:").type_text("/tmp/example-client.key");
    h.run_steps(2);
    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    h.get_by_label("PKCS#12 password:").scroll_to_me();
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(shots.join("83-tls-settings-small.png"))
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
        if disk["data_sources"][0]["use_tls"] == true {
            assert_eq!(
                disk["data_sources"][0]["tls"]["ca_file"],
                "/tmp/example-ca.pem"
            );
            assert_eq!(disk["data_sources"][0]["tls"]["key_file"], "/tmp/example-client.key");
            assert_eq!(disk["data_sources"][0]["tls"]["mode"], "VerifyFull");
            break;
        }
        assert!(Instant::now() < deadline, "Apply did not finish");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("synthetic-ui-tls-secret")
    );
    drop(h);
    std::fs::remove_dir_all(dir).unwrap();
}
