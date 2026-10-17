//! Record View beside Geo Viewer on a restored WKT result. No database is opened.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;

fn places() -> QueryResult {
    let rows = [("Warsaw", 21.01, 52.23), ("Krakow", 19.94, 50.06), ("Gdansk", 18.65, 54.35)]
        .iter()
        .enumerate()
        .map(|(index, (name, x, y))| {
            vec![
                Value::Int(index as i64 + 1),
                Value::Text((*name).into()),
                Value::Text(format!("SRID=4326;POINT({x} {y})")),
            ]
        })
        .collect();
    QueryResult {
        columns: [("id", "int4"), ("city", "text"), ("location", "text")]
            .iter()
            .map(|(name, ty)| Column {
                name: (*name).into(),
                type_name: (*ty).into(),
            })
            .collect(),
        rows,
        ..Default::default()
    }
}

#[test]
fn record_view_stays_beside_the_map() {
    let directory = std::env::temp_dir().join(format!("dbeast_geo_record_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "geo-fixture".into();
    source.name = "Places".into();
    source.file = directory
        .join("never-opened.sqlite")
        .to_string_lossy()
        .into_owned();
    let config = Config {
        data_sources: vec![source],
        tile_url: String::new(),
        files_root: Some(directory.clone()),
        ..Config::default()
    };
    std::fs::write(
        directory.join("config.json"),
        common::legacy_config(&config).unwrap(),
    )
    .unwrap();
    let session = serde_json::json!({"active": 0, "tabs": [{
        "title": "places", "ds_id": "geo-fixture",
        "kind": {"Table": {"schema": null, "table": "places", "filter": "", "order": ""}},
        "view": 0, "show_value_editor": false,
        "results": [{"title": "places", "sql": "SELECT * FROM places", "page": Page::first(500),
            "data": places(), "show_value_editor": false}]
    }]});
    std::fs::write(directory.join("session.json"), session.to_string()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", directory.join("config.json")) };
    let mut h = Harness::builder()
        .with_size([1600.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    h.get_by_label("Krakow").click();
    h.run_steps(3);
    h.get_by_label("Record View").click();
    h.run_steps(3);
    assert!(h.query_by_label("2 of 3").is_some(), "Record View shows the selected row");

    h.get_by_label("🌐").click();
    h.run_steps(5);
    assert!(h.query_by_label("3 objects").is_some(), "the map is open");
    assert!(h.query_by_label("2 of 3").is_some(), "Record View stays beside the map");
    assert!(h.query_by_label("Krakow").is_some());
    h.render()
        .unwrap()
        .save(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/screenshots")
                .join("94-geo-record-view.png"),
        )
        .unwrap();

    // Next Record moves the selection, also on the map.
    h.query_all_by_label("▶")
        .max_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("Next Record in Record View")
        .click();
    h.run_steps(3);
    assert!(h.query_by_label("3 of 3").is_some());
    assert!(h.query_by_label("Gdansk").is_some());

    // Closing Record View leaves the map; the globe returns to the grid.
    h.query_all_by_label("×")
        .max_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("Close Record View")
        .click();
    h.run_steps(3);
    assert!(h.query_by_label("3 of 3").is_none());
    assert!(h.query_by_label("3 objects").is_some());
    h.get_by_label("🌐").click();
    h.run_steps(3);
    assert!(h.query_by_label("3 objects").is_none());
    assert!(!directory.join("never-opened.sqlite").exists());
    drop(h);
    std::fs::remove_dir_all(directory).unwrap();
}
