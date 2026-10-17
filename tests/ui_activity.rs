//! Activity Monitor against the PostgreSQL container: a session holding a
//! lock in an open transaction blocks another one; the dashboard shows the
//! blocking tree and terminating the blocker releases the waiting session.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::time::{Duration, Instant};

type H<'a> = Harness<'a, dbeast::ui::App>;

fn wait_until(h: &mut H<'_>, what: &str, done: impl Fn(&H<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.run_steps(3);
}

fn click(h: &mut H<'_>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn pg() -> dbeast::model::DataSource {
    let mut ds = dbeast::model::DataSource::new(dbeast::model::DriverKind::Postgres);
    ds.port = 55432;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    ds
}

#[test]
fn activity_monitor_shows_blocking_and_terminates_the_blocker() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping: local PostgreSQL is unavailable");
        return;
    }
    let table = format!("activity_lock_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ds = pg();
    let (blocker_pid, waiter) = rt.block_on(async {
        let mut setup = dbeast::drivers::Connection::open(&ds).await.unwrap();
        setup.execute(&format!("DROP TABLE IF EXISTS {table}"), dbeast::model::Page::ALL).await.unwrap();
        setup.execute(&format!("CREATE TABLE {table} (id int PRIMARY KEY, n int)"), dbeast::model::Page::ALL).await.unwrap();
        setup.execute(&format!("INSERT INTO {table} VALUES (1, 0)"), dbeast::model::Page::ALL).await.unwrap();
        let mut blocker = dbeast::drivers::Connection::open(&ds).await.unwrap();
        let pid = blocker.execute("SELECT pg_backend_pid()", dbeast::model::Page::ALL).await.unwrap().rows[0][0].clone();
        blocker.execute("BEGIN", dbeast::model::Page::ALL).await.unwrap();
        blocker.execute(&format!("UPDATE {table} SET n = 1 WHERE id = 1"), dbeast::model::Page::ALL).await.unwrap();
        let waiting_ds = ds.clone();
        let waiting_sql = format!("UPDATE {table} SET n = 2 WHERE id = 1");
        let waiter = tokio::spawn(async move {
            let mut conn = dbeast::drivers::Connection::open(&waiting_ds).await.unwrap();
            conn.execute(&waiting_sql, dbeast::model::Page::ALL).await.map(|_| ())
        });
        // Keep the blocker's connection alive inside the runtime.
        std::mem::forget(blocker);
        (pid, waiter)
    });
    let blocker_pid = match blocker_pid {
        dbeast::model::Value::Int(pid) => pid,
        other => panic!("pid {other:?}"),
    };

    let dir = std::env::temp_dir().join(format!("dbeast_activity_ui_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let config = serde_json::json!({
        "data_sources": [{"id":"local", "name":"Shop PG", "driver":"Postgres",
            "host":"127.0.0.1", "port":55432, "user":"dbeast", "password":"dbeast",
            "database":"dbeast", "file":"", "use_tls":false, "read_only":true}],
        "max_rows": 500,
        "tile_url": ""
    });
    std::fs::write(&path, config.to_string()).unwrap();
    // SAFETY: the only test in this binary, before starting the application.
    unsafe { std::env::set_var("DBEAST_CONFIG", &path) };
    let mut h = Harness::builder()
        .with_size([1600.0, 1000.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(5);
    h.get_all_by_label("Shop PG")
        .find(|n| n.rect().left() < 200.0 && n.rect().top() > 60.0 && n.rect().top() < 300.0)
        .expect("data source in Database Explorer")
        .click_secondary();
    h.run_steps(3);
    click(&mut h, "Activity Monitor");
    wait_until(&mut h, "one blocked session", |h| h.query_by_label("Blocked: 1").is_some());
    assert!(h.query_by_label_contains("Waiting on locks: 1").is_some());
    assert!(h.query_by_label_contains("Idle in transaction: ").is_some());
    let blocker = format!("PID {blocker_pid}");
    assert!(h.query_by_label_contains("blocks 1").is_some(), "the blocking tree names the blocker");
    h.get_all_by_label(&blocker).next().expect("blocker in the tree").click();
    h.run_steps(3);
    wait_until(&mut h, "the details", |h| h.query_by_label("Terminate Session").is_some());
    assert!(h.query_all_by_label_contains(&format!("UPDATE {table} SET n = 1")).count() >= 2, "the query is in the table and the details");
    h.render()
        .unwrap()
        .save(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/103-activity-monitor.png"))
        .unwrap();
    click(&mut h, "Terminate Session");
    // Confirmation first, then the signal.
    click(&mut h, "Terminate");
    wait_until(&mut h, "the terminated blocker", |h| h.query_by_label_contains("Terminate sent").is_some());
    wait_until(&mut h, "nothing blocked", |h| h.query_by_label("Blocked: 0").is_some());
    let finished = rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), waiter).await });
    assert!(matches!(finished, Ok(Ok(Ok(())))), "the waiting UPDATE completes once the blocker is gone");
    drop(h);
    rt.block_on(async {
        let mut conn = dbeast::drivers::Connection::open(&ds).await.unwrap();
        conn.execute(&format!("DROP TABLE {table}"), dbeast::model::Page::ALL).await.unwrap();
    });
    std::fs::remove_dir_all(dir).ok();
}
