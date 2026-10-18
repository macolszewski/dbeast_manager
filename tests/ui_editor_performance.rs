//! Large-console regressions and an opt-in CPU UI-frame benchmark.
//!
//! Run the benchmark with:
//! cargo test --release --test ui_editor_performance editor_ui_cpu_frames -- --ignored --nocapture
//! It measures egui/AccessKit frame construction through Harness::step, not GPU
//! rendering, tessellation, an operating-system window, or application startup.

use egui::accesskit::Role;
use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static CONFIG_ENVIRONMENT: Mutex<()> = Mutex::new(());
const MARKER: &str = "needle_Żółw";

struct MeasuredApp {
    app: dbeast::ui::App,
    first_ui: Option<Duration>,
}

impl eframe::App for MeasuredApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.first_ui.is_none() {
            let start = Instant::now();
            self.app.ui(ui, frame);
            self.first_ui = Some(start.elapsed());
        } else {
            self.app.ui(ui, frame);
        }
    }
}

type H<'a> = Harness<'a, MeasuredApp>;

struct Fixture {
    dir: PathBuf,
    config: PathBuf,
    previous_config: Option<std::ffi::OsString>,
}

impl Fixture {
    fn new(sql: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("dbeast_large_editor_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        std::fs::write(
            &config,
            serde_json::json!({"consoles": [{"title": "large.sql", "ds_id": "none", "sql": sql}]})
                .to_string(),
        )
        .unwrap();
        let previous_config = std::env::var_os("DBEAST_CONFIG");
        // SAFETY: all tests hold CONFIG_ENVIRONMENT throughout the app lifetime.
        unsafe { std::env::set_var("DBEAST_CONFIG", &config) };
        Self {
            dir,
            config,
            previous_config,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: the app is dropped before this fixture, under the same lock.
        unsafe {
            if let Some(previous) = &self.previous_config {
                std::env::set_var("DBEAST_CONFIG", previous);
            } else {
                std::env::remove_var("DBEAST_CONFIG");
            }
        }
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn large_sql(min_bytes: usize) -> String {
    let mut sql = String::with_capacity(min_bytes + 512);
    let mut batch = 0;
    while sql.len() < min_bytes {
        writeln!(
            sql,
            "-- Batch {batch}: Łódź, żółć and 😀\nSELECT item_id, 'Łódź' AS city,\n       (SELECT count(*) FROM inventory WHERE active = true) AS total\nFROM inventory WHERE item_id = {batch};\n"
        )
        .unwrap();
        batch += 1;
    }
    writeln!(
        sql,
        "-- Unique target near the end\nselect '{MARKER}' as marker, (select 42) as answer;"
    )
    .unwrap();
    sql
}

fn focus_editor(h: &mut H<'_>) {
    h.get_by_role(Role::MultilineTextInput).focus();
    h.run_steps(1);
}

fn search(h: &mut H<'_>, query: &str) {
    focus_editor(h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(2);
    h.get_by_role_and_label(Role::TextInput, "Find").focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Find")
        .type_text(query);
    h.run_steps(3);
}

fn editor(h: &H<'_>) -> String {
    h.get_by_role(Role::MultilineTextInput)
        .value()
        .unwrap_or_default()
}

fn selection(ctx: &egui::Context, id: egui::Id) -> std::ops::Range<usize> {
    let range = egui::TextEdit::load_state(ctx, id)
        .unwrap()
        .cursor
        .char_range()
        .unwrap();
    range.primary.index.0.min(range.secondary.index.0)
        ..range.primary.index.0.max(range.secondary.index.0)
}

fn set_selection(ctx: &egui::Context, id: egui::Id, range: std::ops::Range<usize>) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(range.start),
        CCursor::new(range.end),
    )));
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

fn selected_text(h: &H<'_>, ctx: &egui::Context, id: egui::Id) -> String {
    let range = selection(ctx, id);
    editor(h)
        .chars()
        .skip(range.start)
        .take(range.len())
        .collect()
}

fn saved_sql(fixture: &Fixture) -> String {
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    saved["consoles"][0]["sql"].as_str().unwrap().to_owned()
}

/// The configuration is written after edits pause, by a background writer, so
/// the file catches up a few frames after the change; `step` runs a frame.
fn wait_for_saved_sql(
    fixture: &Fixture,
    expected: impl Fn(&str) -> bool,
    mut step: impl FnMut(),
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saved = saved_sql(fixture);
        if expected(&saved) || Instant::now() > deadline {
            return saved;
        }
        step();
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn large_unicode_console_keeps_search_folds_edits_and_source_changes_in_sync() {
    let _environment_guard = CONFIG_ENVIRONMENT.lock().unwrap();
    let sql = large_sql(100_000).replace(
        &format!("select '{MARKER}' as marker, (select 42) as answer;"),
        &format!("select\n    '{MARKER}' as marker,\n    (select 42) as answer;"),
    );
    let fixture = Fixture::new(&sql);
    let mut context = None;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| {
            context = Some(cc.egui_ctx.clone());
            MeasuredApp {
                app: dbeast::ui::App::new_for_test(cc),
                first_ui: None,
            }
        });
    let ctx = context.unwrap();
    focus_editor(&mut h);
    let editor_id = ctx.memory(|memory| memory.focused()).unwrap();
    assert_eq!(editor(&h), sql);

    // A Unicode match near the end uses character offsets, including after wrap changes.
    search(&mut h, MARKER);
    h.get_by_label("1 of 1");
    let marker_start = sql[..sql.find(MARKER).unwrap()].chars().count();
    let marker_range = marker_start..marker_start + MARKER.chars().count();
    assert_eq!(selection(&ctx, editor_id), marker_range);
    assert_eq!(selected_text(&h, &ctx, editor_id), MARKER);
    h.set_size(egui::vec2(1100.0, 760.0));
    h.run_steps(3);
    h.key_press_modifiers(Modifiers::COMMAND, Key::G);
    h.run_steps(3);
    assert_eq!(selected_text(&h, &ctx, editor_id), MARKER);
    let screenshot =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/41-editor-large.png");
    h.render().unwrap().save(screenshot).unwrap();
    h.key_press(Key::Escape);
    h.run_steps(3);

    // Copy expands every folded placeholder; Find reveals its hidden target.
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Minus);
    h.run_steps(3);
    assert!(!editor(&h).contains(MARKER));
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(1);
    h.event(egui::Event::Copy);
    h.run_steps(1);
    assert!(
        h.output().platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == &sql)
        })
    );
    h.key_press(Key::ArrowRight);
    h.run_steps(1);
    search(&mut h, MARKER);
    assert_eq!(selected_text(&h, &ctx, editor_id), MARKER);
    assert!(editor(&h).len() < sql.len(), "other statements stay folded");
    assert_eq!(wait_for_saved_sql(&fixture, |saved| saved == sql, || h.step()), sql);

    // Replacing a match with itself still unfolds the editor. A text-only
    // invalidation would leave stale placeholders in the cached projection.
    h.key_press_modifiers(Modifiers::COMMAND, Key::R);
    h.run_steps(2);
    h.get_by_role_and_label(Role::TextInput, "Replace with")
        .focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, "Replace with")
        .type_text(MARKER);
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "Replace All").click();
    h.run_steps(3);
    assert_eq!(editor(&h), sql);
    let saved = wait_for_saved_sql(&fixture, |saved| saved == sql, || h.step());
    assert_eq!(saved, sql);
    assert!(
        saved
            .chars()
            .all(|character| dbeast::folding::placeholder_index(character).is_none())
    );
    h.key_press(Key::Escape);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Equals);
    h.run_steps(3);
    assert_eq!(editor(&h), sql);

    // Editing a selected match invalidates the caches and preserves undo selection.
    search(&mut h, MARKER);
    h.key_press(Key::Escape);
    h.run_steps(2);
    const REPLACEMENT: &str = "renamed_Łódź_🐢";
    h.get_by_role(Role::MultilineTextInput)
        .type_text(REPLACEMENT);
    h.run_steps(3);
    let edited = sql.replace(MARKER, REPLACEMENT);
    assert_eq!(editor(&h), edited);
    assert_eq!(wait_for_saved_sql(&fixture, |saved| saved == edited, || h.step()), edited);
    let after_edit = marker_start + REPLACEMENT.chars().count();
    assert_eq!(selection(&ctx, editor_id), after_edit..after_edit);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), sql);
    assert_eq!(selection(&ctx, editor_id), marker_range);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), edited);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);

    // Formatting mutates source outside TextEdit and must refresh the same caches.
    let tail_byte = sql.rfind("select\n").unwrap();
    let tail_chars = sql[..tail_byte].chars().count()..sql.chars().count();
    set_selection(&ctx, editor_id, tail_chars.clone());
    h.run_steps(2);
    if h.query_by_label("Format").is_none() {
        h.get_by_label("More Console Actions").click();
        h.run_steps(3);
    }
    h.get_by_label("Format").click();
    h.run_steps(3);
    let formatted = editor(&h);
    assert_ne!(formatted, sql);
    assert!(formatted.starts_with(&sql[..tail_byte]));
    assert!(formatted[tail_byte..].starts_with("SELECT\n"));
    assert!(formatted[tail_byte..].contains(MARKER));
    assert_eq!(wait_for_saved_sql(&fixture, |saved| saved == formatted, || h.step()), formatted);
    focus_editor(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), sql);
    assert_eq!(selection(&ctx, editor_id), tail_chars);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(3);
    assert_eq!(editor(&h), formatted);

    // Deleting a folded token reveals its text without deleting the hidden SQL.
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Minus);
    h.run_steps(3);
    let folded = editor(&h);
    let first_fold = folded
        .chars()
        .position(|character| dbeast::folding::placeholder_index(character).is_some())
        .unwrap();
    set_selection(&ctx, editor_id, first_fold + 1..first_fold + 1);
    h.run_steps(1);
    h.key_press(Key::Backspace);
    h.run_steps(3);
    assert!(editor(&h).len() > folded.len());
    assert_eq!(wait_for_saved_sql(&fixture, |saved| saved == formatted, || h.step()), formatted);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Equals);
    h.run_steps(3);
    assert_eq!(editor(&h), formatted);
    drop(h);
    drop(fixture);
}

