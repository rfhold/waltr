use super::*;
use serde_json::{Value, json};

#[tokio::test]
async fn resource_selected_capture_and_payload_use_assembled_normal_helper() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::media_workflows::helper_inputs::resource_selected_capture_and_payload_use_assembled_normal_helper",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_address = model_listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let recorded = bodies.clone();
        let model_stop = CancellationToken::new();
        let model_shutdown = model_stop.clone();
        let model_task = tokio::spawn(async move {
            axum::serve(model_listener, axum::Router::new().route("/responses", axum::routing::post(
                move |axum::Json(body): axum::Json<Value>| {
                    recorded.lock().unwrap().push(body.clone());
                    async move {
                        assert!(body["input"].to_string().contains("exact untrusted payload"));
                        assert!(body["input"].to_string().contains("resource-selected task"));
                        let captured = body["input"].as_array().unwrap().iter().any(|item| item["type"] == "function_call_output");
                        let events = if captured {
                            if body["input"].to_string().contains("\\\"decision\\\"") {
                                // Complete normally without emitting any assistant text event.
                                vec![]
                            } else {
                                vec![json!({"type":"response.output_text.delta","delta":"accepted normal response"})]
                            }
                        } else {
                            let tool = body["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == "_helper_capture").unwrap();
                            let key = tool["parameters"]["properties"].as_object().unwrap().keys().next().unwrap();
                            let arguments = json!({(key.clone()):"resource-shaped capture"}).to_string();
                            vec![
                                json!({"type":"response.output_item.added","item":{"id":"capture-item","call_id":"actual-call","type":"function_call","name":"_helper_capture","arguments":""}}),
                                json!({"type":"response.function_call_arguments.done","item_id":"capture-item","arguments":arguments}),
                            ]
                        };
                        let mut data = String::new();
                        for event in events.into_iter().chain([json!({"type":"response.completed","response":{"id":"normal-helper","output":[]}})]) {
                            data.push_str(&format!("data: {event}\n\n"));
                        }
                        ([(axum::http::header::CONTENT_TYPE,"text/event-stream")], data)
                    }
                }
            ))).with_graceful_shutdown(model_shutdown.cancelled_owned()).await.unwrap();
        });
        let root = crate::test_support::temp_root();
        let resources = root.join("resources");
        let speech = Upstream::start().await;
        super::resources(&resources, &speech.endpoint, true);
        let agents = resources.join(".waltr/resources/agents");
        std::fs::create_dir_all(&agents).unwrap();
        for (name, key) in [("editor-a", "decision"), ("editor-b", "different-key"), ("editor-denied", "no-grant")] {
            let permission = if name == "editor-denied" { "deny" } else { "allow" };
            std::fs::write(agents.join(format!("{name}.yaml")), format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: {name}}}\nspec:\n  systemPrompt: Normal resource-selected helper\n  permissions: [{{name: _helper_capture, permission: {permission}}}]\n")).unwrap();
            let source = format!(r#"export default async function () {{
                const payload = await host.call('media.collect', {{port:'text',cap:65536}});
                const result = await host.call('helper.run', {{selection:{{kind:'named',name:'{name}'}},task:'resource-selected task',capture:'declared',maxTurns:4,deadlineMs:10000,responseBytes:65536,completionBytes:65536}}, null, null, payload);
                const response = await host.call('media.read', {{field:'response'}}, null, null, result);
                if (response.byteLength !== {response_bytes}) throw new Error('unexpected helper response');
                const selected = await host.call('media.select', {{field:'/{key}'}}, null, null, result);
                const capture = await host.call('media.read', {{}}, null, null, selected);
                if (capture.byteLength !== 23) throw new Error('capture is not usable');
                await host.call('media.publish', {{name:'business-result'}}, null, null, selected);
                await host.call('media.finish', {{}});
                return true;
            }}"#, response_bytes = if name == "editor-a" { 0 } else { 24 });
            std::fs::write(resources.join(format!(".waltr/resources/routines/explicit-{name}.yaml")), serde_json::to_vec(&json!({
                "apiVersion":"waltr.routine/v1","kind":"Routine","name":format!("explicit-{name}"),"description":"Resource-selected helper contract",
                "spec":{"source":source,"entrypoints":{"run":{"trigger":"interactive"}},
                    "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
                    "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"boolean"}},
                    "ports":{"text":{"direction":"input","media":if permission == "deny" {"audio"} else {"text"},"required":true,"maxBytes":65536,"formats":[if permission == "deny" {"pcm16le-24000-mono"} else {"utf8"}]}},
                    "capabilities":["media.execute","helper.run",format!("helper.named.{name}"),"helper.capture.declared"],
                    "captures":{"declared":{"selection":{"kind":"named","name":name},"contract":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{key:{"type":"string","maxLength":64}},"required":[key]}}}}
                }
            })).unwrap()).unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config = ConfigService::new(resources, loader.clone()).await.unwrap();
        let state = config.state().clone();
        let inference = bindings::default_inference(&state.snapshot()).unwrap();
        let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}",root.join("waltr.sqlite3").display()), &format!("sqlite://{}",root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
        let authority = RoutineAuthority {
            recognition:Some(("media-stt".into(),"stream".into())),
            helper_inputs:vec![crate::routine::production::HelperInputAuthority {
                provider:inference.provider.key, model:inference.provider.model_id,
                capabilities:waltr_agent::TransientModelCapabilities {png:false,jpeg:false,webp:false,pdf:false,text:true,max_body_bytes:1048576},
            }],
            media_restriction:Some(PermissionPolicy::new(PermissionLevel::Deny).with_tool_rule(PermissionToolSelector::exact("_helper_capture"),PermissionEffect::allow())),
            ..Default::default()
        };
        let provider = Arc::new(waltr_agent::provider::openai::OpenAiResponses::new("test").with_url(format!("http://{model_address}/responses")));
        let sessions = SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
        let helper_sessions = sessions.clone();
        let app = assemble_app(AppServices {
            config_service:config,persistence,catalog_loader:loader,access_authenticator:crate::access_auth::AccessAuthenticator::disabled(),
            mcp_oauth:None,outbound_private_origins:Vec::new(),outbound_oauth:OutboundOAuthConfig::default(),automatic_self_mcp_resource:Some(format!("http://{address}/mcp").parse().unwrap()),
            agent_state:Some(state),local_admission:None,workspace_root:workspace,sandbox_socket:None,routine_authority:authority,routine_private_parent:root,
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
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = p::routine_service_client::RoutineServiceClient::new(channel);
        let definitions = client.watch_definitions(p::WatchDefinitionsRequest {}).await.unwrap().into_inner().message().await.unwrap().unwrap();
        let denied = definitions.definitions.iter().find(|definition| definition.name == "explicit-editor-denied").unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(p::InteractRequest { frame:Some(p::interact_request::Frame::Open(p::InteractionOpen {
            selection:Some(p::InvocationSelection {routine:denied.name.clone(),revision:denied.revision.clone(),entrypoint:"run".into(),parameters:Some(p::StructuredValue {json:b"null".to_vec()}),..Default::default()}),
            inputs:vec![p::InputDeclaration {port:"text".into(),format:Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {sample_rate:24000,channels:1}))}],
        })) }).await.unwrap();
        drop(tx);
        let mut rejected = client.interact(ReceiverStream::new(rx)).await.unwrap().into_inner();
        let frame = rejected.message().await.unwrap().unwrap();
        let Some(p::output_event::Event::Rejected(rejection)) = frame.output.unwrap().event else { panic!("declared capture granted permission or transport started before denial"); };
        assert_eq!(rejection.code,p::RejectionCode::Denied as i32);
        assert!(rejected.message().await.unwrap().is_none());
        assert!(bodies.lock().unwrap().is_empty());
        assert_eq!(speech.metrics.speech_calls.load(Ordering::SeqCst),0);
        for name in ["explicit-editor-a","explicit-editor-b"] {
            let definition = definitions.definitions.iter().find(|definition| definition.name == name).unwrap();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.send(p::InteractRequest { frame:Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection:Some(p::InvocationSelection {routine:name.into(),revision:definition.revision.clone(),entrypoint:"run".into(),parameters:Some(p::StructuredValue {json:b"null".to_vec()}),..Default::default()}),
                inputs:vec![p::InputDeclaration {port:"text".into(),format:Some(p::input_declaration::Format::Text(p::TextFormat {}))}],
            })) }).await.unwrap();
            tx.send(input(p::input_event::Event::Open(p::StreamOpen {port:"text".into(),stream_id:"input".into(),format:Some(p::stream_open::Format::Text(p::TextFormat {}))}))).await.unwrap();
            tx.send(input(p::input_event::Event::Chunk(p::StreamChunk {stream_id:"input".into(),sequence:0,payload:Some(p::stream_chunk::Payload::Text(p::TextRevision {segment_id:"one".into(),text:"exact untrusted payload".into(),mutation:p::TextMutation::Append as i32,committed:true}))}))).await.unwrap();
            tx.send(input(p::input_event::Event::Close(p::StreamClose {stream_id:"input".into(),next_sequence:1}))).await.unwrap();
            tx.send(input(p::input_event::Event::Finish(p::InputFinish {}))).await.unwrap();
            drop(tx);
            let mut stream = client.interact(ReceiverStream::new(rx)).await.unwrap().into_inner();
            let mut terminal = None;
            while let Some(frame) = stream.message().await.unwrap() {
                match frame.output.unwrap().event.unwrap() {
                    p::output_event::Event::Rejected(rejected) => panic!("explicit helper rejected: {rejected:?}"),
                    p::output_event::Event::Terminal(value) => terminal = Some(value),
                    _ => {}
                }
            }
            let terminal = terminal.unwrap();
            if name == "explicit-editor-a" {
                let records = helper_sessions.store().list().await.unwrap();
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].status, waltr_agent::SessionStatus::Finished);
                let record = helper_sessions.store().load(&records[0].id).await.unwrap();
                assert!(!record.timeline_events().any(|event| event.channel.as_ref().is_some_and(|channel| channel.as_str() == waltr_agent::timeline::CHANNEL_ASSISTANT_TEXT)), "empty completion must not be a synthetic Some(empty) text event");
            }
            assert_eq!(terminal.status,p::InvocationStatus::Succeeded as i32);
            assert!(terminal.cleanup_complete);
            assert_eq!(terminal.text_results.len(),1);
        }
        assert_eq!(bodies.lock().unwrap().len(),4);
        stop.cancel();
        server.await.unwrap().unwrap();
        model_stop.cancel();
        model_task.await.unwrap();
        speech.shutdown().await;
    }).await.expect("bounded explicit helper production contract");
}
