//! Cached explorer navigation uses no database; explicit reveal loads lazily
//! from a local SQLite fixture and ignores results for an abandoned target.

mod common;

use dbeast::config::Config;
use dbeast::model::{
    Column, DataSource, DbObject, DriverKind, ObjectKind, Page, QueryResult, Value,
};
use egui::accesskit::{Role, Toggled};
use egui::{Id, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;
const SOURCE: &str = "explorer-fixture";
const SOURCE_LABEL: &str = "Library";
const SCRATCH_SQL: &str = "-- Editor focus remains here after cancelled reveal";
const LAST_TABLE: &str = "record_0999";

fn app() -> (H<'static>, egui::Context) {
    let mut context = None;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| {
            context = Some(cc.egui_ctx.clone());
            dbeast::ui::App::new_for_test(cc)
        });
    h.run_steps(3);
    (h, context.unwrap())
}

fn key(h: &mut H<'_>, modifiers: Modifiers, key: Key) {
    h.key_press_modifiers(modifiers, key);
    h.run_steps(3);
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn menu_action(h: &mut H<'_>, label: &str) {
    // The explorer menu publishes this label separately from its ellipsis icon.
    click(h, "More actions");
    click(h, label);
}

#[track_caller]
fn row<'h>(h: &'h H<'_>, name: &'h str) -> egui_kittest::Node<'h> {
    let caller = std::panic::Location::caller();
    h.query_all_by_role_and_label(Role::Button, name)
        .find(|node| node.accesskit_node().toggled().is_some())
        .unwrap_or_else(|| panic!("missing explorer row {name:?}, requested at {caller}: {h:#?}"))
}

#[track_caller]
fn selected(h: &H<'_>, name: &str) -> bool {
    h.query_all_by_role(Role::Button).any(|node| {
        let node = node.accesskit_node();
        node.label().as_deref() == Some(name) && node.toggled() == Some(Toggled::True)
    })
}

fn tree_state(h: &H<'_>) -> String {
    let selected: Vec<_> = h
        .get_all_by_role(Role::Button)
        .filter(|node| node.accesskit_node().toggled() == Some(Toggled::True))
        .map(|node| (node.accesskit_node().id(), node.accesskit_node().label()))
        .collect();
    let focused: Vec<_> = h
        .root()
        .children_recursive()
        .filter(|node| node.is_focused())
        .map(|node| {
            (
                node.accesskit_node().id(),
                node.accesskit_node().role(),
                node.accesskit_node().label(),
                node.accesskit_node().value(),
            )
        })
        .collect();
    format!("selected rows: {selected:?}; focused ID/role/label/value: {focused:?}")
}

#[track_caller]
fn assert_selected(h: &H<'_>, name: &str) {
    assert!(
        selected(h, name),
        "expected selected row {name:?}; {}",
        tree_state(h)
    );
}

fn click_row(h: &mut H<'_>, name: &str) {
    row(h, name).click();
    h.run_steps(3);
    assert_selected(h, name);
    assert!(h.get_by_label("Database Explorer Tree").is_focused());
}

fn query(h: &mut H<'_>, text: &str) {
    h.get_by_role_and_label(Role::TextInput, "Speed Search")
        .focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Speed Search")
        .type_text(text);
    h.run_steps(3);
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Speed Search")
            .value()
            .as_deref(),
        Some(text)
    );
}

fn palette_action(h: &mut H<'_>, name: &str) {
    key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    let query = h.get_by_role_and_label(Role::TextInput, "Find Action");
    assert!(query.is_focused(), "{}", tree_state(h));
    query.type_text(name);
    h.run_steps(3);
    let action = h
        .get_all_by_role_and_label(Role::Button, name)
        .find(|node| node.accesskit_node().toggled().is_some())
        .unwrap_or_else(|| panic!("palette action {name:?} is missing: {h:#?}"));
    assert!(
        !action.accesskit_node().is_disabled(),
        "action {name:?} must be enabled"
    );
    assert_eq!(action.accesskit_node().toggled(), Some(Toggled::True));
    key(h, Modifiers::NONE, Key::Enter);
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Find Action")
            .is_none()
    );
}

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {h:#?}"
        );
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn shot(h: &mut H<'_>, name: &str) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render().unwrap().save(directory.join(name)).unwrap();
}

fn saved_table(name: &str) -> serde_json::Value {
    let data = QueryResult {
        columns: vec![Column {
            name: "note".into(),
            type_name: "text".into(),
        }],
        rows: vec![vec![Value::Text("cached editor contents".into())]],
        ..QueryResult::default()
    };
    serde_json::json!({
        "title": name, "ds_id": SOURCE,
        "kind": {"Table": {"schema": null, "table": name, "filter": "", "order": "", "primary_key": []}},
        "view": 0,
        "results": [{"title": name, "sql": "SELECT note", "page": Page::first(10), "data": data, "show_value_editor": false}]
    })
}

