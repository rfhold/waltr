use super::media_upstream::Upstream;
use super::*;
use std::{sync::atomic::Ordering, time::Duration};
use tokio_stream::wrappers::ReceiverStream;
#[path = "media_browser.rs"]
mod browser;
#[path = "media_model.rs"]
mod media_model;
use media_model::Models;
#[path = "media_cancel.rs"]
mod media_cancel;
#[path = "helper_inputs.rs"]
mod helper_inputs;
#[path = "ask_and_speak.rs"]
mod ask_and_speak;
#[path = "media_speech_selection.rs"]
mod speech_selection;
fn authority() -> RoutineAuthority {
    RoutineAuthority::default()
}
fn resources(root: &std::path::Path, endpoint: &str, diarization: bool) {
    seed(root);
    speech_defaults(root, if diarization { Some(("media-stt", "stream")) } else { None });
    let mirror = serde_json::json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"blob-mirror","description":"Production binary port fixture","spec":{"source":"export default async function(){const value=await host.call('media.upload',{port:'source'});await host.call('media.lease',{output:'copy'},null,null,value);await host.call('media.finish',{});return {completed:true};}","entrypoints":{"run":{"trigger":"interactive"}},"parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{},"required":[]}},"result":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"completed":{"type":"boolean"}},"required":["completed"]}},"capabilities":["media.execute"],"ports":{"source":{"direction":"input","media":"file","required":true,"maxBytes":67108864,"formats":["blob"]},"copy":{"direction":"output","media":"file","required":true,"maxBytes":67108864,"formats":["blob"]}}}});
    std::fs::write(
        root.join(".waltr/resources/routines/blob-mirror.yaml"),
        serde_json::to_vec(&mirror).unwrap(),
    )
    .unwrap();
    let providers = root.join(".waltr/resources/providers");
    std::fs::create_dir_all(&providers).unwrap();
    std::fs::write(providers.join("media-stt.yaml"),format!("apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata: {{name: media-stt}}\nspec:\n  transport: googleSpeechV2\n  url: {endpoint}\n  models:\n    stream:\n      name: Local fake recognition\n      type: 3\n      speech_capabilities: {{unary: true, streaming: true}}\n      recognition:\n        recognizer: projects/test/locations/global/recognizers/_\n        model: chirp_3\n        language_codes: [en-US]\n        sample_rate_hz: 24000\n        channels: 1\n        interim_results: true\n        automatic_punctuation: true\n        diarization: {diarization}\n")).unwrap();
    std::fs::write(providers.join("media-tts.yaml"),format!("apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata: {{name: media-tts}}\nspec:\n  transport: googleTextToSpeechV1\n  url: {endpoint}\n  models:\n    stream:\n      name: Local fake synthesis\n      type: 4\n      speech_capabilities: {{unary: true, streaming: true}}\n      synthesis:\n        voice: voice\n        language_code: en-US\n        sample_rate_hz: 24000\n        channels: 1\n")).unwrap();
}
fn input(event: p::input_event::Event) -> p::InteractRequest {
    p::InteractRequest {
        frame: Some(p::interact_request::Frame::Input(p::InputEvent {
            event: Some(event),
        })),
    }
}
fn parameters(_: &str) -> Vec<u8> {
    b"{}".to_vec()
}
fn speech_defaults(root: &std::path::Path, diarize: Option<(&str, &str)>) {
    let mut defaults: serde_json::Value = serde_yaml_ng::from_str(include_str!("../../../../../resources/defaults/default.yaml")).unwrap();
    defaults["spec"]["speech"] = serde_json::json!({"stt":{"provider":"media-stt","model":"stream"},"tts":{"provider":"media-tts","model":"stream"}});
    if let Some((provider, model)) = diarize {
        defaults["spec"]["speech"]["diarize"] = serde_json::json!({"provider":provider,"model":model});
    }
    let path = root.join(".waltr/resources/defaults");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("default.yaml"), serde_json::to_vec(&defaults).unwrap()).unwrap();
}
async fn native(
    address: std::net::SocketAddr,
    auth: Option<&str>,
    name: &str,
    entry: &str,
    bytes: usize,
) -> p::InteractionTerminal {
    native_status(
        address,
        auth,
        name,
        entry,
        bytes,
        p::InvocationStatus::Succeeded,
    )
    .await
}
async fn native_status(
    address: std::net::SocketAddr,
    auth: Option<&str>,
    name: &str,
    entry: &str,
    bytes: usize,
    expected: p::InvocationStatus,
) -> p::InteractionTerminal {
    native_marked(address, auth, name, entry, bytes, expected, 0).await
}
async fn native_marked(
    address: std::net::SocketAddr,
    auth: Option<&str>,
    name: &str,
    entry: &str,
    bytes: usize,
    expected: p::InvocationStatus,
    marker: u8,
) -> p::InteractionTerminal {
    native_gated(address, auth, name, entry, bytes, expected, marker, None).await
}
async fn native_gated(
    address: std::net::SocketAddr,
    auth: Option<&str>,
    name: &str,
    entry: &str,
    bytes: usize,
    expected: p::InvocationStatus,
    marker: u8,
    gate: Option<(tokio::sync::oneshot::Sender<()>, tokio::sync::oneshot::Receiver<()>)>,
) -> p::InteractionTerminal {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
    let request = |value| {
        let mut r = tonic::Request::new(value);
        if let Some(auth) = auth {
            r.metadata_mut()
                .insert("authorization", format!("Bearer {auth}").parse().unwrap());
        }
        r
    };
    let definitions = client
        .watch_definitions(request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();
    let definition = definitions
        .definitions
        .iter()
        .find(|row| row.name == name)
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(p::InteractRequest {
        frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
            selection: Some(p::InvocationSelection {
                routine: name.into(),
                revision: definition.revision.clone(),
                entrypoint: entry.into(),
                parameters: Some(p::StructuredValue {
                    json: parameters(name),
                }),
                ..Default::default()
            }),
            inputs: vec![p::InputDeclaration {
                port: if bytes == 0 { "text" } else { "audio" }.into(),
                format: Some(if bytes == 0 {
                    p::input_declaration::Format::Text(p::TextFormat {})
                } else {
                    p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })
                }),
            }],
        })),
    })
    .await
    .unwrap();
    let mut output = client
        .interact({
            let mut req = tonic::Request::new(ReceiverStream::new(rx));
            if let Some(auth) = auth {
                req.metadata_mut()
                    .insert("authorization", format!("Bearer {auth}").parse().unwrap());
            }
            req
        })
        .await
        .unwrap()
        .into_inner();
    let live = matches!(name, "audio-transcribe" | "audio-synthesize");
    if let Some((admitted, release)) = gate {
        let frame = output.message().await.unwrap().unwrap();
        assert!(matches!(frame.output.unwrap().event, Some(p::output_event::Event::Accepted(_))));
        admitted.send(()).unwrap();
        release.await.unwrap();
    }
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let producer_release = release.clone();
    let input_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer_closed = input_closed.clone();
    let producer = tokio::spawn(async move {
        tx.send(input(p::input_event::Event::Open(p::StreamOpen {
            port: if bytes == 0 { "text" } else { "audio" }.into(),
            stream_id: "input".into(),
            format: Some(if bytes == 0 {
                p::stream_open::Format::Text(p::TextFormat {})
            } else {
                p::stream_open::Format::Pcm16le(p::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })
            }),
        })))
        .await
        .unwrap();
        let mut sequence = 0;
        if bytes == 0 {
            for (segment, text, committed) in [
                ("one", "draft", false),
                ("one", "Committed first sentence.", true),
                ("two", " Second sentence.", true),
            ] {
                tx.send(input(p::input_event::Event::Chunk(p::StreamChunk {
                    stream_id: "input".into(),
                    sequence,
                    payload: Some(p::stream_chunk::Payload::Text(p::TextRevision {
                        segment_id: segment.into(),
                        mutation: if segment == "one" && committed {
                            p::TextMutation::Replace
                        } else {
                            p::TextMutation::Append
                        } as i32,
                        text: text.into(),
                        committed,
                    })),
                })))
                .await
                .unwrap();
                sequence += 1;
            }
        } else {
            let mut left = bytes;
            while left > 0 {
                let n = left.min(65536);
                tx.send(input(p::input_event::Event::Chunk(p::StreamChunk {
                    stream_id: "input".into(),
                    sequence,
                    payload: Some(p::stream_chunk::Payload::Audio(vec![marker; n])),
                })))
                .await
                .unwrap();
                sequence += 1;
                left -= n;
            }
        }
        if live {
            producer_release.acquire().await.unwrap().forget();
        }
        producer_closed.store(true, Ordering::SeqCst);
        tx.send(input(p::input_event::Event::Close(p::StreamClose {
            stream_id: "input".into(),
            next_sequence: sequence,
        })))
        .await
        .unwrap();
        tx.send(input(p::input_event::Event::Finish(p::InputFinish {})))
            .await
            .unwrap();
    });
    let mut terminal = None;
    let mut marked_seen = marker == 0;
    let mut opens = 0;
    let mut closes = 0;
    let mut chunks = 0;
    let mut previews: Vec<p::TextRevision> = Vec::new();
    let mut released = false;
    let mut pcm = 0;
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(15), output.message())
        .await
        .unwrap()
        .unwrap()
    {
        match frame.output.unwrap().event.unwrap() {
            p::output_event::Event::Accepted(_) => {}
            p::output_event::Event::Rejected(value) => {
                panic!("production admission rejected: {value:?}")
            }
            p::output_event::Event::Open(_) => opens += 1,
            p::output_event::Event::Close(_) => closes += 1,
            p::output_event::Event::Chunk(chunk) => {
                chunks += 1;
                if let Some(p::stream_chunk::Payload::Audio(data)) = chunk.payload {
                    assert_eq!(data.len() % 2, 0);
                    pcm += data.len();
                    if live && !released {
                        assert!(!input_closed.load(Ordering::SeqCst));
                        release.add_permits(1);
                        released = true;
                    }
                } else if let Some(p::stream_chunk::Payload::Text(text)) = chunk.payload {
                    if live && !text.committed && !released {
                        assert!(!input_closed.load(Ordering::SeqCst));
                        previews.push(text);
                        if previews.len() == 2 {
                            assert_eq!(previews[0].segment_id, previews[1].segment_id);
                            assert_ne!(previews[0].text, previews[1].text);
                            release.add_permits(1);
                            released = true;
                        }
                    } else if live && text.committed {
                        assert!(input_closed.load(Ordering::SeqCst));
                        if marker != 0 {
                            assert!(text.text.contains(if marker == 1 {
                                "source-one"
                            } else {
                                "source-two"
                            }));
                            assert!(!text.text.contains(if marker == 1 {
                                "source-two"
                            } else {
                                "source-one"
                            }));
                            marked_seen = true;
                        }
                    }
                }
            }
            p::output_event::Event::Terminal(value) => {
                assert!(terminal.replace(value).is_none());
            }
            _ => {}
        }
    }
    producer.await.unwrap();
    assert!(marked_seen);
    let terminal = terminal.unwrap();
    assert_eq!(terminal.status, expected as i32, "{name}: {terminal:?}");
    assert!(terminal.cleanup_complete);
    if expected != p::InvocationStatus::Succeeded {
        assert_eq!(chunks, if name == "audio-transcribe" { 2 } else { 0 });
        assert!(opens <= 1 && closes <= opens);
        assert!(terminal.text_results.is_empty());
        return terminal;
    }
    assert_eq!(opens, closes);
    assert!(chunks > 0);
    if bytes == 0 {
        assert_eq!(pcm, 16);
    } else if live {
        assert_eq!(previews.len(), 2);
    }
    terminal
}

