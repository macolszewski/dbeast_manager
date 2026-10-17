//! Per-console schema selection, transaction guards and offline restoration.

mod common;

use dbeast::config::Config;
use dbeast::model::{DataSource, DbObject, DriverKind, ObjectKind, SchemaSelection};
use egui::accesskit::{Role, Toggled};
use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;
const SQL: &str = "SELECT 'Unicode Żółw' AS note;";
const PG_ID: &str = "schema-pg";
const PG_NAME: &str = "PostgreSQL schema fixture";

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

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        if Instant::now() >= deadline {
            let caption = h
                .query_by_role_and_label(Role::ComboBox, "Default schema")
                .and_then(|node| node.value());
            let labels: Vec<_> = h
                .query_all_by_label_contains("")
                .map(|node| {
                    (
                        node.accesskit_node().role(),
                        node.accesskit_node().label(),
                        node.value(),
                    )
                })
                .collect();
            shot(h, "schema-timeout.png");
            if let Some(output) = h.query_by_label("Output") {
                output.click();
                h.run_steps(3);
            }
            let output: Vec<_> = h
                .query_all_by_label_contains("")
                .filter_map(|node| node.accesskit_node().label())
                .collect();
            shot(h, "schema-timeout-output.png");
            panic!(
                "timed out waiting for {what}; schema caption: {caption:?}; \
                 visible nodes: {labels:#?}; output nodes: {output:#?}"
            );
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn schema_caption(h: &H<'_>) -> String {
    h.get_by_role_and_label(Role::ComboBox, "Default schema")
        .value()
        .expect("schema selector value")
}

fn editor(h: &H<'_>) -> String {
    h.get_by_role(Role::MultilineTextInput).value().unwrap()
}

fn set_sql(h: &mut H<'_>, sql: &str) {
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_by_role(Role::MultilineTextInput).type_text(sql);
    h.run_steps(3);
    assert_eq!(editor(h), sql);
}

fn same_source(h: &mut H<'_>) {
    h.get_by_role_and_label(Role::ComboBox, "Data source")
        .click();
    h.run_steps(3);
    h.query_all_by_role_and_label(Role::Button, PG_NAME)
        .find(|node| node.accesskit_node().toggled() == Some(Toggled::True))
        .expect("currently selected source in the popup")
        .click();
    h.run_steps(3);
}

fn choose_schema(h: &mut H<'_>, name: &str) {
    h.get_by_role_and_label(Role::ComboBox, "Default schema")
        .click();
    h.run_steps(3);
    wait_until(h, &format!("schema choices for {name:?}"), |h| {
        h.query_by_label(&format!("Schema: {name}")).is_some()
    });
    click(h, &format!("Schema: {name}"));
    wait_until(h, &format!("confirmed schema choice {name:?}"), |h| {
        schema_caption(h) == name && h.query_by_label("Changing schema…").is_none()
    });
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

fn pg(port: u16) -> DataSource {
    let mut source = DataSource::new(DriverKind::Postgres);
    source.id = PG_ID.into();
    source.name = PG_NAME.into();
    source.host = "127.0.0.1".into();
    source.port = port;
    source.database = "dbeast".into();
    source.user = "dbeast".into();
    source.password = "dbeast".into();
    source
}

fn console(
    title: &str,
    database: Option<&str>,
    schema: Option<SchemaSelection>,
) -> serde_json::Value {
    let mut tab = serde_json::json!({
        "title": title, "ds_id": PG_ID, "database": database,
        "kind": {"Console": {"sql": SQL}}, "results": []
    });
    if let Some(schema) = schema {
        tab["schema"] = serde_json::to_value(schema).unwrap();
    }
    tab
}

fn write_fixture(directory: &Path, sources: Vec<DataSource>, tabs: Vec<serde_json::Value>) {
    let config = Config {
        data_sources: sources,
        max_rows: 1,
        tile_url: String::new(),
        files_root: Some(directory.to_owned()),
        ..Config::default()
    };
    let roots = [
        DbObject::new(ObjectKind::Database, "dbeast", &[])
            .with_detail(dbeast::drivers::DEFAULT_DATABASE),
        DbObject::new(ObjectKind::Database, "dbeast_archive", &[]),
    ];
    std::fs::write(
        directory.join("config.json"),
        common::legacy_config(&config).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.join("session.json"),
        serde_json::json!({
            "active": 0, "tabs": tabs, "trees": {(PG_ID): roots}, "expanded": []
        })
        .to_string(),
    )
    .unwrap();
}

fn verify_offline(
    h: &mut H<'_>,
    context: &egui::Context,
    listener: &std::net::TcpListener,
    sqlite: &Path,
) {
    assert_eq!(schema_caption(h), "archive");
    assert_eq!(
        h.get_by_role_and_label(Role::ComboBox, "Database")
            .value()
            .as_deref(),
        Some("dbeast_archive")
    );
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    let editor_id = context.memory(|memory| memory.focused()).unwrap();
    let mut state = egui::text_edit::TextEditState::load(context, editor_id).unwrap();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(9))));
    state.store(context, editor_id);
    same_source(h);
    assert_eq!(
        schema_caption(h),
        "archive",
        "choosing the same source preserves schema"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::ComboBox, "Database")
            .value()
            .as_deref(),
        Some("dbeast_archive")
    );
    assert_eq!(editor(h), SQL);
    assert_eq!(
        egui::text_edit::TextEditState::load(context, editor_id)
            .unwrap()
            .cursor
            .char_range()
            .unwrap()
            .primary
            .index
            .0,
        9
    );
    assert!(h.query_by_label("Changing schema…").is_none());
    click(h, "QL Schema B");
    assert_eq!(schema_caption(h), "shop");
    click(h, "QL Legacy");
    assert_eq!(
        schema_caption(h),
        "Default",
        "old JSON without schema preserves server defaults"
    );
    click(h, "QL SQLite");
    assert_eq!(schema_caption(h), "main");
    assert!(
        h.get_by_role_and_label(Role::ComboBox, "Default schema")
            .accesskit_node()
            .is_disabled()
    );
    assert!(!sqlite.exists(), "restoring SQLite must not open its file");
    click(h, "QL Schema A");
    assert_eq!(schema_caption(h), "archive");
    assert_eq!(editor(h), SQL);
    h.run_steps(10);
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "restoring or reselecting the same source must not connect"
    );
    shot(h, "52-console-schema-restored.png");
}

