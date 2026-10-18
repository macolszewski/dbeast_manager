//! Connections through an SSH tunnel, against the Docker bastion from
//! `scripts/test-dbs.sh` (databases reachable from it as "pg" and "mysql").

use dbeast::drivers::Connection;
use dbeast::model::{DataSource, DriverKind, Page, SshAuth, Value};
use std::path::PathBuf;
use std::process::{Child, Command};

fn keys_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ssh/keys")
}

fn tunneled(driver: DriverKind) -> DataSource {
    let mut ds = DataSource::new(driver);
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    (ds.host, ds.port, ds.database) = match driver {
        DriverKind::MySql => ("mysql".into(), 3306, "shop".into()),
        _ => ("pg".into(), 5432, "dbeast".into()),
    };
    ds.ssh.enabled = true;
    ds.ssh.host = "localhost".into();
    ds.ssh.port = 52222;
    ds.ssh.user = "tunnel".into();
    ds.ssh.password = "tunnel".into();
    ds
}

async fn scalar(conn: &mut Connection, sql: &str) -> Value {
    conn.execute(sql, Page::first(1)).await.unwrap().rows[0][0].clone()
}

async fn open_err(ds: &DataSource) -> String {
    match Connection::open(ds).await {
        Ok(_) => panic!("connection should fail"),
        Err(e) => format!("{e:#}"),
    }
}

/// ssh-agent with the test key loaded, stopped when dropped.
struct Agent(Child);

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn start_agent(dir: &std::path::Path) -> Agent {
    let sock = dir.join("agent.sock");
    let child = Command::new("ssh-agent")
        .args(["-D", "-a"])
        .arg(&sock)
        .spawn()
        .expect("ssh-agent");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let added = Command::new("ssh-add")
        .arg(keys_dir().join("id_ed25519"))
        .env("SSH_AUTH_SOCK", &sock)
        .output()
        .expect("ssh-add");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    // SAFETY: the only test in this binary sets process environment before any agent use.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", &sock) };
    Agent(child)
}

#[tokio::test]
async fn ssh_tunnel_auth_methods_and_host_key() {
    if std::net::TcpStream::connect("127.0.0.1:52222").is_err()
        || !keys_dir().join("id_ed25519").exists()
    {
        eprintln!("skipping: SSH test server is not running (scripts/test-dbs.sh)");
        return;
    }
    // Keep the known_hosts file of this run away from the real configuration.
    let dir = std::env::temp_dir().join(format!("dbeast_ssh_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: the only test in this binary; set before anything reads it.
    unsafe { std::env::set_var("DBEAST_CONFIG", dir.join("config.json")) };
    let known_hosts = dir.join("known_hosts");

    // Password authentication, PostgreSQL behind the bastion.
    let pg = tunneled(DriverKind::Postgres);
    let mut conn = Connection::open(&pg).await.unwrap();
    assert_eq!(
        scalar(&mut conn, "select count(*) from shop.customers").await,
        Value::Int(1200)
    );
    let learned = std::fs::read_to_string(&known_hosts).unwrap();
    assert!(
        learned.trim_start().starts_with("[localhost]:52222 ssh-"),
        "{learned}"
    );

    // A second session shares the tunnel and works independently.
    let mut second = Connection::open(&pg).await.unwrap();
    assert_eq!(scalar(&mut second, "select 2").await, Value::Int(2));
    drop((conn, second));

    // Key pair, MySQL behind the bastion.
    let mut my = tunneled(DriverKind::MySql);
    my.ssh.auth = SshAuth::KeyPair;
    my.ssh.key_file = keys_dir().join("id_ed25519").to_string_lossy().into_owned();
    let mut conn = Connection::open(&my).await.unwrap();
    assert_eq!(
        scalar(&mut conn, "select count(*) from customers").await,
        Value::Int(2)
    );
    drop(conn);

    // Passphrase-protected key.
    let mut protected = tunneled(DriverKind::Postgres);
    protected.ssh.auth = SshAuth::KeyPair;
    protected.ssh.key_file = keys_dir()
        .join("id_protected")
        .to_string_lossy()
        .into_owned();
    protected.ssh.passphrase = "secret".into();
    let mut conn = Connection::open(&protected).await.unwrap();
    assert_eq!(scalar(&mut conn, "select 1").await, Value::Int(1));
    drop(conn);
    protected.ssh.passphrase = "wrong".into();
    let err = open_err(&protected).await;
    assert!(err.contains("cannot load private key"), "{err}");

    // Wrong password.
    let mut bad = tunneled(DriverKind::Postgres);
    bad.ssh.password = "nope".into();
    let err = open_err(&bad).await;
    assert!(
        err.contains("SSH authentication failed for tunnel@localhost"),
        "{err}"
    );

    // An SSH server that accepts TCP but never answers does not hold up
    // tunnels to other servers, nor reuse of an open tunnel.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    let hold = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = silent.accept().await {
            held.push(socket);
        }
    });
    let mut stuck = tunneled(DriverKind::Postgres);
    stuck.ssh.port = silent_port;
    let stuck_open = tokio::spawn(async move { Connection::open(&stuck).await.is_ok() });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let conn = tokio::time::timeout(std::time::Duration::from_secs(10), Connection::open(&pg))
        .await
        .expect("an unresponsive SSH server must not block other tunnels")
        .unwrap();
    let mut reused =
        tokio::time::timeout(std::time::Duration::from_secs(10), Connection::open(&pg))
            .await
            .expect("reusing an open tunnel must not wait")
            .unwrap();
    assert_eq!(scalar(&mut reused, "select 4").await, Value::Int(4));
    assert!(!stuck_open.is_finished());
    stuck_open.abort();
    hold.abort();
    drop((conn, reused));

    // Keys from a running ssh-agent.
    let _agent = start_agent(&dir);
    let mut via_agent = tunneled(DriverKind::Postgres);
    via_agent.ssh.auth = SshAuth::Agent;
    via_agent.ssh.password.clear();
    let mut conn = Connection::open(&via_agent).await.unwrap();
    assert_eq!(scalar(&mut conn, "select 3").await, Value::Int(3));
    drop(conn);

    // A different host key than the remembered one is refused.
    let other_key = std::fs::read_to_string(keys_dir().join("id_ed25519.pub")).unwrap();
    let key_part: Vec<&str> = other_key.split_whitespace().take(2).collect();
    std::fs::write(
        &known_hosts,
        format!("[localhost]:52222 {}\n", key_part.join(" ")),
    )
    .unwrap();
    let err = open_err(&pg).await;
    assert!(
        err.contains("host key of localhost:52222 has changed"),
        "{err}"
    );

    std::fs::remove_dir_all(dir).ok();
}
