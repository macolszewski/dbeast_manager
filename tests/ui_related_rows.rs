//! Related Rows use immutable bound consoles, including paging and restoration.

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
const SOURCE_SQL: &str =
    "SELECT company_id AS organization_ref,name FROM people WHERE id IN (11,15) ORDER BY id";
const COMPOSITE_KEY: &str = "A'\\Żółw; --";

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

fn select_tab(h: &mut H<'_>, title: &str) {
    click(h, &format!("QL {title}"));
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

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        if Instant::now() >= deadline {
            let labels: Vec<_> = h
                .query_all_by_label_contains("")
                .map(|node| {
                    (
                        node.accesskit_node().role(),
                        node.accesskit_node().label(),
                        node.accesskit_node().description(),
                        node.value(),
                    )
                })
                .collect();
            shot(h, "related-rows-timeout.png");
            panic!("timed out waiting for {what}; visible labels: {labels:#?}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn editor<'h>(h: &'h H<'_>) -> egui_kittest::Node<'h> {
    h.get_all_by_role(Role::MultilineTextInput)
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("SQL editor above the result")
}

fn execute(h: &mut H<'_>) {
    editor(h).focus();
    h.run_steps(2);
    key(h, Modifiers::COMMAND, Key::Enter);
}

fn cell<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    h.get_all_by_label(label)
        .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("cell in the result grid")
}

fn select(h: &mut H<'_>, label: &str) {
    cell(h, label).click();
    h.run_steps(3);
}

fn choose(h: &mut H<'_>, label: &str) {
    key(h, Modifiers::NONE, Key::F4);
    wait_until(h, label, |h| {
        h.query_by_role_and_label(Role::Button, label)
            .is_some_and(|node| !node.accesskit_node().is_disabled())
    });
    h.get_by_role_and_label(Role::Button, label).click();
    h.run_steps(3);
}

fn page_action(h: &mut H<'_>, label: &str) {
    let y = h.get_by_label("Options  ⏷").rect().center().y;
    h.get_all_by_label(label)
        .find(|node| (node.rect().center().y - y).abs() < 5.0)
        .expect("action in the result pagination toolbar")
        .click();
    h.run_steps(3);
}

fn session(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Rows of a saved result, kept in their own file next to session.json.
fn saved_rows(path: &Path, result: &serde_json::Value) -> serde_json::Value {
    result["file"]
        .as_str()
        .and_then(|file| {
            std::fs::read_to_string(path.with_file_name("session-results").join(file)).ok()
        })
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .map_or(serde_json::Value::Null, |rows| rows["rows"].clone())
}

#[test]
fn related_rows_preserve_keys_context_pending_edits_and_session_payload() {
    let dir = std::env::temp_dir().join(format!("dbeast_related_ui_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("relations.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
         CREATE TABLE companies(id INTEGER PRIMARY KEY,label TEXT NOT NULL);
         CREATE TABLE people(id INTEGER PRIMARY KEY,company_id INTEGER,name TEXT,note TEXT,
             FOREIGN KEY(company_id) REFERENCES companies(id));
         INSERT INTO companies VALUES(1,'Acme Żółw'),(2,'Other company');
         INSERT INTO people VALUES(11,1,'Ada','original note'),(12,1,'Olek','second'),
             (13,1,'Iga','third'),(14,2,'Sentinel','other company'),(15,NULL,'No company','no relation');
         CREATE TABLE accounts(tenant INTEGER NOT NULL,code TEXT NOT NULL,label TEXT,
             PRIMARY KEY(tenant,code),UNIQUE(code,tenant));
         CREATE TABLE invoices(id INTEGER PRIMARY KEY,tenant INTEGER NOT NULL,account_code TEXT,payer_code TEXT,note TEXT,
             FOREIGN KEY(account_code,tenant) REFERENCES accounts(code,tenant),
             FOREIGN KEY(tenant,payer_code) REFERENCES accounts);"
    ).unwrap();
    for (tenant, code, label) in [
        (7, COMPOSITE_KEY, "North account"),
        (8, COMPOSITE_KEY, "Other tenant"),
        (7, "B", "South account"),
    ] {
        db.execute(
            "INSERT INTO accounts VALUES(?1,?2,?3)",
            rusqlite::params![tenant, code, label],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO invoices VALUES(101,7,?1,'B','invoice north')",
        [COMPOSITE_KEY],
    )
    .unwrap();
    db.execute(
        "INSERT INTO invoices VALUES(102,8,?1,NULL,'invoice other')",
        [COMPOSITE_KEY],
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "related-fixture".into();
    source.name = "Related fixtures".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        max_rows: 2,
        files_root: Some(dir.clone()),
        tile_url: String::new(),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    let session_path = dir.join("session.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    std::fs::write(&session_path, serde_json::json!({"active":0,"tabs":[
        {"title":"Relations","ds_id":"related-fixture","kind":{"Console":{"sql":SOURCE_SQL}},"results":[]},
        {"title":"Mutable","ds_id":"related-fixture","kind":{"Console":{"sql":"SELECT * FROM people WHERE id=11"}},"results":[]},
        {"title":"Composite","ds_id":"related-fixture","kind":{"Console":{"sql":"SELECT payer_code AS payer,id AS invoice_id,account_code AS account_ref,tenant AS tenant_ref,note FROM invoices ORDER BY id"}},"results":[]}
    ]}).to_string()).unwrap();
    // SAFETY: one test in this binary, before constructing the app or workers.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = app();
    execute(&mut h);
    wait_until(&mut h, "read-only source with aliases", |h| {
        h.query_by_label("Ada").is_some() && h.query_by_label_contains("Read-only:").is_some()
    });
    select(&mut h, "Ada");
    choose(&mut h, "Referenced Rows: main.companies · FK 0");
    wait_until(&mut h, "referenced company", |h| {
        h.query_by_label("Acme Żółw").is_some()
    });
    h.get_by_label("Related rows · bound parameters");
    assert!(editor(&h).accesskit_node().is_read_only());
    assert!(h.get_by_label("Data source").accesskit_node().is_disabled());
    let generated_sql = editor(&h).value().unwrap();
    assert!(
        generated_sql.contains('?'),
        "the editor displays placeholders"
    );

    select(&mut h, "Acme Żółw");
    choose(&mut h, "Referencing Rows: main.people · FK 0");
    wait_until(&mut h, "incoming first page", |h| {
        h.query_by_label("Ada").is_some() && h.query_by_label("Olek").is_some()
    });
    assert!(h.query_by_label("Sentinel").is_none());
    page_action(&mut h, "▶");
    wait_until(&mut h, "incoming second page", |h| {
        h.query_by_label("Iga").is_some() && h.query_by_label("3-3 of 3").is_some()
    });
    assert!(h.query_by_label("Sentinel").is_none());
    page_action(&mut h, "⟳");
    // Reload follows the existing console behavior: restart at the first page,
    // while retaining the captured relationship rather than dropping its filter.
    wait_until(&mut h, "reloaded incoming first page", |h| {
        h.query_by_label("Ada").is_some()
            && h.query_by_label("Olek").is_some()
            && h.query_by_label("1-2 of 2+").is_some()
    });
    assert!(h.query_by_label("Sentinel").is_none());
    page_action(&mut h, "▶");
    wait_until(&mut h, "incoming next page after reload", |h| {
        h.query_by_label("Iga").is_some() && h.query_by_label("3-3 of 3").is_some()
    });
    shot(&mut h, "58-related-rows.png");

    select_tab(&mut h, "Relations");
    assert_eq!(editor(&h).value().as_deref(), Some(SOURCE_SQL));
    select(&mut h, "No company");
    key(&mut h, Modifiers::NONE, Key::F4);
    wait_until(&mut h, "NULL relationship disabled", |h| {
        h.query_by_role_and_label(Role::Button, "Referenced Rows: main.companies · FK 0")
            .is_some_and(|node| {
                node.accesskit_node().is_disabled()
                    && node
                        .accesskit_node()
                        .description()
                        .is_some_and(|reason| reason.contains("NULL"))
            })
    });
    assert!(
        h.get_by_role_and_label(Role::Button, "Referenced Rows: main.companies · FK 0")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, "Cancel");

    select_tab(&mut h, "Composite");
    execute(&mut h);
    wait_until(&mut h, "composite source", |h| {
        h.query_by_label("invoice north").is_some()
    });
    // Sorting must not turn the displayed row number into a database identity.
    click(&mut h, "Sort by invoice_id");
    click(&mut h, "Sort by invoice_id");
    select(&mut h, "invoice north");
    key(&mut h, Modifiers::NONE, Key::F4);
    wait_until(&mut h, "both composite relationships", |h| {
        h.query_by_role_and_label(Role::Button, "Referenced Rows: main.accounts · FK 0")
            .is_some()
            && h.query_by_role_and_label(Role::Button, "Referenced Rows: main.accounts · FK 1")
                .is_some()
    });
    shot(&mut h, "59-related-rows-chooser.png");
    // SQLite assigns the last declared FK id 0: the implicit (tenant,payer) key.
    click(&mut h, "Referenced Rows: main.accounts · FK 0");
    wait_until(&mut h, "composite payer account", |h| {
        h.query_by_label("South account").is_some()
    });
    assert!(h.query_by_label("Other tenant").is_none());
    assert!(!editor(&h).value().unwrap().contains(COMPOSITE_KEY));

    select_tab(&mut h, "Mutable");
    execute(&mut h);
    wait_until(&mut h, "editable source", |h| {
        h.query_by_label("Editable result").is_some()
    });
    if h.query_by_label("Close Value Editor").is_some() {
        click(&mut h, "Close Value Editor");
    }
    select(&mut h, "original note");
    key(&mut h, Modifiers::NONE, Key::Enter);
    key(&mut h, Modifiers::COMMAND, Key::A);
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("inline note editor")
        .type_text("pending note outside FK");
    h.run_steps(2);
    // The active buffer itself counts as pending, including columns outside the FK.
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .type_text("Related Rows");
    h.run_steps(3);
    assert!(
        h.get_by_role_and_label(Role::Button, "Related Rows")
            .accesskit_node()
            .is_disabled()
    );
    key(&mut h, Modifiers::NONE, Key::Escape);
    // Opening the palette may already commit the inline buffer on focus loss.
    // Enter without an active editor would instead open the Value Editor.
    if h.query_all_by_role(Role::TextInput)
        .any(|node| node.is_focused())
    {
        key(&mut h, Modifiers::NONE, Key::Enter);
    }
    if h.query_by_label("Close Value Editor").is_some() {
        click(&mut h, "Close Value Editor");
    }
    h.get_by_label("pending note outside FK");
    select(&mut h, "Ada");
    assert!(
        !h.query_all_by_role(Role::TextInput)
            .any(|node| node.is_focused())
    );
    key(&mut h, Modifiers::NONE, Key::F4);
    wait_until(&mut h, "pending changes prevent navigation", |h| {
        h.query_by_label_contains("Submit or revert pending changes before opening Related Rows")
            .is_some()
            && h.query_by_label("DML").is_some()
    });
    assert_eq!(
        db.query_row("SELECT note FROM people WHERE id=11", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "original note"
    );
    h.set_size(egui::vec2(1100.0, 720.0));
    h.run_steps(3);
    shot(&mut h, "60-related-rows-disabled.png");
    if h.query_by_role_and_label(Role::Button, "Cancel").is_some() {
        click(&mut h, "Cancel");
    }
    click(&mut h, "Revert");
    assert!(h.query_by_label("DML").is_none());
    assert!(h.query_by_label("original note").is_some());

    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);
    select_tab(&mut h, "Related: main.people");
    h.get_by_label("Iga");
    wait_until(&mut h, "persisted active bound tab and page", |_| {
        let saved = session(&session_path);
        let Some(active) = saved["active"].as_u64() else {
            return false;
        };
        let tab = &saved["tabs"][active as usize];
        let Some(view) = tab["view"].as_u64() else {
            return false;
        };
        let result = &tab["results"][view as usize];
        tab["title"] == "Related: main.people"
            && tab["bound_query"].is_object()
            && result["page"]["skip"] == 2
            && result["page"]["limit"] == 2
            && saved_rows(&session_path, result)
                == serde_json::json!([
                    [{"Int":13},{"Int":1},{"Text":"Iga"},{"Text":"third"}]
                ])
    });
    let before = session(&session_path);
    let bound_tab = before["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tab| tab["title"] == "Related: main.people")
        .unwrap();
    assert_eq!(
        bound_tab["bound_query"]["values"],
        serde_json::json!([{"Int":1}])
    );
    drop(h);

    let mut h = app();
    h.get_by_label("Iga");
    h.get_by_label("Related rows · bound parameters");
    assert!(editor(&h).accesskit_node().is_read_only());
    select(&mut h, "Iga");
    key(&mut h, Modifiers::NONE, Key::F4);
    h.get_by_label_contains("Execute the query again to establish column origins");
    // A restored console still executes its bound query, not the placeholder SQL alone.
    execute(&mut h);
    wait_until(&mut h, "bound Execute after restart", |h| {
        h.query_by_label("Ada").is_some() && h.query_by_label("Olek").is_some()
    });
    assert!(h.query_by_label("Sentinel").is_none());
    page_action(&mut h, "▶");
    wait_until(&mut h, "bound paging after restart", |h| {
        h.query_by_label("Iga").is_some() && h.query_by_label("3-3 of 3").is_some()
    });
    assert!(h.query_by_label("Sentinel").is_none());
    drop(h);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
