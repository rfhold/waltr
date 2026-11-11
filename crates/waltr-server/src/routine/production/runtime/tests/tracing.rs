use super::*;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use prost::Message as _;
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};

struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
impl Default for Gate {
    fn default() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

struct Provider(Arc<Gate>);
impl waltr_agent::LlmStream for Provider {
    fn name(&self) -> &'static str {
        "trace-fixture"
    }
    fn stream_request<'a>(
        &'a self,
        _: waltr_agent::CompletionRequest<'a>,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        let gate = self.0.clone();
        Box::pin(async move {
            let stream = futures_util::stream::once(async move {
                gate.entered.notify_one();
                gate.release.acquire().await.unwrap().forget();
                Ok(waltr_agent::CompletionEvent::Delta(
                    "SECRET_TRACE_RESULT".into(),
                ))
            });
            Ok(Box::pin(futures_util::StreamExt::chain(
                stream,
                futures_util::stream::iter([Ok(waltr_agent::CompletionEvent::Finished)]),
            )) as waltr_agent::BoxCompletionStream)
        })
    }
}

fn register<S: waltr_agent::SessionStore + Clone + 'static>(
    routes: &mut tonic::service::RoutesBuilder,
    input: crate::session::SessionRegistrationInput,
    sessions: waltr_agent::SessionCoordinator<S>,
    gate: Arc<Gate>,
) -> Arc<dyn crate::session::SessionHost> {
    let service = Arc::new(
        Arc::try_unwrap(crate::session::SessionService::from_sessions(
            sessions, input,
        ))
        .ok()
        .unwrap()
        .with_test_provider(Arc::new(Provider(gate))),
    );
    routes.add_service(
        waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(
            service.clone(),
        ),
    );
    service
}

