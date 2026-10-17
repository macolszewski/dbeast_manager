//! Explain Analyse executes one PostgreSQL statement and shows actual statistics.

mod common;

use dbeast::config::Config;
use dbeast::drivers::Connection;
use dbeast::model::{DataSource, DriverKind, Page, Value};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;
const PG_ID: &str = "analyse-pg";

fn app() -> H<'static> {
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
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

fn label_text(node: &egui_kittest::Node<'_>) -> Option<String> {
    node.value().or_else(|| node.accesskit_node().label())
}

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(h) {
        if Instant::now() >= deadline {
            let labels: Vec<_> = h
                .query_all_by_role(Role::Label)
                .filter_map(|node| label_text(&node))
                .collect();
            shot(h, "explain-analyse-timeout.png");
            panic!("timed out waiting for {what}; labels: {labels:#?}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn editor<'h>(h: &'h H<'_>) -> egui_kittest::Node<'h> {
    h.get_all_by_role(Role::MultilineTextInput)
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("SQL editor above Services")
}

fn set_sql(h: &mut H<'_>, text: &str) {
    editor(h).focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::A);
    editor(h).type_text(text);
    h.run_steps(3);
}

fn run_action(h: &mut H<'_>, name: &str) {
    key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .type_text(name);
    h.run_steps(3);
    let action = h
        .get_all_by_label(name)
        .find(|node| node.accesskit_node().toggled().is_some())
        .expect("selectable action in Find Action");
    assert!(!action.accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Enter);
}

fn has_label(h: &H<'_>, text: &str) -> bool {
    h.query_all_by_role(Role::Label)
        .any(|node| label_text(&node).is_some_and(|label| label.contains(text)))
}

fn log_count(h: &mut H<'_>, text: &str) -> usize {
    click(h, "Output");
    let count = h
        .query_all_by_role(Role::Label)
        .filter(|node| label_text(node).is_some_and(|label| label.contains(text)))
        .count();
    click(h, "Query Plan");
    count
}

fn copy_plan(h: &mut H<'_>) -> String {
    h.get_by_label("Copy Original Query Plan").click();
    let mut copied = None;
    for _ in 0..3 {
        h.run_steps(1);
        for command in &h.output().platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                copied = Some(text.clone());
            }
        }
    }
    copied.expect("complete native plan in clipboard output")
}

fn inside(h: &H<'_>, label: &str, size: egui::Vec2) {
    let rect = h.get_by_label(label).rect();
    assert!(
        rect.left() >= 0.0
            && rect.top() >= 0.0
            && rect.right() <= size.x
            && rect.bottom() <= size.y,
        "{label} must remain inside the window: {rect:?}"
    );
}