fn scratch_tab() -> serde_json::Value {
    serde_json::json!({
        "title": "Scratch", "ds_id": SOURCE, "kind": {"Console": {"sql": SCRATCH_SQL}}, "results": []
    })
}

fn source(path: &Path) -> DataSource {
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = SOURCE.into();
    source.name = SOURCE_LABEL.into();
    source.file = path.to_string_lossy().into_owned();
    source
}

fn write_fixture(directory: &Path, source: DataSource, session: serde_json::Value) {
    let config = Config {
        data_sources: vec![source],
        max_rows: 10,
        tile_url: String::new(),
        files_root: Some(directory.to_owned()),
        ..Config::default()
    };
    std::fs::write(
        directory.join("config.json"),
        common::legacy_config(&config).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.join("session.json"),
        serde_json::to_string(&session).unwrap(),
    )
    .unwrap();
}

fn cached_session() -> serde_json::Value {
    let mut main = DbObject::new(ObjectKind::Schema, "main", &[]);
    let mut tables = vec![
        DbObject::new(ObjectKind::Table, "film_actor", &main.path),
        DbObject::new(ObjectKind::Table, "film_archive", &main.path),
        DbObject::new(ObjectKind::Table, "Łódź", &main.path),
    ];
    tables.extend(
        (0..1000).map(|index| {
            DbObject::new(ObjectKind::Table, format!("record_{index:04}"), &main.path)
        }),
    );
    let folder = DbObject::folder("tables", &main.path, tables);
    let folder_path = folder.path.clone();
    main.children = Some(vec![folder]);

    let mut archive = DbObject::new(ObjectKind::Schema, "archive", &[]);
    archive.children = Some(vec![DbObject::new(
        ObjectKind::Table,
        "secret_unexpanded",
        &archive.path,
    )]);
    let unloaded = DbObject::new(ObjectKind::Schema, "unloaded", &[]);
    serde_json::json!({
        "active": 0, "tabs": [saved_table(LAST_TABLE), scratch_tab()],
        "trees": {(SOURCE): [main, archive, unloaded]},
        "expanded": [[SOURCE, []], [SOURCE, ["main"]], [SOURCE, folder_path]],
        "selected": [SOURCE, []]
    })
}

