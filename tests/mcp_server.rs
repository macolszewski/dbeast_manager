use dbeast::mcp::{
    Approval, McpController, ShareAccess, SharedFile, SharedSource, SourceAccess, TableAccess,
    TableRef,
};
use dbeast::model::{DataSource, DriverKind};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt,
    model::{CallToolRequestParams, ClientConfig, Implementation, ProtocolVersion},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

struct Server {
    controller: McpController,
    url: String,
    port: u16,
}
struct TemporaryDatabase(std::path::PathBuf);
impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.controller.stop();
    }
}
async fn server() -> Server {
    let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let controller = McpController::new(tokio::runtime::Handle::current(), || {});
    controller.start(port);
    let until = Instant::now() + Duration::from_secs(3);
    while !controller.snapshot().running {
        assert!(Instant::now() < until, "local MCP listener failed to start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Server {
        controller,
        url: format!("http://127.0.0.1:{port}/mcp"),
        port,
    }
}
fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap()
}
fn args(name: &'static str, value: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(value.as_object().unwrap().clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn official_sdk_modern_and_legacy_clients_use_explicit_approvals() {
    for lifecycle in [
        ClientLifecycleMode::Initialize,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    ] {
        let server = server().await;
        server.controller.update_resources(
            Vec::new(),
            vec![SharedFile {
                id: "/private/test-document.sql".into(),
                alias: "Open document 1".into(),
                version: 7,
                text: Arc::from("SELECT 42;"),
            }],
        );
        let connection = server
            .controller
            .create_client("SDK compatibility".into(), dbeast::mcp::DEFAULT_PORT)
            .unwrap();
        let transport = StreamableHttpClientTransport::with_client(
            http(),
            StreamableHttpClientTransportConfig::with_uri(server.url.clone())
                .auth_header(connection.token),
        );
        let client = ClientConfig::new(Default::default(), Implementation::new("dbeast-test", "1"))
            .serve_with_lifecycle(transport, lifecycle)
            .await
            .unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|tool| tool.name == "query_sql"));
        let session = client
            .call_tool(args("begin_session", json!({"label":"SDK test"})))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        let token = session["session_token"].as_str().unwrap();
        let catalog = client
            .call_tool(args("list_catalog", json!({"session_token":token})))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert!(!catalog.to_string().contains("/private"));
        let file = &catalog["files"][0];
        let request=client.call_tool(args("read_open_file",json!({
            "session_token":token,"file_id":file["file_id"],"version":7,"start":0,"max_chars":100,"request_key":uuid::Uuid::new_v4().to_string()
        }))).await.unwrap().structured_content.unwrap();
        assert_eq!(request["status"], "pending");
        let request_id = request["request_id"].as_str().unwrap();
        assert!(request["result"].is_null());
        server
            .controller
            .approve(request_id, Approval::Once)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let result = client
                .call_tool(args(
                    "get_request",
                    json!({"session_token":token,"request_id":request_id}),
                ))
                .await
                .unwrap()
                .structured_content
                .unwrap();
            if result["status"] == "completed" {
                assert_eq!(result["result"]["text"], "SELECT 42;");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "approved document did not finish"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        client
            .call_tool(args("end_session", json!({"session_token":token})))
            .await
            .unwrap();
        let rejected = client
            .call_tool(args("list_catalog", json!({"session_token":token})))
            .await
            .unwrap();
        assert_eq!(rejected.is_error, Some(true));
        client.cancel().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_table_grant_requires_consent_and_does_not_authorize_writes() {
    let database_file = TemporaryDatabase(
        std::env::temp_dir().join(format!("dbeast-mcp-http-{}.sqlite", uuid::Uuid::new_v4())),
    );
    let path = &database_file.0;
    let database = rusqlite::Connection::open(path).unwrap();
    database.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'fixture'); CREATE TABLE other(id INTEGER)").unwrap();
    drop(database);
    let server = server().await;
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.file = path.to_string_lossy().into();
    server.controller.update_resources(
        vec![SharedSource::for_all_agents("Synthetic source", source)],
        Vec::new(),
    );
    let connection = server
        .controller
        .create_client("table scope".into(), dbeast::mcp::DEFAULT_PORT)
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http(),
        StreamableHttpClientTransportConfig::with_uri(server.url.clone())
            .auth_header(connection.token),
    );
    let client = ClientConfig::new(
        Default::default(),
        Implementation::new("table-scope-test", "1"),
    )
    .serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .unwrap();
    let session = client
        .call_tool(args("begin_session", json!({})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let token = session["session_token"].as_str().unwrap();
    let catalog = client
        .call_tool(args("list_catalog", json!({"session_token":token})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let source_id = &catalog["sources"][0]["source_id"];
    let read=client.call_tool(args("query_sql",json!({"session_token":token,"source_id":source_id,"sql":"select id, name from main.items","request_key":uuid::Uuid::new_v4().to_string()}))).await.unwrap().structured_content.unwrap();
    assert_eq!(read["status"], "pending");
    let pending = server.controller.snapshot().requests[0].clone();
    assert_eq!(pending.preview, "SELECT id, name FROM main.items");
    assert_eq!(pending.eligible_table.unwrap().name, "items");
    server
        .controller
        .approve(
            read["request_id"].as_str().unwrap(),
            Approval::ReadTableForSession,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = client
            .call_tool(args(
                "get_request",
                json!({"session_token":token,"request_id":read["request_id"]}),
            ))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        if result["status"] == "completed" {
            assert_eq!(result["result"]["rows"][0][1], "fixture");
            break;
        }
        assert_eq!(result["status"], "running");
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let allowed=client.call_tool(args("query_sql",json!({"session_token":token,"source_id":source_id,"sql":"SELECT count(*) FROM main.items","request_key":uuid::Uuid::new_v4().to_string()}))).await.unwrap().structured_content.unwrap();
    assert!(matches!(
        allowed["status"].as_str(),
        Some("running" | "completed")
    ));
    let other=client.call_tool(args("query_sql",json!({"session_token":token,"source_id":source_id,"sql":"SELECT id FROM main.other","request_key":uuid::Uuid::new_v4().to_string()}))).await.unwrap().structured_content.unwrap();
    assert_eq!(other["status"], "pending");
    let write=client.call_tool(args("request_sql",json!({"session_token":token,"source_id":source_id,"sql":"DELETE FROM items","request_key":uuid::Uuid::new_v4().to_string()}))).await.unwrap().structured_content.unwrap();
    assert_eq!(write["status"], "pending");
    server
        .controller
        .approve(write["request_id"].as_str().unwrap(), Approval::Deny)
        .unwrap();
    let count: i64 = rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_table_write_runs_without_prompt_only_inside_that_table() {
    let database_file = TemporaryDatabase(
        std::env::temp_dir().join(format!("dbeast-mcp-share-{}.sqlite", uuid::Uuid::new_v4())),
    );
    let path = &database_file.0;
    let database = rusqlite::Connection::open(path).unwrap();
    database.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'fixture'); CREATE TABLE other(id INTEGER); INSERT INTO other VALUES(7)").unwrap();
    drop(database);
    let server = server().await;
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.file = path.to_string_lossy().into();
    server.controller.update_resources(
        vec![SharedSource {
            alias: "Synthetic source".into(),
            source,
            audiences: vec![(None, ShareAccess {
                source: SourceAccess::Ask,
                tables: vec![(
                    TableRef {
                        schema: "main".into(),
                        name: "items".into(),
                    },
                    TableAccess::ReadWrite,
                )],
            })],
        }],
        Vec::new(),
    );
    let connection = server.controller.create_client("shared".into(), dbeast::mcp::DEFAULT_PORT).unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http(),
        StreamableHttpClientTransportConfig::with_uri(server.url.clone())
            .auth_header(connection.token),
    );
    let client = ClientConfig::new(Default::default(), Implementation::new("share-test", "1"))
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let session = client
        .call_tool(args("begin_session", json!({})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let token = session["session_token"].as_str().unwrap();
    let catalog = client
        .call_tool(args("list_catalog", json!({"session_token":token})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let source = &catalog["sources"][0];
    assert_eq!(source["shared_access"], "ask");
    assert_eq!(source["shared_tables"][0]["name"], "items");
    assert_eq!(source["shared_tables"][0]["access"], "read_write");
    let source_id = &source["source_id"];
    let call = |tool: &'static str, sql: &str| {
        args(
            tool,
            json!({"session_token":token,"source_id":source_id,"sql":sql,"request_key":uuid::Uuid::new_v4().to_string()}),
        )
    };
    let wait = async |request: Value| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = client
                .call_tool(args(
                    "get_request",
                    json!({"session_token":token,"request_id":request["request_id"]}),
                ))
                .await
                .unwrap()
                .structured_content
                .unwrap();
            if result["status"] != "running" {
                return result;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let read = client
        .call_tool(call("query_sql", "SELECT name FROM main.items"))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_ne!(read["status"], "pending");
    assert_eq!(wait(read).await["status"], "completed");
    let update = client
        .call_tool(call("request_sql", "UPDATE main.items SET name = 'changed' WHERE id = 1"))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_ne!(update["status"], "pending");
    assert_eq!(wait(update).await["status"], "completed");
    for (tool, sql) in [
        ("query_sql", "SELECT id FROM main.other"),
        ("request_sql", "DELETE FROM main.other"),
        ("request_sql", "DELETE FROM main.items WHERE id IN (SELECT id FROM main.other)"),
    ] {
        let pending = client
            .call_tool(call(tool, sql))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(pending["status"], "pending", "{sql}");
        server
            .controller
            .approve(pending["request_id"].as_str().unwrap(), Approval::Deny)
            .unwrap();
    }
    let database = rusqlite::Connection::open(path).unwrap();
    let name: String = database
        .query_row("SELECT name FROM items WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "changed");
    let other: i64 = database
        .query_row("SELECT count(*) FROM other", [], |row| row.get(0))
        .unwrap();
    assert_eq!(other, 1);
    let audit = server.controller.snapshot().audit.clone();
    assert!(audit.iter().any(|event| event.action == "shared access"
        && event.sql.as_deref() == Some("UPDATE main.items SET name = 'changed' WHERE id = 1")));
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_rejects_missing_auth_wrong_host_origins_and_oversized_chunked_body() {
    let server = server().await;
    let client = http();
    assert_eq!(
        client
            .post(&server.url)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let connection = server
        .controller
        .create_client("HTTP checks".into(), dbeast::mcp::DEFAULT_PORT)
        .unwrap();
    assert_eq!(
        client
            .post(&server.url)
            .bearer_auth(&connection.token)
            .header("Host", "attacker.invalid")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for origin in ["null", "https://attacker.invalid", "http://127.0.0.1"] {
        assert_eq!(
            client
                .post(&server.url)
                .bearer_auth(&connection.token)
                .header("Origin", origin)
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", server.port))
        .await
        .unwrap();
    let header = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        server.port, connection.token
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    let body = vec![b'x'; 256 * 1024 + 1];
    socket
        .write_all(format!("{:x}\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(&body).await.unwrap();
    socket.write_all(b"\r\n0\r\n\r\n").await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio::io::BufReader::new(socket).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(line.contains("413"), "oversized body was not rejected");
    server.controller.revoke_client(&connection.id);
    assert_eq!(
        client
            .post(&server.url)
            .bearer_auth(connection.token)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_request_body_has_a_deadline_and_does_not_block_controller() {
    let server = server().await;
    let connection = server.controller.create_client("slow HTTP".into(), dbeast::mcp::DEFAULT_PORT).unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", server.port))
        .await
        .unwrap();
    let header = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{",
        server.port, connection.token
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    let start = Instant::now();
    assert!(
        server
            .controller
            .create_client("still responsive".into(), dbeast::mcp::DEFAULT_PORT)
            .is_ok()
    );
    assert!(start.elapsed() < Duration::from_millis(100));
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(12),
        tokio::io::BufReader::new(socket).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        line.contains("408"),
        "slow body did not hit the request deadline"
    );
}
