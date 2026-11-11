use super::*;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test]
async fn production_helpers_send_defaults_exact_source_and_disabled_reasoning() {
    if !crate::test_support::isolated_process_test(
        "routine::production::runtime::tests::helper_model_selection::production_helpers_send_defaults_exact_source_and_disabled_reasoning",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(45), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_address = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let recorded = bodies.clone();
        let model_stop = CancellationToken::new();
        let shutdown = model_stop.clone();
        let model_task = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().route("/{backend}", axum::routing::post(
                move |axum::extract::Path(backend): axum::extract::Path<String>, axum::Json(body): axum::Json<Value>| {
                    recorded.lock().unwrap().push((backend, body));
                    async {
                        ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"verified helper\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\",\"output\":[]}}\n\n")
                    }
                }
            ))).with_graceful_shutdown(shutdown.cancelled_owned()).await.unwrap();
        });
        let root = crate::test_support::temp_root();
        let resources = root.join("resources");
        seed(&resources);
        for dir in ["providers", "defaults", "agents"] {
            std::fs::create_dir_all(resources.join(format!(".waltr/resources/{dir}"))).unwrap();
        }
        for name in ["default-model", "exact-model", "source-model"] {
            std::fs::write(resources.join(format!(".waltr/resources/providers/{name}.yaml")), format!(
                "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata: {{name: {name}}}\nspec:\n  transport: openaiResponses\n  url: http://{model_address}/{name}\n  models:\n    selected:\n      name: Fixture\n      type: 1\n      options:\n        reasoning_efforts: [low, high, none]\n"
            )).unwrap();
        }
        std::fs::write(resources.join(".waltr/resources/defaults/default.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: Defaults\nmetadata: {name: default}\nspec:\n  inference: {provider: default-model, model: selected}\n").unwrap();
        std::fs::write(resources.join(".waltr/resources/agents/selector-source.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: selector-source}\nspec:\n  systemPrompt: Source\n").unwrap();
        let program = r#"export default async function () {
            for (const inference of [{from:'defaults'}, {provider:'exact-model',model:'selected',reasoningEffort:'low'}, {from:'source-session'}, {from:'source-session',reasoningEffort:null}]) {
                await host.call('helper.run', {selection:{kind:'named',name:'routine-title'},inference,task:'Observe selected inference',maxTurns:1,deadlineMs:10000,responseBytes:65536,completionBytes:65536});
            }
            return true;
        }"#;
        std::fs::write(resources.join(".waltr/resources/routines/helper-selectors.yaml"), serde_json::to_vec(&json!({
            "apiVersion":"waltr.routine/v1","kind":"Routine","name":"helper-selectors","description":"Production helper selectors",
            "spec":{"source":program,"entrypoints":{"run":{"trigger":"manual"}},"parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},"result":{"schemaVersion":"waltr.value/v1","schema":{"type":"boolean"}},"capabilities":["context.snapshot","helper.run","helper.named.routine-title"]}
        })).unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config_service = ConfigService::new(resources, loader.clone()).await.unwrap();
        let state = config_service.state().clone();
        let sessions = SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
        let source_id = waltr_agent::SessionId::generate();
        let mut create = waltr_agent::SessionCreateRequest::new(source_id.clone());
        create.agent_definition = state.snapshot().catalog().bundle("selector-source").map(|bundle| serde_json::to_value(bundle).unwrap());
        let mut inference = crate::session::pinned_inference_settings("source-model".into(), "selected".into(), state.snapshot().configuration()).unwrap();
        inference.provider.reasoning_effort = Some("high".into());
        create.inference_settings = Some(inference.clone());
        sessions.create(create).await.unwrap();
        let runtime = waltr_agent::AgentRuntime::builder(bindings::model(&inference), Arc::new(ScriptedProvider::default())).build();
        let events = sessions.prompt(&runtime, source_id.clone(), "establish source context").stream().collect::<Vec<_>>().await;
        assert!(events.iter().all(Result::is_ok));
        let check_sessions = sessions.clone();
        let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
            &format!("sqlite://{}", root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
        let app = assemble_app(AppServices {
            config_service,persistence,catalog_loader:loader,access_authenticator:crate::access_auth::AccessAuthenticator::disabled(),mcp_oauth:None,
            outbound_private_origins:vec![],outbound_oauth:Default::default(),automatic_self_mcp_resource:Some(format!("http://{address}/mcp").parse().unwrap()),
            agent_state:Some(state),local_admission:None,workspace_root:workspace,sandbox_socket:None,routine_authority:RoutineAuthority::default(),routine_private_parent:root,
        }, move |routes,input| {
            let service = crate::session::SessionService::from_sessions(sessions,input);
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
        let definition = definitions.definitions.iter().find(|row| row.name == "helper-selectors").unwrap();
        let invocation = client.start_invocation(p::StartInvocationRequest {selection:Some(p::InvocationSelection {
            routine:definition.name.clone(),revision:definition.revision.clone(),entrypoint:"run".into(),parameters:Some(p::StructuredValue {json:b"null".to_vec()}),source_session:Some(source_id.as_str().into()),..Default::default()
        })}).await.unwrap().into_inner().invocation.unwrap();
        let mut watch = client.watch_invocations(p::WatchInvocationsRequest {limit:100,..Default::default()}).await.unwrap().into_inner();
        loop {
            let update = watch.message().await.unwrap().unwrap();
            if let Some(row) = update.invocations.iter().find(|row| row.id == invocation.id && row.status >= p::InvocationStatus::Succeeded as i32) {
                assert_eq!(row.status,p::InvocationStatus::Succeeded as i32,"{row:?}");
                break;
            }
        }
        let observed = bodies.lock().unwrap().clone();
        assert_eq!(observed.len(),4,"{observed:?}");
        assert_eq!(observed.iter().map(|(name,_)| name.as_str()).collect::<Vec<_>>(),["default-model","exact-model","source-model","source-model"]);
        assert_eq!(observed[1].1["reasoning"]["effort"],"low");
        assert_eq!(observed[2].1["reasoning"]["effort"],"high");
        assert_eq!(observed[3].1["reasoning"]["effort"],"none");
        assert_eq!(check_sessions.store().list().await.unwrap().len(),5);
        stop.cancel();
        server.await.unwrap().unwrap();
        model_stop.cancel();
        model_task.await.unwrap();
    }).await.expect("bounded real helper selection");
}
