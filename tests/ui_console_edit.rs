//! Console UPDATE uses verified provenance and the same guards in every grid view.

mod common;

use dbeast::config::Config;
use dbeast::model::{DataSource, DriverKind};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;
const SELECT: &str = "SELECT id AS selected_id, tenant AS selected_tenant, name AS display_name, note, amount, generated_name, amount * 2 AS doubled FROM contacts ORDER BY tenant, id";

fn app() -> H<'static> {
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    let deadline = Instant::now() + Duration::from_secs(15);
    while h.state().starting() {
        assert!(Instant::now() < deadline, "timed out waiting for the session");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(2);
    h
}

fn key(h: &mut H<'_>, modifiers: Modifiers, key: Key) {
    h.key_press_modifiers(modifiers, key);
    h.run_steps(3);
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn view_action(h: &mut H<'_>, label: &str) {
    if let Some(action) = h.query_by_role_and_label(Role::Button, label) {
        action.click();
    } else {
        click(h, "Options  ⏷");
        h.query_all_by_label(label)
            .find(|node| matches!(node.accesskit_node().role(), Role::Button | Role::CheckBox))
            .expect("view action in the result Options menu")
            .click();
    }
    h.run_steps(3);
}

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn shot(h: &mut H<'_>, name: &str) {
    h.render()
        .unwrap()
        .save(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/screenshots")
                .join(name),
        )
        .unwrap();
}

fn execute(h: &mut H<'_>, sql: &str) {
    h.get_all_by_role(Role::MultilineTextInput)
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("SQL editor above the result")
        .focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_all_by_role(Role::MultilineTextInput)
        .find(|node| node.is_focused())
        .expect("focused SQL editor")
        .type_text(sql);
    h.run_steps(2);
    key(h, Modifiers::COMMAND, Key::Enter);
}

#[track_caller]
fn grid_cell<'h>(h: &'h H<'_>, value: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(value)
        .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("cell in the result grid")
}

#[track_caller]
fn edit_selected(h: &mut H<'_>, value: &str, finish: bool) {
    key(h, Modifiers::NONE, Key::Enter);
    key(h, Modifiers::COMMAND, Key::A);
    let field = h
        .query_all_by_role(Role::TextInput)
        .find(|node| node.is_focused());
    if let Some(field) = field {
        field.type_text(value);
    } else {
        let nodes: Vec<_> = h
            .query_all_by_label_contains("")
            .map(|node| {
                (
                    node.accesskit_node().role(),
                    node.accesskit_node().label(),
                    node.value(),
                    node.is_focused(),
                    node.rect(),
                )
            })
            .collect();
        shot(h, "console-edit-focus-failure.png");
        panic!("no focused inline editor when typing {value:?}; nodes: {nodes:#?}");
    }
    h.run_steps(2);
    if finish {
        key(h, Modifiers::NONE, Key::Enter);
    }
}

fn assert_action_enabled(h: &mut H<'_>, label: &str) {
    key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .type_text(label);
    h.run_steps(3);
    assert!(
        !h.get_by_role_and_label(Role::Button, label)
            .accesskit_node()
            .is_disabled(),
        "{label} is available for a verified result"
    );
    key(h, Modifiers::NONE, Key::Escape);
}