fn seed_trace(root: &std::path::Path) {
    seed(root);
    let value = serde_json::json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"trace-probe","description":"Trace ownership fixture","spec":{
        "source":"export default async function (input) { const response = (await host.call('helper.run', {selection:{kind:'named',name:'routine-title'},task:'SECRET_TRACE_INPUT',maxTurns:1,deadlineMs:10000,responseBytes:65536,completionBytes:65536})).result.response; if(input) throw new Error('SECRET_SCRIPT_ERROR'); return response; }",
        "entrypoints":{"run":{"trigger":"manual"}},"capabilities":["helper.run","helper.named.routine-title"],
        "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"boolean"}},
        "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":64}}
    }});
    std::fs::write(
        root.join(".waltr/resources/routines/trace-probe.yaml"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

async fn exercise_trace(
    address: std::net::SocketAddr,
    bearer: Option<&str>,
    gate: &Gate,
    exporter: &InMemorySpanExporter,
) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
    let header = bearer.map(|value| format!("Bearer {value}"));
    let auth = header.as_deref().map(|value| ("authorization", value));
    let mut definitions = client
        .watch_definitions(request(p::WatchDefinitionsRequest {}, auth))
        .await
        .unwrap()
        .into_inner();
    let snapshot = definitions.message().await.unwrap().unwrap();
    drop(definitions);
    let definition = snapshot
        .definitions
        .iter()
        .find(|row| row.name == "trace-probe")
        .unwrap();
    let valid = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
    for browser in [false, true] {
        for mode in ["success", "cancel", "script_failure"] {
            let cancel = mode == "cancel";
            let script_failure = mode == "script_failure";
            for (parent, duplicate) in [
                (Some(valid), false),
                (None, false),
                (Some("malformed"), false),
                (
                    Some("01-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
                    false,
                ),
                (Some(valid), true),
            ] {
                exporter.reset();
                let mut watch = client
                    .watch_invocations(request(
                        p::WatchInvocationsRequest {
                            limit: 100,
                            ..Default::default()
                        },
                        auth,
                    ))
                    .await
                    .unwrap()
                    .into_inner();
                let selection = p::InvocationSelection {
                    routine: "trace-probe".into(),
                    revision: definition.revision.clone(),
                    entrypoint: "run".into(),
                    parameters: Some(p::StructuredValue {
                        json: if script_failure {
                            b"true".to_vec()
                        } else {
                            b"false".to_vec()
                        },
                    }),
                    ..Default::default()
                };
                let start = p::StartInvocationRequest {
                    selection: Some(selection),
                };
                let id = if browser {
                    let payload = start.encode_to_vec();
                    let mut frame = vec![0];
                    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                    frame.extend(payload);
                    let mut req = reqwest::Client::new()
                        .post(format!(
                            "http://{address}/waltr.routine.v2.RoutineService/StartInvocation"
                        ))
                        .header("content-type", "application/grpc-web+proto")
                        .header("x-grpc-web", "1")
                        .header("origin", format!("http://{address}"))
                        .header("tracestate", "secret=value")
                        .header("baggage", "SECRET_BAGGAGE")
                        .body(frame)
                        .timeout(std::time::Duration::from_secs(5));
                    if let Some(parent) = parent {
                        req = req.header("traceparent", parent);
                    }
                    if duplicate {
                        req = req.header("traceparent", valid);
                    }
                    if let Some(value) = &header {
                        req = req.bearer_auth(value.strip_prefix("Bearer ").unwrap());
                    }
                    let bytes = req.send().await.unwrap().bytes().await.unwrap();
                    assert!(bytes.len() >= 5 && bytes.len() <= 131072);
                    let length = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
                    p::StartInvocationResponse::decode(&bytes[5..5 + length])
                        .unwrap()
                        .invocation
                        .unwrap()
                        .id
                } else {
                    let mut req = request(start, auth);
                    if let Some(parent) = parent {
                        req.metadata_mut()
                            .insert("traceparent", parent.parse().unwrap());
                    }
                    if duplicate {
                        req.metadata_mut()
                            .append("traceparent", valid.parse().unwrap());
                    }
                    req.metadata_mut()
                        .insert("tracestate", "secret=value".parse().unwrap());
                    req.metadata_mut()
                        .insert("baggage", "SECRET_BAGGAGE".parse().unwrap());
                    client
                        .start_invocation(req)
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap()
                        .id
                };
                tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
                    .await
                    .unwrap();
                let spans = exporter.get_finished_spans().unwrap();
                let rpc = spans
                    .iter()
                    .find(|span| span.name == "routine.v2.start")
                    .expect("RPC span must end before its original helper");
                assert!(
                    spans.iter().all(|span| span.name != "routine.invocation"
                        && span.name != "routine.host_call")
                );
                let rpc_id = rpc.span_context.span_id();
                let trace_id = rpc.span_context.trace_id();
                assert_eq!(rpc.span_context.trace_state().header(), "");
                assert_eq!(
                    trace_id.to_string() == "0123456789abcdef0123456789abcdef",
                    parent == Some(valid) && !duplicate
                );
                if cancel {
                    client
                        .cancel_invocation(request(
                            p::CancelInvocationRequest {
                                invocation_id: id.clone(),
                            },
                            auth,
                        ))
                        .await
                        .unwrap();
                } else {
                    gate.release.add_permits(1);
                }
                let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let event = watch.message().await.unwrap().unwrap();
                        if event.invocations.iter().any(|row| row.id == id) {
                            let detail = client
                                .get_invocation(request(
                                    p::GetInvocationRequest {
                                        invocation_id: id.clone(),
                                    },
                                    auth,
                                ))
                                .await
                                .unwrap()
                                .into_inner()
                                .invocation
                                .unwrap();
                            assert!(
                                script_failure
                                    || detail.status != p::InvocationStatus::Failed as i32,
                                "unexpected trace fixture failure: {detail:?}"
                            );
                            if detail.status == p::InvocationStatus::Failed as i32
                                || detail.status == p::InvocationStatus::Succeeded as i32
                                || detail.status == p::InvocationStatus::Uncertain as i32
                                || detail.status == p::InvocationStatus::Interrupted as i32
                            {
                                break;
                            }
                        }
                    }
                })
                .await;
                if terminal.is_err() {
                    let detail = client
                        .get_invocation(request(
                            p::GetInvocationRequest {
                                invocation_id: id.clone(),
                            },
                            auth,
                        ))
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    panic!(
                        "trace fixture terminal wait: browser={browser} cancel={cancel} parent={parent:?} status={}",
                        detail.status
                    );
                }
                drop(watch);
                let spans = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let spans = exporter.get_finished_spans().unwrap();
                        if spans.iter().any(|span| span.name == "routine.invocation")
                            && spans.iter().any(|span| span.name == "routine.host_call")
                        {
                            break spans;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "owned spans did not end: {:?}",
                        exporter
                            .get_finished_spans()
                            .unwrap()
                            .iter()
                            .map(|span| span.name.as_ref())
                            .collect::<Vec<_>>()
                    )
                });
                let invocation = spans
                    .iter()
                    .find(|span| span.name == "routine.invocation")
                    .unwrap();
                let host = spans
                    .iter()
                    .find(|span| span.name == "routine.host_call")
                    .unwrap();
                assert_eq!(invocation.parent_span_id, rpc_id);
                assert_eq!(host.parent_span_id, invocation.span_context.span_id());
                assert!(
                    invocation
                        .attributes
                        .iter()
                        .any(|field| field.key.as_str() == "routine_name"
                            && field.value.as_str() == "trace-probe")
                );
                assert!(invocation.attributes.iter().any(|field| field.key.as_str()
                    == "invocation_id"
                    && field.value.as_str() == id));
                if script_failure {
                    for (key, value) in [
                        ("outcome", "failure"),
                        ("failed_stage", "engine"),
                        ("error_category", "script"),
                    ] {
                        assert!(
                            invocation
                                .attributes
                                .iter()
                                .any(|field| field.key.as_str() == key
                                    && field.value.as_str() == value),
                            "missing {key}={value}"
                        );
                    }
                    assert!(
                        host.attributes
                            .iter()
                            .any(|field| field.key.as_str() == "outcome"
                                && field.value.as_str() == "success")
                    );
                }
                for span in [invocation, host] {
                    assert_eq!(span.span_context.trace_id(), trace_id);
                    assert_eq!(span.span_context.trace_state().header(), "");
                }
                let diagnostic = format!("{spans:?}");
                for secret in [
                    "SECRET_TRACE_INPUT",
                    "SECRET_TRACE_RESULT",
                    "SECRET_BAGGAGE",
                    "secret=value",
                    "SECRET_SCRIPT_ERROR",
                ] {
                    assert!(!diagnostic.contains(secret), "trace leaked {secret}");
                }
                assert!(rpc.attributes.iter().any(
                    |field| field.key.as_str() == "grpc_status" && field.value.as_str() == "0"
                ));
            }
        }
    }
    exporter.reset();
    let mut rejected = request(
        p::StartInvocationRequest {
            selection: Some(p::InvocationSelection {
                routine: "trace-probe".into(),
                revision: definition.revision.clone(),
                entrypoint: "run".into(),
                parameters: Some(p::StructuredValue {
                    json: b"1".to_vec(),
                }),
                ..Default::default()
            }),
        },
        auth,
    );
    rejected
        .metadata_mut()
        .insert("traceparent", valid.parse().unwrap());
    assert_eq!(
        client.start_invocation(rejected).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    let spans = exporter.get_finished_spans().unwrap();
    let rejected = spans
        .iter()
        .find(|span| span.name == "routine.v2.start")
        .unwrap();
    assert!(
        rejected
            .attributes
            .iter()
            .any(|field| field.key.as_str() == "grpc_status" && field.value.as_str() == "3")
    );
    assert!(
        rejected
            .attributes
            .iter()
            .any(|field| field.key.as_str() == "outcome" && field.value.as_str() == "failure")
    );
    assert!(spans.iter().all(|span| span.name != "routine.invocation"));

    exporter.reset();
    let media = snapshot
        .definitions
        .iter()
        .find(|row| row.name == "media-probe")
        .unwrap();
    let frames = tokio_stream::iter([
        p::InteractRequest {
            frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    routine: media.name.clone(),
                    revision: media.revision.clone(),
                    entrypoint: "run".into(),
                    parameters: Some(p::StructuredValue {
                        json: b"null".to_vec(),
                    }),
                    ..Default::default()
                }),
                inputs: vec![],
            })),
        },
        p::InteractRequest {
            frame: Some(p::interact_request::Frame::Input(p::InputEvent {
                event: Some(p::input_event::Event::Finish(p::InputFinish {})),
            })),
        },
    ]);
    let mut req = request(frames, auth);
    req.metadata_mut()
        .insert("traceparent", valid.parse().unwrap());
    let mut output = client.interact(req).await.unwrap().into_inner();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut terminal = false;
        while let Some(frame) = output.message().await.unwrap() {
            if let Some(p::output_event::Event::Terminal(value)) = frame.output.unwrap().event {
                assert_eq!(value.status, p::InvocationStatus::Succeeded as i32);
                assert!(value.cleanup_complete);
                terminal = true;
            }
        }
        assert!(terminal);
    })
    .await
    .unwrap();
    drop(output);
    let spans = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let spans = exporter.get_finished_spans().unwrap();
            if spans.iter().any(|span| span.name == "routine.invocation")
                && spans.iter().any(|span| span.name == "routine.host_call")
            {
                break spans;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rpc = spans
        .iter()
        .find(|span| span.name == "routine.v2.interact")
        .unwrap();
    let invocation = spans
        .iter()
        .find(|span| span.name == "routine.invocation")
        .unwrap();
    let host = spans
        .iter()
        .find(|span| span.name == "routine.host_call")
        .unwrap();
    assert_eq!(invocation.parent_span_id, rpc.span_context.span_id());
    assert_eq!(host.parent_span_id, invocation.span_context.span_id());
    assert_eq!(
        host.span_context.trace_id().to_string(),
        "0123456789abcdef0123456789abcdef"
    );
}

