//! Compare Data uses explicit keys over immutable fetched-page snapshots.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

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

#[track_caller]
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

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(h) {
        if Instant::now() >= deadline {
            let nodes: Vec<_> = h
                .query_all_by_label_contains("")
                .map(|node| {
                    (
                        node.accesskit_node().role(),
                        node.accesskit_node().label(),
                        node.value(),
                        node.accesskit_node().description(),
                        node.rect(),
                    )
                })
                .collect();
            shot(h, "compare-data-timeout.png");
            panic!("timed out waiting for {what}; nodes: {nodes:#?}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
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

fn text(text: &str) -> Value {
    Value::Text(text.into())
}

fn columns(names: &[&str]) -> Vec<Column> {
    names
        .iter()
        .map(|name| Column {
            name: (*name).into(),
            type_name: "fixture".into(),
        })
        .collect()
}

fn pages() -> (QueryResult, QueryResult) {
    let mut binary_a = vec![0x41; 64];
    let mut binary_b = binary_a.clone();
    binary_a[63] = 1;
    binary_b[63] = 2;
    let left = QueryResult {
        columns: columns(&[
            "tenant",
            "id",
            "name",
            "amount",
            "payload",
            "note",
            "left_metadata",
        ]),
        rows: vec![
            vec![
                Value::Int(7),
                Value::Int(1),
                text("Ada Żółw"),
                Value::Decimal("10.00".into()),
                Value::Bytes(vec![9]),
                text("keep\tŁódź\n\"quoted\""),
                text("left ignored"),
            ],
            vec![
                Value::Int(7),
                Value::Int(2),
                text("Olek before"),
                Value::Decimal("20.0".into()),
                Value::Bytes(binary_a),
                Value::Null,
                text("left ignored"),
            ],
            vec![
                Value::Int(8),
                Value::Int(1),
                text("Iga equal"),
                Value::Decimal("30".into()),
                Value::Bytes(Vec::new()),
                text("<null>"),
                text("left ignored"),
            ],
            vec![
                Value::Int(7),
                Value::Int(3),
                text("Lena left only"),
                Value::Decimal("40".into()),
                Value::Bytes(vec![3]),
                text("left"),
                text("left ignored"),
            ],
        ],
        truncated: true,
        ..QueryResult::default()
    };
    let reordered = |row: &[Value]| {
        vec![
            row[2].clone(),
            row[1].clone(),
            row[0].clone(),
            row[3].clone(),
            row[4].clone(),
            row[5].clone(),
            text("right ignored"),
        ]
    };
    let right = QueryResult {
        columns: columns(&[
            "name",
            "id",
            "tenant",
            "balance",
            "payload",
            "note",
            "right_metadata",
        ]),
        rows: vec![
            reordered(&left.rows[2]),
            vec![
                text("Olek after"),
                Value::Int(2),
                Value::Int(7),
                Value::Decimal("20.00".into()),
                Value::Bytes(binary_b),
                text("<null>"),
                text("right ignored"),
            ],
            vec![
                text("Mira right only"),
                Value::Int(4),
                Value::Int(7),
                Value::Decimal("50".into()),
                Value::Bytes(vec![4]),
                text("right"),
                text("right ignored"),
            ],
            reordered(&left.rows[0]),
        ],
        truncated: true,
        ..QueryResult::default()
    };
    (left, right)
}

fn write_fixture(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let mut left_source = DataSource::new(DriverKind::Sqlite);
    left_source.id = "compare-left".into();
    left_source.name = "Offline left".into();
    left_source.file = directory
        .join("never-open-left.sqlite")
        .to_string_lossy()
        .into_owned();
    let mut right_source = DataSource::new(DriverKind::Sqlite);
    right_source.id = "compare-right".into();
    right_source.name = "Offline right".into();
    right_source.file = directory
        .join("never-open-right.sqlite")
        .to_string_lossy()
        .into_owned();
    right_source.read_only = true;
    // A separate source: editing needs fresh rows and metadata from a database.
    let mut editable_source = DataSource::new(DriverKind::Sqlite);
    editable_source.id = "compare-editable".into();
    editable_source.name = "Editable source".into();
    editable_source.file = directory
        .join("editable.sqlite")
        .to_string_lossy()
        .into_owned();
    rusqlite::Connection::open(&editable_source.file)
        .unwrap()
        .execute_batch(
            "CREATE TABLE editable_data (tenant INTEGER NOT NULL, id INTEGER NOT NULL, name TEXT, PRIMARY KEY (tenant, id));
             INSERT INTO editable_data VALUES (7, 1, 'Editable original');",
        )
        .unwrap();
    let config_path = directory.join("config.json");
    let session_path = directory.join("session.json");
    let config = Config {
        data_sources: vec![left_source, right_source, editable_source],
        files_root: Some(directory.to_owned()),
        tile_url: String::new(),
        ..Config::default()
    };
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let (left, right) = pages();
    let pending = QueryResult {
        columns: columns(&["tenant", "id", "name"]),
        rows: vec![vec![
            Value::Int(7),
            Value::Int(1),
            text("Editable original"),
        ]],
        ..QueryResult::default()
    };
    let saved = serde_json::json!({"active":0,"tabs":[
        {"title":"Left snapshot","ds_id":"compare-left","kind":{"Console":{"sql":"SELECT tenant,id,name,amount,payload,note,left_metadata FROM left_data"}},"view":0,"show_value_editor":false,"results":[{"title":"Same result title","sql":"SELECT * FROM left_data","page":Page::first(4),"data":left,"show_value_editor":false}]},
        {"title":"Right snapshot","ds_id":"compare-right","kind":{"Console":{"sql":"SELECT name,id,tenant,balance,payload,note,right_metadata FROM right_data"}},"view":0,"show_value_editor":false,"results":[{"title":"Same result title","sql":"SELECT * FROM right_data","page":Page{skip:1000,limit:4},"data":right,"show_value_editor":false}]},
        {"title":"Editable snapshot","ds_id":"compare-editable","kind":{"Table":{"schema":null,"table":"editable_data","filter":"","order":"","primary_key":["tenant","id"]}},"view":0,"show_value_editor":false,"results":[{"title":"Editable result","sql":"SELECT tenant,id,name FROM editable_data","page":Page::first(4),"data":pending,"show_value_editor":false}]}
    ]});
    std::fs::write(&session_path, saved.to_string()).unwrap();
    (config_path, session_path)
}

fn choose_result(h: &mut H<'_>, side: &str, tab_title: &str) {
    click(h, side);
    // The underlying modeless app also has a "QL {tab_title}" button. Only the
    // popup option includes the result separator, so do not click a source tab.
    let prefix = format!("{tab_title} /");
    h.query_all_by_label_contains(&prefix)
        .find(|node| node.accesskit_node().role() == Role::Button)
        .expect("open-result choice identified by its owning tab")
        .click();
    h.run_steps(3);
    assert!(
        h.get_by_label(side)
            .value()
            .is_some_and(|value| value.starts_with(&prefix)),
        "the requested side must select the requested result, not a background tab"
    );
}

fn has_text(h: &H<'_>, text: &str) -> bool {
    let expected = text.to_lowercase();
    h.query_all_by_label_contains("").any(|node| {
        node.accesskit_node()
            .label()
            .is_some_and(|label| label.to_lowercase().contains(&expected))
            || node
                .value()
                .is_some_and(|value| value.to_lowercase().contains(&expected))
    })
}

fn counts(h: &H<'_>) {
    for label in ["Equal: 2", "Changed: 1", "Left only: 1", "Right only: 1"] {
        h.get_by_label(label);
    }
}

fn wait_diff(h: &mut H<'_>) {
    wait_until(h, "comparison of the fetched snapshots", |h| {
        h.query_by_label("Diff Viewer").is_some() && h.query_by_label("Changed: 1").is_some()
    });
    counts(h);
}

fn changed_cell(h: &H<'_>, label: &str) {
    let node = h.get_by_label(label);
    assert!(
        node.accesskit_node()
            .description()
            .is_some_and(|description| description.contains("Changed")),
        "a changed cell needs an accessible status in addition to its background color"
    );
}

fn assert_no_database_open(directory: &Path) {
    for file in ["never-open-left.sqlite", "never-open-right.sqlite"] {
        assert!(
            !directory.join(file).exists(),
            "Compare Data must not open a missing SQLite source"
        );
    }
}

#[test]
fn compare_data_maps_explicit_keys_and_preserves_fetched_snapshots() {
    let directory =
        std::env::temp_dir().join(format!("dbeast_compare_ui_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (config_path, _) = write_fixture(&directory);
    // SAFETY: one test in this binary, before the application starts any workers.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let mut h = app();
    click(&mut h, "Ada Żółw");
    view_action(&mut h, "Compare Data");
    h.get_by_label("Fetched pages · Exact typed values");
    choose_result(&mut h, "Right result", "Right snapshot");
    assert!(
        h.get_by_role_and_label(Role::Button, "Compare")
            .accesskit_node()
            .is_disabled(),
        "restored console results without a key require an explicit choice"
    );
    assert!(has_text(&h, "left_metadata") && has_text(&h, "right_metadata"));
    click(&mut h, "Map left column: 4 amount");
    click(&mut h, "4: balance");
    click(&mut h, "Key: tenant = tenant");
    click(&mut h, "Key: id = id");
    assert!(
        !h.get_by_role_and_label(Role::Button, "Compare")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        has_text(&h, "1000"),
        "the right snapshot retains its nonzero page offset"
    );
    assert!(
        has_text(&h, "more rows available"),
        "the dialog identifies incomplete fetched pages"
    );
    shot(&mut h, "64-compare-data-setup.png");
    click(&mut h, "Compare");
    wait_diff(&mut h);
    changed_cell(&h, "Left row 2, name: Olek before");
    changed_cell(&h, "Right row 2, name: Olek after");
    h.get_by_label("Left row 4, name: Lena left only");
    h.get_by_label("Right row 3, name: Mira right only");
    assert!(h.query_by_label("Left row 1, name: Ada Żółw").is_none());
    assert!(h.query_by_label("Submit").is_none());
    assert!(h.query_by_label("DML").is_none());
    assert_no_database_open(&directory);
    shot(&mut h, "65-compare-data.png");
    click(&mut h, "Show identical");
    h.get_by_label("Left row 1, name: Ada Żółw");
    h.get_by_label("Right row 4, name: Ada Żółw");
    h.get_by_label("Left row 3, name: Iga equal");
    counts(&h);
    click(&mut h, "Show identical");

    // Choosing an incomplete key must fail instead of matching equal ids by position.
    click(&mut h, "Change Comparison");
    click(&mut h, "Key: tenant = tenant");
    click(&mut h, "Compare");
    wait_until(&mut h, "duplicate key diagnostics", |h| {
        has_text(h, "same comparison key")
    });
    assert!(
        has_text(&h, "left"),
        "the ambiguous side must be identified"
    );
    assert!(h.query_by_label("Diff Viewer").is_none());
    click(&mut h, "Key: tenant = tenant");
    click(&mut h, "Compare");
    wait_diff(&mut h);

    click(&mut h, "Close");

    // Local sorting and filtering affect the source grid, not the fetched snapshot.
    click(&mut h, "Sort by name");
    click(&mut h, "Sort by name");
    click(&mut h, "Ada Żółw");
    key(&mut h, Modifiers::COMMAND, Key::F);
    let query = h.get_by_role_and_label(Role::TextInput, "Find on Current Page");
    query.focus();
    h.run_steps(1);
    key(&mut h, Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
        .type_text("Ada");
    h.run_steps(2);
    click(&mut h, "Filter rows");
    assert!(h.query_by_label("Lena left only").is_none());
    view_action(&mut h, "Compare Data");
    choose_result(&mut h, "Right result", "Right snapshot");
    click(&mut h, "Map left column: 4 amount");
    click(&mut h, "4: balance");
    click(&mut h, "Key: tenant = tenant");
    click(&mut h, "Key: id = id");
    click(&mut h, "Compare");
    wait_diff(&mut h);
    h.get_by_label("Left row 4, name: Lena left only");
    h.get_by_label("Right row 3, name: Mira right only");
    click(&mut h, "Close");

    // A restored table can stage edits locally without contacting its source.
    click(&mut h, "⊞ Editable snapshot");
    // Restored rows are read-only; the first edit attempt reloads them.
    click(&mut h, "Editable original");
    key(&mut h, Modifiers::NONE, Key::Enter);
    wait_until(&mut h, "refreshed editable table", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    click(&mut h, "Editable original");
    key(&mut h, Modifiers::NONE, Key::Enter);
    key(&mut h, Modifiers::COMMAND, Key::A);
    h.query_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("active inline buffer")
        .type_text("Pending edited value");
    h.run_steps(2);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .type_text("Compare Data");
    h.run_steps(3);
    assert!(
        h.query_by_label("No actions found").is_none(),
        "Find Action must discover Compare Data by its full displayed name"
    );
    assert!(
        h.query_all_by_role_and_label(Role::Button, "Compare Data")
            .find(|node| node.accesskit_node().toggled().is_some())
            .expect("Compare Data palette action")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("Submit or revert pending changes before comparing data");
    key(&mut h, Modifiers::NONE, Key::Escape);
    h.get_by_label("Pending edited value");

    // A clean left source must not bypass changes in the chosen right source.
    click(&mut h, "QL Left snapshot");
    view_action(&mut h, "Compare Data");
    choose_result(&mut h, "Right result", "Editable snapshot");
    assert!(
        h.get_by_role_and_label(Role::Button, "Compare")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("Submit or revert pending changes before comparing data");
    click(&mut h, "Cancel");
    click(&mut h, "⊞ Editable snapshot •");
    h.get_by_label("Pending edited value");
    click(&mut h, "Revert");
    h.get_by_label("Editable original");

    click(&mut h, "QL Left snapshot");
    view_action(&mut h, "Compare Data");
    choose_result(&mut h, "Right result", "Right snapshot");
    click(&mut h, "Map left column: 4 amount");
    click(&mut h, "4: balance");
    click(&mut h, "Key: tenant = tenant");
    click(&mut h, "Key: id = id");
    click(&mut h, "Compare");
    wait_diff(&mut h);
    // Modeless comparison owns its data independently of the source tabs.
    h.get_by_label("QL Right snapshot").click_secondary();
    h.run_steps(3);

    // Egui appends the platform shortcut to this menu button's accessible name.
    h.query_all_by_role(Role::Button)
        .find(|node| {
            node.accesskit_node()
                .label()
                .is_some_and(|label| label == "Close Tab" || label.starts_with("Close Tab "))
        })
        .expect("Close Tab action, optionally followed by its platform shortcut")
        .click();
    h.run_steps(3);
    // A nonempty console is deliberately protected even when its result has no
    // pending edits: closing also removes its SQL from the saved workspace.
    wait_until(&mut h, "confirmation for the source console SQL", |h| {
        h.query_by_role_and_label(Role::Button, "Close and Discard")
            .is_some_and(|button| !button.accesskit_node().is_disabled())
    });
    click(&mut h, "Close and Discard");
    wait_until(
        &mut h,
        "source tab closed while its diff remains open",
        |h| h.query_by_label("QL Right snapshot").is_none(),
    );
    counts(&h);
    h.get_by_label("Right row 2, name: Olek after");
    h.get_by_label("Right row 3, name: Mira right only");
    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(3);
    for label in [
        "Fetched pages · Exact typed values",
        "Show identical",
        "Close",
    ] {
        let rect = h.get_by_label(label).rect();
        assert!(
            rect.left() >= 0.0
                && rect.right() <= 1100.0
                && rect.top() >= 0.0
                && rect.bottom() <= 500.0,
            "{label} must remain reachable in a narrow window: {rect:?}"
        );
    }
    counts(&h);
    shot(&mut h, "66-compare-data-narrow.png");
    click(&mut h, "Close");
    assert_no_database_open(&directory);
    drop(h);
    std::fs::remove_dir_all(directory).unwrap();
}
