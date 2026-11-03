//! Sessions whose connection the server closed reconnect on the next use,
//! except when a transaction was open, against the Docker databases.

use dbeast::backend::{Backend, Event, RunRequest};
use dbeast::model::{DataSource, DriverKind, ExecutionContext, Page, Value};
use std::time::{Duration, Instant};

fn source(driver: DriverKind, port: u16) -> Option<DataSource> {
    if let Err(error) = std::net::TcpStream::connect(("127.0.0.1", port)) {
        eprintln!("skipping {driver:?} reconnect test: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(driver);
    ds.host = "127.0.0.1".into();
    ds.port = port;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    Some(ds)
}

#[derive(Default)]
struct Run {
    results: Vec<Result<Vec<Vec<Value>>, String>>,
    notices: Vec<String>,
    links: Vec<bool>,
    in_tx: bool,
}

fn run(backend: &Backend, ds: &DataSource, console_id: u64, sql: &str, manual_tx: bool) -> Run {
    backend.run(RunRequest {
        console_id,
        ds: ds.clone(),
        statements: vec![sql.to_owned()],
        page: Page::first(500),
        manual_tx,
        context: ExecutionContext::default(),
        expected_context: None,
    });
    let mut run = Run::default();
    loop {
        match backend.rx.recv_timeout(Duration::from_secs(20)).expect("backend event") {
            Event::StatementDone { console_id: c, result, .. } if c == console_id => {
                run.results.push(result.map(|result| result.rows));
            }
            Event::Notice { console_id: c, text } if c == console_id => run.notices.push(text),
            Event::SourceLink { open, .. } => run.links.push(open),
            Event::RunFinished { console_id: c, in_tx } if c == console_id => {
                run.in_tx = in_tx;
                return run;
            }
            _ => {}
        }
    }
}

fn single(run: &Run) -> Value {
    let rows = run.results.last().unwrap().as_ref().expect("statement succeeds");
    rows[0][0].clone()
}

/// Waits until the backend reports the source's connection state.
fn wait_link(backend: &Backend, open: bool) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) {
        if let Ok(Event::SourceLink { open: reported, .. }) =
            backend.rx.recv_timeout(Duration::from_millis(200))
            && reported == open
        {
            return;
        }
    }
    panic!("no SourceLink {{ open: {open} }} event");
}

#[test]
fn postgres_session_closed_by_server_reconnects() {
    let Some(ds) = source(DriverKind::Postgres, 55432) else {
        return;
    };
    let backend = Backend::new(egui::Context::default());
    let pid = single(&run(&backend, &ds, 1, "select pg_backend_pid()", false));
    let killed = run(&backend, &ds, 2, &format!("select pg_terminate_backend({pid})"), false);
    assert_eq!(single(&killed), Value::Bool(true));
    // Idle sessions closed by the server are noticed without running anything.
    wait_link(&backend, false);
    let after = run(&backend, &ds, 1, "select pg_backend_pid()", false);
    assert_ne!(single(&after), pid, "a new session serves the query");
    assert!(
        after.notices.iter().any(|notice| notice.contains("reconnected")),
        "{:?}",
        after.notices
    );
    assert_eq!(after.links, [true], "the source is shown as connected again");

    // An open transaction is never replaced silently.
    let pid = single(&run(&backend, &ds, 3, "select pg_backend_pid()", true));
    run(&backend, &ds, 2, &format!("select pg_terminate_backend({pid})"), false);
    wait_link(&backend, false);
    let lost = run(&backend, &ds, 3, "select 1", true);
    let error = lost.results[0].as_ref().unwrap_err();
    assert!(error.contains("uncommitted changes are lost"), "{error}");
    assert!(!lost.in_tx);
    let again = run(&backend, &ds, 3, "select 1", false);
    assert_eq!(single(&again), Value::Int(1));
}

#[test]
fn mysql_session_killed_by_server_reconnects() {
    let Some(ds) = source(DriverKind::MySql, 53306) else {
        return;
    };
    let backend = Backend::new(egui::Context::default());
    let id = single(&run(&backend, &ds, 1, "select connection_id()", false));
    let killed = run(&backend, &ds, 2, &format!("kill {id}"), false);
    assert!(killed.results[0].is_ok(), "{:?}", killed.results);
    // MySQL gives no sign of a closed session until it is used. Within the
    // idle check interval the statement fails; it is never retried.
    let failed = run(&backend, &ds, 1, "select 1", false);
    assert!(failed.results[0].is_err());
    let after = run(&backend, &ds, 1, "select connection_id()", false);
    assert_ne!(single(&after), id, "a new session serves the query");
    assert!(after.notices.iter().any(|notice| notice.contains("reconnected")));
}
