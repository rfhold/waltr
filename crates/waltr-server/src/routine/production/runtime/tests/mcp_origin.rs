use super::*;
use base64::Engine as _;
use futures_util::StreamExt as _;
use p::routine_service_server::RoutineService as _;
use sha2::{Digest as _, Sha256};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use waltr_agent::LlmStream;

struct Provider {
    turns: AtomicUsize,
    entered: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Notify>,
    live: Arc<AtomicBool>,
}
struct HelperGuard(Arc<AtomicBool>, Arc<tokio::sync::Notify>);
impl Drop for HelperGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
        self.1.notify_one();
    }
}
impl LlmStream for Provider {
    fn name(&self) -> &'static str {
        "mcp-origin-fixture"
    }
    fn stream_request<'a>(
        &'a self,
        request: waltr_agent::CompletionRequest<'a>,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        if request.tools.iter().any(|tool| tool.name == "code") {
            let event = if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
                waltr_agent::CompletionEvent::ToolCall(waltr_agent::ToolCall::new(
                    "mcp-origin-code",
                    "code",
                    serde_json::json!({"action":"execute","source":"return await host.call('helper.run',{selection:{kind:'named',name:'routine-title'},task:'MCP_LEASE_HELPER',maxTurns:1,deadlineMs:10000,responseBytes:65536,completionBytes:65536});"}),
                ))
            } else {
                waltr_agent::CompletionEvent::Delta("Original run completed".into())
            };
            return Box::pin(async {
                Ok(Box::pin(futures_util::stream::iter([
                    Ok(event),
                    Ok(waltr_agent::CompletionEvent::Finished),
                ])) as waltr_agent::BoxCompletionStream)
            });
        }
        if format!("{:?}", request.prompt).contains("MCP_LEASE_HELPER") {
            let guard = HelperGuard(self.live.clone(), self.released.clone());
            self.live.store(true, Ordering::SeqCst);
            self.entered.notify_one();
            return Box::pin(async move {
                Ok(Box::pin(futures_util::stream::once(async move {
                    let _guard = guard;
                    std::future::pending::<
                        Result<waltr_agent::CompletionEvent, waltr_agent::LlmError>,
                    >()
                    .await
                })) as waltr_agent::BoxCompletionStream)
            });
        }
        Box::pin(async {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(waltr_agent::CompletionEvent::Delta("Normal title".into())),
                Ok(waltr_agent::CompletionEvent::Finished),
            ])) as waltr_agent::BoxCompletionStream)
        })
    }
}

fn hidden<'a>(html: &'a str, name: &str) -> &'a str {
    let marker = format!("name=\"{name}\" value=\"");
    html.split_once(&marker)
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap()
}

async fn issue(address: std::net::SocketAddr, browser: &str) -> serde_json::Value {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let verifier = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let mut authorization = url::Url::parse(&format!("http://{address}/oauth/authorize")).unwrap();
    authorization.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", "mcp-origin-client"),
        ("redirect_uri", "https://client.example/callback"),
        ("scope", "mcp:use"),
        ("state", "mcp-origin"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("resource", "https://waltr.example/mcp"),
    ]);
    let consent = client
        .get(authorization)
        .header("cookie", format!("waltr-session-local={browser}"))
        .send()
        .await
        .unwrap();
    let approved = if consent.status() == reqwest::StatusCode::SEE_OTHER {
        consent
    } else {
        assert_eq!(consent.status(), reqwest::StatusCode::OK);
        let cookie = consent
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("waltr-mcp-consent-local-"))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let html = consent.text().await.unwrap();
        let approved = client
            .post(format!("http://{address}/oauth/authorize/decision"))
            .header("cookie", format!("waltr-session-local={browser}; {cookie}"))
            .form(&[
                ("transaction_id", hidden(&html, "transaction_id")),
                ("csrf", hidden(&html, "csrf")),
                ("decision", "approve"),
            ])
            .send()
            .await
            .unwrap();
        approved
    };
    assert_eq!(approved.status(), reqwest::StatusCode::SEE_OTHER);
    let location = url::Url::parse(approved.headers()["location"].to_str().unwrap()).unwrap();
    let code = location
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    let response = client
        .post(format!("http://{address}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", "https://client.example/callback"),
            ("code_verifier", verifier),
            ("client_id", "mcp-origin-client"),
            ("resource", "https://waltr.example/mcp"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.unwrap()
}

pub(super) async fn call(
    address: std::net::SocketAddr,
    token: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let _ = lists_code(address, token).await;
    let response = reqwest::Client::new().post(format!("http://{address}/mcp"))
        .bearer_auth(token).header("accept","application/json, text/event-stream")
        .header("mcp-protocol-version",mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method","tools/call").header("mcp-name",name)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments,"_meta":{"io.modelcontextprotocol/protocolVersion":mcp::protocol::MCP_PROTOCOL_VERSION,"io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"mcp-origin-fixture","version":"1"},"principal_id":"forged-operator","issuer":"https://forged.example"}}}))
        .send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(
        status == reqwest::StatusCode::OK
            || (name == "code" && status == reqwest::StatusCode::BAD_REQUEST),
        "{text}"
    );
    serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or(&text),
    )
    .unwrap()
}

