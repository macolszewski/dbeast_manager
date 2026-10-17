//! Isolated transport tests. Run `scripts/test-tls.sh`; never uses normal database fixtures.
use dbeast::drivers::Connection;
use dbeast::mcp::{ExecutionRequest, Operation, executor};
use dbeast::model::{TlsMode, DataSource, DriverKind, Page};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn fixture_source(driver: DriverKind, tls: bool) -> DataSource {
    assert_eq!(
        std::env::var("DBEAST_TLS_FIXTURES").as_deref(),
        Ok("1"),
        "run scripts/test-tls.sh"
    );
    let config = PathBuf::from(std::env::var_os("DBEAST_CONFIG").expect("isolated config"));
    let certs = certs();
    assert_eq!(
        config.parent().and_then(|p| p.parent()),
        certs.parent(),
        "config must belong to disposable fixture"
    );
    assert!(
        certs
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("dbeast-tls.")
    );
    let mut source = DataSource::new(driver);
    // Hosts are hardcoded: an environment variable cannot redirect a test outside loopback.
    source.host = "localhost".into();
    source.port = fixture_port(match (driver, tls) {
        (DriverKind::Postgres, true) => "DBEAST_TLS_PG_PORT",
        (DriverKind::Postgres, false) => "DBEAST_PLAIN_PG_PORT",
        (DriverKind::MySql, true) => "DBEAST_TLS_MYSQL_PORT",
        (DriverKind::MySql, false) => "DBEAST_PLAIN_MYSQL_PORT",
        _ => unreachable!(),
    });
    source.database = "dbeast".into();
    source.user = "dbeast".into();
    source.password = "fixture-only".into();
    source.use_tls = tls;
    source.tls.ca_file = certs.join("ca.pem").to_string_lossy().into_owned();
    source
}

fn fixture_port(name: &str) -> u16 {
    let port = std::env::var(name).expect("fixture port").parse().unwrap();
    assert_ne!(port, 0);
    port
}

fn certs() -> PathBuf {
    PathBuf::from(std::env::var_os("DBEAST_TLS_CERTS").expect("fixture certificates"))
}

async fn rejected(source: &DataSource, scenario: &str) {
    assert!(
        Connection::open(source).await.is_err(),
        "{} unexpectedly accepted {scenario}",
        source.driver.label()
    );
    let error = executor::execute(ExecutionRequest {
        source: source.clone(),
        operation: Operation::ReadSql {
            source_id: "tls-fixture".into(),
            sql: "SELECT 1".into(),
        },
        expected_table_identity: None,
        cancellation: CancellationToken::new(),
        execution_lease: None,
    })
    .await
    .err()
    .unwrap_or_else(|| {
        panic!(
            "MCP {} unexpectedly accepted {scenario}",
            source.driver.label()
        )
    });
    assert_eq!(error.code, "execution_failed");
    assert!(
        !error.message.contains(&source.host),
        "MCP must sanitize transport errors"
    );
    assert!(!error.message.contains("fixture-only"));
}

async fn transport(source: &DataSource, expected: &str) {
    let mut connection = Connection::open(source)
        .await
        .unwrap_or_else(|e| panic!("{} connection failed: {e:#}", source.driver.label()));
    assert_eq!(connection.server_info().await.unwrap().transport, expected);
    connection
        .execute("SELECT 1", Page::first(1))
        .await
        .unwrap();
}

async fn mcp_transport(source: &DataSource) {
    let result = executor::execute(ExecutionRequest {
        source: source.clone(),
        operation: Operation::ReadSql {
            source_id: "tls-fixture".into(),
            sql: "SELECT 1".into(),
        },
        expected_table_identity: None,
        cancellation: CancellationToken::new(),
        execution_lease: None,
    })
    .await
    .unwrap_or_else(|error| panic!("MCP {} connection failed: {error}", source.driver.label()));
    assert_eq!(result.value["rows"], serde_json::json!([["1"]]));
}

async fn verified_transport(source: &DataSource, expected: &str) {
    transport(source, expected).await;
    mcp_transport(source).await;
}

