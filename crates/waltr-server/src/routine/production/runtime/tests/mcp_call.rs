use super::*;
use serde_json::{Value, json};
use waltr_agent::{MemorySessionStore, SessionCreateRequest, SessionId};

const TOOL: &str = "read.marker";

const CHILD: &str = r#"
IFS= read -r marker < marker || exit 1
printf '%s\n' "$$" >> launched
while IFS= read -r request; do
    id=${request#*'"id":'}
    id=${id%%,*}
    id=${id%%\}*}
    case "$request" in
        *'"method":"server/discover"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}}\n' "$id" ;;
        *'"method":"tools/list"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"read.marker","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
        *'"method":"tools/call"'*)
            printf 'called\n' >> calls
            case "$request" in
                *'"mode":"cancel"'*|*'"mode":"retire-source"'*) continue ;;
                *'"mode":"disconnect"'*) exit 0 ;;
                *'"mode":"catalog-edit"'*)
                    while [ ! -f allow-result ]; do sleep 0.02; done ;;
                *'"mode":"replace-restore"'*)
                    while [ ! -f allow-continuation ]; do sleep 0.02; done
                    printf '{"jsonrpc":"2.0","id":%s,"result":{"resultType":"input_required","requestState":"fixture"}}\n' "$id"
                    continue ;;
            esac
            printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$marker" ;;
    esac
done
"#;

struct HttpMarker(Arc<std::sync::atomic::AtomicUsize>);

