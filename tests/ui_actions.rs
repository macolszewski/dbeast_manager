//! Action discovery and document navigation keep the current editing context.

mod common;

use dbeast::config::Config;
use dbeast::model::{Column, DataSource, DriverKind, Page, QueryResult, Value};
use egui::accesskit::{Role, Toggled};
use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

const ALPHA_SQL: &str = "-- Unicode żółć 😀\nselect (\n  select 'Alpha needle'\n) as note;\nselect 'Tail żółw' as tail;";
const BETA_SQL: &str = "select 'Beta scratch' as note;";
const GAMMA_SQL: &str = "select 'Gamma scratch' as note;";
const CSV_SOURCE: &str = "id;name\n1;CSV before\n2;CSV other\n";

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

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn key(h: &mut H<'_>, modifiers: Modifiers, key: Key) {
    h.key_press_modifiers(modifiers, key);
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

fn file_menu(h: &mut H<'_>, name: &str, action: &str) {
    // The Opened section precedes the directory's copy of the same file.
    h.get_all_by_role_and_label(Role::Button, name)
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("file row")
        .click_secondary();
    h.run_steps(3);
    click(h, action);
}

fn sql(h: &H<'_>) -> String {
    h.get_by_role(Role::MultilineTextInput)
        .value()
        .expect("active console SQL")
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render().unwrap().save(dir.join(name)).unwrap();
}

fn tab_menu(h: &mut H<'_>, tab: &str, action: &str) {
    h.get_by_label(tab).click_secondary();
    h.run_steps(3);
    h.get_by_label(action).click();
    h.run_steps(3);
}

fn panel_visible(h: &H<'_>, label: &str) -> bool {
    h.query_by_role_and_label(Role::Label, label).is_some()
}

fn open_actions(h: &mut H<'_>) {
    key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    let query = h.get_by_role_and_label(Role::TextInput, "Find Action");
    assert!(query.is_focused());
    assert_eq!(query.value().as_deref(), Some(""));
}

fn action_query(h: &mut H<'_>, text: &str) {
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .focus();
    h.run_steps(1);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find Action")
        .type_text(text);
    h.run_steps(3);
}

#[track_caller]
fn action<'h>(h: &'h H<'_>, label: &'h str) -> egui_kittest::Node<'h> {
    let caller = std::panic::Location::caller();
    h.get_all_by_label(label)
        .find(|node| node.accesskit_node().toggled().is_some())
        .unwrap_or_else(|| {
            panic!(
                "missing selectable palette action {label:?}, requested at {caller}; query: {:?}; matching nodes: {:#?}; current UI tree: {h:#?}",
                h.query_by_role_and_label(Role::TextInput, "Find Action")
                    .and_then(|query| query.value()),
                h.get_all_by_label(label).collect::<Vec<_>>()
            )
        })
}

fn copy(h: &mut H<'_>) -> String {
    h.event(egui::Event::Copy);
    h.run_steps(1);
    h.output()
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
        .expect("clipboard output from the restored editing context")
}

fn editor_selection(context: &egui::Context, editor: egui::Id) -> Range<usize> {
    let cursor = egui::text_edit::TextEditState::load(context, editor)
        .unwrap()
        .cursor
        .char_range()
        .unwrap();
    let start = cursor.primary.index.0.min(cursor.secondary.index.0);
    let end = cursor.primary.index.0.max(cursor.secondary.index.0);
    start..end
}

fn set_editor_selection(context: &egui::Context, editor: egui::Id, range: Range<usize>) {
    let mut state = egui::text_edit::TextEditState::load(context, editor).unwrap();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(range.start),
        CCursor::new(range.end),
    )));
    state.store(context, editor);
    context.memory_mut(|memory| memory.request_focus(editor));
}

fn run_action(h: &mut H<'_>, name: &str) {
    open_actions(h);
    action_query(h, name);
    assert!(!action(h, name).accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Enter);
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Find Action")
            .is_none()
    );
}

