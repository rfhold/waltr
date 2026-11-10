use super::*;
use futures_util::StreamExt as _;
use serde_json::json;
use waltr_agent::{AgentRuntime, MemorySessionStore, SessionCreateRequest, SessionId};

#[tokio::test]
async fn generic_program_through_assembled_production_constructor_commits_and_starts_once() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let root = crate::test_support::temp_root();
        let resources = root.join("resources");
        seed(&resources);
        let agents = resources.join(".waltr/resources/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("primitive-source.yaml"), "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: primitive-source}\nspec:\n  systemPrompt: SOURCE\n  permissions: [{name: manage-context, permission: allow}]\n").unwrap();
        let message = "  JS chooses exact initial bytes\n\nno host concatenation\n";
        let program = format!(r#"export default async function () {{
            const before = (await host.call('context.snapshot', {{}})).result;
            const absent = (await host.call('context.metadata', {{source:'effective',key:'editable.key'}})).result;
            if (absent.state !== 'absent') throw Error('absence');
            await host.call('context.transact', {{source:'effective',expectedRevision:before.revision,
                predicates:[{{key:'editable.key',value:{{state:'absent'}}}}],
                updates:[{{key:'editable.key',value:{{state:'present',value:null}}}}]}});
            const present = (await host.call('context.metadata', {{source:'effective',key:'editable.key'}})).result;
            if (present.state !== 'present' || present.value !== null) throw Error('null');
            const selected = (await host.call('context.history', {{source:'effective'}})).result;
            const created = (await host.call('context.create', {{source:selected.source,copyHistory:false,
                inheritDefinition:true,inheritInference:true,message:{}}})).result;
            await host.call('context.start', {{destination:created.destination}});
            let denied = false;
            try {{ await host.call('context.start', {{destination:created.destination}}); }} catch (_) {{ denied = true; }}
            if (!denied) throw Error('duplicate start');
            return created.sessionId;
        }}"#, serde_json::to_string(message).unwrap());
        std::fs::write(resources.join(".waltr/resources/routines/primitive-program.yaml"), serde_json::to_vec(&json!({
            "apiVersion":"waltr.routine/v1","kind":"Routine","name":"primitive-program","description":"Generic primitive constructor contract",
            "spec":{"source":program,"entrypoints":{"run":{"trigger":"manual"}},
                "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
                "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":128}},
                "capabilities":["context.snapshot","context.metadata","context.transact","context.history","context.create","context.start"]}
        })).unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config = ConfigService::new(resources, loader.clone()).await.unwrap();
        let state = config.state().clone();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let provider = Arc::new(ScriptedProvider::default());
        let inference = bindings::default_inference(&state.snapshot()).unwrap();
        let source_id = SessionId::generate();
        let mut create = SessionCreateRequest::new(source_id.clone());
        create.agent_definition = state.snapshot().catalog().bundle("primitive-source").map(|bundle| serde_json::to_value(bundle).unwrap());
        create.inference_settings = Some(inference.clone());
        sessions.create(create).await.unwrap();
        let runtime = AgentRuntime::builder(bindings::model(&inference), provider.clone()).build();
        let events = sessions.prompt(&runtime, source_id.clone(), "source prompt").stream().collect::<Vec<_>>().await;
        assert!(events.iter().all(Result::is_ok));
        let supplied_sessions = sessions.clone();
        let persistence = Persistence::initialize(crate::persistence::DatabaseConfig::from_urls(
            &format!("sqlite://{}", root.join("waltr.sqlite3").display()),
            &format!("sqlite://{}", root.join("mcp.sqlite3").display())).unwrap()).await.unwrap();
        let app = assemble_app(AppServices {
            config_service:config,persistence,catalog_loader:loader,
            access_authenticator:crate::access_auth::AccessAuthenticator::disabled(),mcp_oauth:None,
            outbound_private_origins:Vec::new(),outbound_oauth:OutboundOAuthConfig::default(),automatic_self_mcp_resource:Some(format!("http://{address}/mcp").parse().unwrap()),
            agent_state:Some(state),local_admission:None,workspace_root:workspace,sandbox_socket:None,
            routine_authority:RoutineAuthority::default(),routine_private_parent:root,
        }, move |routes,input| {
            let service = Arc::try_unwrap(crate::session::SessionService::from_sessions(supplied_sessions,input)).ok().unwrap().with_test_provider(provider);
            let service = Arc::new(service);
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
        let definition = definitions.definitions.iter().find(|definition| definition.name == "primitive-program").unwrap();
        let invocation = client.start_invocation(p::StartInvocationRequest { selection:Some(p::InvocationSelection {
            routine:"primitive-program".into(),revision:definition.revision.clone(),entrypoint:"run".into(),
            parameters:Some(p::StructuredValue { json:b"null".to_vec() }),correlation:"generic-primitives".into(),source_session:Some(source_id.as_str().into()),
        }) }).await.unwrap().into_inner().invocation.unwrap();
        let mut watch = client.watch_invocations(p::WatchInvocationsRequest { limit:100,..Default::default() }).await.unwrap().into_inner();
        loop {
            let update = watch.message().await.unwrap().unwrap();
            if update.invocations.iter().any(|row| row.id == invocation.id && row.status >= p::InvocationStatus::Succeeded as i32) { break; }
        }
        let detail = client.get_invocation(p::GetInvocationRequest { invocation_id:invocation.id }).await.unwrap().into_inner().invocation.unwrap();
        assert_eq!(detail.status,p::InvocationStatus::Succeeded as i32);
        let destination: String = serde_json::from_slice(&detail.result.unwrap().json).unwrap();
        let record = sessions.store().load(&SessionId::new(destination)).await.unwrap();
        let prompts = record.journal.iter().filter_map(|event| match event { waltr_agent::SessionJournalEvent::PromptAppended { prompt } => Some(prompt.as_str()),_ => None }).collect::<Vec<_>>();
        assert_eq!(prompts,vec![message]);
        assert_eq!(sessions.store().load(&source_id).await.unwrap().metadata_value("editable.key"),Some(&serde_json::Value::Null));
        stop.cancel();
        server.await.unwrap().unwrap();
    }).await.expect("bounded production primitive constructor contract");
}