impl mcp::server::ServerHandler for HttpMarker {
    fn capabilities(&self) -> mcp::server::ServerCapabilities {
        mcp::server::ServerCapabilities::new().tools()
    }
    fn server_info(&self) -> mcp::server::ServerInfo {
        mcp::server::ServerInfo::new("routine-http-marker", "1")
    }
    fn list_tools(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpToolList>> {
        Box::pin(async {
            Ok(mcp::McpToolList {
                tools: [
                    json!({"name":"http.marker","inputSchema":{"type":"object"}}),
                    json!({"name":"http.app-only","inputSchema":{"type":"object"},"_meta":{"ui":{"resourceUri":"ui://routine/fixture","visibility":["app"]}}}),
                ]
                .iter()
                .map(|definition| mcp::parse_tool_definition(definition).unwrap())
                .collect(),
                next_cursor: None,
            })
        })
    }
    fn call_tool(
        &self,
        call: mcp::McpToolCall,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpToolResult>> {
        assert_eq!(call.name, "http.marker");
        self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Box::pin(async move {
            let text = if call.arguments.get("large") == Some(&json!(true)) {
                "x".repeat(70_000)
            } else {
                "server-http-marker".into()
            };
            Ok(mcp::McpToolResult {
                raw: json!({"content":[{"type":"text","text":text}]}),
            })
        })
    }
}

fn install(resources: &std::path::Path, http: &str) {
    seed(resources);
    let files = resources.join(".waltr/resources");
    let agents = files.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("routine-mcp-source.yaml"),
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: routine-mcp-source}\nspec:\n  features:\n    - mcp: {servers: {include: [routine-stdio, routine-refused, routine-http]}, defaultPermissions: false}\n  permissions: [{feature: mcp, permission: allow}]\n"
    ).unwrap();
    std::fs::write(files.join("mcp-servers/routine-http.yaml"),
        format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: routine-http}}\nspec: {{transport: streamableHttp, url: '{http}'}}\n")
    ).unwrap();
    std::fs::write(files.join("mcp-servers/routine-stdio.yaml"),
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: routine-stdio}\nspec: {transport: stdio, command: '/bin/sh ./child.sh'}\n"
    ).unwrap();
    let missing = resources.join("uninstalled-fixture-executable");
    std::fs::write(files.join("mcp-servers/routine-refused.yaml"),
        format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: routine-refused}}\nspec: {{transport: stdio, command: '{}'}}\n", missing.display())
    ).unwrap();
    let script = r#"const call = (server, tool, args) => host.call('tools.execute', {target:{server,tool},...args});
     export default async function (mode) {
         if (mode === 'catalog-edit') {
             const actual = await call('routine-stdio','read.marker',{arguments:{mode},workspace:{machine:'local',workingDirectory:'bound'}});
             const stale = await call('routine-refused','read.marker',{arguments:{},workspace:{machine:'local',workingDirectory:'bound'}});
             return actual.status === 'completed' && JSON.stringify(actual.result).includes('worker-only-marker') && stale.status === 'denied';
         }
         if (mode !== 'success') {
            try {
                await call('routine-stdio','read.marker',{arguments:{mode},workspace:{machine:'local',workingDirectory:'bound'}});
            } catch (_) {}
             if (mode === 'disconnect' || mode === 'replace-restore') {
                try {
                    await call('routine-stdio','read.marker',{arguments:{},workspace:{machine:'local',workingDirectory:'bound'}});
                } catch (_) {}
            }
            return false;
        }
        for (const tool of ['read_marker', 'not-granted']) {
            let denied = false;
            try { await call('routine-stdio',tool,{arguments:{}}); } catch (_) { denied = true; }
            if (!denied) throw Error('missing exact grant');
        }
        let retired = false;
        try { await host.call('mcp.call', {server:'routine-stdio',tool:'read.marker',arguments:{}}); } catch (_) { retired = true; }
        if (!retired) throw Error('retired operation accepted');
        const noMachine = await call('routine-stdio','read.marker',{arguments:{}});
        if (noMachine.status !== 'denied') throw Error('implicit workspace');
        const wrongMachine = await call('routine-stdio','read.marker',{arguments:{},workspace:{machine:'unclaimed-machine',workingDirectory:'bound'}});
        if (wrongMachine.status !== 'denied') throw Error('unauthorized workspace');
        let first;
        for (let i = 0; i < 2; ++i) {
            if (i === 1) {
                const refused = await call('routine-refused','read.marker',{arguments:{},workspace:{machine:'local',workingDirectory:'bound'}});
                if (refused.status !== 'denied') throw Error('worker open refusal was not known');
            }
            const actual = await call('routine-stdio','read.marker',{arguments:{},workspace:{machine:'local',workingDirectory:'bound'}});
            if (actual.status !== 'completed' || actual.result.is_error || !JSON.stringify(actual.result).includes('worker-only-marker')) {
                throw Error('worker result did not match');
            }
            first = actual.result;
        }
         const http = await call('routine-http','http.marker',{arguments:{},workspace:{machine:'unclaimed-machine',workingDirectory:'bound'}});
         const large = await call('routine-http','http.marker',{arguments:{large:true},result:'handle'});
         const projection = await host.call('tools.read', {}, null, null, large);
         if (projection.status !== 'completed' || projection.resultUnavailable !== 'limit') throw Error('completed effect lost on result limit');
        const app = await call('routine-http','http.app-only',{arguments:{}});
        if (app.status !== 'denied') throw new Error('App-only tool exposed to Routine');
        return JSON.stringify(first).includes('worker-only-marker') && http.status === 'completed' && !http.result.is_error && JSON.stringify(http.result).includes('server-http-marker');
    }"#;
    std::fs::write(files.join("routines/direct-mcp.yaml"), serde_json::to_vec(&json!({
        "apiVersion":"waltr.routine/v1","kind":"Routine","name":"direct-mcp","description":"Direct MCP contract",
        "spec":{"source":script,"entrypoints":{"run":{"trigger":"manual"}},
            "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":32}},
            "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"boolean"}},
            "capabilities":["tools.execute"],
            "mcpTools":[{"server":"routine-stdio","tool":TOOL},{"server":"routine-refused","tool":TOOL},
                {"server":"routine-http","tool":"http.marker"},{"server":"routine-http","tool":"http.app-only"}]}
    })).unwrap()).unwrap();
}