fn saved_schema(directory: &Path, file: &str, title: &str) -> Option<SchemaSelection> {
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(directory.join(file)).ok()?).ok()?;
    let tabs = saved[if file == "config.json" {
        "consoles"
    } else {
        "tabs"
    }]
    .as_array()?;
    serde_json::from_value(tabs.iter().find(|tab| tab["title"] == title)?["schema"].clone()).ok()
}

fn verify_live(h: &mut H<'_>, directory: &Path) {
    choose_schema(h, "shop");
    assert_eq!(editor(h), SQL, "schema changes never rewrite SQL");
    same_source(h);
    assert_eq!(schema_caption(h), "shop");
    const QUERY_A: &str = "SELECT current_schema() AS active_schema, 'schema_ui_a' AS marker, i FROM generate_series(1, 3) AS i";
    set_sql(h, QUERY_A);
    key(h, Modifiers::COMMAND, Key::Enter);
    wait_until(h, "first console result", |h| {
        h.query_by_label("schema_ui_a").is_some()
    });
    h.get_by_label("shop");

    // Page and Reload must not silently switch the session back to this result.
    choose_schema(h, "public");
    for label in ["▶", "⟳"] {
        let control = h
            .query_all_by_role_and_label(Role::Button, label)
            .max_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
            .expect("result paging/reload control below the editor toolbar");
        control.click();
        h.run_steps(3);
        assert_eq!(schema_caption(h), "public");
        assert_eq!(editor(h), QUERY_A);
        h.get_by_label("shop");
    }
    click(h, "Output");
    assert!(h.query_all_by_label_contains("This result belongs to another database or schema. Switch back to its context or execute the query again before reloading or fetching another page.").filter(|node| node.value().is_some_and(|value| value.starts_with('['))).count() >= 2, "both Page and Reload explain the context mismatch");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::Enter);
    wait_until(h, "explicit execution in the new context", |h| {
        h.query_by_label("schema_ui_a").is_some()
    });
    h.get_by_label("public");
    choose_schema(h, "shop");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::Enter);
    wait_until(h, "first console restored to shop", |h| {
        h.query_by_label("shop").is_some()
    });
    click(h, "QL Live B");
    choose_schema(h, "public");
    set_sql(
        h,
        "SELECT current_schema() AS active_schema, 'schema_ui_b' AS marker",
    );
    key(h, Modifiers::COMMAND, Key::Enter);
    wait_until(h, "second console result", |h| {
        h.query_by_label("schema_ui_b").is_some()
    });
    h.get_by_label("public");
    shot(h, "53-console-schema-selection.png");
    click(h, "QL Live A");
    assert_eq!(schema_caption(h), "shop");
    h.get_by_label("schema_ui_a");

    set_sql(h, "BEGIN");
    key(h, Modifiers::COMMAND, Key::Enter);
    wait_until(h, "open transaction", |h| {
        !h.get_by_label("Rollback").accesskit_node().is_disabled()
    });
    assert!(
        h.get_by_role_and_label(Role::ComboBox, "Default schema")
            .accesskit_node()
            .is_disabled()
    );
    set_sql(h, "SELECT 1 / 0");
    key(h, Modifiers::COMMAND, Key::Enter);
    wait_until(h, "aborted transaction", |h| {
        h.query_all_by_label_contains("division by zero")
            .next()
            .is_some()
    });
    assert!(
        h.get_by_role_and_label(Role::ComboBox, "Default schema")
            .accesskit_node()
            .is_disabled()
    );
    assert!(!h.get_by_label("Rollback").accesskit_node().is_disabled());
    shot(h, "54-console-schema-transaction.png");
    click(h, "Rollback");
    wait_until(h, "rollback restores schema selection", |h| {
        !h.get_by_role_and_label(Role::ComboBox, "Default schema")
            .accesskit_node()
            .is_disabled()
    });
    assert_eq!(schema_caption(h), "shop");
    for file in ["config.json", "session.json"] {
        wait_until(h, "confirmed schema persistence", |_| {
            saved_schema(directory, file, "Live A") == Some(SchemaSelection::Named("shop".into()))
                && saved_schema(directory, file, "Live B")
                    == Some(SchemaSelection::Named("public".into()))
        });
    }
}