fn verify_editor_actions(h: &mut H<'_>, context: &egui::Context, config_path: &Path) {
    click(h, "QL Alpha");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    let editor = context.memory(|memory| memory.focused()).unwrap();
    let marker_byte = ALPHA_SQL.find("Tail żółw").unwrap();
    let caret = ALPHA_SQL[..marker_byte].chars().count();
    set_editor_selection(context, editor, caret..caret);
    h.run_steps(2);
    run_action(h, "Fold All");
    assert!(
        sql(h)
            .chars()
            .any(|character| dbeast::folding::placeholder_index(character).is_some())
    );
    run_action(h, "Unfold All");
    assert_eq!(sql(h), ALPHA_SQL);
    assert_eq!(editor_selection(context, editor), caret..caret);
    const INSERTED: &str = "marked_Łódź_";
    h.get_by_role(Role::MultilineTextInput).type_text(INSERTED);
    h.run_steps(3);
    let mut expected = ALPHA_SQL.to_owned();
    expected.insert_str(marker_byte, INSERTED);
    assert_eq!(
        sql(h),
        expected,
        "unfolding preserves the source insertion point"
    );
    key(h, Modifiers::COMMAND, Key::Z);
    assert_eq!(sql(h), ALPHA_SQL);
    assert_eq!(editor_selection(context, editor), caret..caret);

    // Reformat a visible tail after an earlier folded region, using source offsets.
    run_action(h, "Fold All");
    let visible = sql(h);
    let visible_start = visible[..visible.rfind("select '").unwrap()]
        .chars()
        .count();
    set_editor_selection(context, editor, visible_start..visible.chars().count());
    h.run_steps(2);
    let tail_byte = ALPHA_SQL.rfind("select '").unwrap();
    let tail_selection = ALPHA_SQL[..tail_byte].chars().count()..ALPHA_SQL.chars().count();
    run_action(h, "Reformat Code");
    let formatted = sql(h);
    assert_ne!(formatted, ALPHA_SQL);
    assert!(formatted.starts_with(&ALPHA_SQL[..tail_byte]));
    assert!(formatted[tail_byte..].contains("Tail żółw"));
    assert!(
        formatted
            .chars()
            .all(|character| dbeast::folding::placeholder_index(character).is_none())
    );
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    let saved = config["consoles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|console| console["title"] == "Alpha")
        .unwrap();
    assert_eq!(saved["sql"].as_str(), Some(formatted.as_str()));
    key(h, Modifiers::COMMAND, Key::Z);
    assert_eq!(sql(h), ALPHA_SQL);
    assert_eq!(editor_selection(context, editor), tail_selection);
    key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    assert_eq!(sql(h), formatted);
    key(h, Modifiers::COMMAND, Key::Z);
    assert_eq!(sql(h), ALPHA_SQL);
}

fn verify_actions(h: &mut H<'_>, context: &egui::Context) {
    click(h, "QL Alpha");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    let editor_focus = context.memory(|memory| memory.focused());
    assert!(editor_focus.is_some());
    open_actions(h);
    action_query(h, "no-such-action-9382");
    h.get_by_label("No actions found");
    key(h, Modifiers::NONE, Key::Enter);
    h.get_by_role_and_label(Role::TextInput, "Find Action");
    key(h, Modifiers::NONE, Key::Escape);
    assert_eq!(context.memory(|memory| memory.focused()), editor_focus);
    assert_eq!(sql(h), ALPHA_SQL);

    open_actions(h);
    action_query(h, "sToP");
    assert!(action(h, "Stop").accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Enter);
    h.get_by_role_and_label(Role::TextInput, "Find Action");
    key(h, Modifiers::NONE, Key::Escape);
    assert_eq!(context.memory(|memory| memory.focused()), editor_focus);

    open_actions(h);
    action_query(h, "tAb");
    assert_eq!(
        action(h, "Next Tab").accesskit_node().toggled(),
        Some(Toggled::True)
    );
    key(h, Modifiers::NONE, Key::ArrowUp);
    assert_eq!(
        action(h, "Close All Tabs").accesskit_node().toggled(),
        Some(Toggled::True)
    );
    key(h, Modifiers::NONE, Key::ArrowDown);
    assert_eq!(
        action(h, "Next Tab").accesskit_node().toggled(),
        Some(Toggled::True)
    );
    key(h, Modifiers::NONE, Key::ArrowDown);
    assert_eq!(
        action(h, "Previous Tab").accesskit_node().toggled(),
        Some(Toggled::True)
    );
    shot(h, "46-find-action.png");
    key(h, Modifiers::NONE, Key::Enter);
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Find Action")
            .is_none()
    );
    assert_eq!(
        sql(h),
        GAMMA_SQL,
        "Enter executes the keyboard-selected action"
    );

    // The same active console has both SQL and fetched cells; focus decides Find's target.
    click(h, "QL Alpha");
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
    open_actions(h);
    action_query(h, "Find");
    key(h, Modifiers::NONE, Key::Enter);
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Find")
            .is_focused()
    );
    assert!(h.query_by_label("Close Grid Find").is_none());

    // Search toolbar buttons retain the SQL context, just like their text fields.
    h.get_by_label("Match Case").focus();
    h.run_steps(1);
    let search_button_focus = context.memory(|memory| memory.focused());
    assert!(search_button_focus.is_some());
    open_actions(h);
    key(h, Modifiers::NONE, Key::Escape);
    assert_eq!(
        context.memory(|memory| memory.focused()),
        search_button_focus
    );
    run_action(h, "Replace");
    h.get_by_role_and_label(Role::TextInput, "Replace with");
    assert!(h.query_by_label("Close Grid Find").is_none());
    key(h, Modifiers::NONE, Key::Escape);
    assert!(h.get_by_role(Role::MultilineTextInput).is_focused());
    assert_eq!(sql(h), ALPHA_SQL);

    click(h, "Alpha needle");
    let grid_focus = context.memory(|memory| memory.focused());
    assert!(grid_focus.is_some());
    open_actions(h);
    action_query(h, "Find on Current Page");
    key(h, Modifiers::NONE, Key::Enter);
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Find on Current Page")
            .value()
            .as_deref(),
        Some("Alpha needle")
    );
    assert!(h.query_by_label("Close Find").is_none());
    key(h, Modifiers::NONE, Key::Escape);
    assert_eq!(context.memory(|memory| memory.focused()), grid_focus);
    assert_eq!(copy(h), "Alpha needle");

    click(h, "⊞ contacts");
    click(h, "Ada before");
    open_actions(h);
    action_query(h, "Reformat Code");
    assert!(action(h, "Reformat Code").accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Enter);
    h.get_by_role_and_label(Role::TextInput, "Find Action");
    key(h, Modifiers::NONE, Key::Escape);

    // A fresh invocation reflects changes made since the previous action search.
    open_actions(h);
    action_query(h, "Submit");
    assert!(action(h, "Submit").accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Escape);
    click(h, "Ada before");
    // Restored rows are read-only; the first edit attempt reloads the page.
    key(h, Modifiers::NONE, Key::Enter);
    wait_until(h, "refreshed restored table", |h| {
        h.query_by_label_contains("Read-only:").is_none()
            && h.query_by_label_contains("Checking whether").is_none()
    });
    click(h, "Ada before");
    key(h, Modifiers::NONE, Key::Enter);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("inline cell editor")
        .type_text("Ada reversible");
    key(h, Modifiers::NONE, Key::Enter);
    open_actions(h);
    action_query(h, "Submit");
    assert!(!action(h, "Submit").accesskit_node().is_disabled());
    action_query(h, "Revert");
    assert!(!action(h, "Revert").accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Enter);
    h.get_by_label("Ada before");
    assert!(h.query_by_label("Ada reversible").is_none());
    open_actions(h);
    action_query(h, "Submit");
    assert!(action(h, "Submit").accesskit_node().is_disabled());
    key(h, Modifiers::NONE, Key::Escape);
}

