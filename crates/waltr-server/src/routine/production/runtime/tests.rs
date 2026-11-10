use super::*;
use crate::{
    app::{AppServices, assemble_app},
    auth::OutboundOAuthConfig,
    config_service::ConfigService,
};
use waltr::grpc::waltr::routine::v2 as p;

mod context_workflows;
mod primitives;
mod corrections;
mod mcp_origin;
mod media_upstream;
mod media_workflows;
mod tracing;
mod title_overlap;
mod socket_workspace;

#[derive(Default)]
struct ScriptedProvider {
    code_turns: std::sync::atomic::AtomicUsize,
    fork_directions: std::sync::atomic::AtomicUsize,
    title_gate: std::sync::Mutex<Option<Arc<TitleGate>>>,
    overlap_gate: std::sync::Mutex<Option<Arc<title_overlap::ParentGate>>>,
}
struct TitleGate {
    helper: tokio::sync::Semaphore,
    parent: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}
impl waltr_agent::LlmStream for ScriptedProvider {
    fn name(&self) -> &'static str {
        "production-fixture"
    }
    fn stream_request<'a>(
        &'a self,
        request: waltr_agent::CompletionRequest<'a>,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        if request.tools.iter().any(|tool| tool.name == "code") {
            let turn = self
                .code_turns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let event = if turn == 0 {
                waltr_agent::CompletionEvent::ToolCall(waltr_agent::ToolCall::new(
                    "production-code-call",
                    "code",
                    serde_json::json!({"action":"execute", "source":"return 'granted-code';"}),
                ))
            } else {
                waltr_agent::CompletionEvent::Delta("Code finished".into())
            };
            return Box::pin(async {
                Ok(Box::pin(futures_util::stream::iter([
                    Ok(event),
                    Ok(waltr_agent::CompletionEvent::Finished),
                ])) as waltr_agent::BoxCompletionStream)
            });
        }
        if request.system_prompt.as_deref().is_some_and(|value| {
            value.starts_with("Compact the immutable source snapshot for a new session.")
        }) {
            assert!(request.prompt.contains(
                "<fork-direction>\nPreserve the exact technical handoff\n</fork-direction>"
            ));
            self.fork_directions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let gate = self.title_gate.lock().unwrap().clone();
        let parent = request.prompt == "Check the shared production helper and completion boundary";
        let title = request.prompt.starts_with('{');
        let overlap = self.overlap_gate.lock().unwrap().clone();
        let overlap_parent = request.prompt == title_overlap::PROMPT;
        Box::pin(async move {
            if overlap_parent {
                let gate = overlap.expect("installed parent barrier");
                gate.requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                gate.entered.add_permits(1);
                tokio::time::timeout(std::time::Duration::from_secs(10), gate.release.acquire())
                    .await.expect("title commit releases parent").unwrap().forget();
            }
            if let Some(gate) = gate {
                if parent { gate.parent.add_permits(1); }
                else if title {
                    gate.helper.add_permits(1);
                    tokio::time::timeout(std::time::Duration::from_secs(3), gate.release.acquire()).await.expect("blocked title released").unwrap().forget();
                }
            }
            Ok(Box::pin(futures_util::stream::iter([
                Ok(waltr_agent::CompletionEvent::Delta(
                    "Verified normal helper".into(),
                )),
                Ok(waltr_agent::CompletionEvent::Finished),
            ])) as waltr_agent::BoxCompletionStream)
        })
    }
}