fn verify_cached_navigation(h: &mut H<'_>, context: &egui::Context, untouched_database: &Path) {
    // Rendering remains bounded by the viewport rather than the 1,003 table objects.
    let rendered_records = h
        .get_all_by_role(Role::Button)
        .filter(|node| {
            node.accesskit_node()
                .label()
                .is_some_and(|name| name.starts_with("record_"))
        })
        .count();
    assert!(
        (1..80).contains(&rendered_records),
        "rendered {rendered_records} cached rows"
    );
    assert!(
        h.query_by_role_and_label(Role::Button, LAST_TABLE)
            .is_none()
    );

    click_row(h, SOURCE_LABEL);
    key(h, Modifiers::NONE, Key::ArrowRight);
    assert_selected(h, "main");
    key(h, Modifiers::NONE, Key::ArrowRight);
    assert_selected(h, "tables");
    key(h, Modifiers::NONE, Key::ArrowLeft);
    assert_selected(h, "tables");
    assert!(h.query_by_label("film_actor").is_none());
    key(h, Modifiers::NONE, Key::ArrowRight);
    key(h, Modifiers::NONE, Key::ArrowDown);
    assert_selected(h, "film_actor");

    // Typing with tree focus opens Speed Search without changing the SQL editor.
    h.event(egui::Event::Text("fA".into()));
    h.run_steps(3);
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Speed Search")
            .is_focused()
    );
    assert_selected(h, "film_actor");
    row(h, "film_archive");
    assert!(h.query_by_label("record_0000").is_none());
    key(h, Modifiers::NONE, Key::ArrowUp);
    assert!(
        selected(h, "film_archive"),
        "previous match wraps; {}",
        tree_state(h)
    );
    key(h, Modifiers::NONE, Key::ArrowDown);
    assert!(
        selected(h, "film_actor"),
        "next match wraps; {}",
        tree_state(h)
    );
    shot(h, "49-explorer-speed-search.png");

    // Turning filtering off keeps navigation/highlights and restores unrelated rows.
    menu_action(h, "Speed Search Filter");
    key(h, Modifiers::NONE, Key::Escape); // close the menu, not the search
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Speed Search")
            .value()
            .as_deref(),
        Some("fA")
    );
    row(h, "record_0000");
    menu_action(h, "Speed Search Filter");
    key(h, Modifiers::NONE, Key::Escape);
    assert!(h.query_by_label("record_0000").is_none());

    query(h, "łÓDŹ");
    assert_selected(h, "Łódź");
    query(h, "secret_unexpanded");
    h.get_by_label("No matches in the expanded tree");
    assert!(h.query_by_label("secret_unexpanded").is_none());
    query(h, "nothing-9382");
    h.get_by_label("No matches in the expanded tree");
    assert!(
        !untouched_database.exists(),
        "Speed Search must not connect or load hidden descendants"
    );
    key(h, Modifiers::NONE, Key::Escape);
    assert_eq!(
        context.memory(|memory| memory.focused()),
        Some(Id::new("database_explorer_tree"))
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Speed Search")
            .value()
            .as_deref(),
        Some("")
    );

    key(h, Modifiers::NONE, Key::End);
    assert_selected(h, "unloaded");
    assert!(
        h.query_by_role_and_label(Role::Button, "film_actor")
            .is_none()
    );
    key(h, Modifiers::NONE, Key::ArrowDown);
    assert!(
        selected(h, "unloaded"),
        "ordinary tree navigation clamps at the end; {}",
        tree_state(h)
    );
    key(h, Modifiers::NONE, Key::Home);
    assert_selected(h, SOURCE_LABEL);
    key(h, Modifiers::NONE, Key::ArrowUp);
    assert!(
        selected(h, SOURCE_LABEL),
        "ordinary tree navigation clamps at the start; {}",
        tree_state(h)
    );
    key(h, Modifiers::NONE, Key::PageDown);
    assert!(
        !selected(h, SOURCE_LABEL),
        "PageDown must move selection; {}",
        tree_state(h)
    );
    key(h, Modifiers::NONE, Key::Home);
    assert_selected(h, SOURCE_LABEL);

    // An explicit reveal walks cached ancestors and scrolls to the virtualized last table.
    menu_action(h, "Collapse All");
    row(h, SOURCE_LABEL);
    assert!(h.query_by_label("main").is_none());
    menu_action(h, "Scroll from Editor");
    wait_until(h, "cached editor target", |h| selected(h, LAST_TABLE));
    assert!(
        context
            .content_rect()
            .contains(row(h, LAST_TABLE).rect().center())
    );
    assert!(
        !untouched_database.exists(),
        "revealing already fetched metadata needs no connection"
    );
    shot(h, "50-explorer-revealed-table.png");

    // Enter reuses the existing document rather than opening a second table tab.
    key(h, Modifiers::NONE, Key::Enter);
    assert_eq!(h.get_all_by_label("⊞ record_0999").count(), 1);
    h.get_by_label("cached editor contents");
}

fn lazy_session() -> serde_json::Value {
    serde_json::json!({
        "active": 0, "tabs": [saved_table("target"), scratch_tab()],
        "expanded": [], "trees": {}, "selected": [SOURCE, []]
    })
}

fn verify_explorer_actions(h: &mut H<'_>, context: &egui::Context, untouched_database: &Path) {
    click(h, "QL Scratch");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    let editor_focus = context.memory(|memory| memory.focused());
    assert!(editor_focus.is_some());
    h.get_by_label("Database Explorer Tree").focus();
    h.run_steps(1);

    // Find Action captures the focused tool window even when SQL is the active tab.
    palette_action(h, "Find");
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Speed Search")
            .is_focused(),
        "{}",
        tree_state(h)
    );
    assert!(h.query_by_label("Close Find").is_none());
    assert!(h.query_by_label("Close Grid Find").is_none());
    query(h, "fA");
    key(h, Modifiers::NONE, Key::Escape);
    assert!(
        h.get_by_label("Database Explorer Tree").is_focused(),
        "{}",
        tree_state(h)
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Speed Search")
            .value()
            .as_deref(),
        Some("")
    );
    key(h, Modifiers::NONE, Key::Escape);
    assert_eq!(
        context.memory(|memory| memory.focused()),
        editor_focus,
        "empty Escape returns to the active editor; {}",
        tree_state(h)
    );
    assert!(h.get_by_role(Role::MultilineTextInput).is_focused());
    assert_eq!(
        h.get_by_role(Role::MultilineTextInput).value().as_deref(),
        Some(SCRATCH_SQL)
    );

    // Hide Active Tool Window keeps the document and returns its input focus.
    h.get_by_label("Database Explorer Tree").focus();
    h.run_steps(1);
    key(h, Modifiers::SHIFT, Key::Escape);
    assert!(
        h.query_by_role_and_label(Role::Label, "Database Explorer")
            .is_none()
    );
    assert!(h.query_by_label("Database Explorer Tree").is_none());
    assert_eq!(
        context.memory(|memory| memory.focused()),
        editor_focus,
        "Shift+Escape returns to the active editor; {}",
        tree_state(h)
    );
    key(h, Modifiers::COMMAND, Key::Num1);
    h.get_by_role_and_label(Role::Label, "Database Explorer");

    palette_action(h, "Collapse All");
    row(h, SOURCE_LABEL);
    assert!(h.query_by_label("main").is_none());
    click(h, "⊞ record_0999");
    palette_action(h, "Scroll from Editor");
    wait_until(h, "palette reveal of the active editor", |h| {
        selected(h, LAST_TABLE)
    });
    assert!(
        h.get_by_label("Database Explorer Tree").is_focused(),
        "{}",
        tree_state(h)
    );
    assert!(
        !untouched_database.exists(),
        "palette actions over cached metadata must not connect"
    );
}