fn console_tab(title: &str, sql: &str, result: Option<QueryResult>) -> serde_json::Value {
    let results: Vec<_> = result
        .into_iter()
        .map(|data| {
            serde_json::json!({
                "title": "Result 1", "sql": sql,
                "page": Page::first(3), "data": data,
                "show_value_editor": false
            })
        })
        .collect();
    serde_json::json!({
        "title": title, "ds_id": "actions-fixture",
        "kind": {"Console": {"sql": sql}},
        "view": if results.is_empty() { None } else { Some(0) },
        "results": results
    })
}

fn verify_navigation_and_panels(h: &mut H<'_>) {
    let next = Modifiers::COMMAND | Modifiers::SHIFT;
    assert_eq!(sql(h), ALPHA_SQL);
    key(h, next, Key::CloseBracket);
    assert_eq!(sql(h), BETA_SQL);
    key(h, next, Key::CloseBracket);
    h.get_by_label("Ada before");
    assert!(h.query_by_role(Role::MultilineTextInput).is_none());
    key(h, next, Key::CloseBracket);
    assert_eq!(sql(h), GAMMA_SQL);
    key(h, next, Key::CloseBracket);
    assert_eq!(sql(h), ALPHA_SQL, "next tab wraps to the first document");
    key(h, next, Key::OpenBracket);
    assert_eq!(sql(h), GAMMA_SQL, "previous tab wraps to the last document");
    click(h, "QL Alpha");

    assert!(panel_visible(h, "Database Explorer"));
    key(h, Modifiers::COMMAND, Key::Num1);
    assert!(!panel_visible(h, "Database Explorer"));
    key(h, Modifiers::COMMAND, Key::Num1);
    assert!(panel_visible(h, "Database Explorer"));
    key(h, Modifiers::COMMAND, Key::Num2);
    assert!(panel_visible(h, "Files"));
    assert!(!panel_visible(h, "Database Explorer"));
    key(h, Modifiers::COMMAND, Key::Num2);
    assert!(!panel_visible(h, "Files"));
    key(h, Modifiers::COMMAND, Key::Num1);
    assert!(panel_visible(h, "Database Explorer"));
    assert!(panel_visible(h, "Services"));
    key(h, Modifiers::COMMAND, Key::Num8);
    assert!(!panel_visible(h, "Services"));
    key(h, Modifiers::COMMAND, Key::Num8);
    assert!(panel_visible(h, "Services"));
    assert_eq!(
        sql(h),
        ALPHA_SQL,
        "panel shortcuts must not edit the console"
    );
}

