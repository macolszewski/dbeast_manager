//! The Explorer compares fresh, read-only DDL from cached object choices.

mod common;

use dbeast::config::Config;
use dbeast::model::{DataSource, DbObject, DriverKind, ObjectKind};
use egui::accesskit::{Role, Toggled};
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;
const TABLE: &str = "accounts Żółw";
const ORIGIN: &str = "ddl-origin";
const TARGET: &str = "ddl-target";
const SCRATCH: &str = "SELECT 'keep this SQL unchanged';";

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
    let deadline = Instant::now() + Duration::from_secs(20);
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
            shot(h, "ddl-diff-timeout.png");
            panic!("timed out waiting for {what}; nodes: {nodes:#?}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
}

fn copy_ddl(h: &mut H<'_>, label: &str) -> String {
    h.get_by_label(label).click();
    let mut copied = None;
    for _ in 0..3 {
        h.step();
        for command in &h.output().platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                copied = Some(text.clone());
            }
        }
    }
    copied.expect("copy action must emit the complete original DDL")
}

fn search(h: &mut H<'_>, value: &str) {
    h.get_by_role_and_label(Role::TextInput, "Find Object")
        .click();
    key(h, Modifiers::COMMAND, Key::A);
    h.event(egui::Event::Text(value.into()));
    h.run_steps(3);
}

fn choice<'h>(h: &'h H<'_>, source: &str, object: &str) -> egui_kittest::Node<'h> {
    let prefix = format!("{source} / main.{object} [");
    h.query_all_by_role(Role::Button)
        .find(|node| {
            node.accesskit_node()
                .label()
                .is_some_and(|label| label.starts_with(&prefix))
        })
        .unwrap_or_else(|| panic!("missing cached object choice {prefix:?}: {h:#?}"))
}

fn open_compare(h: &mut H<'_>) {
    // Only the origin branch is expanded, so this selects an Explorer table row.
    h.query_all_by_role_and_label(Role::Button, TABLE)
        .find(|node| node.accesskit_node().toggled().is_some())
        .expect("origin table in Explorer")
        .click_secondary();
    h.run_steps(3);
    click(h, "Compare Structure With");
    h.get_by_role_and_label(Role::TextInput, "Find Object");
}

fn difference_position(h: &H<'_>) -> (usize, usize) {
    h.query_all_by_role(Role::Label)
        .find_map(|node| {
            let text = node.value().or_else(|| node.accesskit_node().label())?;
            let (current, total) = text.strip_prefix("Difference ")?.split_once(" of ")?;
            Some((current.parse().unwrap(), total.parse().unwrap()))
        })
        .expect("accessible current difference counter")
}

fn selected_line(h: &H<'_>) -> String {
    h.query_all_by_role(Role::Button)
        .find_map(|node| {
            let label = node.accesskit_node().label()?;
            ((label.starts_with("Origin line ") || label.starts_with("Target line "))
                && node.accesskit_node().toggled() == Some(Toggled::True))
            .then(|| label.to_owned())
        })
        .expect("active difference has a visible, selected line")
}

fn wait_diff(h: &mut H<'_>) {
    wait_until(h, "fresh DDL snapshots", |h| {
        h.query_by_label("Copy Origin DDL").is_some()
            && h.query_by_label("Copy Target DDL").is_some()
    });
    h.get_by_label("Change Objects");
    h.get_by_label("Definition coverage");
}

