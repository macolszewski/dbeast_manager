//! Database Graph on a real SQLite file: opened from Database Explorer, read
//! from the schema only, sized, filtered, pinned and saved with the session.

mod common;

use dbeast::config::Config;
use dbeast::model::{DataSource, DriverKind};
use egui::Modifiers;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn wait_for(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

#[test]
fn database_graph_reads_metadata_and_keeps_a_saved_layout() {
    let directory = std::env::temp_dir().join(format!("dbeast_graph_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("shop.sqlite");
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch(
            "create table customers(id integer primary key, name text);
             create table orders(id integer primary key, customer_id integer references customers(id), total real);
             create index orders_customer on orders(customer_id);
             create table order_items(order_id integer references orders(id), sku text);
             create table notes(body text);
             create view big_orders as select * from orders where total > 100;
             with recursive n(v) as (select 1 union all select v + 1 from n where v < 300)
               insert into customers select v, 'c' || v from n;
             with recursive n(v) as (select 1 union all select v + 1 from n where v < 2000)
               insert into orders select v, v % 300 + 1, v from n;
             analyze;",
        )
        .unwrap();
    }
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "graph-fixture".into();
    source.name = "Shop".into();
    source.file = file.to_string_lossy().into_owned();
    let config_path = directory.join("config.json");
    let config = Config {
        data_sources: vec![source],
        tile_url: String::new(),
        files_root: Some(directory.clone()),
        ..Config::default()
    };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = Harness::builder()
        .with_size([1500.0, 950.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    h.get_by_label("Shop").click_secondary();
    h.run_steps(3);
    click(&mut h, "Show Database Graph");
    wait_for(&mut h, "the catalog scan", |h| h.query_by_label_contains("5 objects").is_some());
    assert!(h.query_by_label_contains("2 relations").is_some(), "two foreign keys and nothing guessed");
    assert!(h.query_by_label_contains("no table scans").is_some());
    assert!(h.query_by_label_contains("SQLite does not record view dependencies").is_some());

    // Enter in the search selects the first match and opens its details.
    h.get_by_label("Search:").click();
    h.run_steps(2);
    h.get_by_label("Search:").type_text("main.orders");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(4);
    assert!(h.query_by_label("Relations (2)").is_some(), "orders references customers and is referenced by order_items");
    assert!(h.query_by_label_contains("2.0K · estimated").is_some(), "rows from sqlite_stat1");
    assert!(h.query_by_label_contains("customer_id → id").is_some());

    click(&mut h, "→ main.customers");
    assert!(h.query_by_label("Relations (1)").is_some());

    h.render().unwrap().save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/95-database-graph.png")).unwrap();

    // View > Spread moves objects apart at once; Repulsion asks for a new layout.
    // The details panel repeats the selected name on the right; scene labels are left of it.
    let scene_label = |h: &H<'_>, name: &str| {
        h.query_all_by_label(name).min_by(|a, b| a.rect().left().total_cmp(&b.rect().left())).unwrap().rect()
    };
    let distance = |h: &H<'_>| (scene_label(h, "orders").center() - scene_label(h, "customers").center()).length();
    // A moved camera stops automatic framing, so the spread shows as distance on screen.
    let middle = (scene_label(&h, "orders").center().to_vec2() + scene_label(&h, "customers").center().to_vec2()) * 0.5;
    h.event(egui::Event::PointerMoved(middle.to_pos2()));
    h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -2.0), modifiers: Modifiers::NONE, phase: egui::TouchPhase::Move });
    h.run_steps(5);
    let before = distance(&h);
    click(&mut h, "View");
    h.get_by_role_and_label(egui::accesskit::Role::Slider, "Spread").focus();
    h.run_steps(2);
    // A few steps apart: objects pushed off the canvas are not drawn and have no label.
    for _ in 0..10 {
        h.key_press(egui::Key::ArrowRight);
        h.run_steps(1);
    }
    h.run_steps(3);
    let spread = distance(&h);
    assert!(spread > before * 1.2, "spread {before} -> {spread}");
    h.get_by_role_and_label(egui::accesskit::Role::Slider, "Repulsion").focus();
    h.run_steps(2);
    for _ in 0..20 {
        h.key_press(egui::Key::ArrowRight);
        h.run_steps(1);
    }
    wait_for(&mut h, "the new layout", |h| (distance(h) - spread).abs() > 1.0);
    h.render().unwrap().save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/95-database-graph-view.png")).unwrap();
    // Turntable rotates the scene while it is on.
    let still = scene_label(&h, "orders").center();
    h.get_by_label_contains("Turntable").click();
    h.run_steps(3);
    for _ in 0..30 {
        h.step();
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!((scene_label(&h, "orders").center() - still).length() > 2.0, "the scene turns");
    h.get_by_label_contains("Turntable").click();
    h.run_steps(3);
    click(&mut h, "Reset View Options");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);

    // Switching to storage size and 2D keeps the same objects.
    click(&mut h, "Storage size");
    click(&mut h, "2D");
    assert!(h.query_by_label_contains("5 objects").is_some());
    h.query_all_by_label("Neighbors Only")
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .unwrap()
        .click();
    h.run_steps(3);
    h.render().unwrap().save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/96-database-graph-2d.png")).unwrap();

    // Dragging a labelled object pins it; Save Layout stores it in the session.
    let label = h.get_by_label("orders").rect();
    let from = egui::pos2(label.center().x, label.top() - 6.0);
    let to = from + egui::vec2(140.0, 60.0);
    let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.event(egui::Event::PointerMoved(from));
    h.run_steps(1);
    h.event(button(from, true));
    h.run_steps(1);
    for step in 1..=6 {
        h.event(egui::Event::PointerMoved(from + (to - from) * (step as f32 / 6.0)));
        h.run_steps(1);
    }
    h.event(button(to, false));
    h.run_steps(3);
    let moved = h.get_by_label("orders").rect();
    assert!((moved.center() - label.center()).length() > 60.0, "the object follows the pointer");
    click(&mut h, "Layout");
    click(&mut h, "Save Layout");
    let session = directory.join("session.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saved = std::fs::read_to_string(&session).unwrap_or_default();
        if saved.contains("graph_layouts") && saved.contains("\"orders\"") {
            assert!(!saved.contains("c299"), "the layout holds no data");
            break;
        }
        assert!(Instant::now() < deadline, "the layout was not saved: {saved}");
        h.step();
        std::thread::sleep(Duration::from_millis(50));
    }
    // Double-click is the explicit way to open data, like in Database Explorer.
    drop(h);
    std::fs::remove_dir_all(directory).unwrap();
}