async fn lists_code(address: std::net::SocketAddr, token: &str) -> bool {
    let response = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .bearer_auth(token)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/list")
        .json(
            &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{
            "io.modelcontextprotocol/protocolVersion":mcp::protocol::MCP_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities":{},
            "io.modelcontextprotocol/clientInfo":{"name":"mcp-origin-fixture","version":"1"},
            "principal_id":"forged-operator"}}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let text = response.text().await.unwrap();
    let value: serde_json::Value = serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or(&text),
    )
    .unwrap();
    value["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tool list response: {value}"))
        .iter()
        .any(|tool| tool["name"] == "code")
}

#[tokio::test]
async fn expired_origin_owns_code_and_joins_original_helper_after_response() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::mcp_origin::expired_origin_owns_code_and_joins_original_helper_after_response",
    ) {
        return;
    }
    run(false, false).await;
}

#[tokio::test]
async fn revoked_origin_owns_code_and_joins_original_helper_after_response() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::mcp_origin::revoked_origin_owns_code_and_joins_original_helper_after_response",
    ) {
        return;
    }
    run(true, false).await;
}

#[tokio::test]
async fn configured_public_code_keeps_concurrent_same_principal_leases_separate() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::mcp_origin::configured_public_code_keeps_concurrent_same_principal_leases_separate",
    ) {
        return;
    }
    run(true, true).await;
}

