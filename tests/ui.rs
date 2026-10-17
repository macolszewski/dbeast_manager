//! Drives the real UI headlessly against the Docker PostgreSQL and saves
//! screenshots to docs/screenshots.

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::time::{Duration, Instant};

const CONSOLE_SQL: &str = "select c.email, c.name, count(o.id) as orders, sum(o.total) as total\nfrom shop.customers c\njoin shop.orders o on o.customer_id = c.id\ngroup by c.email, c.name\norder by total desc;\n";

fn wait_until(
    harness: &mut Harness<'_, dbeast::ui::App>,
    what: &str,
    done: impl Fn(&Harness<'_, dbeast::ui::App>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(harness) {
        if Instant::now() > deadline {
            shot(harness, "timeout");
            panic!("timed out waiting for {what}");
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    harness.run_steps(3);
}

/// The harness does not advance `RawInput::time` by itself, and egui counts
/// clicks at identical timestamps as triple clicks, so time is set explicitly.
fn double_click(harness: &mut Harness<'_, dbeast::ui::App>, label: &str) {
    static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(10);
    let t = CLOCK.fetch_add(10, std::sync::atomic::Ordering::Relaxed) as f64;
    harness.input_mut().time = Some(t);
    tree_row(harness, label).click();
    harness.step();
    harness.input_mut().time = Some(t + 0.1);
    tree_row(harness, label).click();
    harness.step();
    harness.input_mut().time = Some(t + 5.0);
    harness.run_steps(2);
}

/// A clickable row (tree node, grid cell) with this label; plain labels with
/// the same text, e.g. in the status bar breadcrumbs or grid headers, are skipped.
fn tree_row<'h>(
    harness: &'h Harness<'_, dbeast::ui::App>,
    label: &'h str,
) -> egui_kittest::Node<'h> {
    // Grid cells are plain labels inside a clickable cell, so fall back to the first match.
    harness
        .get_all_by_label(label)
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
        .unwrap_or_else(|| {
            harness
                .get_all_by_label(label)
                .next()
                .unwrap_or_else(|| panic!("no {label}"))
        })
}

/// Clicks the expand arrow of a tree row at the given depth.
fn click_arrow(harness: &mut Harness<'_, dbeast::ui::App>, label: &str, depth: usize) {
    let rect = tree_row(harness, label).rect();
    let pos = egui::pos2(rect.left() + depth as f32 * 16.0 + 9.0, rect.center().y);
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    harness.run_steps(2);
}

/// Presses at `from`, moves to `to` over a few frames and releases.
fn drag(harness: &mut Harness<'_, dbeast::ui::App>, from: egui::Pos2, to: egui::Pos2) {
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    harness.event(egui::Event::PointerMoved(from));
    harness.run_steps(1);
    harness.event(button(from, true));
    harness.run_steps(1);
    for i in 1..=5 {
        harness.event(egui::Event::PointerMoved(
            from + (to - from) * (i as f32 / 5.0),
        ));
        harness.run_steps(1);
    }
    harness.event(button(to, false));
    harness.run_steps(2);
}