fn verify_closing(h: &mut H<'_>, db: &rusqlite::Connection, config_path: &Path) {
    // Cancelling a batch is atomic, including otherwise clean table tabs.
    click(h, "QL Beta");
    tab_menu(h, "QL Alpha", "Close Other Tabs");
    h.get_by_label("Close Tabs");
    assert!(h.get_by_label("Cancel").is_focused());
    shot(h, "48-close-other-tabs.png");
    click(h, "Cancel");
    for label in ["QL Alpha", "QL Beta", "⊞ contacts", "QL Gamma"] {
        h.get_by_label(label);
    }
    assert_eq!(
        sql(h),
        BETA_SQL,
        "the context-menu target must not activate"
    );

    // Closing to the right of an inactive target preserves the active identity.
    tab_menu(h, "⊞ contacts", "Close Tabs to the Right");
    h.get_by_label("Close Tab");
    click(h, "Close and Discard");
    assert!(h.query_by_label("QL Gamma").is_none());
    assert_eq!(sql(h), BETA_SQL);

    // A live input buffer is staged before the close decision, even without Enter.
    click(h, "⊞ contacts");
    click(h, "Ada before");
    key(h, Modifiers::NONE, Key::Enter);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("inline cell editor")
        .type_text("Ada staged");
    h.run_steps(2);
    key(h, Modifiers::COMMAND, Key::W);
    h.get_by_label("Close Tab");
    h.get_by_label("Pending cell and row changes");
    shot(h, "47-close-dirty-tab.png");
    click(h, "Cancel");
    h.get_by_label("Ada staged");
    let stored: String = db
        .query_row("SELECT name FROM contacts WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, "Ada before", "closing must not submit the edit");
    key(h, Modifiers::COMMAND, Key::W);
    h.get_by_label("Pending cell and row changes");
    click(h, "Close and Discard");
    assert!(h.query_by_label_contains("⊞ contacts").is_none());
    assert_eq!(
        sql(h),
        BETA_SQL,
        "closing the last tab selects its left neighbor"
    );

    tab_menu(h, "QL Alpha", "Close Other Tabs");
    click(h, "Close and Discard");
    h.get_by_label("QL Alpha");
    assert!(h.query_by_label("QL Beta").is_none());
    assert_eq!(sql(h), ALPHA_SQL);
    for key_name in [Key::OpenBracket, Key::CloseBracket] {
        key(h, Modifiers::COMMAND | Modifiers::SHIFT, key_name);
        assert_eq!(sql(h), ALPHA_SQL, "a single tab remains active");
    }
    tab_menu(h, "QL Alpha", "Close All Tabs");
    key(h, Modifiers::NONE, Key::Escape);
    h.get_by_label("QL Alpha");
    assert!(h.query_by_label("Close and Discard").is_none());
    tab_menu(h, "QL Alpha", "Close All Tabs");
    click(h, "Close and Discard");
    assert!(h.query_by_label("QL Alpha").is_none());
    assert!(h.query_by_role(Role::MultilineTextInput).is_none());
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    assert!(config["consoles"].as_array().unwrap().is_empty());
    let stored: String = db
        .query_row("SELECT name FROM contacts WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, "Ada before");
}

fn verify_close_file(h: &mut H<'_>, directory: &Path) {
    key(h, Modifiers::COMMAND, Key::Num2);
    assert!(panel_visible(h, "Files"));
    file_menu(h, "discard_changes.csv", "Open");
    wait_until(h, "imported CSV", |h| {
        h.query_by_label("CSV before").is_some()
    });
    let temporary_files: Vec<_> = std::fs::read_dir(directory.join("open-files"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(temporary_files.len(), 1);
    let temporary_database = &temporary_files[0];
    assert!(temporary_database.is_file());

    // Closing a file applies atomically to every document using its temporary source.
    run_action(h, "Open New Query Console");
    h.get_by_label("QL console");
    assert_eq!(sql(h), "");
    click(h, "⊞ discard_changes");
    h.get_all_by_label("CSV before")
        .min_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .unwrap()
        .click();
    h.run_steps(3);
    key(h, Modifiers::NONE, Key::Enter);
    key(h, Modifiers::COMMAND, Key::A);
    h.get_all_by_role(Role::TextInput)
        .find(|node| node.is_focused())
        .expect("CSV cell input")
        .type_text("CSV staged żółw");
    h.run_steps(2);

    // Do not press Enter: the close path must account for a live input buffer.
    file_menu(h, "discard_changes.csv", "Close File");
    h.get_by_label("Close File");
    h.get_by_label("Pending cell and row changes");
    assert!(h.get_by_label("Cancel").is_focused());
    click(h, "Cancel");
    assert!(h.query_by_label_contains("⊞ discard_changes").is_some());
    h.get_by_label("QL console");
    assert!(h.query_all_by_label("CSV staged żółw").next().is_some());
    assert!(
        temporary_database.is_file(),
        "Cancel keeps the working copy"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("discard_changes.csv")).unwrap(),
        CSV_SOURCE,
        "Cancel must not submit changes to the original CSV"
    );

    file_menu(h, "discard_changes.csv", "Close File");
    h.get_by_label("Close File");
    h.get_by_label("Pending cell and row changes");
    click(h, "Close and Discard");
    wait_until(h, "temporary file cleanup", |h| {
        h.query_by_label_contains("⊞ discard_changes").is_none()
            && h.query_by_label("QL console").is_none()
            && !temporary_database.exists()
    });
    assert!(h.query_by_label("Opened").is_none());
    assert_eq!(
        std::fs::read_dir(directory.join("open-files"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("discard_changes.csv")).unwrap(),
        CSV_SOURCE,
        "discarding the working copy leaves the original CSV byte-for-byte unchanged"
    );
}

#[test]
fn actions_and_tab_lifecycle_preserve_focus_and_unsaved_work() {
    let dir = std::env::temp_dir().join(format!("dbeast_actions_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("discard_changes.csv"), CSV_SOURCE).unwrap();
    let database = dir.join("contacts.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE contacts (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO contacts VALUES (1, 'Ada before'), (2, 'Olek before');",
    )
    .unwrap();
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = "actions-fixture".into();
    source.name = "Local actions".into();
    source.file = database.to_string_lossy().into_owned();
    let config = Config {
        data_sources: vec![source],
        max_rows: 3,
        tile_url: String::new(),
        files_root: Some(dir.clone()),
        ..Config::default()
    };
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, common::legacy_config(&config).unwrap()).unwrap();
    let table_data = QueryResult {
        columns: [("id", "integer"), ("name", "text")]
            .into_iter()
            .map(|(name, type_name)| Column {
                name: name.into(),
                type_name: type_name.into(),
            })
            .collect(),
        rows: vec![
            vec![Value::Int(1), Value::Text("Ada before".into())],
            vec![Value::Int(2), Value::Text("Olek before".into())],
        ],
        ..QueryResult::default()
    };
    let console_data = QueryResult {
        columns: vec![Column {
            name: "note".into(),
            type_name: "text".into(),
        }],
        rows: vec![vec![Value::Text("Alpha needle".into())]],
        ..QueryResult::default()
    };
    let session = serde_json::json!({
        "active": 0,
        "tabs": [
            console_tab("Alpha", ALPHA_SQL, Some(console_data)),
            console_tab("Beta", BETA_SQL, None),
            {
                "title": "contacts", "ds_id": "actions-fixture",
                "kind": {"Table": {
                    "schema": null, "table": "contacts", "filter": "", "order": "id",
                    "primary_key": ["id"]
                }},
                "view": 0,
                "results": [{
                    "title": "contacts", "sql": "SELECT * FROM contacts ORDER BY id",
                    "page": Page::first(3), "data": table_data,
                    "show_value_editor": false
                }]
            },
            console_tab("Gamma", GAMMA_SQL, None)
        ]
    });
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_string(&session).unwrap(),
    )
    .unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config_path) };
    let (mut h, context) = app();
    verify_navigation_and_panels(&mut h);
    verify_actions(&mut h, &context);
    verify_editor_actions(&mut h, &context, &config_path);
    verify_closing(&mut h, &db, &config_path);
    verify_close_file(&mut h, &dir);
    drop(h);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
