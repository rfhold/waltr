use super::*;
use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::test]
async fn completed_answer_is_retained_and_spoken_without_tools_or_history() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::ask_and_speak::completed_answer_is_retained_and_spoken_without_tools_or_history",
    ) {
        return;
    }
    completed_answer(false).await;
}

#[tokio::test]
async fn audio_question_is_transcribed_then_answered_and_spoken() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::ask_and_speak::audio_question_is_transcribed_then_answered_and_spoken",
    ) { return; }
    completed_answer(true).await;
}

async fn completed_answer(audio_question: bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let speech = Upstream::start().await;
        let answer = "Café plants use sunlight. 日本語 🌱";
        *speech.metrics.expected_synthesis_text.lock().unwrap() = Some(answer.into());
        let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_address = model_listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let recorded = bodies.clone();
        let model_stop = CancellationToken::new();
        let model_shutdown = model_stop.clone();
        let metrics = speech.metrics.clone();
        let model_task = tokio::spawn(async move {
            axum::serve(model_listener, axum::Router::new().route("/responses", axum::routing::post(
                move |axum::Json(body): axum::Json<Value>| {
                    recorded.lock().unwrap().push(body);
                    let metrics = metrics.clone();
                    async move {
                        let events = futures_util::stream::iter([
                            json!({"type":"response.output_text.delta","delta":"Café plants use sunlight. "}),
                            json!({"type":"response.output_text.delta","delta":"日本語 🌱"}),
                            json!({"type":"response.completed","response":{"id":"answer","output":[]}}),
                        ]).then(move |event| {
                            let metrics = metrics.clone();
                            async move {
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                assert_eq!(metrics.synthesis_calls.load(Ordering::SeqCst), 0);
                                Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(event.to_string()))
                            }
                        });
                        axum::response::Sse::new(events)
                    }
                }
            ))).with_graceful_shutdown(model_shutdown.cancelled_owned()).await.unwrap();
        });
        let root = crate::test_support::temp_root();
        let resources = root.join("resources");
        super::resources(&resources, &speech.endpoint, false);
        let cases = [
            ("string", "{text: ['arbitrary', ' café 日本語'].join(''), output:'audio'}"),
            ("missing", "{output:'audio'}"),
            ("ambiguous", "{text:'hello', port:'text', output:'audio'}"),
            ("null-text", "{text:null, output:'audio'}"),
            ("null-port", "{port:null, text:'hello', output:'audio'}"),
            ("wrong-text", "{text:42, output:'audio'}"),
            ("wrong-port", "{port:42, output:'audio'}"),
            ("oversize", "{text:'x'.repeat(65537), output:'audio'}"),
            ("oversize-utf8", "{text:'日'.repeat(6000), output:'audio'}"),
        ];
        for (name, arguments) in cases {
            let source = format!("export default async function() {{ await host.call('media.synthesize', {arguments}); await host.call('media.finish', {{}}); return true; }}");
            let resource = json!({
                "apiVersion":"waltr.routine/v1", "kind":"Routine", "name":format!("synthesis-{name}"), "description":"String synthesis contract",
                "spec": {"source":source,"entrypoints":{"run":{"trigger":"interactive"}},
                    "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
                    "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"boolean"}},
                    "capabilities":["media.execute"],
                    "ports":{"audio":{"direction":"output","media":"audio","required":true,"maxBytes":1048576,"formats":["pcm16le-24000-mono"]}}
                }
            });
            std::fs::write(resources.join(format!(".waltr/resources/routines/synthesis-{name}.yaml")), serde_json::to_vec(&resource).unwrap()).unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config = ConfigService::new(resources, loader.clone()).await.unwrap();
        let state = config.state().clone();
        let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}",root.join("waltr.sqlite3").display()), &format!("sqlite://{}",root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
        let provider = Arc::new(waltr_agent::provider::openai::OpenAiResponses::new("test").with_url(format!("http://{model_address}/responses")));
        let sessions = SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
        let helper_sessions = sessions.clone();
        let app = assemble_app(AppServices {
            config_service:config,persistence,catalog_loader:loader,access_authenticator:crate::access_auth::AccessAuthenticator::disabled(),
            mcp_oauth:None,outbound_private_origins:Vec::new(),outbound_oauth:OutboundOAuthConfig::default(),automatic_self_mcp_resource:Some(format!("http://{address}/mcp").parse().unwrap()),
            agent_state:Some(state),local_admission:None,workspace_root:workspace,sandbox_socket:None,routine_authority:RoutineAuthority::default(),routine_private_parent:root,
        }, move |routes,input| {
            let service = Arc::new(Arc::try_unwrap(crate::session::SessionService::from_sessions(sessions,input)).ok().unwrap().with_test_provider(provider));
            routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
            Ok(service)
        }).await.unwrap();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let router = app.router().layer(axum::Extension(crate::local_admission::ValidatedLocalOrigin(Arc::new(CancellationToken::new()))));
        let server = tokio::spawn(async move {
            let result = crate::http_lifecycle::serve_owned_connections(listener,router,shutdown.cancelled_owned()).await;
            app.shutdown().await;
            result
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
        let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
        let definitions = client.watch_definitions(p::WatchDefinitionsRequest {}).await.unwrap().into_inner().message().await.unwrap().unwrap();
        let name = if audio_question { "audio-ask-and-speak" } else { "ask-and-speak" };
        let definition = definitions.definitions.iter().find(|row| row.name == name).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(p::InteractRequest { frame:Some(p::interact_request::Frame::Open(p::InteractionOpen {
            selection:Some(p::InvocationSelection {routine:definition.name.clone(),revision:definition.revision.clone(),entrypoint:"ask".into(),parameters:Some(p::StructuredValue {json:serde_json::to_vec(&if audio_question { json!({}) } else { json!({"question":"How do plants use sunlight?"}) }).unwrap()}),..Default::default()}),
            inputs: if audio_question { vec![p::InputDeclaration { port:"question".into(), format:Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {sample_rate:24000, channels:1})) }] } else {vec![]},
        })) }).await.unwrap();
        let mut stream = client.interact(ReceiverStream::new(rx)).await.unwrap().into_inner();
        let producer = tokio::spawn(async move {
            if audio_question {
                tx.send(input(p::input_event::Event::Open(p::StreamOpen {port:"question".into(), stream_id:"question".into(), format:Some(p::stream_open::Format::Pcm16le(p::Pcm16Le {sample_rate:24000, channels:1}))}))).await.unwrap();
                tx.send(input(p::input_event::Event::Chunk(p::StreamChunk {stream_id:"question".into(), sequence:0, payload:Some(p::stream_chunk::Payload::Audio(vec![0;4800]))}))).await.unwrap();
                tx.send(input(p::input_event::Event::Close(p::StreamClose {stream_id:"question".into(), next_sequence:1}))).await.unwrap();
            }
            tx.send(input(p::input_event::Event::Finish(p::InputFinish {}))).await.unwrap();
        });
        let mut terminal = None;
        let mut invocation_id = String::new();
        let mut text = String::new();
        let mut audio = Vec::new();
        while let Some(frame) = stream.message().await.unwrap() {
            match frame.output.unwrap().event.unwrap() {
                p::output_event::Event::Accepted(value) => invocation_id = value.invocation_id,
                p::output_event::Event::Rejected(value) => panic!("rejected: {value:?}"),
                p::output_event::Event::Chunk(chunk) => match chunk.payload.unwrap() {
                    p::stream_chunk::Payload::Text(value) => text.push_str(&value.text),
                    p::stream_chunk::Payload::Audio(value) => audio.extend(value),
                },
                p::output_event::Event::Terminal(value) => terminal = Some(value),
                _ => {}
            }
        }
        producer.await.unwrap();
        let terminal = terminal.unwrap();
        assert_eq!(terminal.status,p::InvocationStatus::Succeeded as i32, "{terminal:?}");
        assert!(terminal.cleanup_complete);
        assert_eq!(terminal.text_results.len(),1);
        assert_eq!(serde_json::from_slice::<Value>(&terminal.result.as_ref().unwrap().json).unwrap(), json!({"answer":answer}));
        let retained = client.read_text_result(p::ReadTextResultRequest {
            invocation_id, result_id:terminal.text_results[0].id.clone(), offset:0, max_bytes:8192,
        }).await.unwrap().into_inner();
        assert_eq!(retained.data, answer.as_bytes());
        assert!(retained.eof);
        assert_eq!(text, if audio_question { "" } else { answer });
        assert!(!audio.is_empty() && audio.len() % 2 == 0);
        assert_eq!(speech.metrics.synthesis_text.lock().unwrap().concat(), answer);
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0]["tools"].is_null() || bodies[0]["tools"].as_array().is_some_and(Vec::is_empty));
        let messages = bodies[0]["input"].as_array().unwrap();
        assert_eq!(messages.iter().filter(|item| item["role"] == "user").count(), 1);
        if audio_question {
            assert_eq!(speech.metrics.speech_calls.load(Ordering::SeqCst), 1);
            assert!(messages.iter().any(|item| item.to_string().contains("Committed conversation")));
        } else {
            assert!(messages.iter().any(|item| item.to_string().contains("How do plants use sunlight?")));
        }
        drop(bodies);
        let records = helper_sessions.store().list().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, waltr_agent::SessionStatus::Finished);
        for (name, _) in cases {
            let definition = definitions.definitions.iter().find(|row| row.name == format!("synthesis-{name}")).unwrap();
            let calls = speech.metrics.synthesis_calls.load(Ordering::SeqCst);
            speech.metrics.synthesis_text.lock().unwrap().clear();
            *speech.metrics.expected_synthesis_text.lock().unwrap() = Some("arbitrary café 日本語".into());
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            tx.send(p::InteractRequest { frame:Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection:Some(p::InvocationSelection {routine:definition.name.clone(),revision:definition.revision.clone(),entrypoint:"run".into(),parameters:Some(p::StructuredValue {json:b"null".to_vec()}),..Default::default()}), inputs:vec![],
            })) }).await.unwrap();
            tx.send(input(p::input_event::Event::Finish(p::InputFinish {}))).await.unwrap();
            drop(tx);
            let mut stream = client.interact(ReceiverStream::new(rx)).await.unwrap().into_inner();
            let mut terminal = None;
            let mut audio = Vec::new();
            while let Some(frame) = stream.message().await.unwrap() {
                match frame.output.unwrap().event.unwrap() {
                    p::output_event::Event::Rejected(value) => panic!("{name} rejected: {value:?}"),
                    p::output_event::Event::Terminal(value) => terminal = Some(value),
                    p::output_event::Event::Chunk(chunk) => if let Some(p::stream_chunk::Payload::Audio(value)) = chunk.payload { audio.extend(value); },
                    _ => {}
                }
            }
            let terminal = terminal.unwrap();
            assert!(terminal.cleanup_complete, "{name}: {terminal:?}");
            if name == "string" {
                assert_eq!(terminal.status, p::InvocationStatus::Succeeded as i32, "{terminal:?}");
                assert_eq!(speech.metrics.synthesis_calls.load(Ordering::SeqCst), calls + 1);
                assert_eq!(speech.metrics.synthesis_text.lock().unwrap().concat(), "arbitrary café 日本語");
                assert!(!audio.is_empty());
            } else {
                assert_ne!(terminal.status, p::InvocationStatus::Succeeded as i32, "{name}");
                assert_eq!(speech.metrics.synthesis_calls.load(Ordering::SeqCst), calls, "{name}");
                assert!(audio.is_empty());
            }
        }
        stop.cancel();
        server.await.unwrap().unwrap();
        model_stop.cancel();
        model_task.await.unwrap();
        speech.shutdown().await;
    }).await.expect("bounded completed-answer speech contract");
}
