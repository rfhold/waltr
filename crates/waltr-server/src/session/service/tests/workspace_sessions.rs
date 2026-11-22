use super::*;
use crate::access_auth::{AccessAuthenticator, AuthenticatedPrincipal};
use crate::agent_host::sandbox_gateway::{SandboxHandle, WorkspaceAccount, peers::MachineRegistry};
use crate::persistence::{DatabaseConfig, Persistence};
use crate::session::SessionHost as _;
use crate::test_support::{composition_auth, sign_access_token_with_ttl};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr::grpc::waltr::session::v1::{
    ContinueSessionRequest, RollbackTarget, SessionSummary, SubmitUserMessageRequest,
    WorkspaceSelection, rollback_target,
};
use waltr_agent::{FilesystemSessionStore, RunConflictPolicy, SessionCreateRequest};

#[allow(dead_code)]
#[path = "../../../../../waltr-agent/tests/support/scripted_provider.rs"]
mod scripted_provider;

fn authorized<T>(value: T, principal: &AuthenticatedPrincipal) -> Request<T> {
    let mut request = Request::new(value);
    request.extensions_mut().insert(principal.clone());
    request
}

async fn verify(auth: &AccessAuthenticator, token: &str) -> AuthenticatedPrincipal {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    auth.authenticate_bearer_headers(&headers, auth.api_required_scopes())
        .await
        .unwrap_or_else(|_| panic!("fixture bearer should authenticate"))
}

