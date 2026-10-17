//! Layout regressions that need no database.

use egui::{Modifiers, PointerButton, Pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

/// Presses at `from`, moves to `to` in a few frames and releases, like a mouse drag.
pub fn drag(harness: &mut Harness<'_, dbeast::ui::App>, from: Pos2, to: Pos2) {
    harness.event(egui::Event::PointerMoved(from));
    harness.run_steps(1);
    harness.event(egui::Event::PointerButton {
        pos: from,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    harness.run_steps(1);
    for i in 1..=5 {
        let p = from + (to - from) * (i as f32 / 5.0);
        harness.event(egui::Event::PointerMoved(p));
        harness.run_steps(1);
    }
    harness.event(egui::Event::PointerButton {
        pos: to,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    harness.run_steps(2);
}

#[test]
fn narrow_explorer_header_does_not_overlap() {
    let dir = std::env::temp_dir().join(format!("dbeast_layout_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(&path, "{}").unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("DBEAST_CONFIG", &path) };

    let mut harness = Harness::builder()
        .with_size([1280.0, 720.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    harness.run_steps(3);

    let title = |h: &Harness<'_, dbeast::ui::App>| {
        h.get_all_by_label_contains("Database")
            .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Label)
            .expect("explorer title")
            .rect()
    };
    let plus = |h: &Harness<'_, dbeast::ui::App>| {
        h.get_all_by_label("New")
            .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
            .expect("+ (New) button")
            .rect()
    };
    // Drag the explorer's right edge (just right of its filter field) as far left as it goes.
    let edge = harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap()
        .rect()
        .right()
        + 6.0;
    drag(
        &mut harness,
        egui::pos2(edge, 400.0),
        egui::pos2(60.0, 400.0),
    );
    let shot = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/screenshots/12-narrow-explorer.png");
    harness.render().unwrap().save(shot).unwrap();

    let filter = harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap()
        .rect();
    assert!(
        filter.width() < 260.0,
        "explorer was not narrowed: {filter:?}"
    );
    let (t, p) = (title(&harness), plus(&harness));
    assert!(
        t.right() <= p.left() + 1.0,
        "title {t:?} overlaps + button {p:?}"
    );
    std::fs::remove_dir_all(dir).ok();
}