#[test]
fn console_schema_selection_and_session_guards() {
    let directory = std::env::temp_dir().join(format!("dbeast_schema_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let offline_port = listener.local_addr().unwrap().port();
    let sqlite_path = directory.join("not-opened.sqlite");
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.id = "schema-sqlite".into();
    sqlite.file = sqlite_path.to_string_lossy().into_owned();
    write_fixture(
        &directory,
        vec![pg(offline_port), sqlite],
        vec![
            console(
                "Schema A",
                Some("dbeast_archive"),
                Some(SchemaSelection::Named("archive".into())),
            ),
            console(
                "Schema B",
                None,
                Some(SchemaSelection::Named("shop".into())),
            ),
            console("Legacy", None, None),
            serde_json::json!({"title": "SQLite", "ds_id": "schema-sqlite", "kind": {"Console": {"sql": SQL}}}),
        ],
    );
    // SAFETY: one test in this binary, before starting any application.
    unsafe { std::env::set_var("DBEAST_CONFIG", directory.join("config.json")) };
    let (mut h, context) = app();
    verify_offline(&mut h, &context, &listener, &sqlite_path);
    drop(h);

    if std::net::TcpStream::connect("127.0.0.1:55432").is_ok() {
        write_fixture(
            &directory,
            vec![pg(55432)],
            vec![console("Live A", None, None), console("Live B", None, None)],
        );
        let (mut h, _) = app();
        verify_live(&mut h, &directory);
        drop(h);
        let config_path = directory.join("config.json");
        let mut config: Config =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        config.data_sources[0].port = offline_port;
        std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
        let (mut h, _) = app();
        click(&mut h, "QL Live A");
        assert_eq!(schema_caption(&h), "shop");
        click(&mut h, "QL Live B");
        assert_eq!(schema_caption(&h), "public");
        assert!(
            listener
                .accept()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
            "restoring confirmed contexts must not connect"
        );
        drop(h);
    } else {
        eprintln!("skipping live schema UI: PostgreSQL fixture is unavailable");
    }
    std::fs::remove_dir_all(directory).unwrap();
}