#[tokio::test]
async fn standalone_media_uses_real_production_speech_and_named_helpers() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::standalone_media_uses_real_production_speech_and_named_helpers",
    ) {
        return;
    }
    standalone_case(true, None).await;
}

#[tokio::test]
async fn production_notes_reject_missing_capture_ceiling_before_upstream_effects() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::production_notes_reject_missing_capture_ceiling_before_upstream_effects",
    ) {
        return;
    }
    standalone_case(false, None).await;
}

#[tokio::test]
async fn separate_snapshot_selection_preserves_plain_dictation_and_cleanup() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::separate_snapshot_selection_preserves_plain_dictation_and_cleanup",
    ) {
        return;
    }
    tokio::time::timeout(
        Duration::from_secs(45),
        standalone_case(true, Some("split")),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn separate_snapshot_pcm_does_not_reject_plain_dictation_and_cleanup() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::separate_snapshot_pcm_does_not_reject_plain_dictation_and_cleanup",
    ) {
        return;
    }
    tokio::time::timeout(
        Duration::from_secs(45),
        standalone_case(true, Some("split-mismatched")),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn standalone_cleanup_catalog_defaults() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::standalone_cleanup_catalog_defaults",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        for mode in ["allowed", "diarized", "missing-capture", "conversation-no-diarization"] {
            standalone_case(mode != "missing-capture", Some(mode)).await;
        }
    }).await.expect("bounded standalone cleanup configuration contract");
}

