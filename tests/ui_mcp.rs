//! Local MCP lifecycle and explicit sharing. No real config, credentials or database.
use egui::{Key, Modifiers, accesskit::Role};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use std::{
    net::TcpListener,
    time::{Duration, Instant},
};

#[test]
fn mcp_settings_start_stop_port_error_and_explicit_sharing() {
    let dir = std::env::temp_dir().join(format!("dbeast-mcp-ui-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let mut source = dbeast::model::DataSource::new(dbeast::model::DriverKind::Sqlite);
    source.name = "Private local source name".into();
    source.file = dir.join("fixture.sqlite").to_string_lossy().into_owned();
    source.password_storage = dbeast::model::PasswordStorage::ForSession;
    std::fs::write(
        &path,
        serde_json::json!({ "data_sources": [source], "mcp_port": port }).to_string(),
    )
    .unwrap();
    // SAFETY: the only test in this binary uses an isolated configuration and memory store.
    unsafe {
        std::env::set_var("DBEAST_CONFIG", &path);
    }
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .with_pixels_per_point(1.5)
        .wgpu()
        .build_eframe(|cc| dbeast::ui::App::new_for_test(cc));
    h.run_steps(3);
    h.get_by_label("MCP").click();
    h.run_steps(3);
    assert!(h.query_by_label("Stopped").is_some());
    // Connections are remembered, so one can be prepared before the server starts.
    assert!(
        !h.get_by_label("Create Agent Connection")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("Start Server").click();
    h.run_steps(3);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        h.step();
        if h.query_by_label("Start Server").is_some()
            && h.query_by_label("Cannot listen on this local port. Choose another port.")
                .is_some()
        {
            // Starting is asynchronous; the occupied socket remains exclusively ours.
            if h.query_by_label("Starting…").is_none() {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "occupied port did not finish reporting"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.query_by_label("Running").is_none());
    drop(occupied);
    h.get_by_label("Start Server").click();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.query_by_label("Running").is_none() {
        assert!(
            Instant::now() < deadline,
            "local MCP listener did not start"
        );
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.get_by_label("Port:").accesskit_node().is_disabled());
    h.get_by_label("Create Agent Connection").click();
    h.run_steps(3);
    let private_configuration = h.get_by_role(Role::MultilineTextInput).value().unwrap();
    let configuration: serde_json::Value = serde_json::from_str(&private_configuration).unwrap();
    assert_eq!(
        configuration["mcpServers"]["dbeast"]["url"],
        format!("http://127.0.0.1:{port}/mcp")
    );
    assert!(
        configuration["mcpServers"]["dbeast"]["headers"]["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer ")
    );
    h.get_by_label("Hide Configuration").scroll_to_me();
    h.run_steps(15);
    h.get_by_label("Hide Configuration").click();
    h.run_steps(3);
    assert!(h.query_by_role(Role::MultilineTextInput).is_none());
    h.render()
        .unwrap()
        .save("docs/screenshots/84-mcp-server.png")
        .unwrap();
    h.get_by_label("Sharing").click();
    h.run_steps(3);
    assert_eq!(
        h.get_by_label("Agent alias:").value().as_deref(),
        Some("Database 1")
    );
    // One row per audience: All Agents first, then the connection created above.
    h.get_all_by_role(Role::ComboBox)
        .filter(|n| n.value().as_deref() == Some("Not Shared"))
        .nth(1)
        .unwrap()
        .click();
    h.run_steps(3);
    h.get_by_label("Read-Only Access").click();
    h.run_steps(3);
    assert!(
        h.get_all_by_role(Role::ComboBox)
            .any(|n| n.value().as_deref() == Some("Read-Only Access"))
    );
    h.set_size(egui::vec2(740.0, 500.0));
    h.run_steps(4);
    h.render()
        .unwrap()
        .save("docs/screenshots/85-mcp-sharing-small.png")
        .unwrap();
    assert!(
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(740.0, 500.0))
            .contains_rect(h.get_by_label("Close").rect())
    );
    h.get_by_label("Server").click();
    h.run_steps(3);
    h.get_by_label("Revoke Connection").scroll_to_me();
    h.run_steps(15);
    h.get_by_label("Revoke Connection").click();
    h.run_steps(3);
    assert!(h.query_by_label("No agent connections.").is_some());
    h.get_by_label("Stop Server").scroll_to_me();
    h.run_steps(15);
    h.get_by_label("Stop Server").click();
    h.run_steps(3);
    assert!(h.query_by_label("Stopped").is_some());
    h.get_by_label("Port:").focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_label("Port:").type_text("0");
    h.run_steps(1);
    h.get_by_label("Start Server").click();
    h.run_steps(2);
    assert!(
        h.query_by_label("Enter a port between 1 and 65535.")
            .is_some()
    );
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(!persisted.contains("Bearer "));
    // Aliases and access live in the credential store, not in config.json.
    assert!(!persisted.contains("Database 1"));
    assert!(persisted.contains("\"mcp_state_saved\": true"), "{persisted}");
    h.get_by_label("Close").click();
    h.run_steps(2);
    drop(h);
    std::fs::remove_dir_all(dir).unwrap();
}