struct Fixture {
    directory: PathBuf,
    origin: PathBuf,
    target: PathBuf,
    unopened: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("dbeast_ddl_ui_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let origin = directory.join("origin.sqlite");
        let target = directory.join("target.sqlite");
        let unopened = directory.join("must-stay-unopened.sqlite");
        rusqlite::Connection::open(&origin).unwrap().execute_batch(
            "CREATE TABLE \"accounts Żółw\" (\n  id INTEGER PRIMARY KEY,\n  name TEXT NOT NULL,\n  balance INTEGER DEFAULT 0,\n  city TEXT,\n  note TEXT\n);\n\
             CREATE INDEX account_city ON \"accounts Żółw\"(city);\n\
             INSERT INTO \"accounts Żółw\"(id,name,city) VALUES(1,'Ada','Łódź');\n\
             CREATE VIEW accounts_view AS SELECT id,name FROM \"accounts Żółw\";"
        ).unwrap();
        rusqlite::Connection::open(&target).unwrap().execute_batch(
            "CREATE TABLE \"accounts Żółw\" (\n  id INTEGER PRIMARY KEY,\n  name TEXT,\n  balance REAL DEFAULT 0,\n  city TEXT,\n  note TEXT,\n  active INTEGER DEFAULT 1\n);\n\
             CREATE INDEX account_name ON \"accounts Żółw\"(name);\n\
             INSERT INTO \"accounts Żółw\"(id,name,city) VALUES(2,'Iga','Gdańsk');\n\
             CREATE VIEW target_view AS SELECT id,name FROM \"accounts Żółw\";"
        ).unwrap();
        let make_source = |id: &str, name: &str, path: &Path| {
            let mut ds = DataSource::new(DriverKind::Sqlite);
            ds.id = id.into();
            ds.name = name.into();
            ds.file = path.to_string_lossy().into_owned();
            ds
        };
        let mut pg = DataSource::new(DriverKind::Postgres);
        pg.id = "ddl-other-driver".into();
        pg.name = "Other driver".into();
        pg.port = 1;
        let config = Config {
            data_sources: vec![
                make_source(ORIGIN, "DDL Origin", &origin),
                make_source(TARGET, "DDL Target", &target),
                make_source("ddl-unopened", "Unopened", &unopened),
                pg,
            ],
            files_root: Some(directory.clone()),
            tile_url: String::new(),
            ..Config::default()
        };
        std::fs::write(
            directory.join("config.json"),
            common::legacy_config(&config).unwrap(),
        )
        .unwrap();
        let tree = |table: &str, view: &str| {
            let mut main = DbObject::new(ObjectKind::Schema, "main", &[]);
            main.children = Some(vec![
                DbObject::folder(
                    "tables",
                    &main.path,
                    vec![DbObject::new(ObjectKind::Table, table, &main.path)],
                ),
                DbObject::folder(
                    "views",
                    &main.path,
                    vec![DbObject::new(ObjectKind::View, view, &main.path)],
                ),
            ]);
            main
        };
        let session = serde_json::json!({
            "active": 0,
            "tabs": [{"title":"Scratch", "ds_id":ORIGIN, "kind":{"Console":{"sql":SCRATCH}}, "results":[]}],
            "trees": {
                (ORIGIN): [tree(TABLE, "accounts_view")],
                (TARGET): [tree(TABLE, "target_view")],
                "ddl-unopened": [tree("cached_only", "unopened_view")],
                "ddl-other-driver": [tree("other_driver_table", "other_driver_view")]
            },
            "expanded": [[ORIGIN, []], [ORIGIN, ["main"]], [ORIGIN, ["main", "\u{0}tables"]]],
            "selected": [ORIGIN, ["main", TABLE]]
        });
        std::fs::write(directory.join("session.json"), session.to_string()).unwrap();
        Self {
            directory,
            origin,
            target,
            unopened,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn database_snapshot(path: &Path) -> Vec<(String, String, String)> {
    rusqlite::Connection::open(path)
        .unwrap()
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn explorer_ddl_diff_uses_loaded_choices_and_immutable_read_only_snapshots() {
    let fixture = Fixture::new();
    let origin_before = database_snapshot(&fixture.origin);
    let target_before = database_snapshot(&fixture.target);
    // SAFETY: this binary has one test and no application workers exist yet.
    unsafe {
        std::env::set_var("DBEAST_CONFIG", fixture.directory.join("config.json"));
    }
    let mut h = app();
    open_compare(&mut h);
    assert!(
        h.get_by_label("Compare").accesskit_node().is_disabled(),
        "the target must be an explicit choice"
    );
    choice(&h, "DDL Target", TABLE);
    choice(&h, "Unopened", "cached_only");
    assert!(h.query_all_by_role(Role::Button).all(|node| {
        node.accesskit_node().label().is_none_or(|label| {
            !label.starts_with("Other driver / ")
                && !label.contains(" / main.target_view [")
                && !label.contains(" / main.accounts_view [")
        })
    }));
    assert!(
        !fixture.unopened.exists(),
        "opening the object picker must not fetch metadata"
    );
    search(&mut h, "not-an-object-9481");
    assert!(h.get_by_label("Compare").accesskit_node().is_disabled());
    assert!(h.query_all_by_role(Role::Button).all(|node| {
        node.accesskit_node()
            .label()
            .is_none_or(|label| !label.starts_with("DDL Target / "))
    }));
    search(&mut h, "aCCOUNTS żÓŁW");
    choice(&h, "DDL Target", TABLE).click();
    h.run_steps(3);
    assert!(!h.get_by_label("Compare").accesskit_node().is_disabled());
    shot(&mut h, "67-ddl-diff-setup.png");
    click(&mut h, "Compare");
    wait_diff(&mut h);
    let origin = copy_ddl(&mut h, "Copy Origin DDL");
    let target = copy_ddl(&mut h, "Copy Target DDL");
    assert!(origin.contains("name TEXT NOT NULL") && origin.contains("account_city"));
    assert!(target.contains("balance REAL") && target.contains("account_name"));
    assert!(!origin.contains("account_name") && !target.contains("account_city"));
    assert!(h.query_all_by_label_contains("Origin line ").any(|node| {
        node.accesskit_node()
            .description()
            .is_some_and(|description| description.contains("Changed"))
    }));
    shot(&mut h, "68-ddl-diff.png");
    let (first, differences) = difference_position(&h);
    assert_eq!(first, 1);
    assert!(differences >= 2, "fixture contains separated changes");
    let first_line = selected_line(&h);
    click(&mut h, "Next Difference");
    assert_eq!(difference_position(&h), (2, differences));
    assert_ne!(selected_line(&h), first_line);
    key(&mut h, Modifiers::NONE, Key::F7);
    assert_eq!(difference_position(&h), (2 % differences + 1, differences));
    key(&mut h, Modifiers::SHIFT, Key::F7);
    assert_eq!(difference_position(&h), (2, differences));
    click(&mut h, "Previous Difference");
    assert_eq!(difference_position(&h), (1, differences));
    assert_eq!(selected_line(&h), first_line);
    assert_eq!(copy_ddl(&mut h, "Copy Origin DDL"), origin);
    assert_eq!(copy_ddl(&mut h, "Copy Target DDL"), target);
    assert_eq!(database_snapshot(&fixture.origin), origin_before);
    assert_eq!(database_snapshot(&fixture.target), target_before);
    assert!(!fixture.unopened.exists());

    h.set_size(egui::vec2(1100.0, 500.0));
    h.run_steps(4);
    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 500.0));
    for label in [
        "Close",
        "Copy Origin DDL",
        "Copy Target DDL",
        "Next Difference",
    ] {
        assert!(
            viewport.contains_rect(h.get_by_label(label).rect()),
            "{label} must remain reachable in a short window"
        );
    }
    shot(&mut h, "69-ddl-diff-narrow.png");
    h.set_size(egui::vec2(1440.0, 900.0));
    h.run_steps(3);

    // Background DDL changes do not mutate the completed comparison snapshot.
    rusqlite::Connection::open(&fixture.target)
        .unwrap()
        .execute_batch("ALTER TABLE \"accounts Żółw\" ADD COLUMN fresh_column TEXT")
        .unwrap();
    h.run_steps(3);
    assert_eq!(copy_ddl(&mut h, "Copy Target DDL"), target);
    click(&mut h, "Change Objects");
    // Compare again explicitly, preserving the same pair of objects.
    assert!(!h.get_by_label("Compare").accesskit_node().is_disabled());
    click(&mut h, "Compare");
    wait_diff(&mut h);
    let fresh_target = copy_ddl(&mut h, "Copy Target DDL");
    assert!(fresh_target.contains("fresh_column"));
    click(&mut h, "Change Objects");
    click(&mut h, "Swap");
    click(&mut h, "Compare");
    wait_diff(&mut h);
    assert_eq!(copy_ddl(&mut h, "Copy Origin DDL"), fresh_target);
    assert_eq!(copy_ddl(&mut h, "Copy Target DDL"), origin);
    click(&mut h, "Close");
    h.get_by_label("QL Scratch");
    assert_eq!(
        h.get_by_role(Role::MultilineTextInput).value().as_deref(),
        Some(SCRATCH)
    );
    assert_eq!(
        h.query_all_by_role(Role::Button)
            .filter(|node| node
                .accesskit_node()
                .label()
                .is_some_and(|label| label.starts_with("QL ")))
            .count(),
        1,
        "DDL Diff must not generate executable SQL tabs"
    );
    assert_eq!(database_snapshot(&fixture.origin), origin_before);
    assert!(!fixture.unopened.exists());
    let origin_count: i64 = rusqlite::Connection::open(&fixture.origin)
        .unwrap()
        .query_row("SELECT count(*) FROM \"accounts Żółw\"", [], |row| {
            row.get(0)
        })
        .unwrap();
    let target_count: i64 = rusqlite::Connection::open(&fixture.target)
        .unwrap()
        .query_row("SELECT count(*) FROM \"accounts Żółw\"", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!((origin_count, target_count), (1, 1));
    drop(h);
}