#[tokio::test]
async fn v2_owned_trace_chain_survives_native_and_browser_handler_return_in_both_compositions() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::tracing::v2_owned_trace_chain_survives_native_and_browser_handler_return_in_both_compositions",
    ) {
        return;
    }
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("v2-test"))
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.is_span()
                    && (metadata.target() == "waltr_server::routine::telemetry"
                        || metadata.target() == "waltr_server::routine::service")
            })),
    );
    let _guard = ::tracing::subscriber::set_default(subscriber);
    let root = crate::test_support::temp_root();
    let resources = root.join("standalone");
    seed_trace(&resources);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mcp = resources.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::write(mcp.join("waltr.yaml"),format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: waltr}}\nspec:\n  transport: streamableHttp\n  url: http://{address}/mcp\n")).unwrap();
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
    let gate = Arc::new(Gate::default());
    let registered_gate = gate.clone();
    let app = assemble_app(
        AppServices {
            config_service,
            persistence,
            catalog_loader: loader,
            access_authenticator: crate::access_auth::AccessAuthenticator::disabled(),
            mcp_oauth: None,
            outbound_private_origins: vec![],
            outbound_oauth: Default::default(),
            automatic_self_mcp_resource: None,
            agent_state: Some(state),
            local_admission: None,
            workspace_root: workspace,
            sandbox_socket: None,
            routine_authority: Default::default(),
            routine_private_parent: root.clone(),
        },
        move |routes, input| {
            Ok(register(
                routes,
                input,
                waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()),
                registered_gate,
            ))
        },
    )
    .await
    .unwrap();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let task =
        tokio::spawn(app.serve_with_shutdown(listener, async move { shutdown.cancelled().await }));
    exercise_trace(address, None, &gate, &exporter).await;
    stop.cancel();
    task.await.unwrap().unwrap();
    let resources = root.join("embedded");
    seed_trace(&resources);
    let gate = Arc::new(Gate::default());
    let registered_gate = gate.clone();
    let embedded = crate::embedded::build_embedded_server_with_registration(
        crate::embedded::EmbeddedServerConfig {
            resources_root: resources,
            waltr_database_path: root.join("data/waltr.sqlite3"),
            mcp_database_path: root.join("data/mcp.sqlite3"),
            session_root: root.join("sessions"),
            workspace_root: root.join("embedded-workspace"),
            catalog_source: crate::embedded::EmbeddedCatalogSource::Path(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/models-dev-defaults.json"),
            ),
            catalog_cache_path: root.join("cache/models.json"),
            catalog_fallback: None,
            default_agent: "coding".into(),
            local_bearer: crate::LocalBearer::new("trace-generation").unwrap(),
            outbound_policy: Default::default(),
            routine_authority: Default::default(),
        },
        move |routes, input, session_root| {
            let store = waltr_agent::FilesystemSessionStore::new(session_root);
            Ok(register(
                routes,
                input,
                waltr_agent::SessionCoordinator::new(store),
                registered_gate,
            ))
        },
    )
    .await
    .unwrap();
    let address = embedded.local_addr();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let task =
        tokio::spawn(embedded.serve_with_shutdown(async move { shutdown.cancelled().await }));
    exercise_trace(address, Some("trace-generation"), &gate, &exporter).await;
    stop.cancel();
    task.await.unwrap().unwrap();
    provider.shutdown().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
