use super::*;

#[tokio::test]
async fn admitted_invocation_pins_backend_across_reload_without_unrelated_defaults() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::speech_selection::admitted_invocation_pins_backend_across_reload_without_unrelated_defaults",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(45), async {
        let old = Upstream::start().await;
        let new = Upstream::start().await;
        let root = crate::test_support::temp_root();
        let source = root.join("resources");
        resources(&source, &old.endpoint, false);
        let invalid = include_str!("../../../../../resources/routines/audio-synthesize.yaml")
            .replace("name: audio-synthesize", "name: wrong-kind-synthesis")
            .replace("speech: {from: 'defaults'}", "speech: {provider: 'media-stt', model: 'stream'}");
        std::fs::write(source.join(".waltr/resources/routines/wrong-kind-synthesis.yaml"), invalid).unwrap();
        std::fs::write(source.join(".waltr/resources/defaults/default.yaml"),
            "apiVersion: waltr.dev/v1alpha1\nkind: Defaults\nmetadata: {name: default}\nspec:\n  speech:\n    tts: {provider: media-tts, model: stream}\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config_service = ConfigService::new(source.clone(), loader.clone()).await.unwrap();
        let state = config_service.state().clone();
        let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
            &format!("sqlite://{}", root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
        let app = assemble_app(AppServices {
            config_service, persistence, catalog_loader: loader,
            access_authenticator: crate::access_auth::AccessAuthenticator::disabled(), mcp_oauth: None,
            outbound_private_origins: vec![], outbound_oauth: Default::default(),
            automatic_self_mcp_resource: Some(format!("http://{address}/mcp").parse().unwrap()),
            agent_state: Some(state.clone()), local_admission: None, workspace_root: workspace, sandbox_socket: None,
            routine_authority: RoutineAuthority::default(), routine_private_parent: root,
        }, |routes, input| {
            let service = crate::session::SessionService::from_sessions(waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()), input);
            routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
            Ok(service)
        }).await.unwrap();
        let stop = CancellationToken::new();
        let end = stop.clone();
        let task = tokio::spawn(app.serve_with_shutdown(listener, end.cancelled_owned()));
        for (name, entry, audio) in [("audio-transcribe", "dictate", true), ("wrong-kind-synthesis", "speak", false)] {
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
            let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
            let definitions = client.watch_definitions(p::WatchDefinitionsRequest {}).await.unwrap().into_inner().message().await.unwrap().unwrap();
            let definition = definitions.definitions.iter().find(|row| row.name == name).unwrap();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.send(p::InteractRequest { frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection: Some(p::InvocationSelection { routine:name.into(), revision:definition.revision.clone(), entrypoint:entry.into(), parameters:Some(p::StructuredValue {json:b"{}".to_vec()}), ..Default::default() }),
                inputs:vec![p::InputDeclaration { port:if audio {"audio"} else {"text"}.into(), format:Some(if audio {p::input_declaration::Format::Pcm16le(p::Pcm16Le {sample_rate:24000,channels:1})} else {p::input_declaration::Format::Text(p::TextFormat {})}) }],
            })) }).await.unwrap();
            let mut stream = client.interact(ReceiverStream::new(rx)).await.unwrap().into_inner();
            tokio::time::timeout(Duration::from_secs(3), async {
                let mut accepted = false;
                loop {
                    match stream.message().await.unwrap().unwrap().output.unwrap().event.unwrap() {
                        p::output_event::Event::Accepted(_) => accepted = true,
                        p::output_event::Event::Terminal(terminal) => {
                            assert!(accepted);
                            assert_eq!(terminal.status, p::InvocationStatus::Failed as i32);
                            assert!(terminal.cleanup_complete);
                            break;
                        }
                        p::output_event::Event::Rejected(value) => panic!("unexpected admission rejection: {value:?}"),
                        _ => {}
                    }
                }
            }).await.expect("invalid selection must finish without requesting any input frames");
            drop(tx);
        }
        assert_eq!(old.metrics.speech_calls.load(Ordering::SeqCst), 0);
        assert_eq!(old.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
        let (admitted, ready) = tokio::sync::oneshot::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        let invocation = tokio::spawn(native_gated(address, None, "audio-synthesize", "speak", 0,
            p::InvocationStatus::Succeeded, 0, Some((admitted, resume))));
        ready.await.unwrap();
        assert_eq!(old.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
        let path = source.join(".waltr/resources/providers/media-tts.yaml");
        let yaml = std::fs::read_to_string(&path).unwrap().replace(&old.endpoint, &new.endpoint);
        std::fs::write(path, yaml).unwrap();
        state.reload_from_disk().await;
        assert!(state.reload_error().is_none());
        release.send(()).unwrap();
        invocation.await.unwrap();
        assert_eq!(old.metrics.synthesis_calls.load(Ordering::SeqCst), 1);
        assert_eq!(new.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
        native(address, None, "audio-synthesize", "speak", 0).await;
        assert_eq!(old.metrics.synthesis_calls.load(Ordering::SeqCst), 1);
        assert_eq!(new.metrics.synthesis_calls.load(Ordering::SeqCst), 1);
        stop.cancel();
        task.await.unwrap().unwrap();
        old.shutdown().await;
        new.shutdown().await;
    }).await.expect("bounded admitted catalog reload");
}