async fn standalone_case(capture_allowed: bool, cleanup_mode: Option<&str>) {
    let upstream = Upstream::start().await;
    let root = crate::test_support::temp_root();
    let source = root.join("resources");
    resources(&source, &upstream.endpoint, cleanup_mode.is_none() || cleanup_mode == Some("diarized"));
    let snapshot_upstream = if matches!(cleanup_mode, Some("split" | "split-mismatched")) {
        let second = Upstream::start().await;
        let path = source.join(".waltr/resources/providers/media-stt.yaml");
        let yaml = std::fs::read_to_string(path)
            .unwrap()
            .replace("name: media-stt", "name: snapshot-stt")
            .replace(&upstream.endpoint, &second.endpoint)
            .replace("stream:", "snapshot:")
            .replace("model: chirp_3", "model: snapshot-pinned")
            .replace("diarization: false", "diarization: true")
            .replace(
                "sample_rate_hz: 24000",
                if cleanup_mode == Some("split-mismatched") {
                    "sample_rate_hz: 16000"
                } else {
                    "sample_rate_hz: 24000"
                },
            );
        std::fs::write(
            source.join(".waltr/resources/providers/snapshot-stt.yaml"),
            yaml,
        )
        .unwrap();
        speech_defaults(&source, Some(("snapshot-stt", "snapshot")));
        Some(second)
    } else {
        None
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mcp = source.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::write(mcp.join("waltr.yaml"),format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: waltr}}\nspec: {{transport: streamableHttp, url: 'http://{address}/mcp'}}\n")).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    if !capture_allowed {
        let permissions = source.join(".waltr/resources/permissions");
        std::fs::create_dir_all(&permissions).unwrap();
        std::fs::write(permissions.join("routine-helper-capture.yaml"), include_str!("../../../../../resources/permissions/routine-helper-capture.yaml").replace("permission: allow", "permission: deny")).unwrap();
    }
    let loader = crate::test_support::default_catalog_loader();
    let config_service = ConfigService::new(source, loader.clone()).await.unwrap();
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
    let models = Models::start(upstream.metrics.clone()).await;
    models.plain_cleanup.store(cleanup_mode.is_some() && cleanup_mode != Some("diarized"), Ordering::SeqCst);
    let model = models.clone();
    let (auth, token, _issuer, issuer_stop, issuer_task) =
        crate::test_support::composition_auth(&persistence, address).await;
    let configured = authority();
    let app=assemble_app(AppServices{config_service,persistence,catalog_loader:loader,access_authenticator:auth,mcp_oauth:None,outbound_private_origins:vec![],outbound_oauth:Default::default(),automatic_self_mcp_resource:None,agent_state:Some(state),local_admission:None,workspace_root:workspace,sandbox_socket:None,routine_authority:configured,routine_private_parent:root.clone()},move|routes,input|{
        let service=Arc::new(Arc::try_unwrap(crate::session::SessionService::from_sessions(waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()),input)).ok().unwrap().with_test_provider(model));routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));Ok(service)
    }).await.unwrap();
    let stop = CancellationToken::new();
    let end = stop.clone();
    let task =
        tokio::spawn(app.serve_with_shutdown(listener, async move { end.cancelled().await }));
    if let Some(second) = snapshot_upstream {
        native(address, Some(&token), "audio-transcribe", "dictate", 65536).await;
        native(
            address,
            Some(&token),
            "audio-transcribe-cleanup",
            "dictate",
            65536,
        )
        .await;
        assert_eq!(models.cleanup.load(Ordering::SeqCst), 1);
        assert_eq!(
            *upstream.metrics.recognition_configs.lock().unwrap(),
            [("chirp_3".into(), false), ("chirp_3".into(), false)]
        );
        if cleanup_mode == Some("split-mismatched") {
            assert_eq!(second.metrics.speech_calls.load(Ordering::SeqCst), 0);
            assert_eq!(models.filter.load(Ordering::SeqCst), 0);
            assert_eq!(models.notes.load(Ordering::SeqCst), 0);
        } else {
            models.plain_cleanup.store(false, Ordering::SeqCst);
            let result = native(
                address,
                Some(&token),
                "audio-conversation-notes",
                "record-notes",
                65536,
            )
            .await;
            assert_eq!(result.text_results.len(), 3);
            assert_eq!(models.filter.load(Ordering::SeqCst), 1);
            assert_eq!(models.notes.load(Ordering::SeqCst), 1);
            assert_eq!(
                *second.metrics.recognition_configs.lock().unwrap(),
                [("snapshot-pinned".into(), true)]
            );
        }
        assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 2);
        stop.cancel();
        task.await.unwrap().unwrap();
        issuer_stop.cancel();
        issuer_task.await.unwrap();
        models.shutdown().await;
        upstream.shutdown().await;
        second.shutdown().await;
        return;
    }
    if !capture_allowed {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
        let auth = |value| {
            let mut request = tonic::Request::new(value);
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            request
        };
        let definitions = client
            .watch_definitions(auth(p::WatchDefinitionsRequest {}))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();
        let definition = definitions
            .definitions
            .iter()
            .find(|row| row.name == if cleanup_mode.is_some() { "audio-transcribe-cleanup" } else { "audio-conversation-notes" })
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(p::InteractRequest {
            frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    routine: definition.name.clone(),
                    revision: definition.revision.clone(),
                    entrypoint: if cleanup_mode.is_some() { "dictate" } else { "record-notes" }.into(),
                    parameters: Some(p::StructuredValue {
                        json: b"{}".to_vec(),
                    }),
                    ..Default::default()
                }),
                inputs: vec![p::InputDeclaration {
                    port: "audio".into(),
                    format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })),
                }],
            })),
        })
        .await
        .unwrap();
        drop(tx);
        let mut request = tonic::Request::new(ReceiverStream::new(rx));
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let mut output = client.interact(request).await.unwrap().into_inner();
        let frame = output.message().await.unwrap().unwrap();
        let Some(p::output_event::Event::Rejected(rejected)) = frame.output.unwrap().event else {
            panic!("capture ceiling was not rejected before admission")
        };
        assert_eq!(rejected.code, p::RejectionCode::Denied as i32);
        assert!(output.message().await.unwrap().is_none());
        assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 0);
        assert_eq!(models.cleanup.load(Ordering::SeqCst), 0);
        assert_eq!(models.filter.load(Ordering::SeqCst), 0);
        assert_eq!(models.notes.load(Ordering::SeqCst), 0);
        stop.cancel();
        task.await.unwrap().unwrap();
        issuer_stop.cancel();
        issuer_task.await.unwrap();
        models.shutdown().await;
        upstream.shutdown().await;
        if let Some(mode) = cleanup_mode {
            eprintln!("standalone cleanup {mode}: denied before speech or model effects");
        }
        return;
    }
    if let Some(mode) = cleanup_mode {
        let succeeds = matches!(mode, "allowed" | "diarized");
        let expected = if succeeds { p::InvocationStatus::Succeeded } else { p::InvocationStatus::Failed };
        let (routine, entry) = if mode == "conversation-no-diarization" { ("audio-conversation-notes", "record-notes") } else { ("audio-transcribe-cleanup", "dictate") };
        let result = native_status(address, Some(&token), routine, entry, 65536, expected).await;
        assert!(result.cleanup_complete);
        assert_eq!(models.cleanup.load(Ordering::SeqCst), usize::from(succeeds));
        if succeeds {
            assert_eq!(result.text_results.len(), 1);
        }
        assert_eq!(models.filter.load(Ordering::SeqCst), 0);
        assert_eq!(models.notes.load(Ordering::SeqCst), 0);
        if mode == "conversation-no-diarization" {
            assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 0);
        } else {
            assert!(upstream.metrics.speech_calls.load(Ordering::SeqCst) > 0);
        }
        stop.cancel();
        task.await.unwrap().unwrap();
        issuer_stop.cancel();
        issuer_task.await.unwrap();
        models.shutdown().await;
        upstream.shutdown().await;
        eprintln!("standalone cleanup {mode}: {expected:?}, normal cleanup model calls={}", models.cleanup.load(Ordering::SeqCst));
        return;
    }
    for (name, entry, bytes) in [
        ("audio-transcribe", "dictate", 65536),
        ("audio-transcribe-cleanup", "dictate", 65536),
        ("audio-conversation-notes", "record-notes", 65536),
        ("audio-synthesize", "speak", 0),
    ] {
        let result = native(address, Some(&token), name, entry, bytes).await;
        if name == "audio-conversation-notes" {
            assert_eq!(result.text_results.len(), 3);
        }
        let result = browser::run(
            address,
            browser::Auth::Cookie("composition-browser".into()),
            name,
            entry,
            bytes,
        )
        .await;
        if name == "audio-conversation-notes" {
            assert_eq!(result.text_results.len(), 3);
        }
    }
    let hour = 24000 * 2 * 3600;
    let result = native(
        address,
        Some(&token),
        "audio-conversation-notes",
        "record-notes",
        hour,
    )
    .await;
    assert_eq!(result.text_results.len(), 3);
    models.invalid_filter.store(true, Ordering::SeqCst);
    native_status(
        address,
        Some(&token),
        "audio-conversation-notes",
        "record-notes",
        65536,
        p::InvocationStatus::Uncertain,
    )
    .await;
    models.invalid_filter.store(false, Ordering::SeqCst);
    for name in ["audio-transcribe", "audio-transcribe-cleanup"] {
        upstream.metrics.fail_final.store(true, Ordering::SeqCst);
        native_status(
            address,
            Some(&token),
            name,
            "dictate",
            65536,
            p::InvocationStatus::Uncertain,
        )
        .await;
        assert_eq!(upstream.metrics.live.load(Ordering::SeqCst), 0);
    }
    assert_eq!(models.cleanup.load(Ordering::SeqCst), 2);
    assert_eq!(models.filter.load(Ordering::SeqCst), 4);
    assert_eq!(models.notes.load(Ordering::SeqCst), 3);
    assert_eq!(
        upstream.metrics.bytes.load(Ordering::SeqCst),
        9 * 65536 + hour as u64
    );
    assert!(upstream.metrics.largest_frame.load(Ordering::SeqCst) <= 15360);
    media_cancel::native(address, &token, false, &upstream.metrics).await;
    media_cancel::native(address, &token, true, &upstream.metrics).await;
    browser::cancel(
        address,
        browser::Auth::Cookie("composition-browser".into()),
        false,
        &upstream.metrics,
    )
    .await;
    browser::cancel(
        address,
        browser::Auth::Cookie("composition-browser".into()),
        true,
        &upstream.metrics,
    )
    .await;
    *upstream.metrics.pair.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));
    let _pair = tokio::join!(
        native_marked(
            address,
            Some(&token),
            "audio-transcribe",
            "dictate",
            32768,
            p::InvocationStatus::Succeeded,
            1
        ),
        browser::marked(
            address,
            browser::Auth::Cookie("composition-browser".into()),
            "audio-transcribe",
            "dictate",
            32768,
            2
        )
    );
    assert!(upstream.metrics.peak_live.load(Ordering::SeqCst) >= 2);
    assert_eq!(upstream.metrics.live.load(Ordering::SeqCst), 0);
    browser::blobs(
        address,
        browser::Auth::Cookie("composition-browser".into()),
        &token,
    )
    .await;
    stop.cancel();
    task.await.unwrap().unwrap();
    issuer_stop.cancel();
    issuer_task.await.unwrap();
    models.shutdown().await;
    upstream.shutdown().await;
}

