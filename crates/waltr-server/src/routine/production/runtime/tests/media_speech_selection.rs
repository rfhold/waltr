use super::*;
use crate::routine::production::standalone::{VARIABLES, speech_authority};

#[tokio::test]
async fn snapshot_selection_startup_fails_closed_without_legacy_fallback() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for (provider, model, streaming, diarized, snapshot_only, valid) in [
            ("secret-sentinel", "stream", true, true, false, false),
            ("snapshot-stt", "missing", true, true, false, false),
            ("media-tts", "stream", true, true, false, false),
            ("snapshot-stt", "stream", false, true, false, false),
            ("snapshot-stt", "stream", true, false, false, false),
            ("snapshot-stt", "stream", true, true, true, true),
        ] {
            let upstream = Upstream::start().await;
            let root = crate::test_support::temp_root();
            let source = root.join("resources");
            resources(&source, &upstream.endpoint, true);
            let path = source.join(".waltr/resources/providers/media-stt.yaml");
            let yaml = std::fs::read_to_string(&path).unwrap()
                .replace("name: media-stt", "name: snapshot-stt")
                .replace("streaming: true", &format!("streaming: {streaming}"))
                .replace("diarization: true", &format!("diarization: {diarized}"));
            std::fs::write(source.join(".waltr/resources/providers/snapshot-stt.yaml"), yaml).unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let loader = crate::test_support::default_catalog_loader();
            let config_service = ConfigService::new(source, loader.clone()).await.unwrap();
            let state = config_service.state().clone();
            let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
                &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
                &format!("sqlite://{}", root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
            let mut map = std::collections::BTreeMap::from([
                ("WALTR_ROUTINE_STT_SNAPSHOT_PROVIDER".into(), provider.into()),
                ("WALTR_ROUTINE_STT_SNAPSHOT_MODEL".into(), model.into()),
            ]);
            if !snapshot_only {
                // An explicitly diarized legacy choice must never hide a bad new selection.
                map.insert("WALTR_ROUTINE_STT_PROVIDER".into(), "media-stt".into());
                map.insert("WALTR_ROUTINE_STT_MODEL".into(), "stream".into());
            }
            let result = assemble_app(AppServices {
                config_service, persistence, catalog_loader: loader,
                access_authenticator: crate::access_auth::AccessAuthenticator::disabled(), mcp_oauth: None,
                outbound_private_origins: vec![], outbound_oauth: Default::default(),
                automatic_self_mcp_resource: None, agent_state: Some(state), local_admission: None,
                workspace_root: workspace, sandbox_socket: None,
                routine_authority: speech_authority(&map).unwrap(), routine_private_parent: root,
            }, |routes, input| {
                let service = crate::session::SessionService::from_sessions(waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()), input);
                routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
                Ok(service)
            }).await;
            assert_eq!(result.is_ok(), valid, "snapshot {provider}/{model}, streaming={streaming}, diarized={diarized}");
            if let Err(error) = &result {
                assert!(!error.to_string().contains("secret-sentinel"));
            }
            if let Ok(app) = result {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                app.serve_with_shutdown(listener, async {}).await.unwrap();
            }
            assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 0);
            assert_eq!(upstream.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
            upstream.shutdown().await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn standalone_speech_selection_reaches_backend_and_rejects_missing_or_wrong_kinds() {
    tokio::time::timeout(Duration::from_secs(90), async {
        for (selected, tts, choices, streaming) in [
            (None, false, None, true),
            (Some(("missing", "stream")), false, None, true),
            (Some(("media-tts", "stream")), false, None, true),
            (Some(("media-stt", "stream")), false, None, false),
            (Some(("media-stt", "stream")), false, None, true),
            (Some(("media-tts", "stream")), true, None, true),
            (Some(("media-tts", "stream")), true, Some("voice"), true),
            (Some(("media-tts", "stream")), true, Some("other-voice"), true),
        ] {
            let upstream = Upstream::start().await;
            let root = crate::test_support::temp_root();
            let source = root.join("resources");
            resources(&source, &upstream.endpoint, false);
            if !streaming {
                let path = source.join(".waltr/resources/providers/media-stt.yaml");
                let yaml = std::fs::read_to_string(&path).unwrap().replace("streaming: true", "streaming: false");
                std::fs::write(path, yaml).unwrap();
            }
            let tts_path = source.join(".waltr/resources/providers/media-tts.yaml");
            let yaml = std::fs::read_to_string(&tts_path).unwrap().replace("voice: voice", "model_name: upstream-pinned\n        voice: voice");
            std::fs::write(tts_path, yaml).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let loader = crate::test_support::default_catalog_loader();
            let config_service = ConfigService::new(source, loader.clone()).await.unwrap();
            let state = config_service.state().clone();
            let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
                &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
                &format!("sqlite://{}", root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
            let offset = if tts { 2 } else { 0 };
            let mut map = selected.into_iter().flat_map(|(provider, model)| [(VARIABLES[offset].into(), provider.into()), (VARIABLES[offset+1].into(), model.into())]).collect();
            if let Some(voice) = choices {
                std::collections::BTreeMap::insert(&mut map, VARIABLES[4].into(), serde_json::json!([{"model":"stream","voice":voice,"language":"en-US","rate":1.0}]).to_string());
            }
            let options = crate::BuildAppOptions { routine_authority: speech_authority(&map).unwrap(), ..Default::default() };
            let app = assemble_app(AppServices {
                config_service, persistence, catalog_loader: loader,
                access_authenticator: crate::access_auth::AccessAuthenticator::disabled(), mcp_oauth: None,
                outbound_private_origins: vec![], outbound_oauth: Default::default(),
                automatic_self_mcp_resource: Some(format!("http://{address}/mcp").parse().unwrap()),
                agent_state: Some(state), local_admission: None, workspace_root: workspace, sandbox_socket: None,
                routine_authority: options.routine_authority, routine_private_parent: root,
            }, |routes, input| {
                let service = crate::session::SessionService::from_sessions(waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()), input);
                routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
                Ok(service)
            }).await.unwrap();
            let stop = CancellationToken::new();
            let end = stop.clone();
            let task = tokio::spawn(app.serve_with_shutdown(listener, end.cancelled_owned()));
            let name = if tts { "audio-synthesize" } else { "audio-transcribe" };
            let entrypoint = if tts { "speak" } else { "dictate" };
            if streaming && (selected == Some(("media-stt", "stream")) || (tts && choices != Some("other-voice"))) {
                native(address, None, name, entrypoint, if tts {0} else {65536}).await;
                if tts {
                    assert_eq!(upstream.metrics.synthesis_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(*upstream.metrics.synthesis_models.lock().unwrap(), ["upstream-pinned"]);
                } else {
                    assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(upstream.metrics.bytes.load(Ordering::SeqCst), 65536);
                }
            } else {
                let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
                let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
                let definitions = client.watch_definitions(p::WatchDefinitionsRequest {}).await.unwrap().into_inner().message().await.unwrap().unwrap();
                let definition = definitions.definitions.iter().find(|row| row.name == name).unwrap();
                let open = p::InteractRequest { frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
                    selection: Some(p::InvocationSelection { routine: definition.name.clone(), revision: definition.revision.clone(), entrypoint: entrypoint.into(), parameters: Some(p::StructuredValue { json: parameters(name) }), ..Default::default() }),
                    inputs: vec![p::InputDeclaration { port: if tts {"text"} else {"audio"}.into(), format: Some(if tts {p::input_declaration::Format::Text(p::TextFormat {})} else {p::input_declaration::Format::Pcm16le(p::Pcm16Le { sample_rate: 24000, channels: 1 })}) }],
                })) };
                let mut frames = vec![open];
                if tts {
                    frames.extend([
                        input(p::input_event::Event::Open(p::StreamOpen { port:"text".into(), stream_id:"input".into(), format:Some(p::stream_open::Format::Text(p::TextFormat {})) })),
                        input(p::input_event::Event::Chunk(p::StreamChunk { stream_id:"input".into(), sequence:0, payload:Some(p::stream_chunk::Payload::Text(p::TextRevision { segment_id:"one".into(), mutation:p::TextMutation::Append as i32, text:"Committed first sentence.".into(), committed:true })) })),
                        input(p::input_event::Event::Close(p::StreamClose { stream_id:"input".into(), next_sequence:1 })),
                        input(p::input_event::Event::Finish(p::InputFinish {})),
                    ]);
                }
                let mut output = client.interact(tokio_stream::iter(frames)).await.unwrap().into_inner();
                let first = output.message().await.unwrap().unwrap();
                if tts {
                    assert!(matches!(first.output.unwrap().event, Some(p::output_event::Event::Accepted(_))));
                    let mut terminal = None;
                    while let Some(frame) = output.message().await.unwrap() {
                        match frame.output.unwrap().event.unwrap() {
                            p::output_event::Event::Terminal(value) => { terminal = Some(value); }
                            p::output_event::Event::Chunk(_) => panic!("unapproved voice produced output"),
                            _ => {}
                        }
                    }
                    assert_eq!(terminal.unwrap().status, p::InvocationStatus::Failed as i32);
                } else {
                    assert!(matches!(first.output.unwrap().event, Some(p::output_event::Event::Rejected(_))), "selection {selected:?} must reject before admission");
                }
                assert!(output.message().await.unwrap().is_none());
                assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 0);
                assert_eq!(upstream.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
            }
            stop.cancel();
            task.await.unwrap().unwrap();
            upstream.shutdown().await;
        }
    }).await.unwrap();
}
