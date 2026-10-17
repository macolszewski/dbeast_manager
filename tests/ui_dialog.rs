//! Data source dialog on a small screen: it must fit and keep its buttons visible.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

#[test]
fn data_source_dialog_fits_small_screen() {
    let dir = std::env::temp_dir().join(format!("dbeast_dialog_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        serde_json::json!({"data_sources": [{"id": "pg", "name": "", "driver": "Postgres", "host": "localhost", "port": 55432,
            "user": "dbeast", "password": "dbeast", "database": "dbeast", "file": "", "use_tls": false, "read_only": false}]})
        .to_string(),
    )
    .unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("DBEAST_CONFIG", &path) };

    let mut harness = Harness::builder()
        .with_size([1280.0, 720.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    harness.run_steps(3);
    harness.get_all_by_label("⚙").next().unwrap().click();
    harness.run_steps(5);
    let dir_shots = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    harness
        .render()
        .unwrap()
        .save(dir_shots.join("09-data-sources.png"))
        .unwrap();

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0));
    for label in ["OK", "Cancel", "Apply", "Test Connection"] {
        let r = harness.get_by_label(label).rect();
        assert!(screen.contains_rect(r), "{label} at {r:?} is off screen");
    }

    // SSH/SSL tab: enabling the tunnel and choosing key authentication shows its fields.
    harness.get_by_label("SSH/SSL").click();
    harness.run_steps(2);
    harness.get_by_label("Use SSH tunnel").click();
    harness.run_steps(2);
    harness
        .get_all_by_role(egui::accesskit::Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("Password"))
        .expect("authentication combo")
        .click();
    harness.run_steps(2);
    harness.get_by_label("Key pair").click();
    harness.run_steps(3);
    for label in [
        "User name:",
        "Private key file:",
        "Passphrase:",
        "Use SSL",
        "OK",
    ] {
        let r = harness.get_by_label(label).rect();
        assert!(screen.contains_rect(r), "{label} at {r:?} is off screen");
    }
    harness
        .render()
        .unwrap()
        .save(dir_shots.join("14-ssh-tunnel.png"))
        .unwrap();

    // SSL: modes, and PEM certificate with key or PKCS#12.
    harness.get_by_label("Use SSL").click();
    harness.run_steps(2);
    harness
        .get_all_by_role(egui::accesskit::Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("Full verification"))
        .expect("SSL mode combo, full verification by default")
        .click();
    harness.run_steps(2);
    harness.get_by_label("Verify CA").click();
    harness.run_steps(3);
    assert!(
        harness.get_all_by_role(egui::accesskit::Role::ComboBox).any(|n| n.value().as_deref() == Some("Verify CA")),
        "the mode is chosen; its explanation is in the ⓘ tooltip"
    );
    harness.get_by_label("Client key file:").scroll_to_me();
    harness.run_steps(3);
    for label in ["CA file:", "Client certificate:", "Client key file:", "PKCS#12 password:"] {
        assert!(harness.query_by_label(label).is_some(), "{label}");
    }
    // Every path field has its own Browse… next to it.
    assert!(harness.query_all_by_label("Browse…").count() >= 3, "CA, client certificate and key");
    harness
        .render()
        .unwrap()
        .save(dir_shots.join("14b-ssl.png"))
        .unwrap();

    // The dialog is a window: it can be dragged by its title bar.
    let title = harness.get_by_label("Data Sources and Drivers").rect();
    // The node spans the window; its title bar is the top strip.
    let from = egui::pos2(title.center().x, title.top() + 12.0);
    let to = from + egui::vec2(-120.0, 40.0);
    let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    harness.event(egui::Event::PointerMoved(from));
    harness.event(button(from, true));
    harness.run_steps(1);
    for step in 1..=6 {
        harness.event(egui::Event::PointerMoved(from + (to - from) * (step as f32 / 6.0)));
        harness.run_steps(1);
    }
    harness.event(button(to, false));
    harness.run_steps(3);
    let moved = harness.get_by_label("Data Sources and Drivers").rect();
    assert!((moved.center() - title.center() - (to - from)).length() < 4.0, "{title:?} -> {moved:?}");

    std::fs::remove_dir_all(dir).ok();
}
