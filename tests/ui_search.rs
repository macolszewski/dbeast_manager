//! Console search, replacement and navigation without a database.

use egui::accesskit::Role;
use egui::text::{CCursor, CCursorRange};
use egui::{Context, Id, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

const SQL: &str = "-- Unicode search: żółw\nWITH recent AS (\n    SELECT city, city_id, CITY, 'Łódź' AS label\n    FROM cities\n    WHERE city = 'Łódź'\n)\nSELECT city FROM recent;";

fn editor(h: &H<'_>) -> String {
    h.get_by_role(Role::MultilineTextInput)
        .value()
        .unwrap_or_default()
}

fn selection(ctx: &Context, id: Id) -> std::ops::Range<usize> {
    let range = egui::text_edit::TextEditState::load(ctx, id)
        .unwrap()
        .cursor
        .char_range()
        .unwrap();
    range.primary.index.0.min(range.secondary.index.0)
        ..range.primary.index.0.max(range.secondary.index.0)
}

fn selected_text(h: &H<'_>, ctx: &Context, id: Id) -> String {
    let range = selection(ctx, id);
    editor(h)
        .chars()
        .skip(range.start)
        .take(range.len())
        .collect()
}

fn set_field(h: &mut H<'_>, label: &str, value: &str) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(value);
    h.run_steps(3);
}

fn focus_editor(h: &mut H<'_>) {
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
}

fn saved_sql(config: &std::path::Path) -> String {
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
    config["consoles"][0]["sql"].as_str().unwrap().to_owned()
}

fn wait_for_saved_sql(h: &mut H<'_>, config: &std::path::Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while saved_sql(config) != expected {
        assert!(Instant::now() < deadline, "timed out waiting for saved SQL");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

#[test]
fn find_replace_and_go_to_line() {
    let dir = std::env::temp_dir().join(format!("dbeast_search_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        serde_json::json!({"consoles": [{"title": "console", "ds_id": "none", "sql": SQL}]})
            .to_string(),
    )
    .unwrap();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("DBEAST_CONFIG", &config) };
    let mut context = None;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| {
            context = Some(cc.egui_ctx.clone());
            dbeast::ui::App::new_for_test(cc)
        });
    let ctx = context.unwrap();
    h.run_steps(3);
    focus_editor(&mut h);
    let editor_id = ctx.memory(|memory| memory.focused()).unwrap();
    let mut state = egui::text_edit::TextEditState::load(&ctx, editor_id).unwrap();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(0))));
    state.store(&ctx, editor_id);
    h.run_steps(1);

    // Search keeps keyboard focus while selecting real Unicode character ranges.
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    set_field(&mut h, "Find", "city");
    h.get_by_label("1 of 5");
    assert_eq!(selected_text(&h, &ctx, editor_id), "city");
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Find")
            .is_focused()
    );
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("2 of 5");
    assert_eq!(editor(&h), SQL, "Enter in Find must not edit SQL");
    h.key_press_modifiers(Modifiers::COMMAND, Key::G);
    h.run_steps(3);
    h.get_by_label("3 of 5");
    assert_eq!(selected_text(&h, &ctx, editor_id), "CITY");
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    h.run_steps(3);
    h.get_by_label("2 of 5");
    h.get_by_label("Previous Occurrence").click();
    h.run_steps(3);
    h.get_by_label("1 of 5");
    h.get_by_label("Previous Occurrence").click();
    h.run_steps(3);
    h.get_by_label("5 of 5");
    h.get_by_label("Next Occurrence").focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::G);
    h.run_steps(3);
    h.get_by_label("1 of 5");
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    h.run_steps(3);
    h.get_by_label("5 of 5");
    shot(&mut h, "31-console-find");

    // Whole words excludes city_id; Match Case also excludes CITY.
    h.get_by_label("Words").click();
    h.run_steps(3);
    h.get_by_label_contains("of 4");
    h.get_by_label("Match Case").click();
    h.run_steps(3);
    h.get_by_label_contains("of 3");
    assert_eq!(selected_text(&h, &ctx, editor_id), "city");
    h.get_by_label("Match Case").click();
    h.run_steps(3);
    set_field(&mut h, "Find", "łódź");
    h.get_by_label_contains("of 2");
    assert_eq!(selected_text(&h, &ctx, editor_id), "Łódź");
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(
        h.get_by_role(Role::MultilineTextInput).is_focused(),
        "focused {:?}, editor {:?}, find visible {}",
        ctx.memory(|m| m.focused()),
        editor_id,
        h.query_by_label("Close Find").is_some()
    );
    assert!(h.query_by_label("Close Find").is_none());
    assert_eq!(selected_text(&h, &ctx, editor_id), "Łódź");

    // Finding text inside a folded block reveals it without altering the source.
    h.get_by_label("Collapse line 3").click();
    h.run_steps(3);
    assert!(!editor(&h).contains("Łódź"));
    focus_editor(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(3);
    set_field(&mut h, "Find", "łódź");
    assert_eq!(editor(&h), SQL);
    assert_eq!(saved_sql(&config), SQL);
    assert_eq!(selected_text(&h, &ctx, editor_id), "Łódź");

    // Replace affects the active match, and Replace All is one undoable edit.
    h.key_press_modifiers(Modifiers::COMMAND, Key::R);
    h.run_steps(3);
    set_field(&mut h, "Replace with", "Gdańsk");
    shot(&mut h, "32-console-replace");
    let active = selection(&ctx, editor_id);
    let mut expected_single: String = SQL.chars().take(active.start).collect();
    expected_single.push_str("Gdańsk");
    expected_single.extend(SQL.chars().skip(active.end));
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(editor(&h), expected_single);
    wait_for_saved_sql(&mut h, &config, &expected_single);
    assert_eq!(saved_sql(&config), expected_single);
    focus_editor(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), SQL);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), expected_single);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), SQL);
    h.get_by_role_and_label(Role::Button, "Replace All").click();
    h.run_steps(3);
    let expected_all = SQL.replace("Łódź", "Gdańsk");
    assert_eq!(editor(&h), expected_all);
    wait_for_saved_sql(&mut h, &config, &expected_all);
    assert_eq!(saved_sql(&config), expected_all);
    focus_editor(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), SQL);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), expected_all);
    h.get_by_label("No matches");
    assert!(
        h.get_by_role_and_label(Role::Button, "Replace All")
            .accesskit_node()
            .is_disabled()
    );

    // Line and column are one based. An out-of-range column stops at line end.
    h.get_by_label("Close Find").click();
    h.run_steps(3);
    h.key_press_modifiers(Modifiers::COMMAND, Key::L);
    h.run_steps(3);
    set_field(&mut h, "Line:Column", "9999");
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Line:Column")
            .is_some()
    );
    set_field(&mut h, "Line:Column", "3:8");
    let position_field = h
        .get_by_role_and_label(Role::TextInput, "Line:Column")
        .rect();
    let go_button = h.get_by_role_and_label(Role::Button, "Go").rect();
    assert!(
        go_button.bottom() - position_field.top() < 120.0,
        "Go to keeps its actions next to the input"
    );
    shot(&mut h, "33-console-goto");
    h.key_press(Key::Enter);
    h.run_steps(3);
    let line_3 = expected_all
        .lines()
        .take(2)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>();
    assert_eq!(selection(&ctx, editor_id), line_3 + 7..line_3 + 7);
    assert!(h.get_by_role(Role::MultilineTextInput).is_focused());
    h.key_press_modifiers(Modifiers::COMMAND, Key::L);
    h.run_steps(3);
    set_field(&mut h, "Line:Column", "5:9999");
    h.key_press(Key::Enter);
    h.run_steps(3);
    let line_5_end = expected_all
        .lines()
        .take(5)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>()
        - 1;
    assert_eq!(selection(&ctx, editor_id), line_5_end..line_5_end);
    assert_eq!(editor(&h), expected_all);
    assert_eq!(saved_sql(&config), expected_all);

    // A fresh editor change after Undo discards the abandoned replacement redo.
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), SQL);
    let range = selection(&ctx, editor_id);
    let mut edited: String = SQL.chars().take(range.start).collect();
    edited.push_str("-- new edit\n");
    edited.extend(SQL.chars().skip(range.end));
    h.get_by_role(Role::MultilineTextInput)
        .type_text("-- new edit\n");
    h.run_steps(3);
    assert_eq!(editor(&h), edited);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), edited);
    assert_eq!(saved_sql(&config), edited);

    std::fs::remove_dir_all(dir).ok();
}