fn shot(harness: &mut Harness<'_, dbeast::ui::App>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    harness
        .render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

#[test]
fn explorer_console_and_table_editor() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: PostgreSQL test container is not running");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dbeast_ui_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = serde_json::json!({
        "data_sources": [{"id": "pg", "name": "", "driver": "Postgres", "host": "localhost", "port": 55432,
            "user": "dbeast", "password": "dbeast", "database": "dbeast", "file": "", "use_tls": false, "read_only": false}],
        "consoles": [{"title": "console", "ds_id": "pg", "sql": CONSOLE_SQL}],
        "max_rows": 500
    });
    let path = dir.join("config.json");
    std::fs::write(&path, config.to_string()).unwrap();
    // SAFETY: this is the only test in this binary, nothing reads the environment concurrently.
    unsafe { std::env::set_var("DBEAST_CONFIG", &path) };

    let mut harness = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    harness.run_steps(3);

    // Expanding the data source connects and shows the schemas.
    double_click(&mut harness, "dbeast@localhost");
    wait_until(&mut harness, "schemas", |h| {
        h.query_by_label("shop").is_some()
    });
    double_click(&mut harness, "shop");
    wait_until(&mut harness, "schema folders", |h| {
        h.query_by_label("tables").is_some()
    });
    double_click(&mut harness, "tables");
    harness.run_steps(2);
    click_arrow(&mut harness, "customers", 4);
    wait_until(&mut harness, "columns", |h| {
        h.query_by_label("columns").is_some()
    });
    click_arrow(&mut harness, "columns", 5);
    wait_until(&mut harness, "column list", |h| {
        h.query_by_label("email").is_some()
    });

    // Cmd+Enter in the console runs the statement under the caret.
    harness.get_by_label("QL console").click();
    harness.run_steps(2);
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    harness.run_steps(1);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    wait_until(&mut harness, "query result", |h| {
        h.query_by_label("⊞ customers").is_some()
    });
    shot(&mut harness, "01-console");

    // Double click on a table opens the table editor with WHERE / ORDER BY filters.
    double_click(&mut harness, "orders");
    wait_until(&mut harness, "table data", |h| {
        h.query_by_label("1-500 of 500+").is_some()
    });
    shot(&mut harness, "02-table-editor");

    // A failing statement reports the SQLSTATE in the output log.
    harness.get_by_label("QL console").click();
    harness.run_steps(2);
    let editor = harness.get_by_role(egui::accesskit::Role::MultilineTextInput);
    editor.focus();
    harness.key_press_modifiers(Modifiers::COMMAND, Key::A);
    harness.run_steps(1);
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text("select * from shop.missing");
    harness.run_steps(2);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    wait_until(&mut harness, "error", |h| {
        h.query_by_label_contains("[42P01]").is_some()
    });
    shot(&mut harness, "03-error");

    // The statement under the caret is outlined; ⌘↵ runs exactly that one.
    set_editor_text(
        &mut harness,
        "select 1;\nselect *\nfrom shop.orders\nwhere id < 3\norder by id;\n\nselect 3;",
    );
    harness.key_press(Key::ArrowUp);
    harness.key_press(Key::ArrowUp);
    harness.key_press(Key::ArrowUp);
    harness.run_steps(2);
    shot(&mut harness, "15-current-statement");
    harness.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    wait_until(&mut harness, "middle statement result", |h| {
        h.query_by_label("⊞ orders").is_some()
    });
    assert!(
        harness.query_by_label("1-2 of 2").is_some(),
        "ran the outlined statement"
    );

    // Completion after FROM lists tables; Enter inserts the selected one.
    set_editor_text(&mut harness, "select * from shop.ord");
    wait_until(&mut harness, "table completion", |h| {
        h.get_all_by_label("orders").count() >= 2
    });
    shot(&mut harness, "04-completion");
    harness.key_press(Key::Enter);
    harness.run_steps(2);
    assert_eq!(editor_text(&harness), "select * from shop.orders");
    // After an alias and a dot, the columns of that table.
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text(" o where o.tot");
    wait_until(&mut harness, "column completion", |h| {
        h.query_by_label("total").is_some()
    });
    harness.key_press(Key::Tab);
    harness.run_steps(2);
    assert_eq!(
        editor_text(&harness),
        "select * from shop.orders o where o.total"
    );

    // Manual transaction mode keeps changes pending until Commit or Rollback.
    harness
        .get_all_by_role(egui::accesskit::Role::ComboBox)
        .find(|n| n.value().as_deref() == Some("Tx: Auto"))
        .unwrap()
        .click();
    harness.run_steps(2);
    harness.get_by_label("Manual").click();
    harness.run_steps(2);
    set_editor_text(
        &mut harness,
        "update shop.customers set name = name where id = 7",
    );
    harness.key_press(Key::Escape);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    wait_until(&mut harness, "open transaction", |h| {
        h.query_by_label("✔ Commit").is_some()
    });
    shot(&mut harness, "05-transaction");
    harness.get_by_label("Rollback").click();
    wait_until(&mut harness, "rollback", |h| {
        h.query_by_label("✔ Commit").is_none()
    });

    // Executed statements are kept in the history menu.
    harness.get_by_label("🕘 History").click();
    harness.run_steps(3);
    assert!(
        harness
            .query_all_by_label_contains("update shop.customers set name = name")
            .count()
            >= 2,
        "history entry next to the output log"
    );
    shot(&mut harness, "06-history");
    harness.key_press(Key::Escape);
    harness.run_steps(2);

    // Editing a cell in the table editor, previewing and submitting the UPDATE.
    restore_customer_name();
    double_click(&mut harness, "customers");
    wait_until(&mut harness, "customers data", |h| {
        h.query_by_label("1-500 of 500+").is_some()
    });
    // Text inputs: the explorer filter first, then WHERE and ORDER BY.
    let input = |h: &Harness<'_, dbeast::ui::App>, n: usize| {
        h.get_all_by_role(egui::accesskit::Role::TextInput)
            .nth(n)
            .unwrap()
            .value()
            .unwrap_or_default()
    };
    // WHERE completes the columns of the open table.
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .nth(1)
        .unwrap()
        .focus();
    harness.run_steps(1);
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .nth(1)
        .unwrap()
        .type_text("em");
    harness.run_steps(3);
    shot(&mut harness, "10-where-completion");
    harness.key_press(Key::Enter);
    harness.run_steps(2);
    assert_eq!(input(&harness, 1), "email");
    harness.key_press_modifiers(Modifiers::COMMAND, Key::A);
    harness.run_steps(1);
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .nth(1)
        .unwrap()
        .type_text("id <= 5");
    harness.run_steps(1);
    {
        let order_field = harness
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .nth(2)
            .unwrap();
        order_field.focus();
        order_field.type_text("id ");
    }
    harness.run_steps(2);
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .nth(1)
        .unwrap()
        .focus();
    harness.run_steps(1);
    harness.key_press(Key::Enter);
    wait_until(&mut harness, "filtered rows", |h| {
        h.query_by_label("1-5 of 5").is_some()
    });
    double_click(&mut harness, "Customer 5");
    harness.key_press_modifiers(Modifiers::COMMAND, Key::A);
    harness.run_steps(1);
    // The cell editor is the focused one; WHERE and ORDER BY are text inputs too.
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .find(|n| n.is_focused())
        .expect("cell editor has focus")
        .type_text("Edited 5");
    harness.key_press(Key::Enter);
    harness.run_steps(3);
    harness.get_by_label("DML").click();
    harness.run_steps(3);
    assert!(
        harness
            .query_by_label_contains("SET \"name\" = $1 WHERE \"id\" IS NOT DISTINCT FROM $2")
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("-- parameters: 1 = 'Edited 5', 2 = 5")
            .is_some()
    );
    shot(&mut harness, "07-grid-edit");
    harness.get_by_label("Submit").click();
    // The updated row moves to the end of the heap, so it is checked in the database below.
    wait_until(&mut harness, "submitted edit", |h| {
        h.query_by_label("DML").is_none() && h.query_by_label("Submit").is_some()
    });
    assert_eq!(customer_name(5), "Edited 5");

    // Row actions from the cell menu and their shortcuts.
    tree_row(&harness, "user1@example.com").click_secondary();
    harness.run_steps(2);
    shot(&mut harness, "25-row-actions");
    assert!(harness.query_by_label_contains("Add Row").is_some());
    assert!(harness.query_by_label_contains("Delete Row").is_some());
    harness.get_by_label_contains("Clone Row").click();
    harness.run_steps(3);
    let dml_has = |h: &Harness<'_, dbeast::ui::App>, text: &str| {
        h.query_all_by_label_contains(text).next().is_some()
    };
    if !dml_has(&harness, "= 'user1@example.com'") {
        harness.get_by_label("DML").click();
        harness.run_steps(3);
    }
    assert!(
        dml_has(&harness, "= 'user1@example.com'"),
        "clone becomes an INSERT"
    );
    // The clone is selected: the delete shortcut drops it, the add shortcut adds an empty row.
    harness.key_press_modifiers(Modifiers::COMMAND, Key::Backspace);
    harness.run_steps(3);
    assert!(
        !dml_has(&harness, "= 'user1@example.com'"),
        "cloned row removed"
    );
    tree_row(&harness, "user1@example.com").click();
    harness.run_steps(2);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::N);
    harness.run_steps(3);
    assert!(
        dml_has(&harness, "DEFAULT VALUES") || dml_has(&harness, "INSERT INTO"),
        "empty row added"
    );
    harness.get_by_label("Revert").click();
    harness.run_steps(2);

    // Transpose shows columns as rows: column names stack vertically.
    let column_header = |h: &Harness<'_, dbeast::ui::App>, name: &str| -> egui::Rect {
        h.get_by_label(&format!("Select Column: {name}")).rect()
    };
    let (id, email) = (
        column_header(&harness, "id"),
        column_header(&harness, "email"),
    );
    assert!(
        (id.center().y - email.center().y).abs() < 2.0,
        "header row before transposing"
    );
    harness.get_by_label("Transpose").click();
    harness.run_steps(3);
    let (id, email) = (
        column_header(&harness, "id"),
        column_header(&harness, "email"),
    );
    assert!(
        email.top() > id.bottom() - 1.0,
        "transposed: email row below id row"
    );
    assert!((id.left() - email.left()).abs() < 2.0);
    shot(&mut harness, "11-transpose");
    harness.get_by_label("Transpose").click();
    harness.run_steps(2);

    // The value panel under the grid is resized by dragging its top edge.
    tree_row(&harness, "user1@example.com").click();
    harness.run_steps(3);
    harness.get_by_label("Edit Value").click();
    harness.run_steps(3);
    let value_box = |h: &Harness<'_, dbeast::ui::App>| {
        h.get_by_role(egui::accesskit::Role::MultilineTextInput)
            .rect()
    };
    let before = value_box(&harness).height();
    let edge = harness.get_by_label("Cancel Value Edit").rect().top() - 5.0;
    drag(
        &mut harness,
        egui::pos2(900.0, edge),
        egui::pos2(900.0, edge - 150.0),
    );
    let after = value_box(&harness).height();
    assert!(
        after > before + 100.0,
        "value panel did not grow: {before} -> {after}"
    );
    harness.get_by_label("Cancel Value Edit").click();
    harness.run_steps(3);

    // JSON values open as a collapsible tree, or as colored text.
    tree_row(&harness, r#"{"n": 1, "vip": false}"#).click();
    harness.run_steps(3);
    assert!(
        harness.query_by_label("\"vip\"").is_some(),
        "tree shows the keys"
    );
    assert!(harness.query_by_label("false").is_some());
    harness.get_by_label("Text").click();
    harness.run_steps(3);
    assert!(
        harness
            .query_all_by_label_contains("\"vip\": false")
            .any(|node| node.value().is_some_and(|value| value.contains('\n'))),
        "formatted text"
    );
    shot(&mut harness, "21-json-value");
    harness.get_by_label("Tree").click();
    harness.run_steps(2);

    // Recent WHERE filters of this table: pick one and it is applied.
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .nth(1)
        .unwrap()
        .focus();
    harness.run_steps(1);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::A);
    harness.key_press(Key::Backspace);
    harness.run_steps(1);
    harness.key_press(Key::Enter);
    wait_until(&mut harness, "unfiltered rows", |h| {
        h.query_by_label("1-500 of 500+").is_some()
    });
    harness.get_all_by_label("🕘").next().unwrap().click();
    harness.run_steps(3);
    shot(&mut harness, "13-filter-history");
    tree_row(&harness, "id <= 5").click();
    wait_until(&mut harness, "filter from history", |h| {
        h.query_by_label("1-5 of 5").is_some()
    });
    assert_eq!(input(&harness, 1), "id <= 5");

    // DDL generated from the explorer opens in a new console.
    harness
        .get_all_by_label("orders")
        .next()
        .unwrap()
        .click_secondary();
    harness.run_steps(2);
    harness
        .get_by_label("Generate DDL to Query Console")
        .click();
    wait_until(&mut harness, "DDL console", |h| {
        h.query_by_label("QL orders.sql").is_some()
    });
    harness.run_steps(3);
    shot(&mut harness, "08-ddl");

    restore_customer_name();
    std::fs::remove_dir_all(dir).ok();
}

fn editor_text(harness: &Harness<'_, dbeast::ui::App>) -> String {
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .value()
        .unwrap_or_default()
}

fn set_editor_text(harness: &mut Harness<'_, dbeast::ui::App>, text: &str) {
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    harness.run_steps(1);
    harness.key_press_modifiers(Modifiers::COMMAND, Key::A);
    harness.run_steps(1);
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text(text);
    harness.run_steps(2);
}

fn pg_execute(sql: &str) -> dbeast::model::QueryResult {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut ds = dbeast::model::DataSource::new(dbeast::model::DriverKind::Postgres);
        ds.port = 55432;
        ds.user = "dbeast".into();
        ds.password = "dbeast".into();
        ds.database = "dbeast".into();
        let mut conn = dbeast::drivers::Connection::open(&ds).await.unwrap();
        conn.execute(sql, dbeast::model::Page::first(10))
            .await
            .unwrap()
    })
}

fn customer_name(id: i64) -> String {
    pg_execute(&format!("select name from shop.customers where id = {id}")).rows[0][0].to_string()
}

fn restore_customer_name() {
    pg_execute("update shop.customers set name = 'Customer 5' where id = 5");
}