async fn run(revoke: bool, public_code: bool) {
    use crate::mcp_oauth::{McpOAuthEnabledConfig, McpOAuthServer};
    use crate::routine::observation::ObservationStore as _;
    use waltr::grpc::waltr::session::v1 as session;
    let root = crate::test_support::temp_root().join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&root).unwrap();
    let resources = root.join("resources");
    seed(&resources);
    let agents = resources.join(".waltr/resources/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("code-origin.yaml"),"apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: code-origin }\nspec:\n  systemPrompt: Use the explicitly bound code tool once.\n  features: [{ codeMode: true }]\n  permissions: []\n").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mcp = resources.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::write(mcp.join("waltr.yaml"),format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{ name: waltr }}\nspec:\n  transport: streamableHttp\n  url: http://{address}/mcp\n")).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let loader = crate::test_support::default_catalog_loader();
    let config_service = ConfigService::new(resources, loader.clone()).await.unwrap();
    let state = config_service.state().clone();
    let persistence = Persistence::initialize(
        crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
            &format!("sqlite://{}", root.join("mcp.sqlite3").display()),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let (authenticator, access_token, browser_issuer, issuer_stop, issuer_task) =
        crate::test_support::composition_auth(&persistence, address).await;
    let now = std::time::SystemTime::now();
    crate::access_auth::AccessAuthStore::new(&persistence)
        .insert_browser_session(&crate::access_auth::BrowserSession {
            token_digest: crate::access_auth::BrowserSessionTokenDigest::from_token(
                "mcp-foreign-browser",
            ),
            issuer: browser_issuer,
            client_id: "waltr-test-client".into(),
            subject: "foreign-browser-subject".into(),
            created_at: now,
            expires_at: now + std::time::Duration::from_secs(3600),
            last_seen_at: now,
        })
        .await
        .unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "cookie",
        "waltr-session-local=composition-browser".parse().unwrap(),
    );
    let subject = authenticator
        .authenticate_browser_session_headers(&headers)
        .await
        .unwrap()
        .mcp_principal_id()
        .as_str()
        .to_owned();
    let key = root.join("keys.json");
    std::fs::write(&key,r#"{"schema_version":1,"active":"test","keys":[{"id":"test","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}]}"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    let oauth = McpOAuthServer::initialize(
        McpOAuthEnabledConfig::new(
            "https://waltr.example",
            "https://waltr.example/oauth",
            vec!["mcp:use".into()],
            key,
        )
        .unwrap()
        .with_access_token_ttl(std::time::Duration::from_secs(if revoke { 60 } else { 3 })),
        &persistence,
        authenticator.clone(),
    )
    .await
    .unwrap();
    oauth
        .register_client(mcp::oauth::OAuthClient {
            client_id: "mcp-origin-client".into(),
            registration_method: mcp::oauth::OAuthClientRegistrationMethod::Preconfigured,
            display_name: Some("MCP origin fixture".into()),
            client_secret_digest: None,
            redirect_uris: vec!["https://client.example/callback".into()],
            created_at_ms: 0,
            expires_at_ms: None,
            disabled_at_ms: None,
        })
        .await
        .unwrap();
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    for id in ["mcp-code-granted", "mcp-code-foreign"] {
        let mut create = waltr_agent::SessionCreateRequest::new(waltr_agent::SessionId::new(id));
        create.agent_definition = state
            .snapshot()
            .catalog()
            .bundle("code-origin")
            .map(|bundle| serde_json::to_value(bundle).unwrap());
        create.inference_settings = Some(bindings::default_inference(&state.snapshot()).unwrap());
        sessions.create(create).await.unwrap();
    }
    let mut normal =
        waltr_agent::SessionCreateRequest::new(waltr_agent::SessionId::new("mcp-normal"));
    normal.agent_definition = state
        .snapshot()
        .catalog()
        .bundle("routine-title")
        .map(|bundle| serde_json::to_value(bundle).unwrap());
    normal.inference_settings = Some(bindings::default_inference(&state.snapshot()).unwrap());
    sessions.create(normal).await.unwrap();
    let mut normal_updates = sessions.watch_sessions();
    let provider = Arc::new(Provider {
        turns: AtomicUsize::new(0),
        entered: Arc::new(tokio::sync::Notify::new()),
        released: Arc::new(tokio::sync::Notify::new()),
        live: Arc::new(AtomicBool::new(false)),
    });
    let check = provider.clone();
    let history = persistence.script_invocation_store();
    let oauth_check = oauth.clone();
    let app = assemble_app(
        AppServices {
            config_service,
            persistence,
            catalog_loader: loader,
            access_authenticator: authenticator,
            mcp_oauth: Some(oauth),
            outbound_private_origins: Vec::new(),
            outbound_oauth: OutboundOAuthConfig::default(),
            automatic_self_mcp_resource: None,
            agent_state: Some(state),
            local_admission: None,
            workspace_root: workspace,
            sandbox_socket: None,
            routine_authority: RoutineAuthority {
                code: vec![crate::routine::production::RoutineCodeGrant {
                    public_mcp_session: public_code
                        .then(|| waltr_agent::SessionId::new("mcp-code-granted")),
                    issuer: "https://waltr.example/oauth".into(),
                    subject,
                    sessions: BTreeSet::from([waltr_agent::SessionId::new("mcp-code-granted")]),
                    profile: crate::routine::code_mode::CodeProfile {
                        restriction: Default::default(),
                        targets: BTreeMap::new(),
                        helpers: BTreeSet::from(["routine-title".into()]),
                    },
                }],
                ..Default::default()
            },
            routine_private_parent: root.clone(),
        },
        |routes, input| {
            let service = Arc::try_unwrap(crate::session::SessionService::from_sessions(
                sessions.clone(),
                input,
            ))
            .ok()
            .unwrap()
            .with_test_provider(provider);
            let service = Arc::new(service);
            routes.add_service(
                session::session_service_server::SessionServiceServer::from_arc(service.clone()),
            );
            Ok(service as Arc<dyn crate::session::SessionHost>)
        },
    )
    .await
    .unwrap();
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    let router = app.router();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(cancel.cancelled_owned())
            .await
            .unwrap()
    });
    let foreign = issue(address, "mcp-foreign-browser").await;
    let denied = call(address, foreign["access_token"].as_str().unwrap(), "execute", serde_json::json!({"action":"session.send","input":{"id":"mcp-code-granted","prompt":"Cannot borrow another principal's grant"}})).await;
    assert!(denied.get("error").is_some(), "{denied:?}");
    assert_eq!(check.turns.load(Ordering::SeqCst), 0);
    let issued = issue(address, "composition-browser").await;
    let token = issued["access_token"].as_str().unwrap();
    assert_eq!(lists_code(address, token).await, public_code);
    assert!(!lists_code(address, foreign["access_token"].as_str().unwrap()).await);
    let proof = match oauth_check
        .authorization()
        .authorize_token(
            mcp::McpAccessToken(token.into()),
            mcp::server::McpAuthorizationContext {
                resource: oauth_check.resource().into(),
                required_scopes: oauth_check.required_scopes().to_vec(),
            },
        )
        .await
    {
        mcp::server::McpHostedTokenValidation::Authorized(proof) => proof,
        _ => panic!("actual token validation"),
    };
    let principal =
        crate::access_auth::AuthenticatedPrincipal::hosted_mcp(oauth_check.issuer(), proof);
    let owner = crate::routine::rpc::principal_id(&principal);
    let observation_service = app.routine_test_service();
    let mut request = tonic::Request::new(p::WatchInvocationsRequest {
        limit: 100,
        ..Default::default()
    });
    request.extensions_mut().insert(principal.clone());
    let mut observations = observation_service
        .watch_invocations(request)
        .await
        .unwrap()
        .into_inner();
    let denied=call(address,token,"execute",serde_json::json!({"action":"session.send","input":{"id":"mcp-code-foreign","prompt":"no grant"}})).await;
    assert!(denied.get("error").is_some(), "{denied:?}");
    let code = call(
        address,
        token,
        "code",
        serde_json::json!({"action":"execute","source":"return 1"}),
    )
    .await;
    assert_eq!(code.get("result").is_some(), public_code, "{code:?}");
    assert_eq!(code.get("error").is_some(), !public_code, "{code:?}");
    if public_code {
        let forged = call(address, token, "code", serde_json::json!({"action":"execute","source":"return 'forbidden'","session":"mcp-code-foreign","profile":"forged"})).await;
        assert!(
            forged.get("error").is_some(),
            "guest selectors cannot replace host configuration"
        );
    }
    let foreign_code = call(
        address,
        foreign["access_token"].as_str().unwrap(),
        "code",
        serde_json::json!({"action":"execute","source":"return 'forbidden'"}),
    )
    .await;
    assert!(foreign_code.get("error").is_some());
    let normal = call(address, token, "execute", serde_json::json!({"action":"session.send","input":{"id":"mcp-normal","prompt":"Normal hosted lifecycle"}})).await;
    assert!(normal.get("result").is_some(), "{normal:?}");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(update) = normal_updates.next().await {
            if let waltr_agent::SessionListEvent::Upsert(summary) = update.event {
                if summary.id.as_str() == "mcp-normal"
                    && summary.current_prompt.is_some()
                    && summary.status == waltr_agent::SessionStatus::Finished
                    && summary.coordination.phase == waltr_agent::SessionCoordinationPhase::Ready
                {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut completed = BTreeSet::new();
        while let Some(update) = observations.next().await {
            for row in update.unwrap().invocations {
                if row.status >= p::InvocationStatus::Succeeded as i32 && row.status != p::InvocationStatus::Succeeded as i32 {
                    let mut request = tonic::Request::new(p::ListOperationsRequest { invocation_id: row.id.clone(), limit: 100, ..Default::default() });
                    request.extensions_mut().insert(principal.clone());
                    let operations = observation_service.list_operations(request).await;
                    panic!("normal lifecycle failed: {row:?}; operations={operations:?}");
                }
                if row.status == p::InvocationStatus::Succeeded as i32 {
                    let mut request = tonic::Request::new(p::GetInvocationRequest {
                        invocation_id: row.id,
                    });
                    request.extensions_mut().insert(principal.clone());
                    let detail = observation_service
                        .get_invocation(request)
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    completed.insert(detail.routine);
                }
            }
            if completed.contains("title") && completed.contains("compact-and-continue") {
                return;
            }
        }
        panic!("normal hosted lifecycle observations ended early");
    })
    .await
    .expect("normal lifecycle invocations must complete before revoking their token");
    drop(observations);
    if public_code {
        let initial = sessions.store().list().await.unwrap().len();
        let default_helper = call(address, token, "code", serde_json::json!({"action":"execute","source":
            "await host.call('helper.run',{selection:{kind:'named',name:'routine-title'},inference:{from:'defaults'},task:'public defaults positive control',maxTurns:1,deadlineMs:1000,responseBytes:1024,completionBytes:1024});return true;"
        })).await;
        let output = &default_helper["result"]["structuredContent"];
        assert_eq!(output["status"], "succeeded", "{default_helper:?}");
        assert_eq!(output["cleanupComplete"], true, "{default_helper:?}");
        assert_eq!(output["result"], true, "{default_helper:?}");
        assert_eq!(sessions.store().list().await.unwrap().len(), initial + 1);
        let before = sessions.store().list().await.unwrap().len();
        let source_less = call(address, token, "code", serde_json::json!({"action":"execute","source":
            "await host.call('helper.run',{selection:{kind:'named',name:'routine-title'},inference:{from:'source-session'},task:'must not reach provider',maxTurns:1,deadlineMs:1000,responseBytes:1024,completionBytes:1024});"
        })).await;
        let output = &source_less["result"]["structuredContent"];
        assert_eq!(output["status"], "uncertain", "{source_less:?}");
        assert_eq!(output["cleanupComplete"], true, "{source_less:?}");
        assert_eq!(output.get("result"), Some(&serde_json::Value::Null), "{source_less:?}");
        assert_eq!(output.get("admissionError"), Some(&serde_json::Value::Null), "{source_less:?}");
        let invocation_id = output["invocationId"].as_str().expect("durable invocation id");
        let mut request = tonic::Request::new(p::GetInvocationRequest {
            invocation_id: invocation_id.into(),
        });
        request.extensions_mut().insert(principal.clone());
        let invocation = observation_service.get_invocation(request).await.unwrap()
            .into_inner().invocation.unwrap();
        assert_eq!(invocation.id, invocation_id);
        assert_eq!(invocation.routine, "code-mode");
        assert_eq!(invocation.status, p::InvocationStatus::Uncertain as i32);
        let mut request = tonic::Request::new(p::ListOperationsRequest {
            invocation_id: invocation_id.into(), limit: 100, ..Default::default()
        });
        request.extensions_mut().insert(principal.clone());
        let operations = observation_service.list_operations(request).await.unwrap()
            .into_inner().operations;
        assert_eq!(operations.len(), 1, "{operations:?}");
        assert_eq!(operations[0].invocation_id, invocation_id);
        assert_eq!(operations[0].name, "helper.run");
        assert_eq!(operations[0].status, p::OperationStatus::Uncertain as i32);
        assert_eq!(sessions.store().list().await.unwrap().len(), before, "rejection precedes helper creation");
        assert_eq!(check.turns.load(Ordering::SeqCst), 0);
    }
    let response=call(address,token,"execute",serde_json::json!({"action":"session.send","input":{"id":"mcp-code-granted","prompt":"Execute the bounded original helper"}})).await;
    assert!(response.get("result").is_some(), "{response:?}");
    tokio::time::timeout(std::time::Duration::from_secs(5), check.entered.notified())
        .await
        .unwrap();
    assert!(check.live.load(Ordering::SeqCst));
    if public_code {
        let independent = issue(address, "composition-browser").await;
        let result = call(
            address,
            independent["access_token"].as_str().unwrap(),
            "execute",
            serde_json::json!({"action":"session.send","input":{"id":"mcp-code-granted","prompt":"Independently leased queued origin"}}),
        )
        .await;
        assert!(result.get("result").is_some(), "{result:?}");
        assert!(
            check.live.load(Ordering::SeqCst),
            "another valid request must not cancel the original helper"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(update) = normal_updates.next().await {
                if let waltr_agent::SessionListEvent::Upsert(summary) = update.event {
                    if summary.id.as_str() == "mcp-code-granted"
                        && summary.coordination.queued_count == 1
                    {
                        return;
                    }
                }
            }
            panic!("independent origin was not queued");
        })
        .await
        .unwrap();
        let response = reqwest::Client::new()
            .post(format!("http://{address}/oauth/revoke"))
            .form(&[
                ("token", independent["refresh_token"].as_str().unwrap()),
                ("token_type_hint", "refresh_token"),
                ("client_id", "mcp-origin-client"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let queued = sessions
            .active_state(&waltr_agent::SessionId::new("mcp-code-granted"))
            .await
            .unwrap();
        assert_eq!(
            queued.queued_count, 1,
            "credential revocation must not remove an admitted ordinary prompt"
        );
        assert!(
            check.live.load(Ordering::SeqCst),
            "revoking queued B must not cancel active A"
        );
    }
    if revoke {
        let response = reqwest::Client::new()
            .post(format!("http://{address}/oauth/revoke"))
            .form(&[
                ("token", issued["refresh_token"].as_str().unwrap()),
                ("token_type_hint", "refresh_token"),
                ("client_id", "mcp-origin-client"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), check.released.notified())
        .await
        .expect("original helper is canceled after response and joined");
    assert!(!check.live.load(Ordering::SeqCst));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(update) = normal_updates.next().await {
            if let waltr_agent::SessionListEvent::Upsert(summary) = update.event {
                if summary.id.as_str() == "mcp-code-granted"
                    && summary.coordination.phase == waltr_agent::SessionCoordinationPhase::Ready
                    && summary.current_prompt.is_some()
                    && summary.status == waltr_agent::SessionStatus::Finished
                {
                    return;
                }
            }
        }
        panic!("originating run lost its completion publication");
    })
    .await
    .expect("original parent run must finish before application shutdown");
    app.shutdown().await;
    let rows = history
        .list(
            crate::routine::observation::ObservationScope {
                principal: owner,
                source_session: None,
            },
            0,
            100,
        )
        .await
        .unwrap();
    let code = rows
        .iter()
        .find(|row| {
            row.revision.name == "code-mode"
                && row.invocation.status == crate::routine::history::Status::Uncertain
        })
        .expect("actual shared-service code admission retained");
    for name in ["title", "compact-and-continue"] {
        assert!(
            rows.iter().any(|row| row.revision.name == name
                && row.invocation.status == crate::routine::history::Status::Succeeded),
            "normal hosted lifecycle uses the shared service: {name}"
        );
    }
    assert_eq!(
        code.invocation.status,
        crate::routine::history::Status::Uncertain,
        "in-flight helper cancellation retains the reviewed sticky uncertainty; it must not become success or be replayed"
    );
    stop.cancel();
    server.await.unwrap();
    issuer_stop.cancel();
    issuer_task.await.unwrap();
    drop(access_token);
    assert_eq!(
        check.turns.load(Ordering::SeqCst),
        2 + usize::from(public_code),
        "the admitted parent observes the canceled code result once; the leased helper is not replayed"
    );
}
