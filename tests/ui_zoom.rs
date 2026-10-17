//! Zoom IDE: the interface scales in 10 % steps, the scale is saved, and ⌘= / ⌘-
//! stay free for code folding. No database is opened.

mod common;

use dbeast::config::Config;
use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use std::time::{Duration, Instant};

fn press(h: &mut Harness<'_, dbeast::ui::App>, logical: Key, physical: Key, modifiers: Modifiers) {
    for pressed in [true, false] {
        h.event(Event::Key { key: logical, physical_key: Some(physical), pressed, repeat: false, modifiers });
    }
    h.run_steps(3);
}

#[test]
fn zoom_ide_scales_the_interface_and_is_saved() {
    let directory = std::env::temp_dir().join(format!("dbeast_zoom_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let config_path = directory.join("config.json");
    let config = Config { tile_url: String::new(), files_root: Some(directory.clone()), ..Config::default() };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    let zoom_keys = Modifiers { ctrl: true, alt: true, shift: true, ..Modifiers::NONE };

    // With ⌥ and ⇧ the logical key is a layout-dependent symbol; the physical key decides.
    press(&mut h, Key::Plus, Key::Equals, zoom_keys);
    assert!((h.ctx.zoom_factor() - 1.1).abs() < 1e-4, "{}", h.ctx.zoom_factor());
    press(&mut h, Key::Plus, Key::Equals, zoom_keys);
    assert!((h.ctx.zoom_factor() - 1.2).abs() < 1e-4);
    press(&mut h, Key::Minus, Key::Minus, zoom_keys);
    assert!((h.ctx.zoom_factor() - 1.1).abs() < 1e-4);

    // ⌘= is code folding, not a zoom.
    press(&mut h, Key::Equals, Key::Equals, Modifiers::COMMAND);
    press(&mut h, Key::Minus, Key::Minus, Modifiers::COMMAND);
    assert!((h.ctx.zoom_factor() - 1.1).abs() < 1e-4, "egui's own keyboard zoom is off");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        if saved["ui_zoom"].as_f64().is_some_and(|z| (z - 1.1).abs() < 1e-4) {
            break;
        }
        assert!(Instant::now() < deadline, "zoom not saved: {}", saved["ui_zoom"]);
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    press(&mut h, Key::Num0, Key::Num0, zoom_keys);
    assert!((h.ctx.zoom_factor() - 1.0).abs() < 1e-4);
    drop(h);
    // A background save may still be finishing.
    std::fs::remove_dir_all(directory).ok();
}
