//! Startup artwork and the persistent empty workspace share the same branding.
use egui_kittest::{Harness, kittest::Queryable};

#[test]
fn branding_survives_startup_and_narrow_windows() {
    let dir = std::env::temp_dir().join(format!("dbeast_branding_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, "{}").unwrap();
    // SAFETY: the only test in this process; set before starting the backend.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config) };
    let mut h = Harness::builder()
        .with_step_dt(0.01)
        .with_size([1280.0, 800.0])
        .with_pixels_per_point(2.0)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc).with_startup_splash());
    h.run_steps(3);
    h.get_by_label("Click or press any key to continue");
    assert!(h.query_by_label("DBeast icon").is_none());
    h.render()
        .unwrap()
        .save("docs/screenshots/branding-timed-splash.png")
        .unwrap();
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(
        h.query_by_label("Click or press any key to continue")
            .is_none()
    );
    h.get_by_label("Ultra Fast Database Manager");
    h.get_by_label("DBeast icon");
    h.render()
        .unwrap()
        .save("docs/screenshots/branding-workspace.png")
        .unwrap();
    h.set_size(egui::vec2(900.0, 600.0));
    h.run_steps(3);
    h.get_by_label("Ultra Fast Database Manager");
    // Shortcuts are written the way the system names them.
    h.get_by_label(if cfg!(target_os = "macos") {
        "New Query Console: ⌘⇧L"
    } else {
        "New Query Console: Ctrl+Shift+L"
    });
    h.render()
        .unwrap()
        .save("docs/screenshots/branding-compact.png")
        .unwrap();
    let icon = dbeast::ui::branding::window_icon();
    assert_eq!((icon.width, icon.height), (128, 128));
    assert_eq!(icon.rgba.len(), 128 * 128 * 4);
    assert_eq!(
        icon.rgba[3], 0,
        "rounded icon corners must remain transparent"
    );
    drop(h);
    let mut timed = Harness::builder()
        .with_step_dt(0.01)
        .with_step_dt(0.1)
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc).with_startup_splash());
    timed.run_steps(2);
    timed.get_by_label("Click or press any key to continue");
    timed.run_steps(10);
    assert!(
        timed
            .query_by_label("Click or press any key to continue")
            .is_none()
    );
    timed.get_by_label("DBeast icon");
    drop(timed);
    let mut clicked = Harness::builder()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc).with_startup_splash());
    clicked.event(egui::Event::PointerButton {
        pos: egui::pos2(100.0, 20.0),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    clicked.run_steps(3);
    assert!(
        clicked
            .query_by_label("Click or press any key to continue")
            .is_none()
    );
    clicked.get_by_label("DBeast icon");
    assert!(
        clicked.query_by_label("Find Action…").is_none(),
        "dismissal must not open the project menu"
    );
    drop(clicked);
    std::fs::remove_dir_all(dir).ok();
}