#[test]
fn explain_analyse_executes_once_and_shows_actual_statistics() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping Explain Analyse UI: PostgreSQL fixture is unavailable");
        return;
    }
    let mut source = DataSource::new(DriverKind::Postgres);
    source.id = PG_ID.into();
    source.name = "Analyse".into();
    source.host = "127.0.0.1".into();
    source.port = 55432;
    source.database = "dbeast".into();
    source.user = "dbeast".into();
    source.password = "dbeast".into();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
    let table = format!("public.analyse_ui_{suffix}");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut observer = runtime.block_on(Connection::open(&source)).unwrap();
    let mut observe = |sql: &str| runtime.block_on(observer.execute(sql, Page::ALL)).unwrap();
    observe(&format!(
        "CREATE TABLE {table}(id INTEGER PRIMARY KEY, name TEXT NOT NULL, n INTEGER NOT NULL)"
    ));
    observe(&format!(
        "INSERT INTO {table} SELECT i, 'row ' || i, 0 FROM generate_series(1, 200) i"
    ));
    observe(&format!("ANALYZE {table}"));
    let changed = format!("SELECT count(*) FILTER (WHERE n = 1), max(n) FROM {table}");

    let directory = std::env::temp_dir().join(format!("dbeast_analyse_ui_{suffix}"));
    std::fs::create_dir_all(&directory).unwrap();
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
    let select = format!("SELECT id, name, n FROM {table} ORDER BY id");
    std::fs::write(
        directory.join("session.json"),
        serde_json::json!({"active": 0, "tabs": [{
            "title": "Analyse", "ds_id": PG_ID, "kind": {"Console": {"sql": select}},
            "results": []
        }]})
        .to_string(),
    )
    .unwrap();
    // SAFETY: one test in this binary, before starting any application.
    unsafe { std::env::set_var("DBEAST_CONFIG", directory.join("config.json")) };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut h = app();
        editor(&h).focus();
        h.run_steps(1);
        key(&mut h, Modifiers::COMMAND, Key::Enter);
        wait_until(&mut h, "ordinary result", |h| {
            h.query_by_label("row 1").is_some()
        });

        let update = format!(
            "UPDATE {table} SET n = n + 1 WHERE id IN (SELECT id FROM {table} WHERE id % 50 = 0)"
        );
        set_sql(&mut h, &update);
        // The toolbar keeps Explain Plan as its default; Analyse is in the menu.
        click(&mut h, "Explain Actions");
        assert!(h.query_by_label("Explain Analyse").is_some());
        click(&mut h, "Explain Analyse");
        wait_until(&mut h, "actual execution plan", |h| {
            has_label(h, "Actual execution")
        });
        let mut observe = |sql: &str| runtime.block_on(observer.execute(sql, Page::ALL)).unwrap();
        assert_eq!(
            observe(&changed).rows[0],
            [Value::Int(4), Value::Int(1)],
            "Auto executes the statement exactly once"
        );
        for header in ["Actual Rows", "Loops", "Actual Time (ms)"] {
            assert!(h.query_all_by_label(header).next().is_some(), "{header}");
        }
        assert!(has_label(&h, "Execution Time: "));
        assert!(
            h.query_by_role_and_label(Role::Button, "Explain Plan")
                .is_some()
        );
        assert_eq!(log_count(&mut h, "executed and analysed in"), 1);
        assert_eq!(log_count(&mut h, "Explain Analyse> UPDATE"), 1);
        shot(&mut h, "72-explain-analyse.png");

        let raw = copy_plan(&mut h);
        assert!(raw.contains("\"Actual Total Time\"") && raw.contains("\"Execution Time\""));
        click(&mut h, "Show Diagram");
        assert!(has_label(&h, "Actual execution hierarchy"));
        shot(&mut h, "73-explain-analyse-diagram.png");
        click(&mut h, "Operations Tree");

        let small = egui::vec2(1100.0, 500.0);
        h.set_size(small);
        h.run_steps(4);
        for label in [
            "Query Plan",
            "Operations Tree",
            "Raw",
            "Copy Original Query Plan",
        ] {
            inside(&h, label, small);
        }
        assert!(h.query_all_by_label("Actual Time (ms)").next().is_some());
        shot(&mut h, "74-explain-analyse-narrow.png");
        h.set_size(egui::vec2(1440.0, 900.0));
        h.run_steps(4);

        // Manual keeps the change in an open transaction until Rollback.
        h.get_all_by_role(Role::ComboBox)
            .find(|node| node.value().as_deref() == Some("Tx: Auto"))
            .expect("transaction mode")
            .click();
        h.run_steps(2);
        click(&mut h, "Manual");
        run_action(&mut h, "Explain Analyse");
        wait_until(&mut h, "open transaction after Analyse", |h| {
            h.query_by_label("✔ Commit").is_some()
        });
        assert_eq!(log_count(&mut h, "executed and analysed in"), 2);
        assert_eq!(observe(&changed).rows[0], [Value::Int(4), Value::Int(1)]);
        click(&mut h, "Rollback");
        wait_until(&mut h, "transaction rolled back", |h| {
            h.query_by_label("✔ Commit").is_none()
        });
        assert_eq!(observe(&changed).rows[0], [Value::Int(4), Value::Int(1)]);

        // Explain Plan still only estimates.
        run_action(&mut h, "Explain Plan");
        wait_until(&mut h, "estimated plan", |h| has_label(h, "Estimated plan"));
        assert_eq!(observe(&changed).rows[0], [Value::Int(4), Value::Int(1)]);
        // The earlier grid is a snapshot: not re-executed, still in the console.
        let result_tab = h
            .query_all_by_label_contains("⊞ ")
            .find_map(|node| node.accesskit_node().label())
            .expect("ordinary result tab");
        click(&mut h, &result_tab);
        assert!(h.query_by_label("row 1").is_some());
        drop(h);
    }));
    let mut observe = |sql: &str| runtime.block_on(observer.execute(sql, Page::ALL));
    let _ = observe(&format!("DROP TABLE IF EXISTS {table}"));
    let _ = std::fs::remove_dir_all(&directory);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