#[test]
fn console_edits_use_physical_names_and_verified_row_operations() {
    let dir = std::env::temp_dir().join(format!("dbeast_console_edit_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("contacts.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE contacts (
            tenant INTEGER NOT NULL, id INTEGER NOT NULL, name TEXT NOT NULL,
            note TEXT, amount INTEGER NOT NULL DEFAULT 0 CHECK(amount >= 0),
            generated_name TEXT GENERATED ALWAYS AS (name || ':' || id) STORED,
            PRIMARY KEY(tenant,id));
         INSERT INTO contacts(tenant,id,name,note,amount) VALUES
            (7,1,'Ada Żółw','first',10), (7,2,'Olek Lis',NULL,20),
            (8,1,'Mira Kot','sentinel',30);
         CREATE TABLE no_key(name TEXT, note TEXT);
         INSERT INTO no_key VALUES ('duplicate','same'),('duplicate','same');",
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "console-edit-fixture".into();
    source.name = "Local console edits".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        max_rows: 500,
        tile_url: String::new(),
        files_root: Some(dir.clone()),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    std::fs::write(
        dir.join("session.json"),
        serde_json::json!({
            "active":0,"tabs":[{"title":"Updates","ds_id":"console-edit-fixture",
                "kind":{"Console":{"sql":SELECT}},"results":[]}]
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: this is the only test in the binary, before constructing the app.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let stored = |tenant: i64, id: i64| -> (String, Option<String>) {
        db.query_row(
            "SELECT name,note FROM contacts WHERE tenant=?1 AND id=?2",
            [tenant, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let mut h = app();
    execute(&mut h, SELECT);
    wait_until(&mut h, "editable console result", |h| {
        h.query_by_label("Ada Żółw").is_some() && h.query_by_label("Editable result").is_some()
    });

    grid_cell(&h, "Ada Żółw").click();
    h.run_steps(3);
    edit_selected(&mut h, "Ada O'Neil Żółw", true);
    assert_eq!(stored(7, 1).0, "Ada Żółw", "Enter only stages a local edit");
    shot(&mut h, "55-console-edit.png");
    click(&mut h, "DML");
    let preview = h
        .query_all_by_label_contains("UPDATE")
        .filter_map(|node| node.value().or_else(|| node.accesskit_node().label()))
        .find(|text| text.contains("UPDATE \"main\".\"contacts\""))
        .expect("DML preview targets the physical table");
    assert!(
        preview.contains("\"name\"")
            && preview.contains("\"tenant\"")
            && preview.contains("\"id\"")
    );
    assert!(
        !preview.contains("selected_id") && !preview.contains("display_name"),
        "DML uses physical names, not the aliases"
    );
    shot(&mut h, "56-console-edit-preview.png");
    click(&mut h, "DML");

    grid_cell(&h, "first").click();
    h.run_steps(3);
    edit_selected(&mut h, "Pending note Żółw", false);
    key(&mut h, Modifiers::COMMAND, Key::Enter);
    wait_until(&mut h, "submitted console edits", |h| {
        stored(7, 1) == ("Ada O'Neil Żółw".into(), Some("Pending note Żółw".into()))
            && h.query_by_label("DML").is_none()
            && h.query_all_by_label("Ada O'Neil Żółw").next().is_some()
    });
    assert_eq!(stored(8, 1), ("Mira Kot".into(), Some("sentinel".into())));

    // Verified results also support row operations; they stay local until Submit.
    grid_cell(&h, "Ada O'Neil Żółw").click();
    h.run_steps(3);
    for action in ["Add Row", "Clone Row", "Delete Row"] {
        assert_action_enabled(&mut h, action);
    }
    grid_cell(&h, "Ada O'Neil Żółw").click();
    h.run_steps(3);
    // The result toolbar is the lowest "−" / "+" pair on screen.
    for label in ["−", "+"] {
        h.query_all_by_label(label)
            .max_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
            .expect("result toolbar row action")
            .click();
        h.run_steps(3);
    }
    click(&mut h, "DML");
    let preview = h
        .query_all_by_label_contains("DELETE")
        .filter_map(|node| node.value().or_else(|| node.accesskit_node().label()))
        .find(|text| text.contains("DELETE FROM \"main\".\"contacts\""))
        .expect("DML preview deletes by key and shown values");
    assert!(preview.contains("\"tenant\" IS ?") && preview.contains("INSERT INTO \"main\".\"contacts\""));
    click(&mut h, "DML");
    click(&mut h, "Revert");
    assert!(h.query_by_label("DML").is_none());
    assert_eq!(
        db.query_row("SELECT count(*) FROM contacts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        3
    );

    // Record View shares the same pending state and Revert does not write.
    if h.query_by_label("Close Value Editor").is_some() {
        click(&mut h, "Close Value Editor");
    }
    view_action(&mut h, "Record View");
    h.get_all_by_label("Ada O'Neil Żółw")
        .max_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .unwrap()
        .click();
    h.run_steps(3);
    edit_selected(&mut h, "Unsubmitted record", true);
    assert_eq!(stored(7, 1).0, "Ada O'Neil Żółw");
    click(&mut h, "Revert");
    assert!(h.query_by_label("Unsubmitted record").is_none());
    assert_eq!(stored(7, 1).0, "Ada O'Neil Żółw");
    view_action(&mut h, "Record View");

    // The multiline value editor stages the same cell in a transposed result.
    view_action(&mut h, "Transpose");
    grid_cell(&h, "Pending note Żółw").click();
    h.run_steps(3);
    key(&mut h, Modifiers::SHIFT, Key::Enter);
    click(&mut h, "Edit Value");
    let field = h.get_by_role_and_label(Role::MultilineTextInput, "Value editor text");
    field.focus();
    h.run_steps(1);
    key(&mut h, Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::MultilineTextInput, "Value editor text")
        .type_text("Value editor Żółw\nsecond line");
    h.run_steps(3);
    click(&mut h, "Apply Value");
    assert_eq!(stored(7, 1).1.as_deref(), Some("Pending note Żółw"));
    click(&mut h, "Submit");
    wait_until(&mut h, "submitted Value Editor change", |h| {
        stored(7, 1).1.as_deref() == Some("Value editor Żółw\nsecond line")
            && h.query_by_label("DML").is_none()
    });
    view_action(&mut h, "Transpose");

    // Generated and computed columns are present, but never become editable.
    grid_cell(&h, "Ada O'Neil Żółw:1").click();
    h.run_steps(3);
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(
        !h.query_all_by_role(Role::TextInput)
            .any(|node| node.is_focused())
    );
    assert!(h.query_by_label("DML").is_none());
    key(&mut h, Modifiers::SHIFT, Key::Enter);
    h.run_steps(3);
    assert!(h.get_by_label("Edit Value").accesskit_node().is_disabled());
    h.get_all_by_label("20")
        .max_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("rightmost doubled expression")
        .click();
    h.run_steps(3);
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(
        !h.query_all_by_role(Role::TextInput)
            .any(|node| node.is_focused())
    );
    assert!(h.get_by_label("Edit Value").accesskit_node().is_disabled());
    assert!(h.query_by_label("DML").is_none());
    h.set_size(egui::vec2(1100.0, 720.0));
    h.run_steps(3);
    shot(&mut h, "57-console-edit-readonly.png");
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);

    // A projection missing one part of the key cannot reuse previous capabilities.
    execute(&mut h, "SELECT id,name FROM contacts ORDER BY tenant,id");
    wait_until(&mut h, "partial-key projection", |h| {
        h.query_by_label("Select Column: name").is_some()
            && h.query_by_label_contains("Read-only:").is_some()
    });
    grid_cell(&h, "Ada O'Neil Żółw").click();
    h.run_steps(3);
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(
        !h.query_all_by_role(Role::TextInput)
            .any(|node| node.is_focused())
    );
    assert!(
        h.query_by_label("Submit")
            .is_none_or(|node| node.accesskit_node().is_disabled())
    );
    assert_eq!(stored(7, 1).0, "Ada O'Neil Żółw");

    execute(&mut h, "SELECT name,note FROM no_key");
    wait_until(&mut h, "result without a primary key", |h| {
        h.query_all_by_label("duplicate").count() == 2
            && h.query_by_label_contains("Read-only:").is_some()
    });
    assert!(
        h.query_by_label("Submit")
            .is_none_or(|node| node.accesskit_node().is_disabled())
    );

    execute(&mut h, SELECT);
    wait_until(&mut h, "fresh editable result", |h| {
        h.query_by_label("Editable result").is_some()
            && h.query_by_label("Select Column: display_name").is_some()
    });
    wait_until(&mut h, "saved current result before restoring", |_| {
        let session: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("session.json")).unwrap())
                .unwrap();
        let tab = &session["tabs"][0];
        let Some(view) = tab["view"].as_u64() else {
            return false;
        };
        let result = &tab["results"][view as usize];
        // The rows are in their own file next to session.json.
        let Some(rows) = result["file"]
            .as_str()
            .and_then(|file| std::fs::read_to_string(dir.join("session-results").join(file)).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        else {
            return false;
        };
        tab["kind"]["Console"]["sql"] == SELECT
            && result["sql"] == SELECT
            && rows["rows"][0][2]["Text"] == "Ada O'Neil Żółw"
            && rows["rows"][0][3]["Text"] == "Value editor Żółw\nsecond line"
    });
    drop(h);
    let mut restored = app();
    grid_cell(&restored, "Ada O'Neil Żółw").click();
    restored.run_steps(3);
    key(&mut restored, Modifiers::NONE, Key::Enter);
    assert!(
        !restored
            .query_all_by_role(Role::TextInput)
            .any(|node| node.is_focused())
    );
    assert!(
        restored
            .query_by_label("Submit")
            .is_none_or(|node| node.accesskit_node().is_disabled())
    );
    assert_eq!(stored(8, 1).0, "Mira Kot");
    drop(restored);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