async fn peer(
    machines: &MachineRegistry,
    root: &Path,
    auth: AccessAuthenticator,
    token: &str,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let service = machines.mount(axum::Router::new(), auth);
    let router_stop = stop.clone();
    let router = tokio::spawn(async move {
        axum::serve(listener, service)
            .with_graceful_shutdown(router_stop.cancelled_owned())
            .await
            .unwrap();
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let connected = Arc::new(AtomicBool::new(false));
    let authorization: tonic::metadata::AsciiMetadataValue =
        format!("Bearer {token}").parse().unwrap();
    let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_authorization(
        channel,
        root.to_owned(),
        "laptop",
        stop.clone(),
        connected.clone(),
        move || Some(authorization.clone()),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !connected.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (
        stop,
        tokio::spawn(async move {
            worker.await.unwrap().unwrap();
            router.await.unwrap();
        }),
    )
}

async fn service(
    root: &Path,
    store: &Path,
    registry: MachineRegistry,
) -> (
    Arc<SessionService<FilesystemSessionStore>>,
    crate::sandbox::LocalSandbox,
) {
    let catalog = SessionCatalogSource::Static(crate::agent_host::definition::AgentDefinitionCatalog::from_sources(vec![
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: coding}\nspec:\n  features:\n    - filesystem: {root: $(session.workspace.root), profile: auto, systemPrompt: true, defaultPermissions: true}\n    - agents: {root: $(session.workspace.root), filename: AGENTS.md, systemPrompt: true}\n    - shell: {root: $(session.workspace.root), systemPrompt: true, defaultPermissions: true}\n".into(),
    ], "coding").unwrap());
    let (mut input, sandbox) = test_input(root, catalog).await;
    input.workspace_machines = registry;
    (
        Arc::new(
            Arc::try_unwrap(SessionService::new(
                FilesystemSessionStore::new(store),
                input,
            ))
            .unwrap()
            .with_test_provider(Arc::new(scripted_provider::ScriptedProvider::new(
                "workspace",
                |request| {
                    let call = match request.prompt {
                        "Edit sentinel" => Some(("edit", serde_json::json!({"filePath":"sentinel","oldString":"b","newString":"updated"}))),
                        "Run pwd" => Some(("shell", serde_json::json!({"command":"pwd","description":"verify session working directory"}))),
                        _ => None,
                    };
                    if let Some((name, arguments)) = call {
                        return match request.tool_results().find(|result| result.name == name) {
                            Some(result) => vec![Ok(waltr_agent::CompletionEvent::Delta(result.content.as_str().into())), Ok(waltr_agent::CompletionEvent::Finished)],
                            None => vec![Ok(waltr_agent::CompletionEvent::ToolCall(waltr_agent::ToolCall::new(name, name, arguments)))],
                        };
                    }
                    let system = request.system_prompt.unwrap_or_default();
                    let context = ["a", "b"]
                        .into_iter()
                        .find(|dir| {
                            system.contains(&format!("project {dir}"))
                                && request.prompt.contains(&format!("body-{dir}"))
                        })
                        .unwrap_or("missing");
                    vec![
                        Ok(waltr_agent::CompletionEvent::Delta(context.into())),
                        Ok(waltr_agent::CompletionEvent::Finished),
                    ]
                },
            ))),
        ),
        sandbox,
    )
}

async fn create(
    service: &SessionService<FilesystemSessionStore>,
    cwd: Option<&str>,
    principal: &AuthenticatedPrincipal,
) -> SessionSummary {
    service
        .create_session(authorized(
            CreateSessionRequest {
                workspace: cwd.map(|cwd| WorkspaceSelection {
                    machine_id: "laptop".into(),
                    working_directory: Some(cwd.into()),
                }),
                ..Default::default()
            },
            principal,
        ))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap()
}

async fn resume(
    service: &SessionService<FilesystemSessionStore>,
    id: &str,
    principal: Option<&AuthenticatedPrincipal>,
) -> Result<SandboxHandle, AgentSessionRuntimeError> {
    let session = service.sessions.load(&SessionId::new(id)).await.unwrap();
    let factory = (*service.runtime_factory)
        .clone()
        .with_workspace_account(principal.and_then(WorkspaceAccount::from_principal))
        .for_session_workspace(&session.record)
        .await?;
    Ok(factory.sandbox)
}

#[tokio::test]
async fn concurrent_scopes_survive_restart_without_local_fallback() {
    let fixture = crate::test_support::temp_root();
    let local = fixture.join("host");
    let remote = fixture.join("export");
    let store = fixture.join("sessions");
    fs::create_dir_all(&local).unwrap();
    fs::write(local.join("sentinel"), "host").unwrap();
    for cwd in ["a", "b"] {
        fs::create_dir_all(remote.join(cwd)).unwrap();
        fs::write(remote.join(cwd).join("sentinel"), cwd).unwrap();
        fs::write(remote.join(cwd).join("context.txt"), format!("body-{cwd}")).unwrap();
        fs::write(remote.join(cwd).join("AGENTS.md"), format!("project {cwd}")).unwrap();
    }
    let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
        .await
        .unwrap();
    let callback = "127.0.0.1:1".parse().unwrap();
    let (auth, token, issuer, auth_stop, auth_task) =
        composition_auth(&persistence, callback).await;
    let owner = verify(&auth, &token).await;
    let refreshed_token =
        sign_access_token_with_ttl(&issuer, "waltr:read", Duration::from_mins(45));
    let refreshed_owner = verify(&auth, &refreshed_token).await;
    let other_persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
        .await
        .unwrap();
    let (other_auth, other_token, _, other_auth_stop, other_auth_task) =
        composition_auth(&other_persistence, callback).await;
    let other = verify(&other_auth, &other_token).await;
    let machines = MachineRegistry::default;
    let registry = machines();
    let (stop, task) = peer(&registry, &remote, auth.clone(), &token).await;
    let (first, local_worker) = service(&local, &store, registry).await;
    let bad = first
        .create_session(authorized(
            CreateSessionRequest {
                workspace: Some(WorkspaceSelection {
                    machine_id: "unknown".into(),
                    working_directory: None,
                }),
                ..Default::default()
            },
            &owner,
        ))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::FailedPrecondition);
    for (principal, code) in [
        (None, tonic::Code::Unauthenticated),
        (Some(&other), tonic::Code::FailedPrecondition),
    ] {
        let value = CreateSessionRequest {
            workspace: Some(WorkspaceSelection {
                machine_id: "laptop".into(),
                working_directory: Some("a".into()),
            }),
            ..Default::default()
        };
        let mut request = Request::new(value);
        if let Some(principal) = principal {
            request.extensions_mut().insert(principal.clone());
        }
        assert_eq!(
            first.create_session(request).await.unwrap_err().code(),
            code
        );
    }
    assert!(first.list().await.unwrap().is_empty());
    let literal_home = local.join("~").join("project");
    fs::create_dir_all(&literal_home).unwrap();
    fs::write(literal_home.join("sentinel"), "literal tilde").unwrap();
    let literal_session = first
        .create_session(Request::new(CreateSessionRequest {
            workspace: Some(WorkspaceSelection {
                machine_id: "local".into(),
                working_directory: Some(literal_home.to_str().unwrap().into()),
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap();
    assert_eq!(
        literal_session
            .workspace
            .unwrap()
            .working_directory
            .as_deref(),
        Some("~/project")
    );
    let local_selection = first
        .create_session(Request::new(CreateSessionRequest {
            workspace: Some(WorkspaceSelection {
                machine_id: "local".into(),
                working_directory: None,
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap();
    assert_eq!(
        local_selection
            .workspace
            .unwrap()
            .working_directory
            .as_deref(),
        Some(".")
    );
    let (a, b, unbound) = tokio::join!(
        create(&first, Some("a"), &owner),
        create(&first, Some("b"), &owner),
        create(&first, None, &owner)
    );
    assert_eq!(
        a.workspace.as_ref().unwrap().working_directory.as_deref(),
        Some("a")
    );
    assert!(unbound.workspace.is_none());
    let source = first.sessions.load(&SessionId::new(&a.id)).await.unwrap();
    let mut legacy = SessionCreateRequest::new(SessionId::generate());
    let mut legacy_bundle = source.record.agent_definition.clone().unwrap();
    let scope = legacy_bundle["workspace"].as_object_mut().unwrap();
    assert!(scope.remove("account").is_some());
    legacy.agent_definition = Some(legacy_bundle);
    let legacy_id = first.sessions.create(legacy).await.unwrap().record.id;
    let factory = (*first.runtime_factory)
        .clone()
        .with_workspace_account(WorkspaceAccount::from_principal(&owner));
    let child = factory
        .create_with_workspace(
            &first.sessions,
            SessionCreateRequest::new(SessionId::generate()),
            None,
            source.record.agent_definition.as_ref(),
        )
        .await
        .unwrap();
    let replacement = factory
        .create_with_workspace(
            &first.sessions,
            SessionCreateRequest::new(SessionId::generate()),
            b.workspace.as_ref(),
            source.record.agent_definition.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(
        resume(&first, replacement.record.id.as_str(), Some(&owner))
            .await
            .unwrap()
            .scope()
            .unwrap()
            .working_directory,
        "b"
    );
    first.shutdown().await;
    drop(first);
    local_worker.shutdown().await;
    stop.cancel();
    task.await.unwrap();

    let registry = machines();
    let (second, local_worker) = service(&local, &store, registry.clone()).await;
    assert_eq!(
        resume(&second, &literal_session.id, None)
            .await
            .unwrap()
            .read_file("sentinel")
            .await
            .unwrap()
            .content,
        "literal tilde"
    );
    assert!(resume(&second, &a.id, Some(&owner)).await.is_err());
    assert_eq!(
        resume(&second, &unbound.id, None)
            .await
            .unwrap()
            .read_file("sentinel")
            .await
            .unwrap()
            .content,
        "host"
    );
    let (stop, task) = peer(&registry, &remote, other_auth, &other_token).await;
    assert!(resume(&second, &a.id, Some(&owner)).await.is_err());
    assert!(resume(&second, &a.id, Some(&other)).await.is_err());
    stop.cancel();
    task.await.unwrap();
    let (stop, task) = peer(&registry, &remote, auth.clone(), &refreshed_token).await;
    let (ha, hb) = tokio::join!(
        resume(&second, &a.id, Some(&refreshed_owner)),
        resume(&second, &b.id, Some(&refreshed_owner))
    );
    let (ha, hb) = (ha.unwrap(), hb.unwrap());
    let legacy_error = second
        .request_sessions(Some(refreshed_owner.clone()), &legacy_id)
        .await
        .err()
        .expect("ownerless workspace must fail before dispatch");
    assert_eq!(legacy_error.code(), tonic::Code::FailedPrecondition);
    assert!(legacy_error.message().contains("no durable account"));
    for (summary, expected) in [(&a, "a"), (&b, "b")] {
        let id = SessionId::new(&summary.id);
        second
            .sessions
            .set_inference_settings(
                &id,
                Some(waltr_agent::InferenceSettings {
                    provider: waltr_agent::InferenceProviderSettings {
                        key: "test".into(),
                        api: waltr_agent::ModelApi::OpenAiResponses,
                        model_id: "fixture".into(),
                        url: String::new(),
                        headers: Default::default(),
                        oauth: None,
                        model_cost: None,
                        reasoning_effort: None,
                    },
                }),
            )
            .await
            .unwrap();
        let outcome = second
            .request_sessions(Some(refreshed_owner.clone()), &id)
            .await
            .unwrap()
            .submit_prompt(id, "Inspect @context.txt", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .unwrap();
        assert_eq!(outcome.assistant_text.as_deref(), Some(expected));
    }
    assert_eq!(
        ha.read_file("AGENTS.md").await.unwrap().content,
        "project a"
    );
    assert_eq!(
        hb.read_file("AGENTS.md").await.unwrap().content,
        "project b"
    );
    assert_eq!(ha.read_file("../b/sentinel").await.unwrap().content, "b");
    assert_eq!(
        resume(&second, child.record.id.as_str(), Some(&refreshed_owner))
            .await
            .unwrap()
            .scope(),
        ha.scope()
    );
    let id = SessionId::new(&b.id);
    let requests = second
        .request_sessions(Some(refreshed_owner.clone()), &id)
        .await
        .unwrap();
    let result = requests
        .submit_prompt(id, "Edit sentinel", RunConflictPolicy::QueueIfActive)
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    assert_eq!(
        result.assistant_text.as_deref(),
        Some("Edit applied successfully.")
    );
    assert_eq!(
        fs::read_to_string(remote.join("b/sentinel")).unwrap(),
        "updated"
    );
    assert_eq!(ha.read_file("sentinel").await.unwrap().content, "a");
    let id = SessionId::new(&b.id);
    let before = second.sessions.load(&id).await.unwrap().record;
    let rejected = second
        .submit_user_message(authorized(
            SubmitUserMessageRequest {
                session_id: b.id.clone(),
                parts: vec![waltr::grpc::waltr::session::v1::UserMessageInputPart::text(
                    "Do not dispatch",
                )],
            },
            &other,
        ))
        .await
        .unwrap_err();
    assert_eq!(rejected.code(), tonic::Code::PermissionDenied);
    let continuation = ContinueSessionRequest {
        session_id: b.id.clone(),
        target: Some(RollbackTarget {
            target: Some(rollback_target::Target::Turn(1)),
        }),
    };
    let rejected = second
        .continue_session(authorized(continuation.clone(), &other))
        .await
        .unwrap_err();
    assert_eq!(rejected.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        second.sessions.load(&id).await.unwrap().record.revision,
        before.revision
    );
    #[cfg(feature = "shell")]
    for (session, dir) in [(&a, "a"), (&b, "b")] {
        let id = SessionId::new(&session.id);
        let output = second
            .request_sessions(Some(refreshed_owner.clone()), &id)
            .await
            .unwrap()
            .submit_prompt(id, "Run pwd", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .unwrap()
            .assistant_text
            .unwrap();
        assert!(
            output.contains(remote.join(dir).canonicalize().unwrap().to_str().unwrap()),
            "{output}"
        );
    }
    stop.cancel();
    task.await.unwrap();
    let revision = second.sessions.load(&id).await.unwrap().record.revision;
    let offline = second
        .continue_session(authorized(continuation, &refreshed_owner))
        .await
        .unwrap_err();
    assert_eq!(offline.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        second.sessions.load(&id).await.unwrap().record.revision,
        revision
    );
    let changed = fixture.join("changed");
    fs::create_dir_all(changed.join("a")).unwrap();
    let (stop, task) = peer(&registry, &changed, auth, &refreshed_token).await;
    assert!(
        resume(&second, &a.id, Some(&refreshed_owner))
            .await
            .is_err()
    );
    stop.cancel();
    task.await.unwrap();
    second.shutdown().await;
    local_worker.shutdown().await;
    auth_stop.cancel();
    auth_task.await.unwrap();
    other_auth_stop.cancel();
    other_auth_task.await.unwrap();
}
