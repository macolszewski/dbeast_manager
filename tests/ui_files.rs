//! Files tool window: browsing, search, opening a CSV and saving edits back.

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

/// Clickable row (button role) with this label; grid cells are plain labels
/// inside a clickable cell, so they fall back to the first match.
fn row<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(label)
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
        .or_else(|| h.get_all_by_label(label).next())
        .unwrap_or_else(|| panic!("no row {label}"))
}

fn double_click(h: &mut H<'_>, label: &str, t: f64) {
    h.input_mut().time = Some(t);
    row(h, label).click();
    h.step();
    h.input_mut().time = Some(t + 0.1);
    row(h, label).click();
    h.step();
    h.input_mut().time = Some(t + 5.0);
    h.run_steps(2);
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

#[test]
fn browse_search_open_and_edit_csv() {
    let dir = std::env::temp_dir().join(format!("dbeast_files_ui_{}", std::process::id()));
    let data = dir.join("data");
    std::fs::create_dir_all(data.join("archive")).unwrap();
    std::fs::write(
        data.join("sales.csv"),
        "id;city;amount\n1;Kraków;10.5\n2;Gdańsk;7\n3;Łódź;12\n",
    )
    .unwrap();
    std::fs::write(data.join("notes.md"), "# not a data file\n").unwrap();
    std::fs::write(
        data.join("archive/regions.tsv"),
        "code\tname\nPL-MA\tMałopolskie\n",
    )
    .unwrap();
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        serde_json::json!({ "files_root": data }).to_string(),
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

    // The Files tool window lists only supported files.
    h.get_all_by_label("🗀").next().unwrap().click();
    // Folders are listed in the background.
    wait_until(&mut h, "folder listing", |h| h.query_by_label("sales.csv").is_some());
    assert!(h.query_by_label("archive").is_some());
    assert!(
        h.query_by_label("notes.md").is_none(),
        "unsupported files are hidden"
    );

    // Search goes into subfolders.
    let search = h
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap();
    search.focus();
    search.type_text("reg");
    wait_until(&mut h, "search results", |h| {
        h.query_by_label("regions.tsv").is_some()
    });
    shot(&mut h, "16-files-search");
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.key_press(Key::Backspace);
    h.run_steps(3);

    // Double click opens the CSV as a table, without a permanent data source.
    double_click(&mut h, "sales.csv", 10.0);
    wait_until(&mut h, "csv table", |h| {
        h.query_by_label("1-3 of 3").is_some()
    });
    assert!(h.query_by_label("Gdańsk").is_some());
    shot(&mut h, "17-csv-opened");

    // Editing a cell and submitting writes the change back to the file.
    double_click(&mut h, "Gdańsk", 20.0);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_all_by_role(egui::accesskit::Role::TextInput)
        .find(|n| n.is_focused())
        .expect("cell editor")
        .type_text("Gdynia");
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.get_by_label("Submit").click();
    let csv = data.join("sales.csv");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !std::fs::read_to_string(&csv).unwrap().contains("Gdynia") {
        assert!(Instant::now() < deadline, "file was not saved");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(&csv).unwrap(),
        "id;city;amount\n1;Kraków;10.5\n2;Gdynia;7\n3;Łódź;12\n",
        "only the edited cell changes"
    );
    let config_text = std::fs::read_to_string(&config).unwrap();
    assert!(
        !config_text.contains("file:"),
        "opened files are not saved as data sources"
    );

    // Closing the file removes its temporary database.
    let temp = dir.join("open-files");
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 1);
    row(&h, "sales.csv").click_secondary();
    h.run_steps(2);
    h.get_by_label("Close File").click();
    h.run_steps(3);
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
    assert!(h.query_by_label("1-3 of 3").is_none());

    std::fs::remove_dir_all(dir).ok();
}