#[tokio::test]
async fn installed_routine_dispatches_exact_worker_mcp_without_inference_or_origin_fallback() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let root = crate::test_support::temp_root();
        let workspace = root.join("workspace");
        let bound = workspace.join("bound");
        std::fs::create_dir_all(&bound).unwrap();
        std::fs::write(bound.join("child.sh"), CHILD).unwrap();
        std::fs::write(bound.join("marker"), "worker-only-marker\n").unwrap();
        let resources = root.join("resources");
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = format!("http://{}/mcp", http_listener.local_addr().unwrap());
        let http_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let http_router =
            mcp::server::streamable_http_router(Arc::new(HttpMarker(http_calls.clone())));
        let http_server =
            tokio::spawn(async move { axum::serve(http_listener, http_router).await.unwrap() });
        install(&resources, &http);
        let loader = crate::test_support::default_catalog_loader();
        let config = ConfigService::new(resources.clone(), loader.clone())
            .await
            .unwrap();
        let state = config.state().clone();
        let snapshot = state.snapshot();
        let (catalog, _) = catalog_entries(&snapshot).unwrap();
        assert_eq!(
            catalog["direct-mcp"].offers,
            BTreeMap::from([(
                "run".into(),
                BTreeSet::from([StartContext::Global, StartContext::Session])
            )])
        );
        let routine = snapshot.scripts().get("direct-mcp").unwrap();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let source_id = SessionId::generate();
        let mut create = SessionCreateRequest::new(source_id.clone());
        create.agent_definition = snapshot
            .catalog()
            .bundle("routine-mcp-source")
            .map(|bundle| serde_json::to_value(bundle).unwrap());
        create.inference_settings = Some(bindings::default_inference(&snapshot).unwrap());
        sessions.create(create).await.unwrap();
        let supplied_sessions = sessions.clone();
        let persistence = Persistence::initialize(
            crate::persistence::DatabaseConfig::from_urls(
                &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
                &format!("sqlite://{}", root.join("mcp.sqlite3").display()),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let app = assemble_app(
            AppServices {
                config_service: config,
                persistence,
                catalog_loader: loader,
                access_authenticator: crate::access_auth::AccessAuthenticator::disabled(),
                mcp_oauth: None,
                outbound_oauth: OutboundOAuthConfig::default(),
                agent_state: Some(state.clone()),
                local_admission: None,
                workspace_root: workspace.clone(),
                sandbox_socket: None,
                routine_private_parent: root,
                routine_authority: RoutineAuthority::default(),
            },
            move |_, input| {
                Ok(crate::session::SessionService::from_sessions(
                    supplied_sessions,
                    input,
                ))
            },
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let router = app.router().layer(axum::Extension(
            crate::local_admission::ValidatedLocalOrigin(Arc::new(CancellationToken::new())),
        ));
        let server = tokio::spawn(async move {
            let result = crate::http_lifecycle::serve_owned_connections(
                listener,
                router,
                shutdown.cancelled_owned(),
            )
            .await;
            app.shutdown().await;
            result
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut service = p::routine_service_client::RoutineServiceClient::new(channel);
        let start = |source_session, mode: &str| p::StartInvocationRequest {
            selection: Some(p::InvocationSelection {
                routine: "direct-mcp".into(),
                revision: routine.revision().to_owned(),
                entrypoint: "run".into(),
                parameters: Some(p::StructuredValue {
                    json: serde_json::to_vec(mode).unwrap(),
                }),
                source_session,
                ..Default::default()
            }),
        };
        let mut session_changes = sessions.watch_sessions();
        let invocation = service
            .start_invocation(start(None, "success"))
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        let mut watch = service
            .watch_invocations(p::WatchInvocationsRequest {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        while let Some(update) = watch.message().await.unwrap() {
            if update.invocations.iter().any(|row| {
                row.id == invocation.id && row.status >= p::InvocationStatus::Succeeded as i32
            }) {
                break;
            }
        }
        let row = service
            .get_invocation(p::GetInvocationRequest {
                invocation_id: invocation.id.clone(),
            })
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        assert_eq!(row.status, p::InvocationStatus::Succeeded as i32, "{row:?}");
        use futures_util::{FutureExt as _, StreamExt as _};
        assert!(session_changes.next().now_or_never().is_none());
        assert_eq!(
            serde_json::from_slice::<Value>(&row.result.unwrap().json).unwrap(),
            json!(true)
        );
        let operations = service
            .list_operations(p::ListOperationsRequest {
                invocation_id: invocation.id,
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
            .operations;
        let refusal = operations
            .iter()
            .find(|operation| {
                operation.arguments.as_ref().is_some_and(|value| {
                    serde_json::from_slice::<Value>(&value.json)
                        .unwrap()
                        .pointer("/target/server")
                        .and_then(Value::as_str)
                        == Some("routine-refused")
                })
            })
            .expect("durably recorded worker refusal");
        assert_eq!(refusal.status, p::OperationStatus::Completed as i32);
        assert_eq!(
            serde_json::from_slice::<Value>(&refusal.result.as_ref().unwrap().json).unwrap(),
            json!({"status":"denied"})
        );
        assert_eq!(
            std::fs::read_to_string(bound.join("calls")).unwrap(),
            "called\ncalled\n"
        );
        assert_eq!(
            std::fs::read_to_string(bound.join("launched"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(!workspace.join("launched").exists());
        assert_eq!(http_calls.load(std::sync::atomic::Ordering::Acquire), 2);
        assert_reaped(&bound);
        for (mode, expected_calls) in [
            ("disconnect", 3),
            ("cancel", 4),
            ("replace-restore", 5),
            ("retire-source", 6),
            ("catalog-edit", 7),
        ] {
            let invocation = service
                .start_invocation(start(
                    (mode != "catalog-edit").then(|| source_id.as_str().into()),
                    mode,
                ))
                .await
                .unwrap()
                .into_inner()
                .invocation
                .unwrap();
            if mode != "disconnect" {
                while std::fs::read_to_string(bound.join("calls"))
                    .unwrap()
                    .lines()
                    .count()
                    < expected_calls
                {
                    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                }
                match mode {
                    "cancel" => {
                        service
                            .cancel_invocation(p::CancelInvocationRequest {
                                invocation_id: invocation.id.clone(),
                            })
                            .await
                            .unwrap();
                    }
                    "replace-restore" => {
                        let path = resources.join(".waltr/resources/routines/direct-mcp.yaml");
                        let original = std::fs::read_to_string(&path).unwrap();
                        std::fs::write(
                            &path,
                            original.replace("Direct MCP contract", "Replacement MCP contract"),
                        )
                        .unwrap();
                        state.reload_from_disk().await;
                        assert!(state.reload_error().is_none());
                        std::fs::write(path, original).unwrap();
                        state.reload_from_disk().await;
                        assert!(state.reload_error().is_none());
                        assert_eq!(
                            state.snapshot().scripts()["direct-mcp"].revision(),
                            routine.revision()
                        );
                        std::fs::write(bound.join("allow-continuation"), "").unwrap();
                    }
                    "retire-source" => sessions.delete(&source_id).await.unwrap(),
                    "catalog-edit" => {
                        let path =
                            resources.join(".waltr/resources/mcp-servers/routine-stdio.yaml");
                        let original = std::fs::read_to_string(&path).unwrap();
                        std::fs::write(
                            &path,
                            original.replace("/bin/sh ./child.sh", "/bin/sh ./replacement.sh"),
                        )
                        .unwrap();
                        let unopened =
                            resources.join(".waltr/resources/mcp-servers/routine-refused.yaml");
                        let original = std::fs::read_to_string(&unopened).unwrap();
                        std::fs::write(
                            unopened,
                            original.replace("uninstalled-fixture-executable", "new-executable"),
                        )
                        .unwrap();
                        state.reload_from_disk().await;
                        assert!(state.reload_error().is_none());
                        std::fs::write(bound.join("allow-result"), "").unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            while let Some(update) = watch.message().await.unwrap() {
                if update.invocations.iter().any(|row| {
                    row.id == invocation.id && row.status >= p::InvocationStatus::Succeeded as i32
                }) {
                    break;
                }
            }
            let row = service
                .get_invocation(p::GetInvocationRequest {
                    invocation_id: invocation.id,
                })
                .await
                .unwrap()
                .into_inner()
                .invocation
                .unwrap();
            if mode == "catalog-edit" {
                assert_eq!(row.status, p::InvocationStatus::Succeeded as i32, "{row:?}");
                assert_eq!(
                    serde_json::from_slice::<Value>(&row.result.unwrap().json).unwrap(),
                    json!(true)
                );
            } else {
                assert_eq!(
                    row.status,
                    p::InvocationStatus::Uncertain as i32,
                    "{mode}: {row:?}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(bound.join("calls"))
                    .unwrap()
                    .lines()
                    .count(),
                expected_calls
            );
            assert_eq!(
                std::fs::read_to_string(bound.join("launched"))
                    .unwrap()
                    .lines()
                    .count(),
                expected_calls - 1
            );
            assert_reaped(&bound);
        }
        drop(watch);
        stop.cancel();
        server.await.unwrap().unwrap();
        http_server.abort();
    })
    .await
    .expect("bounded Routine worker fixture");
}

fn assert_reaped(bound: &std::path::Path) {
    for pid in std::fs::read_to_string(bound.join("launched"))
        .unwrap()
        .lines()
    {
        assert!(
            !std::process::Command::new("/bin/kill")
                .arg("-0")
                .arg(pid)
                .output()
                .unwrap()
                .status
                .success(),
            "stdio child {pid} still running"
        );
    }
}
