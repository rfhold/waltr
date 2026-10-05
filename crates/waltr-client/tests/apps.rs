//! Protected Apps command mapping, request correlation, and lifecycle tests.

mod support;

use waltr_client::{Client, ClientEffect, ClientEvent, McpAppEffect, McpAppEvent, McpAppView};

fn app() -> McpAppView {
    McpAppView {
        app_id: "lease".into(),
        html: "<html>untrusted</html>".into(),
        tool_json: r#"{"name":"status"}"#.into(),
        arguments_json: r#"{"key":"original"}"#.into(),
        result_json: r#"{"structuredContent":{"version":1},"_meta":{"secret":"private"}}"#.into(),
        resource_meta_json: "{}".into(),
    }
}

fn opened(client: &mut Client) {
    client.open_session_mcp_app("session".into(), "call".into(), "open".into());
    client.apply(ClientEvent::McpApp(McpAppEvent::Opened {
        request_id: "open".into(),
        app: app(),
    }));
}

#[test]
fn apps_map_typed_open_call_close_and_preserve_original() {
    let mut client = Client::new();
    let update = client.open_session_mcp_app("session".into(), "call".into(), "open".into());
    let [
        ClientEffect::McpApp(McpAppEffect::Open {
            request_id,
            request,
        }),
    ] = update.effects()
    else {
        panic!("open RPC effect");
    };
    assert_eq!(request_id, "open");
    assert_eq!(request.session_id, "session");
    assert_eq!(request.call_id, "call");
    assert_eq!(client.state().apps.requests["open"].status, "loading");
    client.apply(ClientEvent::McpApp(McpAppEvent::Opened {
        request_id: "open".into(),
        app: app(),
    }));
    for id in ["refresh-1", "refresh-2"] {
        let update = client.call_session_mcp_app_tool(
            "lease".into(),
            "status".into(),
            "{}".into(),
            id.into(),
        );
        let [
            ClientEffect::McpApp(McpAppEffect::Call {
                request_id,
                request,
            }),
        ] = update.effects()
        else {
            panic!("call RPC effect");
        };
        assert_eq!(request_id, id);
        assert_eq!(request.app_id, "lease");
        assert_eq!(request.name, "status");
        assert_eq!(request.arguments_json, "{}");
        client.apply(ClientEvent::McpApp(McpAppEvent::Called {
            request_id: id.into(),
            app_id: "lease".into(),
            result_json: "{\"refreshed\":true}".into(),
        }));
        assert_eq!(client.state().apps.current["lease"], app());
        assert_eq!(
            client.state().apps.requests[id].result_json.as_deref(),
            Some("{\"refreshed\":true}")
        );
    }
    assert!(client.state().sessions.current.is_empty());
    assert!(client.state().sessions.timelines.is_empty());
    let update = client.close_session_mcp_app("lease".into());
    assert!(client.state().apps.current.is_empty());
    let [
        ClientEffect::McpApp(McpAppEffect::Close {
            request_id,
            request,
        }),
    ] = update.effects()
    else {
        panic!("close RPC effect");
    };
    assert_eq!(request.app_id, "lease");
    client.apply(ClientEvent::McpApp(McpAppEvent::Closed {
        request_id: request_id.clone(),
    }));
    assert!(
        client
            .state()
            .apps
            .requests
            .values()
            .all(|op| op.result_json.is_none())
    );
}

#[test]
fn canceled_open_closes_late_server_lease_and_ready_cancel_closes() {
    let mut client = Client::new();
    client.open_session_mcp_app("session".into(), "call".into(), "open".into());
    client.cancel_session_mcp_app_open("open".into());
    let update = client.apply(ClientEvent::McpApp(McpAppEvent::Opened {
        request_id: "open".into(),
        app: app(),
    }));
    assert!(client.state().apps.current.is_empty());
    assert!(
        matches!(update.effects(), [ClientEffect::McpApp(McpAppEffect::Close { request, .. })] if request.app_id == "lease")
    );
    let mut client = Client::new();
    opened(&mut client);
    assert!(!client.cancel_session_mcp_app_open("open".into()).is_empty());
    assert!(client.state().apps.current.is_empty());
}

#[test]
fn closed_call_and_stale_failures_do_not_resurrect_or_expire_access() {
    let mut client = Client::new();
    opened(&mut client);
    client.call_session_mcp_app_tool(
        "lease".into(),
        "status".into(),
        "{}".into(),
        "refresh".into(),
    );
    client.close_session_mcp_app("lease".into());
    client.apply(ClientEvent::McpApp(McpAppEvent::Called {
        request_id: "refresh".into(),
        app_id: "lease".into(),
        result_json: "secret".into(),
    }));
    client.apply(ClientEvent::McpApp(McpAppEvent::Failed {
        request_id: "refresh".into(),
        error: waltr_client::ClientError::waltr_login_required("stale"),
    }));
    assert!(client.state().apps.current.is_empty());
    assert!(!client.state().apps.requests.contains_key("refresh"));
    assert_eq!(
        client.state().access.status,
        waltr_client::AccessStatus::UnknownChecking
    );
}