#[tokio::test]
async fn speech_selectors_resolve_exact_options_from_one_pinned_catalog() {
    use crate::routine::media::speech::{CatalogSpeech, SpeechBackend, SpeechSelector};
    let upstream = Upstream::start().await;
    let root = crate::test_support::temp_root();
    resources(&root, &upstream.endpoint, true);
    speech_defaults(&root, Some(("media-stt", "stream")));
    let config = ConfigService::new(root.clone(), crate::test_support::default_catalog_loader()).await.unwrap();
    let state = config.state();
    let pinned = CatalogSpeech(state.snapshot());
    for (synthesis, snapshot) in [(false, false), (false, true), (true, false)] {
        assert!(pinned.select(&SpeechSelector::default(), synthesis, snapshot).is_ok());
    }
    let exact: SpeechSelector = serde_json::from_value(serde_json::json!({"provider":"media-tts","model":"stream","voice":"alternate","language":"en-US","rate":1.25})).unwrap();
    assert!(pinned.select(&exact, true, false).is_ok());
    for value in [
        serde_json::json!({"provider":"missing","model":"stream"}),
        serde_json::json!({"provider":"media-stt","model":"stream"}),
        serde_json::json!({"provider":"media-tts","model":"stream","rate":5}),
        serde_json::json!({"provider":"media-tts","model":"stream","voice":""}),
    ] {
        let selection = serde_json::from_value(value).unwrap();
        assert!(pinned.select(&selection, true, false).is_err());
    }
    std::fs::write(root.join(".waltr/resources/defaults/default.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: Defaults\nmetadata:\n  name: default\nspec: {}\n").unwrap();
    state.reload_from_disk().await;
    assert!(state.reload_error().is_none());
    let reloaded = CatalogSpeech(state.snapshot());
    assert!(reloaded.select(&SpeechSelector::default(), true, false).is_err());
    assert!(reloaded.select(&exact, true, false).is_ok());
    assert!(pinned.select(&SpeechSelector::default(), true, false).is_ok());
    assert_eq!(upstream.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
    assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 0);
    upstream.shutdown().await;
}

#[tokio::test]
async fn helper_selectors_pin_defaults_and_preserve_authorized_source_settings() {
    use crate::routine::adapters::helper::InferenceSelector;
    let root = crate::test_support::temp_root();
    resources(&root, "http://localhost:1", true);
    let config = ConfigService::new(root.clone(), crate::test_support::default_catalog_loader()).await.unwrap();
    let state = config.state();
    let pinned = state.snapshot();
    let defaults = InferenceSelector::default().resolve(&pinned, None).unwrap();
    let selector = |value| serde_json::from_value::<InferenceSelector>(value).unwrap();
    let mut source = defaults.clone();
    source.provider.reasoning_effort = Some("source-sentinel".into());
    let inherited = selector(serde_json::json!({"from":"source-session"}));
    assert_eq!(inherited.resolve(&pinned, Some(&source)).unwrap().provider.reasoning_effort.as_deref(), Some("source-sentinel"));
    assert!(inherited.resolve(&pinned, None).is_err());
    assert_eq!(InferenceSelector::default().resolve(&pinned, Some(&source)).unwrap().provider.reasoning_effort, defaults.provider.reasoning_effort);
    let disabled = selector(serde_json::json!({"from":"source-session","reasoningEffort":null}));
    assert_eq!(disabled.resolve(&pinned, Some(&source)).unwrap().provider.reasoning_effort, source.clone().with_reasoning_disabled().provider.reasoning_effort);
    for value in [
        serde_json::json!({"provider":"missing","model":"missing"}),
        serde_json::json!({"provider":"media-stt","model":"stream"}),
        serde_json::json!({"provider":defaults.provider.key,"model":defaults.provider.model_id,"reasoningEffort":"unsupported-sentinel"}),
    ] { assert!(selector(value).resolve(&pinned, Some(&source)).is_err()); }
    for value in [
        serde_json::json!({"from":"defaults","provider":"p","model":"m"}),
        serde_json::json!({"from":"source-session","sessionId":"foreign"}),
        serde_json::json!({"provider":"p","model":"m","apiKey":"foreign"}),
    ] { assert!(serde_json::from_value::<InferenceSelector>(value).is_err()); }
    std::fs::write(root.join(".waltr/resources/defaults/default.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: Defaults\nmetadata:\n  name: default\nspec: {}\n").unwrap();
    state.reload_from_disk().await;
    assert!(state.reload_error().is_none());
    assert!(InferenceSelector::default().resolve(&state.snapshot(), None).is_err());
    assert!(InferenceSelector::default().resolve(&pinned, None).is_ok());
    assert!(inherited.resolve(&state.snapshot(), Some(&source)).is_ok());
}

#[tokio::test]
async fn snapshot_defaults_validate_before_upstream_effects() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for (provider, model, streaming, diarized, valid) in [
            ("missing", "stream", true, true, false),
            ("snapshot-stt", "missing", true, true, false),
            ("media-tts", "stream", true, true, false),
            ("snapshot-stt", "stream", false, true, false),
            ("snapshot-stt", "stream", true, false, false),
            ("snapshot-stt", "stream", true, true, true),
        ] {
            let upstream = Upstream::start().await;
            let root = crate::test_support::temp_root();
            resources(&root, &upstream.endpoint, true);
            let yaml = std::fs::read_to_string(root.join(".waltr/resources/providers/media-stt.yaml")).unwrap()
                .replace("name: media-stt", "name: snapshot-stt")
                .replace("streaming: true", &format!("streaming: {streaming}"))
                .replace("diarization: true", &format!("diarization: {diarized}"));
            std::fs::write(root.join(".waltr/resources/providers/snapshot-stt.yaml"), yaml).unwrap();
            speech_defaults(&root, Some((provider, model)));
            let result = ConfigService::new(root, crate::test_support::default_catalog_loader()).await;
            assert_eq!(result.is_ok(), valid, "{provider}/{model}, streaming={streaming}, diarized={diarized}");
            assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 0);
            assert_eq!(upstream.metrics.synthesis_calls.load(Ordering::SeqCst), 0);
            upstream.shutdown().await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn catalog_speech_defaults_reach_production_backends_without_environment_authority() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::speech_selection::catalog_speech_defaults_reach_production_backends_without_environment_authority",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        let upstream = Upstream::start().await;
        let root = crate::test_support::temp_root();
        let source = root.join("resources");
        resources(&source, &upstream.endpoint, false);
        let path = source.join(".waltr/resources/providers/media-tts.yaml");
        let yaml = std::fs::read_to_string(&path).unwrap().replace("voice: voice", "model_name: upstream-pinned\n        voice: voice");
        std::fs::write(path, yaml).unwrap();
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
        let app = assemble_app(AppServices {
            config_service, persistence, catalog_loader: loader,
            access_authenticator: crate::access_auth::AccessAuthenticator::disabled(), mcp_oauth: None,
            outbound_private_origins: vec![], outbound_oauth: Default::default(),
            automatic_self_mcp_resource: Some(format!("http://{address}/mcp").parse().unwrap()),
            agent_state: Some(state), local_admission: None, workspace_root: workspace, sandbox_socket: None,
            routine_authority: RoutineAuthority::default(), routine_private_parent: root,
        }, |routes, input| {
            let service = crate::session::SessionService::from_sessions(waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new()), input);
            routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
            Ok(service)
        }).await.unwrap();
        let stop = CancellationToken::new();
        let end = stop.clone();
        let task = tokio::spawn(app.serve_with_shutdown(listener, end.cancelled_owned()));
        native(address, None, "audio-transcribe", "dictate", 65536).await;
        native(address, None, "audio-synthesize", "speak", 0).await;
        assert_eq!(upstream.metrics.speech_calls.load(Ordering::SeqCst), 1);
        assert_eq!(upstream.metrics.bytes.load(Ordering::SeqCst), 65536);
        assert_eq!(upstream.metrics.synthesis_calls.load(Ordering::SeqCst), 1);
        assert_eq!(*upstream.metrics.synthesis_models.lock().unwrap(), ["upstream-pinned"]);
        stop.cancel();
        task.await.unwrap().unwrap();
        upstream.shutdown().await;
    }).await.unwrap();
}