fn blocked_schema_session() -> serde_json::Value {
    // Restoring just the schema avoids depending on whether SQLite can enumerate
    // database_list while the exclusive lock is held. Its children are unloaded.
    let main = DbObject::new(ObjectKind::Schema, "main", &[]);
    serde_json::json!({
        "active": 0, "tabs": [saved_table("target"), scratch_tab()],
        "expanded": [[SOURCE, []], [SOURCE, ["main"]]],
        "trees": {(SOURCE): [main]}, "selected": [SOURCE, []]
    })
}

fn verify_lazy_reveal(h: &mut H<'_>) {
    menu_action(h, "Scroll from Editor");
    wait_until(h, "lazy schema and table metadata", |h| {
        selected(h, "target")
    });
    row(h, "main");
    row(h, "tables");
    row(h, "target");
    assert!(h.get_by_label("Database Explorer Tree").is_focused());
    shot(h, "51-explorer-lazy-reveal.png");
}

fn verify_cancelled_reveal(
    h: &mut H<'_>,
    context: &egui::Context,
    lock: rusqlite::Transaction<'_>,
) {
    // BEGIN EXCLUSIVE holds schema reads until the active editor has changed.
    // The restored schema is already expanded, so the explicit reveal issues its
    // single Load(main) step before any editor switch can cancel the request.
    row(h, "main");
    menu_action(h, "Scroll from Editor");
    wait_until(h, "source roots before blocked schema load", |h| {
        h.query_by_role_and_label(Role::Button, "main").is_some()
    });
    assert!(h.query_by_role_and_label(Role::Button, "target").is_none());
    click(h, "QL Scratch");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    let editor_focus = context.memory(|memory| memory.focused());
    assert!(editor_focus.is_some());
    lock.rollback().unwrap();

    // A late Children event is allowed to populate `tables`, but must not finish
    // the old reveal, expand that folder, select target, or steal editor focus.
    wait_until(h, "late schema metadata after cancellation", |h| {
        h.query_by_label("tables").is_some()
    });
    h.run_steps(10);
    assert!(
        !selected(h, "target"),
        "late reveal must not select target; {}",
        tree_state(h)
    );
    assert!(h.query_by_role_and_label(Role::Button, "target").is_none());
    assert_eq!(context.memory(|memory| memory.focused()), editor_focus);
    assert_eq!(
        h.get_by_role(Role::MultilineTextInput).value().as_deref(),
        Some(SCRATCH_SQL)
    );
}

#[test]
fn explorer_search_navigation_and_lazy_reveal_preserve_context() {
    let directory = std::env::temp_dir().join(format!("dbeast_explorer_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let config_path = directory.join("config.json");
    let untouched_database = directory.join("must-not-connect.sqlite");
    write_fixture(&directory, source(&untouched_database), cached_session());
    // SAFETY: one test in this binary, before starting the first application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let (mut h, context) = app();
    verify_cached_navigation(&mut h, &context, &untouched_database);
    verify_explorer_actions(&mut h, &context, &untouched_database);
    drop(h);

    let database = directory.join("live.sqlite");
    let mut db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch("CREATE TABLE target (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO target VALUES(1, 'live row');").unwrap();
    write_fixture(&directory, source(&database), lazy_session());
    let (mut h, _) = app();
    verify_lazy_reveal(&mut h);
    drop(h);

    // Recreate an unloaded schema, without mutating the process environment.
    write_fixture(&directory, source(&database), blocked_schema_session());
    // Transaction::drop rolls back if application setup or an assertion fails.
    let lock = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
        .unwrap();
    let (mut h, context) = app();
    verify_cancelled_reveal(&mut h, &context, lock);
    drop(h);
    drop(db);
    std::fs::remove_dir_all(directory).unwrap();
}
