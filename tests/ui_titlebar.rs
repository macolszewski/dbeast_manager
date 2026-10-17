//! The integrated macOS title bar keeps room for the native traffic lights,
//! which ignore Zoom IDE, at every interface scale. No database is opened.

mod common;

use dbeast::config::Config;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

#[test]
fn traffic_lights_keep_their_room_at_every_zoom() {
    let directory = std::env::temp_dir().join(format!("dbeast_titlebar_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let config_path = directory.join("config.json");
    let config = Config { tile_url: String::new(), files_root: Some(directory.clone()), ..Config::default() };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc).with_integrated_titlebar(true));
    h.run_steps(3);
    for zoom in [0.5, 0.7, 1.0, 1.5, 2.5] {
        h.ctx.set_zoom_factor(zoom);
        h.run_steps(3);
        // Positions are in zoomed points; the lights span 72 screen points.
        let menu = h.get_by_label_contains("DBeast  ⏷").rect();
        assert!(menu.min.x * zoom >= 72.0, "zoom {zoom}: project menu at {} screen points", menu.min.x * zoom);
        let explorer = h.get_by_label("Database Explorer").rect();
        assert!(explorer.min.y * zoom >= 34.0, "zoom {zoom}: content starts {} screen points from the top", explorer.min.y * zoom);
    }
    drop(h);
    std::fs::remove_dir_all(directory).ok();
}