fn measure_frames(h: &mut H<'_>, samples: usize, scrolling: bool) -> (f64, f64) {
    let mut elapsed = Vec::with_capacity(samples);
    for index in 0..samples {
        if scrolling {
            h.event(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, if index < samples / 2 { -100.0 } else { 100.0 }),
                phase: egui::TouchPhase::Move,
                modifiers: Modifiers::NONE,
            });
        }
        let start = Instant::now();
        h.step();
        elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    elapsed.sort_unstable_by(f64::total_cmp);
    (
        elapsed[samples / 2],
        elapsed[(samples * 95 / 100).min(samples - 1)],
    )
}

fn report_frames(h: &mut H<'_>, bytes: usize, phase: &str, scrolling: bool) {
    h.run_steps(3);
    let (p50, p95) = measure_frames(h, 20, scrolling);
    eprintln!(
        "editor_ui_cpu bytes={bytes} phase={phase} samples=20 p50_ms={p50:.3} p95_ms={p95:.3}"
    );
}

#[test]
#[ignore = "manual release benchmark of CPU UI-frame construction; no rendering"]
#[allow(clippy::assertions_on_constants)] // Prevent misleading debug-build measurements.
fn editor_ui_cpu_frames() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    let _environment_guard = CONFIG_ENVIRONMENT.lock().unwrap();
    eprintln!(
        "method: 1440x900, 1x pixels, 60Hz simulated input; whole Harness::step CPU time including egui and AccessKit, without render/tessellate/GPU; fixture/app setup excluded from frame samples"
    );
    for target in [100_000, 1_000_000] {
        let sql = large_sql(target);
        let fixture = Fixture::new(&sql);
        let build_start = Instant::now();
        // The default lazy renderer is never invoked: no .wgpu() or .render().
        let mut h = Harness::builder()
            .with_size([1440.0, 900.0])
            .with_pixels_per_point(1.0)
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MeasuredApp {
                app: dbeast::ui::App::new_for_test(cc),
                first_ui: None,
            });
        eprintln!(
            "editor_ui_cpu bytes={} lines={} first_app_ui_ms={:.3} harness_build_ms={:.3}",
            sql.len(),
            sql.lines().count(),
            h.state().first_ui.unwrap().as_secs_f64() * 1000.0,
            build_start.elapsed().as_secs_f64() * 1000.0
        );
        focus_editor(&mut h);
        report_frames(&mut h, sql.len(), "idle", false);
        h.event(egui::Event::PointerMoved(egui::pos2(900.0, 300.0)));
        h.step();
        report_frames(&mut h, sql.len(), "scroll", true);
        search(&mut h, "Łódź");
        report_frames(&mut h, sql.len(), "find_idle", false);
        h.key_press(Key::Escape);
        h.run_steps(2);
        focus_editor(&mut h);
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Minus);
        h.run_steps(3);
        report_frames(&mut h, sql.len(), "folded_idle", false);
        report_frames(&mut h, sql.len(), "folded_scroll", true);
        drop(h);
        drop(fixture);
    }
}

