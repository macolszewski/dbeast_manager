//! Console editor: Format (⌥⌘L) and code folding, without a database.

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

type H<'a> = Harness<'a, dbeast::ui::App>;

const SQL: &str = "-- remove duplicates\nwith ranked as (select id, row_number() over (partition by translation_key order by last_change desc) as rn from translations_translation where translation_key in (select translation_key from translations_translation where translation_key ~ '^\\s')) delete from translations_translation t using ranked r where t.id = r.id and r.rn > 1;";

fn editor(h: &H<'_>) -> String {
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .value()
        .unwrap_or_default()
}

fn shot(h: &mut H<'_>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    h.render()
        .unwrap()
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

#[test]
fn format_and_fold() {
    let dir = std::env::temp_dir().join(format!("dbeast_editor_{}", std::process::id()));
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
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);

    // Format lays the statement out.
    h.get_by_label("Format").click();
    h.run_steps(3);
    let formatted = editor(&h);
    assert!(
        formatted.starts_with(
            "-- remove duplicates\nWITH ranked AS (\n    SELECT\n        id,\n        row_number() OVER (\n            PARTITION BY\n                translation_key\n"
        ),
        "{formatted}"
    );
    assert!(formatted.ends_with("DELETE FROM\n    translations_translation t\nUSING\n    ranked r\nWHERE\n    t.id = r.id\n    AND r.rn > 1;\n"), "{formatted}");
    shot(&mut h, "18-formatted");

    // The subquery of the CTE (line 3) folds into a summary.
    h.get_by_label("Collapse line 3").click();
    h.run_steps(3);
    assert!(
        h.query_by_label("SELECT id, … FROM translations_translation")
            .is_some()
    );
    assert!(
        !editor(&h).contains("row_number"),
        "folded text is hidden in the editor"
    );
    shot(&mut h, "19-folded");

    // Copying gets the full text back, and the saved console keeps it.
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.event(egui::Event::Copy);
    h.run_steps(1);
    let copied: Vec<String> = h
        .output()
        .platform_output
        .commands
        .iter()
        .filter_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert!(
        copied.iter().any(|t| t.contains("row_number() OVER (")),
        "{copied:?}"
    );
    h.key_press(Key::ArrowRight);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text("\n-- end");
    h.run_steps(3);
    // The console text is kept in the session, written every two seconds.
    let session = config.with_file_name("session.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let saved = std::fs::read_to_string(&session).unwrap_or_default();
        if saved.contains("row_number() OVER (") && saved.contains("-- end") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{saved}");
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Clicking the summary unfolds it.
    h.get_by_label("SELECT id, … FROM translations_translation")
        .click();
    h.run_steps(3);
    assert!(editor(&h).contains("row_number"));

    // ⌘⇧- folds everything, ⌘⇧= unfolds everything.
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Minus);
    h.run_steps(3);
    // Only the comments stay visible next to the folded statement.
    let all_folded = editor(&h);
    assert!(
        !all_folded.contains("SELECT") && all_folded.starts_with("-- remove duplicates\n"),
        "{all_folded}"
    );
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Equals);
    h.run_steps(3);
    assert!(editor(&h).contains("row_number"));

    std::fs::remove_dir_all(dir).ok();
}
