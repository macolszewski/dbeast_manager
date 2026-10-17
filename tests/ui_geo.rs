//! Geo Viewer against the Docker PostGIS: toggling the map, choosing the
//! geometry column and the SRID handling. Tiles are off unless
//! DBEAST_TILES=1, so the test does not depend on the network.

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn wait_until(h: &mut H<'_>, what: &str, secs: u64, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

#[test]
fn geo_viewer_shows_postgis_rows() {
    if std::net::TcpStream::connect("127.0.0.1:55433").is_err() {
        eprintln!("skipping: PostGIS test container is not running");
        return;
    }
    let tiles = std::env::var("DBEAST_TILES").is_ok_and(|v| v == "1");
    let dir = std::env::temp_dir().join(format!("dbeast_geo_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    let sql = "select p.number, p.shape, st_centroid(p.shape) as centroid\nfrom geo.parcels p\norder by p.id;";
    std::fs::write(
        &config,
        serde_json::json!({
            "data_sources": [{"id": "gis", "name": "", "driver": "Postgres", "host": "localhost", "port": 55433,
                "user": "dbeast", "password": "dbeast", "database": "gis", "file": "", "use_tls": false, "read_only": false}],
            "consoles": [{"title": "console", "ds_id": "gis", "sql": sql}],
            "tile_url": if tiles { dbeast::ui::OSM_TILES } else { "" },
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config) };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    wait_until(&mut h, "query result", 15, |h| {
        h.query_by_label("⊞ parcels").is_some()
    });

    // The globe opens the map with the first geometry column, SRID from the data.
    h.get_by_label("🌐").click();
    h.run_steps(3);
    assert!(h.query_by_label("15 objects").is_some());
    assert!(h.query_by_label("(from data)").is_some());
    assert!(h.query_by_label("Map").is_some());
    if tiles {
        // Give the tile downloads time to finish for the screenshot.
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            h.step();
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    shot(
        &mut h,
        if tiles {
            "20-geo-viewer-osm"
        } else {
            "20-geo-viewer"
        },
    );

    // Choosing the other geometry column.
    h.get_all_by_role(egui::accesskit::Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("shape"))
        .expect("geometry column combo")
        .click();
    h.run_steps(2);
    h.get_by_label("centroid").click();
    h.run_steps(3);
    assert!(h.query_by_label("15 objects").is_some());

    // A wrong SRID entered by hand moves the data off the map area; clearing it restores the data SRID.
    let srid = h
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .last()
        .unwrap();
    srid.focus();
    srid.type_text("4326");
    h.run_steps(3);
    assert!(h.query_by_label("(set manually)").is_some());
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.key_press(Key::Backspace);
    h.run_steps(3);
    assert!(h.query_by_label("(from data)").is_some());

    // The globe toggles back to the grid.
    h.get_by_label("🌐").click();
    h.run_steps(2);
    assert!(h.query_by_label("Map").is_none());
    assert!(h.query_by_label("B/0").is_some());
    std::fs::remove_dir_all(dir).ok();
}