async fn verification_matrix(driver: DriverKind) {
    let source = fixture_source(driver, true);
    verified_transport(&source, "TLS (verified)").await;
    for ca in ["ca.der", "ca-bundle.pem"] {
        let mut alternate_ca = source.clone();
        alternate_ca.tls.ca_file = certs().join(ca).to_string_lossy().into_owned();
        verified_transport(&alternate_ca, "TLS (verified)").await;
    }
    let mut invalid = source.clone();
    invalid.tls.ca_file = certs().join("wrong-ca.pem").to_string_lossy().into_owned();
    rejected(&invalid, "untrusted CA").await;
    invalid.tls.ca_file.clear();
    rejected(&invalid, "self-signed CA absent from system trust").await;
    invalid = source.clone();
    invalid.host = "127.0.0.1".into();
    rejected(&invalid, "hostname mismatch").await;
    invalid = source.clone();
    invalid.tls.ca_file = certs().join("missing.pem").to_string_lossy().into_owned();
    rejected(&invalid, "missing CA file").await;

    let mut plain = fixture_source(driver, false);
    verified_transport(&plain, "Unencrypted").await;
    plain.use_tls = true;
    rejected(&plain, "server without TLS (no plaintext fallback)").await;
    plain.use_tls = false;
    plain.tls.ca_file = "/missing/ignored-when-disabled.pem".into();
    plain.tls.identity_file = "/missing/ignored-when-disabled.p12".into();
    verified_transport(&plain, "Unencrypted").await;
    // Disabling TLS is explicit even when the server offers encryption.
    plain = source.clone();
    plain.use_tls = false;
    verified_transport(&plain, "Unencrypted").await;

    let mut mutual = source.clone();
    mutual.user = "dbeast_mtls".into();
    rejected(&mutual, "missing client certificate").await;
    mutual.tls.identity_file = certs().join("client.p12").to_string_lossy().into_owned();
    mutual.tls_identity_password = "fixture-only".into();
    verified_transport(&mutual, "TLS (verified)").await;
    mutual.tls_identity_password = "incorrect-fixture-password".into();
    rejected(&mutual, "wrong client identity password").await;

    // PEM client certificates with their keys.
    let mut pem = source.clone();
    pem.user = "dbeast_mtls".into();
    for (certificate, key, label) in [
        ("client.pem", "client.key", "PKCS#8 key"),
        ("client.pem", "client-rsa.key", "PKCS#1 RSA key"),
        ("client-ec.pem", "client-ec.key", "SEC1 EC key"),
    ] {
        pem.tls.identity_file = certs().join(certificate).to_string_lossy().into_owned();
        pem.tls.key_file = certs().join(key).to_string_lossy().into_owned();
        eprintln!("{} PEM client certificate with {label}", driver.label());
        verified_transport(&pem, "TLS (verified)").await;
    }
    pem.tls.identity_file = certs().join("client.pem").to_string_lossy().into_owned();
    pem.tls.key_file = certs().join("client-ec.key").to_string_lossy().into_owned();
    rejected(&pem, "client key of another certificate").await;
    pem.tls.key_file = certs().join("missing.key").to_string_lossy().into_owned();
    rejected(&pem, "missing client key file").await;

    // Modes: Require skips certificate checks, Verify CA skips only the host name.
    let mut require = source.clone();
    require.tls.mode = TlsMode::Require;
    require.tls.ca_file = certs().join("wrong-ca.pem").to_string_lossy().into_owned();
    require.host = "127.0.0.1".into();
    transport(&require, "TLS (certificate not verified)").await;
    let mut verify_ca = source.clone();
    verify_ca.tls.mode = TlsMode::VerifyCa;
    verify_ca.host = "127.0.0.1".into();
    transport(&verify_ca, "TLS (CA verified, host name not checked)").await;
    verify_ca.tls.ca_file = certs().join("wrong-ca.pem").to_string_lossy().into_owned();
    rejected(&verify_ca, "Verify CA with an untrusted CA").await;

    mutual.tls_identity_password = "fixture-only".into();
    let mut unencrypted = source.clone();
    unencrypted.use_tls = false;
    for (label, measured, expected) in [
        ("plain", &unencrypted, "Unencrypted"),
        ("TLS", &source, "TLS (verified)"),
        ("mTLS", &mutual, "TLS (verified)"),
    ] {
        let mut times = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            transport(measured, expected).await;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "{} {label} connect + status + SELECT 1: median {:.1} ms, range {:.1}..{:.1} ms (5 local samples)",
            driver.label(),
            times[2],
            times[0],
            times[4]
        );
        let mut mcp_times = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            mcp_transport(measured).await;
            mcp_times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        mcp_times.sort_by(f64::total_cmp);
        eprintln!(
            "{} MCP {label} connect + bounded read-only SELECT 1 + close: median {:.1} ms, range {:.1}..{:.1} ms (5 local samples)",
            driver.label(),
            mcp_times[2],
            mcp_times[0],
            mcp_times[4]
        );
    }
}

#[tokio::test]
#[ignore = "isolated Docker TLS fixtures: scripts/test-tls.sh"]
async fn postgres_tls_verification_matrix() {
    verification_matrix(DriverKind::Postgres).await;
}

#[tokio::test]
#[ignore = "isolated Docker TLS fixtures: scripts/test-tls.sh"]
async fn mysql_tls_verification_matrix() {
    verification_matrix(DriverKind::MySql).await;
}