#[test]
fn apps_errors_are_sanitized_and_auth_changes_clear_protected_state() {
    let mut client = Client::new();
    opened(&mut client);
    client.call_session_mcp_app_tool(
        "lease".into(),
        "status".into(),
        "{}".into(),
        "refresh".into(),
    );
    client.apply(ClientEvent::McpApp(McpAppEvent::Failed {
        request_id: "refresh".into(),
        error: waltr_client::ClientError::new("secret _meta bearer html"),
    }));
    assert_eq!(
        client.state().apps.requests["refresh"].error.as_deref(),
        Some("MCP app operation failed")
    );
    assert!(!format!("{:?}", client.state().apps).contains("private"));
    client.logout_waltr();
    assert!(client.state().apps.current.is_empty());
    assert!(client.state().apps.requests.is_empty());
    let late = client.apply(ClientEvent::McpApp(McpAppEvent::Opened {
        request_id: "open".into(),
        app: app(),
    }));
    assert!(!late.is_empty());
    assert!(client.state().apps.current.is_empty());
    opened(&mut client);
    client.call_session_mcp_app_tool(
        "lease".into(),
        "status".into(),
        "{}".into(),
        "auth-fail".into(),
    );
    client.apply(ClientEvent::McpApp(McpAppEvent::Failed {
        request_id: "auth-fail".into(),
        error: waltr_client::ClientError::waltr_login_required("Waltr login required"),
    }));
    assert_eq!(
        client.state().access.status,
        waltr_client::AccessStatus::LoginRequired
    );
    assert!(client.state().apps.current.is_empty());
    assert!(client.state().apps.requests.is_empty());
}

#[test]
fn missing_app_fails_locally_and_request_ids_are_single_use() {
    let mut client = Client::new();
    assert!(
        client
            .call_session_mcp_app_tool("missing".into(), "status".into(), "{}".into(), "r".into())
            .is_empty()
    );
    assert_eq!(client.state().apps.requests["r"].status, "failed");
    assert!(
        client
            .open_session_mcp_app("s".into(), "c".into(), "r".into())
            .is_empty()
    );
}

#[tokio::test]
async fn apps_runtime_dispatches_open_over_server_transport_and_reports_failure()
-> Result<(), Box<dyn std::error::Error>> {
    use waltr_client::{ClientCommand, ClientRuntime};
    use support::snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep};
    let server = waltr_server::test_support::setup_server_with_seeded_sessions(
        waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()),
    );
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;
    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::OpenSessionMcpApp {
            session_id: "missing-session".into(),
            call_id: "missing-call".into(),
            request_id: "open".into(),
        },
    )
    .await?;
    assert_eq!(started.apps.requests["open"].status, "loading");
    let failed = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.apps.requests["open"].status == "failed"
    })
    .await;
    assert_eq!(
        failed.apps.requests["open"].error.as_deref(),
        Some("MCP app operation failed")
    );
    assert!(failed.apps.current.is_empty());
    let close = dispatch_stream_command(
        &mut stream,
        ClientCommand::CloseSessionMcpApp {
            app_id: "missing-app".into(),
        },
    )
    .await?;
    let close_id = close
        .apps
        .requests
        .iter()
        .find(|(_, op)| op.kind == waltr_client::McpAppOperationKind::Close)
        .unwrap()
        .0
        .clone();
    let closed = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.apps.requests[&close_id].status != "loading"
    })
    .await;
    assert!(closed.apps.current.is_empty());
    Ok(())
}

#[test]
fn apps_commands_have_stable_json_contracts() {
    use waltr_client::ClientCommand;
    for json in [
        r#"{"type":"openSessionMcpApp","sessionId":"s","callId":"c","requestId":"o"}"#,
        r#"{"type":"callSessionMcpAppTool","appId":"a","name":"status","argumentsJson":"{}","requestId":"r"}"#,
        r#"{"type":"closeSessionMcpApp","appId":"a"}"#,
        r#"{"type":"cancelSessionMcpAppOpen","requestId":"o"}"#,
    ] {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let command: ClientCommand = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(command).unwrap(), value);
    }
}

#[test]
fn completed_refresh_results_are_bounded_in_completion_order() {
    let mut client = Client::new();
    opened(&mut client);
    for i in 0..140 {
        let request_id = format!("refresh-{i}");
        client.call_session_mcp_app_tool(
            "lease".into(),
            "status".into(),
            "{}".into(),
            request_id.clone(),
        );
        client.apply(ClientEvent::McpApp(McpAppEvent::Called {
            request_id,
            app_id: "lease".into(),
            result_json: i.to_string(),
        }));
    }
    assert_eq!(client.state().apps.requests.len(), 129);
    assert!(client.state().apps.requests.contains_key("open"));
    assert!(!client.state().apps.requests.contains_key("refresh-0"));
    assert_eq!(
        client.state().apps.requests["refresh-139"]
            .result_json
            .as_deref(),
        Some("139")
    );
    assert_eq!(client.state().apps.current["lease"], app());
}

#[test]
fn mismatched_and_duplicate_completions_preserve_current_lease() {
    let mut client = Client::new();
    opened(&mut client);
    let duplicate = client.apply(ClientEvent::McpApp(McpAppEvent::Opened {
        request_id: "open".into(),
        app: app(),
    }));
    assert!(duplicate.is_empty());
    client.call_session_mcp_app_tool(
        "lease".into(),
        "status".into(),
        "{}".into(),
        "refresh".into(),
    );
    client.apply(ClientEvent::McpApp(McpAppEvent::Called {
        request_id: "refresh".into(),
        app_id: "other-lease".into(),
        result_json: "wrong".into(),
    }));
    assert_eq!(client.state().apps.requests["refresh"].status, "loading");
    assert!(
        client.state().apps.requests["refresh"]
            .result_json
            .is_none()
    );
    assert_eq!(client.state().apps.current["lease"], app());
}
