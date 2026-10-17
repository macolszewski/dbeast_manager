//! Data source groups and ordering in Database Explorer. No database is opened.

mod common;

use dbeast::config::Config;
use dbeast::model::{DataSource, DriverKind};
use egui::Modifiers;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn source(directory: &Path, id: &str, name: &str) -> DataSource {
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = id.into();
    source.name = name.into();
    source.file = directory
        .join(format!("{id}-never-opened.sqlite"))
        .to_string_lossy()
        .into_owned();
    source
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn row_top(h: &H<'_>, label: &str) -> f32 {
    h.get_by_label(label).rect().top()
}

fn drag(h: &mut H<'_>, from: egui::Pos2, to: egui::Pos2) {
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.event(egui::Event::PointerMoved(from));
    h.run_steps(1);
    h.event(button(from, true));
    h.run_steps(1);
    for step in 1..=6 {
        h.event(egui::Event::PointerMoved(
            from + (to - from) * (step as f32 / 6.0),
        ));
        h.run_steps(1);
    }
    h.event(button(to, false));
    h.run_steps(3);
}

fn saved(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn wait_saved(h: &mut H<'_>, path: &Path, done: impl Fn(&serde_json::Value) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(&saved(path)) {
        assert!(
            Instant::now() < deadline,
            "configuration was not saved: {} {:?}",
            serde_json::to_string(&saved(path)["source_group"]).unwrap(),
            saved(path)["data_sources"].as_array().unwrap().iter().map(|s| s["id"].clone()).collect::<Vec<_>>()
        );
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn groups_and_drag_order_are_saved_without_connecting() {
    let directory = std::env::temp_dir().join(format!("dbeast_groups_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let config_path = directory.join("config.json");
    let config = Config {
        data_sources: vec![
            source(&directory, "alpha", "Alpha shop"),
            source(&directory, "beta", "Beta billing"),
            source(&directory, "gamma", "Gamma reports"),
        ],
        tile_url: String::new(),
        files_root: Some(directory.clone()),
        ..Config::default()
    };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    // New > Data Source Group, nested with "/".
    h.get_by_label("New").click();
    h.run_steps(3);
    click(&mut h, "Data Source Group…");
    h.get_by_label("Name:").type_text("Clients/Acme");
    h.run_steps(2);
    click(&mut h, "Create");
    assert!(h.query_by_label("Clients").is_some() && h.query_by_label("Acme").is_some());

    // Move to Group from the data source context menu.
    h.get_by_label("Beta billing").click_secondary();
    h.run_steps(3);
    h.get_by_label_contains("Move to Group").click();
    h.run_steps(3);
    h.query_all_by_label_contains("Acme")
        .max_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("Acme in the Move to Group menu")
        .click();
    h.run_steps(3);
    assert!(row_top(&h, "Beta billing") > row_top(&h, "Acme"));
    wait_saved(&mut h, &config_path, |config| {
        config["source_group"]["beta"] == "Clients/Acme"
    });

    // Dragging a source above another reorders the top level.
    let gamma = h.get_by_label("Gamma reports").rect();
    let alpha = h.get_by_label("Alpha shop").rect();
    drag(&mut h, gamma.center(), alpha.center() - egui::vec2(0.0, alpha.height() * 0.3));
    assert!(row_top(&h, "Gamma reports") < row_top(&h, "Alpha shop"));
    wait_saved(&mut h, &config_path, |config| {
        let order: Vec<_> = config["data_sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["id"].as_str().unwrap().to_owned())
            .collect();
        order.iter().position(|id| id == "gamma") < order.iter().position(|id| id == "alpha")
    });

    // Dropping onto a group moves the source into it.
    let alpha = h.get_by_label("Alpha shop").rect();
    let clients = h.get_by_label("Clients").rect();
    drag(&mut h, alpha.center(), clients.center());
    wait_saved(&mut h, &config_path, |config| config["source_group"]["alpha"] == "Clients");
    h.render()
        .unwrap()
        .save(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/screenshots")
                .join("92-explorer-groups.png"),
        )
        .unwrap();

    // Collapsing a group hides its sources; nothing connected to any database.
    click(&mut h, "Clients");
    h.key_press(egui::Key::ArrowLeft);
    h.run_steps(3);
    assert!(h.query_by_label("Beta billing").is_none());
    for id in ["alpha", "beta", "gamma"] {
        assert!(!directory.join(format!("{id}-never-opened.sqlite")).exists());
    }
    drop(h);
    std::fs::remove_dir_all(directory).unwrap();
}