fn seed(root: &std::path::Path) {
    let mcp = root.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::write(mcp.join("waltr.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: waltr}\nspec:\n  transport: streamableHttp\n  url: http://127.0.0.1:1\n  path: /mcp\n").unwrap();
    let routines = root.join(".waltr/resources/routines");
    std::fs::create_dir_all(&routines).unwrap();
    for (name, source) in [
        (
            "probe",
            "export default async function () { return 'shared-service'; }",
        ),
        (
            "pending",
            "export default async function () { while (true) {} }",
        ),
        (
            "media-probe",
            "export default async function () { await host.call('media.finish', {}); return 'transport-finished'; }",
        ),
    ] {
        let value = serde_json::json!({"apiVersion":"waltr.routine/v1", "kind":"Routine", "name":name, "description":"Production composition fixture", "spec":{
            "source":source, "entrypoints":{"run":{"trigger":if name == "media-probe" { "interactive" } else { "manual" }}},
            "capabilities": if name == "media-probe" { vec!["media.execute"] } else { vec![] },
            "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
            "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":64}}
        }});
        std::fs::write(
            routines.join(format!("{name}.yaml")),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
}

fn request<T>(value: T, auth: Option<(&str, &str)>) -> tonic::Request<T> {
    let mut request = tonic::Request::new(value);
    if let Some((name, value)) = auth {
        request.metadata_mut().insert(
            tonic::metadata::MetadataKey::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    request
}
async fn exercise(address: std::net::SocketAddr, bearer: Option<&str>) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let header = bearer.map(|value| {
        if value.starts_with("waltr-session-local=") {
            ("cookie", value.to_owned())
        } else {
            ("authorization", format!("Bearer {value}"))
        }
    });
    let auth = header.as_ref().map(|(name, value)| (*name, value.as_str()));
    let interceptor_header = header.clone();
    let mut client = p::routine_service_client::RoutineServiceClient::with_interceptor(
        channel.clone(),
        move |mut request: tonic::Request<()>| {
            if let Some((name, value)) = &interceptor_header {
                request.metadata_mut().insert(*name, value.parse().unwrap());
            }
            Ok(request)
        },
    );
    let mut definitions = client
        .watch_definitions(p::WatchDefinitionsRequest {})
        .await
        .unwrap()
        .into_inner();
    let snapshot = definitions.message().await.unwrap().unwrap();
    assert!(
        snapshot
            .definitions
            .iter()
            .any(|definition| definition.name == "configuration")
    );
    assert!(
        snapshot
            .definitions
            .iter()
            .flat_map(|definition| &definition.entrypoints)
            .filter(|entry| entry.trigger == p::Trigger::Schedule as i32)
            .all(|entry| entry.start_contexts.is_empty())
    );
    for (name, cancel) in [("probe", false), ("pending", true)] {
        let definition = snapshot
            .definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap();
        let mut watch = client
            .watch_invocations(p::WatchInvocationsRequest {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let invocation = client
            .start_invocation(p::StartInvocationRequest {
                selection: Some(p::InvocationSelection {
                    routine: name.into(),
                    revision: definition.revision.clone(),
                    entrypoint: "run".into(),
                    parameters: Some(p::StructuredValue {
                        json: b"null".to_vec(),
                    }),
                    correlation: "production-composition".into(),
                    source_session: None,
                }),
            })
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        if cancel {
            client
                .cancel_invocation(p::CancelInvocationRequest {
                    invocation_id: invocation.id.clone(),
                })
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let update = watch.message().await.unwrap().unwrap();
                if update.invocations.iter().any(|row| {
                    row.id == invocation.id && row.status >= p::InvocationStatus::Succeeded as i32
                }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let detail = client
            .get_invocation(p::GetInvocationRequest {
                invocation_id: invocation.id,
            })
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        if cancel {
            assert_eq!(detail.status, p::InvocationStatus::Interrupted as i32);
        } else {
            assert_eq!(detail.status, p::InvocationStatus::Succeeded as i32);
            assert_eq!(detail.result.unwrap().json, br#""shared-service""#);
        }
    }
    let media = snapshot
        .definitions
        .iter()
        .find(|definition| definition.name == "media-probe")
        .unwrap();
    let frames = futures_util::stream::iter([
        p::InteractRequest {
            frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    routine: media.name.clone(),
                    revision: media.revision.clone(),
                    entrypoint: "run".into(),
                    parameters: Some(p::StructuredValue {
                        json: b"null".to_vec(),
                    }),
                    correlation: "owned-native-media".into(),
                    source_session: None,
                }),
                inputs: Vec::new(),
            })),
        },
        p::InteractRequest {
            frame: Some(p::interact_request::Frame::Input(p::InputEvent {
                event: Some(p::input_event::Event::Finish(p::InputFinish {})),
            })),
        },
    ]);
    let mut output = client.interact(frames).await.unwrap().into_inner();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut accepted = false;
        let mut terminal = false;
        while let Some(frame) = output.message().await.unwrap() {
            match frame.output.unwrap().event.unwrap() {
                p::output_event::Event::Accepted(_) => accepted = true,
                p::output_event::Event::Terminal(value) => {
                    assert!(accepted);
                    assert_eq!(value.status, p::InvocationStatus::Succeeded as i32);
                    assert!(value.cleanup_complete);
                    assert_eq!(value.result.unwrap().json, br#""transport-finished""#);
                    terminal = true;
                }
                p::output_event::Event::Rejected(_) => {
                    panic!("configured native media host rejected admission")
                }
                _ => {}
            }
        }
        assert!(terminal);
    })
    .await
    .unwrap();
    drop(definitions);
    let mut removed = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap()
        .post(format!(
            "http://{address}/waltr.routine.v1.RoutineService/ListRoutineInvocations"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(vec![0_u8; 5]);
    if let Some((name, value)) = auth {
        removed = removed.header(name, value);
    }
    let response = removed.send().await.unwrap();
    assert_eq!(response.headers().get("grpc-status").unwrap(), "12");
}

async fn browser_frame(address: std::net::SocketAddr, method: &str, payload: Vec<u8>) -> Vec<u8> {
    let mut body = vec![0];
    body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    body.extend_from_slice(&payload);
    let mut response = reqwest::Client::new()
        .post(format!("http://{address}/waltr.routine.v2.{method}"))
        .header("content-type", "application/grpc-web+proto")
        .header("origin", format!("http://{address}"))
        .header("cookie", "waltr-session-local=composition-browser")
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    if let Some(status) = response
        .headers()
        .get("grpc-status")
        .filter(|status| *status != "0")
    {
        let payload = format!("grpc-status: {}\r\n", status.to_str().unwrap());
        let mut frame = vec![128];
        frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(payload.as_bytes());
        return frame;
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            bytes.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            assert!(bytes.len() <= 131072);
            if bytes.len() >= 5
                && bytes.len() >= 5 + u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize
            {
                return bytes;
            }
        }
    })
    .await
    .unwrap()
}

async fn browser_routes(address: std::net::SocketAddr) {
    use prost::Message as _;
    let bytes = browser_frame(
        address,
        "RoutineService/WatchDefinitions",
        p::WatchDefinitionsRequest {}.encode_to_vec(),
    )
    .await;
    assert_eq!(bytes[0], 0);
    let size = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
    let definitions = p::WatchDefinitionsResponse::decode(&bytes[5..5 + size]).unwrap();
    assert!(
        definitions
            .definitions
            .iter()
            .any(|definition| definition.name == "probe")
    );
    for (name, cancel) in [("probe", false), ("pending", true)] {
        let definition = definitions
            .definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap();
        let bytes = browser_frame(
            address,
            "RoutineService/StartInvocation",
            p::StartInvocationRequest {
                selection: Some(p::InvocationSelection {
                    routine: name.into(),
                    revision: definition.revision.clone(),
                    entrypoint: "run".into(),
                    parameters: Some(p::StructuredValue {
                        json: b"null".to_vec(),
                    }),
                    correlation: "browser-composition".into(),
                    source_session: None,
                }),
            }
            .encode_to_vec(),
        )
        .await;
        assert_eq!(bytes[0], 0);
        let size = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        let id = p::StartInvocationResponse::decode(&bytes[5..5 + size])
            .unwrap()
            .invocation
            .unwrap()
            .id;
        if cancel {
            let bytes = browser_frame(
                address,
                "RoutineService/CancelInvocation",
                p::CancelInvocationRequest {
                    invocation_id: id.clone(),
                }
                .encode_to_vec(),
            )
            .await;
            assert_eq!(bytes[0], 0);
        }
        browser_terminal(address, &id, cancel, "shared-service").await;
    }
}

async fn lifecycle_routes(address: std::net::SocketAddr, bearer: &str, provider: &ScriptedProvider) {
    let gate = Arc::new(TitleGate { helper: tokio::sync::Semaphore::new(0), parent: tokio::sync::Semaphore::new(0), release: tokio::sync::Semaphore::new(0) });
    *provider.title_gate.lock().unwrap() = Some(gate.clone());
    use waltr::grpc::waltr::session::v1 as session;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let authorization = format!("Bearer {bearer}");
    let header = Some(("authorization", authorization.as_str()));
    let mut sessions = session::session_service_client::SessionServiceClient::new(channel.clone());
    let id = sessions
        .create_session(request(
            session::CreateSessionRequest {
                agent_selection: Some(session::AgentSelectionRequest {
                    agent_name: "routine-title".into(),
                }),
                ..Default::default()
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap()
        .id;
    let mut routines = p::routine_service_client::RoutineServiceClient::new(channel);
    let mut events = routines
        .watch_invocations(request(
            p::WatchInvocationsRequest {
                limit: 100,
                ..Default::default()
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    sessions
        .submit_user_message(request(
            session::SubmitUserMessageRequest {
                session_id: id.clone(),
                text: "Check the shared production helper and completion boundary".into(),
            },
            header,
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        gate.helper.acquire().await.unwrap().forget();
        gate.parent.acquire().await.unwrap().forget();
    }).await.expect("parent inference must begin while the title helper is blocked");
    gate.release.add_permits(1);
    *provider.title_gate.lock().unwrap() = None;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut completed = BTreeSet::new();
        loop {
            let update = events.message().await.unwrap().unwrap();
            for row in update.invocations {
                if matches!(row.routine.as_str(), "title" | "compact-and-continue") && row.status > p::InvocationStatus::Succeeded as i32 {
                    let operations = routines.list_operations(request(p::ListOperationsRequest { invocation_id: row.id.clone(), limit: 100, ..Default::default() }, header)).await;
                    panic!("normal lifecycle failed: {row:?}; {operations:?}");
                }
                if row.status == p::InvocationStatus::Succeeded as i32 {
                    let detail = routines
                        .get_invocation(request(
                            p::GetInvocationRequest {
                                invocation_id: row.id,
                            },
                            header,
                        ))
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    if detail.routine == "title" {
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(
                                &detail.result.as_ref().unwrap().json
                            )
                            .unwrap(),
                            true
                        );
                    } else if detail.routine == "compact-and-continue" {
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(
                                &detail.result.as_ref().unwrap().json
                            )
                            .unwrap(),
                            false
                        );
                    }
                    completed.insert(detail.routine);
                }
            }
            if completed.contains("title") && completed.contains("compact-and-continue") {
                break;
            }
        }
    })
    .await
    .unwrap();
    let mut state = sessions
        .watch_sessions(request(session::WatchSessionsRequest {}, header))
        .await
        .unwrap()
        .into_inner();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let rows = match state.message().await.unwrap().unwrap().event.unwrap() {
                session::watch_sessions_response::Event::Snapshot(snapshot) => snapshot.sessions,
                session::watch_sessions_response::Event::Session(row) => vec![row],
                _ => Vec::new(),
            };
            if rows.iter().any(|row| {
                row.id == id
                    && row.coordination.as_ref().is_some_and(|coordination| {
                        coordination.phase == session::SessionCoordinationPhase::Ready as i32
                    })
            }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let mut definitions = routines
        .watch_definitions(request(p::WatchDefinitionsRequest {}, header))
        .await
        .unwrap()
        .into_inner();
    let snapshot = definitions.message().await.unwrap().unwrap();
    let fork = snapshot
        .definitions
        .iter()
        .find(|definition| definition.name == "fork")
        .unwrap();
    let invocation = routines
        .start_invocation(request(
            p::StartInvocationRequest {
                selection: Some(p::InvocationSelection {
                    routine: "fork".into(),
                    revision: fork.revision.clone(),
                    entrypoint: "manual".into(),
                    parameters: Some(p::StructuredValue {
                        json: br#"{"direction":"Preserve the exact technical handoff"}"#.to_vec(),
                    }),
                    correlation: "normal-fork".into(),
                    source_session: Some(id.clone()),
                }),
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner()
        .invocation
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let update = events.message().await.unwrap().unwrap();
            if let Some(row) = update.invocations.iter().find(|row| {
                row.id == invocation.id && row.status >= p::InvocationStatus::Succeeded as i32
            }) {
                assert_eq!(row.status, p::InvocationStatus::Succeeded as i32);
                break;
            }
        }
    })
    .await
    .unwrap();
    let detail = routines
        .get_invocation(request(
            p::GetInvocationRequest {
                invocation_id: invocation.id,
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner()
        .invocation
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&detail.result.unwrap().json).unwrap();
    assert_eq!(result["created"], true);
    assert_eq!(result["started"], true);
    let summaries = sessions
        .list_sessions(request(session::ListSessionsRequest {}, header))
        .await
        .unwrap()
        .into_inner()
        .sessions;
    let source = summaries.iter().find(|row| row.id == id).unwrap();
    let destination = summaries
        .iter()
        .find(|row| row.id == result["sessionId"].as_str().unwrap())
        .unwrap();
    assert_eq!(destination.agent_name, source.agent_name);
    assert_eq!(destination.inference_settings, source.inference_settings);
    let destination_raw = sessions
        .get_timeline(request(
            session::GetTimelineRequest {
                session_id: destination.id.clone(),
                query: None,
                projection_mode: session::TimelineProjectionMode::Raw as i32,
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(destination_raw.events.iter().any(|event| event.channel.as_deref() == Some("user_text") && matches!(event.content.as_ref().and_then(|value| value.content.as_ref()), Some(session::timeline_content::Content::TextSnapshot(value)) if value.text.contains("Preserve the exact technical handoff"))));
    let raw_before = sessions
        .get_timeline(request(
            session::GetTimelineRequest {
                session_id: id.clone(),
                query: None,
                projection_mode: session::TimelineProjectionMode::Raw as i32,
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!raw_before.events.is_empty());
    let compact = snapshot
        .definitions
        .iter()
        .find(|value| value.name == "compact-and-continue")
        .unwrap();
    let compact_id = routines
        .start_invocation(request(
            p::StartInvocationRequest {
                selection: Some(p::InvocationSelection {
                    routine: compact.name.clone(),
                    revision: compact.revision.clone(),
                    entrypoint: "manual".into(),
                    parameters: Some(p::StructuredValue {
                        json: b"{}".to_vec(),
                    }),
                    correlation: "production-manual-compaction".into(),
                    source_session: Some(id.clone()),
                }),
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner()
        .invocation
        .unwrap()
        .id;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let update = events.message().await.unwrap().unwrap();
            if let Some(row) = update.invocations.iter().find(|row| {
                row.id == compact_id && row.status >= p::InvocationStatus::Succeeded as i32
            }) {
                assert_eq!(row.status, p::InvocationStatus::Succeeded as i32);
                break;
            }
        }
    })
    .await
    .unwrap();
    let detail = routines
        .get_invocation(request(
            p::GetInvocationRequest {
                invocation_id: compact_id,
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner()
        .invocation
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&detail.result.unwrap().json).unwrap(),
        true
    );
    let raw_after = sessions
        .get_timeline(request(
            session::GetTimelineRequest {
                session_id: id.clone(),
                query: None,
                projection_mode: session::TimelineProjectionMode::Raw as i32,
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(raw_before.events, raw_after.events);
    let effective = sessions
        .get_timeline(request(
            session::GetTimelineRequest {
                session_id: id,
                query: None,
                projection_mode: session::TimelineProjectionMode::Effective as i32,
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        effective
            .projected_events
            .iter()
            .any(|event| event.presentation
                == session::TimelinePresentation::MaskReplacement as i32
                && event.mask_id.as_ref().is_some_and(|id| !id.is_empty())
                && matches!(
                    event.event,
                    Some(session::projected_timeline_event::Event::Synthetic(_))
                ))
    );
}

async fn browser_terminal(
    address: std::net::SocketAddr,
    id: &str,
    cancelled: bool,
    expected: &str,
) {
    use prost::Message as _;
    let payload = p::WatchInvocationsRequest {
        limit: 100,
        ..Default::default()
    }
    .encode_to_vec();
    let mut body = vec![0];
    body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    body.extend(payload);
    let mut response = reqwest::Client::new()
        .post(format!(
            "http://{address}/waltr.routine.v2.RoutineService/WatchInvocations"
        ))
        .header("content-type", "application/grpc-web+proto")
        .header("origin", format!("http://{address}"))
        .header("cookie", "waltr-session-local=composition-browser")
        .body(body)
        .send()
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            bytes.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            assert!(bytes.len() <= 131072);
            while bytes.len() >= 5 {
                let size = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
                if bytes.len() < 5 + size {
                    break;
                }
                assert_eq!(bytes[0], 0);
                let update = p::WatchInvocationsResponse::decode(&bytes[5..5 + size]).unwrap();
                bytes.drain(..5 + size);
                if let Some(invocation) = update
                    .invocations
                    .iter()
                    .find(|row| row.id == id && row.status >= p::InvocationStatus::Succeeded as i32)
                {
                    assert_eq!(
                        invocation.status,
                        if cancelled {
                            p::InvocationStatus::Interrupted
                        } else {
                            p::InvocationStatus::Succeeded
                        } as i32
                    );
                    return;
                }
            }
        }
    })
    .await
    .unwrap();
    drop(response);
    let bytes = browser_frame(
        address,
        "RoutineService/GetInvocation",
        p::GetInvocationRequest {
            invocation_id: id.into(),
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(bytes[0], 0);
    let size = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
    let row = p::GetInvocationResponse::decode(&bytes[5..5 + size])
        .unwrap()
        .invocation
        .unwrap();
    if !cancelled {
        assert_eq!(
            row.result.unwrap().json,
            serde_json::to_vec(expected).unwrap()
        );
    }
}

async fn code_routes(address: std::net::SocketAddr, bearer: &str) {
    use waltr::grpc::waltr::session::v1 as session;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let bearer = format!("Bearer {bearer}");
    let header = Some(("authorization", bearer.as_str()));
    let mut routines = p::routine_service_client::RoutineServiceClient::new(channel.clone());
    let mut events = routines
        .watch_invocations(request(
            p::WatchInvocationsRequest {
                limit: 100,
                ..Default::default()
            },
            header,
        ))
        .await
        .unwrap()
        .into_inner();
    let mut sessions = session::session_service_client::SessionServiceClient::new(channel);
    sessions
        .submit_user_message(request(
            session::SubmitUserMessageRequest {
                session_id: "production-code-session".into(),
                text: "Execute the explicitly granted bounded code once".into(),
            },
            header,
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            for row in events.message().await.unwrap().unwrap().invocations {
                if row.status >= p::InvocationStatus::Succeeded as i32 {
                    let detail = routines
                        .get_invocation(request(
                            p::GetInvocationRequest {
                                invocation_id: row.id,
                            },
                            header,
                        ))
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    if detail.routine == "code-mode" {
                        assert_eq!(detail.status, p::InvocationStatus::Succeeded as i32);
                        assert!(
                            String::from_utf8(detail.result.unwrap().json)
                                .unwrap()
                                .contains("granted-code")
                        );
                        return;
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    let denied = sessions
        .submit_user_message(request(
            session::SubmitUserMessageRequest {
                session_id: "production-code-ungranted".into(),
                text: "This session has no host code grant".into(),
            },
            header,
        ))
        .await;
    assert!(
        denied.is_err(),
        "feature opt-in alone must not grant code authority"
    );
}

#[tokio::test]
async fn standalone_and_embedded_compose_real_shared_v2_service() {
    if !crate::routine::telemetry::isolated_exporter_test(
        "routine::production::runtime::tests::standalone_and_embedded_compose_real_shared_v2_service",
    ) {
        return;
    }
    let root = crate::test_support::temp_root();
    let resources = root.join("standalone");
    seed(&resources);
    let agents = resources.join(".waltr/resources/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("code-test.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: code-test }\nspec:\n  systemPrompt: Use the explicitly bound code tool once.\n  features: [{ codeMode: true }]\n  permissions: []\n").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mcp = resources.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::write(mcp.join("waltr.yaml"), format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: http://{address}/mcp\n")).unwrap();
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
    let (authenticator, access_token, issuer, issuer_stop, issuer_task) =
        crate::test_support::composition_auth(&persistence, address).await;
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    let mut create = waltr_agent::SessionCreateRequest::new(waltr_agent::SessionId::new(
        "production-code-session",
    ));
    create.agent_definition = state
        .snapshot()
        .catalog()
        .bundle("code-test")
        .map(|bundle| serde_json::to_value(bundle).unwrap());
    create.inference_settings = Some(bindings::default_inference(&state.snapshot()).unwrap());
    sessions.create(create).await.unwrap();
    let mut ungranted = waltr_agent::SessionCreateRequest::new(waltr_agent::SessionId::new(
        "production-code-ungranted",
    ));
    ungranted.agent_definition = state
        .snapshot()
        .catalog()
        .bundle("code-test")
        .map(|bundle| serde_json::to_value(bundle).unwrap());
    ungranted.inference_settings = Some(bindings::default_inference(&state.snapshot()).unwrap());
    sessions.create(ungranted).await.unwrap();
    let provider = Arc::new(ScriptedProvider::default());
    let provider_check = provider.clone();
    let app = assemble_app(
        AppServices {
            config_service,
            persistence,
            catalog_loader: loader,
            access_authenticator: authenticator,
            mcp_oauth: None,
            outbound_private_origins: Vec::new(),
            outbound_oauth: OutboundOAuthConfig::default(),
            automatic_self_mcp_resource: None,
            agent_state: Some(state),
            local_admission: None,
            workspace_root: workspace,
            sandbox_socket: None,
            routine_authority: RoutineAuthority {
                code: vec![crate::routine::production::RoutineCodeGrant {
                    public_mcp_session: None,
                    issuer: issuer.clone(), subject: "api-subject".into(),
                    sessions: BTreeSet::from([waltr_agent::SessionId::new("production-code-session")]),
                    profile: crate::routine::code_mode::CodeProfile { restriction: Default::default(), targets: BTreeMap::new(), helpers: BTreeSet::new() },
                }],
                ..Default::default()
            },
            routine_private_parent: root.clone(),
        },
        |routes, input| {
            let service = Arc::try_unwrap(crate::session::SessionService::from_sessions(
                sessions, input,
            )).ok().unwrap().with_test_provider(provider.clone());
            let service = Arc::new(service);
            routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
            Ok(service)
        },
    )
    .await
    .unwrap();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let task =
        tokio::spawn(app.serve_with_shutdown(listener, async move { shutdown.cancelled().await }));
    exercise(address, Some(&access_token)).await;
    browser_routes(address).await;
    lifecycle_routes(address, &access_token, &provider).await;
    title_overlap::run(address, &access_token, &provider).await;
    code_routes(address, &access_token).await;
    assert_eq!(
        provider_check
            .fork_directions
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        provider_check
            .code_turns
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    issuer_stop.cancel();
    issuer_task.await.unwrap();

    let resources = root.join("embedded");
    seed(&resources);
    let embedded = crate::embedded::build_embedded_server(crate::embedded::EmbeddedServerConfig {
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
        local_bearer: crate::LocalBearer::new("explicit-test-generation").unwrap(),
        outbound_policy: Default::default(),
        routine_authority: RoutineAuthority::default(),
    })
    .await
    .unwrap();
    let address = embedded.local_addr();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let task =
        tokio::spawn(embedded.serve_with_shutdown(async move { shutdown.cancelled().await }));
    exercise(address, Some("explicit-test-generation")).await;
    stop.cancel();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