fn measure_without_accesskit(
    ctx: &egui::Context,
    app: &mut MeasuredApp,
    frame: &mut eframe::Frame,
    input: &mut egui::RawInput,
    samples: usize,
    scrolling: bool,
) -> (f64, f64) {
    let mut elapsed = Vec::with_capacity(samples);
    let mut previous_output = None;
    for index in 0..samples {
        input.time = Some(input.time.unwrap_or_default() + 1.0 / 60.0);
        input.events.clear();
        if scrolling {
            input.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, if index < samples / 2 { -100.0 } else { 100.0 }),
                phase: egui::TouchPhase::Move,
                modifiers: Modifiers::NONE,
            });
        }
        let start = Instant::now();
        previous_output = Some(ctx.run_ui(input.clone(), |ui| {
            eframe::App::ui(app, ui, frame);
        }));
        elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(
            previous_output
                .as_ref()
                .unwrap()
                .platform_output
                .accesskit_update
                .is_none(),
            "this benchmark must not produce accessibility trees"
        );
    }
    drop(previous_output);
    elapsed.sort_unstable_by(f64::total_cmp);
    (
        elapsed[samples / 2],
        elapsed[(samples * 95 / 100).min(samples - 1)],
    )
}

/// Separate from editor_ui_cpu_frames: setup uses Harness to verify the editor
/// and pointer target, then consumes it. Timed frames run Context::run_ui with
/// AccessKit disabled, so no test-tree maintenance or accessibility generation
/// is included. Frame input, viewport, fonts and SQL match the first benchmark.
#[test]
#[ignore = "manual release CPU-frame benchmark without AccessKit or rendering"]
#[allow(clippy::assertions_on_constants)] // Prevent misleading debug-build measurements.
fn editor_cpu_frames_without_accesskit() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    let _environment_guard = CONFIG_ENVIRONMENT.lock().unwrap();
    eprintln!(
        "method: 1440x900, 1x pixels, 60Hz simulated input; Context::run_ui CPU time, AccessKit disabled after verified Harness setup; no render/tessellate/GPU; setup excluded"
    );
    for target in [100_000, 1_000_000] {
        let sql = large_sql(target);
        let fixture = Fixture::new(&sql);
        let mut context = None;
        let mut h = Harness::builder()
            .with_size([1440.0, 900.0])
            .with_pixels_per_point(1.0)
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| {
                context = Some(cc.egui_ctx.clone());
                MeasuredApp {
                    app: dbeast::ui::App::new_for_test(cc),
                    first_ui: None,
                }
            });
        focus_editor(&mut h);
        assert_eq!(editor(&h), sql);
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused());
        let pointer = egui::pos2(900.0, 300.0);
        assert!(
            h.get_by_role(Role::MultilineTextInput)
                .rect()
                .contains(pointer)
        );
        h.event(egui::Event::PointerMoved(pointer));
        h.step();
        let mut input = h.input().clone();
        input.events.clear();
        let mut app = h.into_state();
        let ctx = context.unwrap();
        assert!(!ctx.any_popup_open());
        ctx.disable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let _ = measure_without_accesskit(&ctx, &mut app, &mut frame, &mut input, 3, false);
        for (phase, scrolling) in [("idle", false), ("scroll", true)] {
            let (p50, p95) =
                measure_without_accesskit(&ctx, &mut app, &mut frame, &mut input, 20, scrolling);
            eprintln!(
                "editor_cpu_without_accesskit bytes={} phase={phase} samples=20 p50_ms={p50:.3} p95_ms={p95:.3}",
                sql.len()
            );
        }

        // Three idle frames let egui evict untouched paragraph layout entries.
        // Time the subsequent text-input frame, which hands the config save to
        // the background writer. File reads that verify the edit happen outside
        // the timed region.
        let editor_id = ctx.memory(|memory| memory.focused()).unwrap();
        let mut typing = Vec::with_capacity(5);
        let mut expected_bytes = sql.len();
        for _ in 0..5 {
            let _ = measure_without_accesskit(&ctx, &mut app, &mut frame, &mut input, 3, false);
            assert_eq!(ctx.memory(|memory| memory.focused()), Some(editor_id));
            assert!(selection(&ctx, editor_id).is_empty());
            input.time = Some(input.time.unwrap_or_default() + 1.0 / 60.0);
            input.events.clear();
            input.events.push(egui::Event::Text(" ".into()));
            let start = Instant::now();
            let output = ctx.run_ui(input.clone(), |ui| {
                eframe::App::ui(&mut app, ui, &mut frame);
            });
            typing.push(start.elapsed().as_secs_f64() * 1000.0);
            assert!(output.platform_output.accesskit_update.is_none());
            expected_bytes += 1;
            let mut idle = input.clone();
            idle.events.clear();
            let saved = wait_for_saved_sql(
                &fixture,
                |saved| saved.len() == expected_bytes,
                || {
                    idle.time = Some(idle.time.unwrap_or_default() + 1.0 / 60.0);
                    let _ = ctx.run_ui(idle.clone(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
                },
            );
            input.time = idle.time;
            assert_eq!(saved.len(), expected_bytes);
        }
        typing.sort_unstable_by(f64::total_cmp);
        eprintln!(
            "editor_cpu_without_accesskit bytes={} phase=typing_after_idle samples=5 idle_before_each=3 config_save=deferred p50_ms={:.3} p95_ms={:.3}",
            sql.len(),
            typing[2],
            typing[4]
        );
        drop(app);
        drop(fixture);
    }
}