#[tokio::test]
async fn embedded_media_uses_generation_auth_native_and_browser_transports() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::embedded_media_uses_generation_auth_native_and_browser_transports",
    ) {
        return;
    }
    let upstream = Upstream::start().await;
    let root = crate::test_support::temp_root();
    let source = root.join("resources");
    resources(&source, &upstream.endpoint, true);
    let models = Models::start(upstream.metrics.clone()).await;
    let model = models.clone();
    let token = "media-explicit-local-generation";
    let configured = authority();
    let embedded=crate::embedded::build_embedded_server_with_registration(crate::embedded::EmbeddedServerConfig{
        resources_root:source,waltr_database_path:root.join("data/waltr.sqlite3"),mcp_database_path:root.join("data/mcp.sqlite3"),session_root:root.join("sessions"),workspace_root:root.join("workspace"),catalog_source:crate::embedded::EmbeddedCatalogSource::Path(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models-dev-defaults.json")),catalog_cache_path:root.join("cache/models.json"),catalog_fallback:None,default_agent:"routine-title".into(),local_bearer:crate::LocalBearer::new(token).unwrap(),outbound_policy:Default::default(),routine_authority:configured,
    },move|routes,input,path|{
        let sessions=waltr_agent::SessionCoordinator::new(waltr_agent::FilesystemSessionStore::new(path));let service=Arc::new(Arc::try_unwrap(crate::session::SessionService::from_sessions(sessions,input)).ok().unwrap().with_test_provider(model));routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));Ok(service)
    }).await.unwrap();
    let address = embedded.local_addr();
    let stop = CancellationToken::new();
    let end = stop.clone();
    let task = tokio::spawn(embedded.serve_with_shutdown(async move { end.cancelled().await }));
    for (name, entry, bytes) in [
        ("audio-transcribe", "dictate", 65536),
        ("audio-transcribe-cleanup", "dictate", 65536),
        ("audio-conversation-notes", "record-notes", 65536),
        ("audio-synthesize", "speak", 0),
    ] {
        let result = native(address, Some(token), name, entry, bytes).await;
        if name == "audio-conversation-notes" {
            assert_eq!(result.text_results.len(), 3);
        }
        let result = browser::run(
            address,
            browser::Auth::Bearer(token.into()),
            name,
            entry,
            bytes,
        )
        .await;
        if name == "audio-conversation-notes" {
            assert_eq!(result.text_results.len(), 3);
        }
    }
    assert_eq!(models.cleanup.load(Ordering::SeqCst), 2);
    assert_eq!(models.filter.load(Ordering::SeqCst), 2);
    assert_eq!(models.notes.load(Ordering::SeqCst), 2);
    assert_eq!(upstream.metrics.bytes.load(Ordering::SeqCst), 6 * 65536);
    media_cancel::native(address, token, false, &upstream.metrics).await;
    media_cancel::native(address, token, true, &upstream.metrics).await;
    browser::cancel(
        address,
        browser::Auth::Bearer(token.into()),
        false,
        &upstream.metrics,
    )
    .await;
    browser::cancel(
        address,
        browser::Auth::Bearer(token.into()),
        true,
        &upstream.metrics,
    )
    .await;
    *upstream.metrics.pair.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));
    let _pair = tokio::join!(
        native_marked(
            address,
            Some(token),
            "audio-transcribe",
            "dictate",
            32768,
            p::InvocationStatus::Succeeded,
            1
        ),
        browser::marked(
            address,
            browser::Auth::Bearer(token.into()),
            "audio-transcribe",
            "dictate",
            32768,
            2
        )
    );
    assert!(upstream.metrics.peak_live.load(Ordering::SeqCst) >= 2);
    assert_eq!(upstream.metrics.live.load(Ordering::SeqCst), 0);
    browser::blobs(address, browser::Auth::Bearer(token.into()), token).await;
    stop.cancel();
    task.await.unwrap().unwrap();
    models.shutdown().await;
    upstream.shutdown().await;
}