async fn cancel_verified(driver: DriverKind, tunneled: bool) {
    let mut source = fixture_source(driver, true);
    source.user = "dbeast_mtls".into();
    source.tls_identity_password = "fixture-only".into();
    // Cancellation must reuse validated material, not reread files that could have changed.
    let copies = certs()
        .parent()
        .unwrap()
        .join(format!("cancel-{}-{tunneled}", driver.label()));
    std::fs::create_dir_all(&copies).unwrap();
    let ca = copies.join("ca.pem");
    let identity = copies.join("client.p12");
    std::fs::copy(certs().join("ca.pem"), &ca).unwrap();
    std::fs::copy(certs().join("client.p12"), &identity).unwrap();
    source.tls.ca_file = ca.to_string_lossy().into_owned();
    source.tls.identity_file = identity.to_string_lossy().into_owned();
    if tunneled {
        source.host = if driver == DriverKind::Postgres {
            "pg"
        } else {
            "mysql"
        }
        .into();
        source.port = if driver == DriverKind::Postgres {
            5432
        } else {
            3306
        };
        source.ssh.enabled = true;
        source.ssh.host = "127.0.0.1".into();
        source.ssh.port = fixture_port("DBEAST_TLS_SSH_PORT");
        source.ssh.user = "tunnel".into();
        source.ssh.password = "tunnel".into();
    }
    let mut connection = Connection::open(&source)
        .await
        .unwrap_or_else(|e| panic!("{} tunnel={tunneled}: {e:#}", driver.label()));
    assert_eq!(
        connection.server_info().await.unwrap().transport,
        "TLS (verified)"
    );
    mcp_transport(&source).await;
    if tunneled {
        let mut wrong_name = source.clone();
        wrong_name.host.push_str("-tls");
        rejected(
            &wrong_name,
            "SSH destination hostname absent from certificate",
        )
        .await;
    }
    let cancel = connection.cancel_handle().expect("cancellable connection");
    std::fs::remove_file(ca).unwrap();
    std::fs::remove_file(identity).unwrap();
    // A standalone MySQL SLEEP returns 1 on interruption; a predicate reports a query error.
    let query = if driver == DriverKind::Postgres {
        "SELECT pg_sleep(30)"
    } else {
        "SELECT 1 WHERE SLEEP(30) = 0"
    };
    let started = Instant::now();
    let (query_result, cancellation) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(connection.execute(query, Page::first(1)), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel().await
        })
    })
    .await
    .expect("TLS cancellation timed out");
    cancellation.unwrap();
    assert!(
        query_result.is_err(),
        "long-running query must be interrupted"
    );
    eprintln!(
        "{} TLS cancellation tunnel={tunneled}: {:?} including 300 ms query start delay",
        driver.label(),
        started.elapsed()
    );
    // The MCP path uses its own bounded connection and must also retain the
    // original certificate hostname through an SSH tunnel during cancellation.
    source.tls.ca_file = certs().join("ca.pem").to_string_lossy().into_owned();
    source.tls.identity_file = certs().join("client.p12").to_string_lossy().into_owned();
    let mut observer = Connection::open(&source).await.unwrap();
    let cancellation = CancellationToken::new();
    let request = ExecutionRequest {
        source,
        operation: Operation::Sql {
            source_id: "tls-fixture".into(),
            sql: query.into(),
        },
        expected_table_identity: None,
        cancellation: cancellation.clone(),
        execution_lease: None,
    };
    let running = tokio::spawn(executor::execute(request));
    let active_sql = if driver == DriverKind::Postgres {
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='DBeast MCP' AND state='active' AND wait_event='PgSleep'".to_owned()
    } else {
        format!("SELECT count(*) FROM information_schema.PROCESSLIST WHERE INFO='{query}'")
    };
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let active = observer.execute(&active_sql, Page::first(1)).await.unwrap();
            if active.rows[0][0] != dbeast::model::Value::Int(0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("MCP TLS fixture query did not become active");
    let started = Instant::now();
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(4), running)
        .await
        .expect("MCP TLS cancellation timed out")
        .unwrap();
    let error = result
        .err()
        .expect("MCP cancellation must suppress a result");
    assert_eq!(error.code, "outcome_unknown");
    assert!(error.outcome_unknown);
    eprintln!(
        "{} MCP TLS cancellation tunnel={tunneled}: {:?} after observing the query active",
        driver.label(),
        started.elapsed()
    );
    transport(&fixture_source(driver, true), "TLS (verified)").await;
}

#[tokio::test]
#[ignore = "isolated Docker TLS fixtures: scripts/test-tls.sh"]
async fn tls_cancellation_reuses_verified_credentials() {
    for driver in [DriverKind::Postgres, DriverKind::MySql] {
        cancel_verified(driver, false).await;
    }
}

#[tokio::test]
#[ignore = "isolated Docker TLS fixtures: scripts/test-tls.sh"]
async fn tls_through_ssh_verifies_database_name_and_cancels() {
    for driver in [DriverKind::Postgres, DriverKind::MySql] {
        cancel_verified(driver, true).await;
    }
}
