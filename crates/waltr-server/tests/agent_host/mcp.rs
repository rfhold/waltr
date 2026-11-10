//! Server-owned MCP agent integration tests.

use crate::agent_host::runtime_factory::{
    McpConnectionKind, McpResolvedConnection,
    ServerAgentRuntimeFactory as AgentSessionRuntimeFactory,
};
use crate::auth::{AuthTarget, FileCredentialStore};
use crate::interaction::{Interaction, InteractionBroker, InteractionResponse};
use crate::permission::*;

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    ops::Deref,
    process::Child,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::agent_host::definition::McpServerSelection;
use futures_util::StreamExt;
use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use serde_json::json;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::{Registry, prelude::*};
use waltr_agent::{
    AgentRunLimits, AgentRuntimeProvider, AgentSessionRunError, AgentSessionRuntimeError,
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_INFO,
    CHANNEL_TOOL_RESULT, CompletionDecision, CompletionEvent, CompletionObserver,
    CompletionObserverFuture, CompletionProposal, CompletionValidation, InferenceProviderSettings,
    InferenceSettings, LlmStream, MemorySessionStore, ModelApi, ModelRef, PendingWorkStatus,
    ProviderId, SessionCoordinator, SessionCreateRequest, SessionExecutionOwner, SessionId,
    SessionStatus, SessionStore, SessionTemplateKind, SessionTemplateSearchRequest,
    SessionTemplateSearchTarget, Suspension, TimelineContent, TimelineEventKind, TimelineQuery,
    TimelineStatus, ToolCall, ToolResultContentBlock,
};

use crate::host_tests::sandbox::TestSandbox;
use crate::host_tests::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::host_tests::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";

#[tokio::test]
async fn native_attachment_keeps_normal_mcp_tools_skills_catalog_and_activation_consent()
-> waltr_agent::Result<()> {
    struct Observer(Arc<std::sync::Mutex<Vec<CompletionProposal>>>);
    impl CompletionObserver for Observer {
        fn observe(
            &self,
            proposal: CompletionProposal,
            _: tokio_util::sync::CancellationToken,
        ) -> CompletionObserverFuture {
            self.0.lock().unwrap().push(proposal);
            Box::pin(async { Ok(CompletionDecision::Accept) })
        }
    }
    struct NativeProvider(ScriptedProvider);
    impl LlmStream for NativeProvider {
        fn name(&self) -> &'static str {
            "native-skills-control"
        }
        fn transient_api(&self) -> Option<waltr_agent::TransientInputApi> {
            Some(waltr_agent::TransientInputApi::OpenAiResponses)
        }
        fn preflight_transient_request(
            &self,
            _: &waltr_agent::CompletionRequest<'_>,
            _: &waltr_agent::AdmittedTransientInput,
        ) -> waltr_agent::Result<()> {
            Ok(())
        }
        fn stream_transient_request<'a>(
            &'a self,
            request: waltr_agent::CompletionRequest<'a>,
            input: &'a waltr_agent::AdmittedTransientInput,
        ) -> waltr_agent::BoxStreamFuture<'a> {
            assert_eq!(
                input.input().audit()["attachments"][0]["mediaType"],
                "application/pdf"
            );
            assert!(
                request
                    .system_prompt
                    .unwrap()
                    .contains("skill://ops/inspect/SKILL.md")
            );
            assert!(request.tools.iter().any(|tool| tool.name == "load_skill"));
            self.0.stream_request(request)
        }
        fn stream_request<'a>(
            &'a self,
            _: waltr_agent::CompletionRequest<'a>,
        ) -> waltr_agent::BoxStreamFuture<'a> {
            panic!("attachment invocation must use native transport")
        }
    }
    let provider = Arc::new(NativeProvider(ScriptedProvider::from_turns(
        "native-skills-control",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "ordinary-echo",
                "origin__echo_marker",
                json!({"value":"ordinary-tool-with-attachment"}),
            ))],
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "activation",
                "load_skill",
                json!({"source":"mcp","server":"origin","uri":"skill://ops/inspect/SKILL.md"}),
            ))],
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "after-consent",
                "origin__echo_marker",
                json!({"value":"after-consent"}),
            ))],
            vec![
                CompletionEvent::Delta("done".into()),
                CompletionEvent::Finished,
            ],
        ],
    )));
    let root = tempfile::tempdir()?;
    let sandbox = TestSandbox::new(root.path()).await;
    let mut definition = crate::host_tests::agent_definitions::mcp_agent(false);
    definition.features.mcp.as_mut().unwrap().skills = true;
    let command = test_stdio_server_command();
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().to_owned(),
        FileCredentialStore::new(root.path().join("credentials.json")),
        definition,
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), provider.clone())))
    .with_mcp_connection_resolver(move |_| {
        let command = command.clone();
        Box::pin(async move {
            Ok(vec![McpResolvedConnection {
                key: "origin".into(),
                kind: McpConnectionKind::Stdio,
                command,
                url: String::new(),
            }])
        })
    });
    let broker = InteractionBroker::new();
    let factory = factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let mut create = SessionCreateRequest::new(SessionId::generate());
    create.inference_settings = Some(test_inference_settings());
    create.start = waltr_agent::SessionStart::Fresh {
        messages: vec![waltr_agent::SessionStartMessage::user("initial task").unwrap()],
    };
    let id = sessions.create(create).await?.id().clone();
    let record = sessions.store().load(&id).await.unwrap();
    let context = record.materialize_context(waltr_agent::SessionContextMode::Effective);
    let runtime = tokio::time::timeout(Duration::from_secs(10), sessions.resolve_runtime(&id))
        .await
        .expect("resolve runtime")?;
    let budget = waltr_agent::TransientInputBudget::process();
    let input = waltr_agent::TransientInput::new(
        &budget,
        waltr_agent::TransientPromptTarget {
            cursor: record.timeline_events().last().unwrap().cursor,
            turn: context.next_turn,
            prompt_index: context.prompt_turns.len() - 1,
        },
        waltr_agent::TransientModelCapabilities {
            png: false,
            jpeg: false,
            webp: false,
            pdf: true,
            text: false,
            max_body_bytes: waltr_agent::MAX_TRANSIENT_BODY_BYTES,
        },
        vec![budget.attachment(
            waltr_agent::TransientInputKind::Pdf,
            b"private-native-pdf".to_vec(),
        )?],
    )?;
    let execution = Arc::new(SessionExecutionOwner::new(sessions.clone(), id.clone()));
    let proposals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let validation = CompletionValidation::new(
        Arc::new(Observer(proposals.clone())),
        6,
        Duration::from_secs(20),
    )
    .unwrap();
    let owner_execution = execution.clone();
    let mut owner = tokio::spawn(async move {
        owner_execution
            .helper_with_transient_input(
                runtime,
                input,
                validation,
                AgentRunLimits::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
    });
    let consent = crate::host_tests::pending(&broker, &id).await;
    assert!(
        matches!(&consent.pending[0].request, Interaction::Permission {identity,..} if identity.contains("skill"))
    );
    assert!(!owner.is_finished());
    let record = sessions.store().load(&id).await.unwrap();
    assert!(record.timeline_events().any(|event| matches!(&event.content, TimelineContent::ToolResult { result } if result.call_id == "ordinary-echo" && !result.is_error && result.content.contains("ordinary-tool-with-attachment"))));
    assert!(!record.timeline_events().any(|event| matches!(&event.content, TimelineContent::ToolResult { result } if result.call_id == "activation")));
    assert!(!serde_json::to_string(&record)?.contains("private-native-pdf"));
    crate::host_tests::respond(
        &broker,
        &consent,
        crate::interaction::InteractionResponse::Permission(PermissionResponse::AllowOnce),
    )
    .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                outcome = &mut owner => break outcome.unwrap().unwrap(),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    let pending = broker.snapshot(&id);
                    if !pending.pending.is_empty() {
                        crate::host_tests::respond(&broker, &pending, InteractionResponse::Permission(PermissionResponse::AllowOnce)).unwrap();
                    }
                }
            }
        }
    }).await.expect("native helper completes with original live consent owner");
    assert_eq!(outcome.status, SessionStatus::Finished);
    let proposals = proposals.lock().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].evidence.len(), 3);
    assert_eq!(
        proposals[0]
            .evidence
            .iter()
            .map(|item| item.call.id.as_str())
            .collect::<Vec<_>>(),
        ["ordinary-echo", "activation", "after-consent"]
    );
    drop(proposals);
    assert!(broker.snapshot(&id).pending.is_empty());
    assert!(
        !serde_json::to_string(&sessions.store().load(&id).await.unwrap())?
            .contains("private-native-pdf")
    );
    execution.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), sessions.shutdown())
        .await
        .expect("shutdown");
    Ok(())
}

#[tokio::test]
async fn concrete_hidden_defaults_and_actual_skills_consent_are_fail_closed()
-> waltr_agent::Result<()> {
    use crate::agent_host::authorized_tools::{
        AuthorizedToolExecutor, AuthorizedToolRequest, ConcreteToolTarget,
        SessionAuthorizedToolExecutor,
    };
    use tokio_util::sync::CancellationToken;
    for explicit in [false, true] {
        let root = tempfile::tempdir()?;
        let sandbox = TestSandbox::new(root.path()).await;
        let provider = Arc::new(ScriptedProvider::from_turns(
            "direct-skills",
            vec![
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "activate",
                    "load_skill",
                    json!({"source":"mcp","server":"origin","uri":"skill://ops/inspect/SKILL.md"}),
                ))],
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "pending-consent",
                    "origin__echo_marker",
                    json!({"value":"pending"}),
                ))],
                vec![CompletionEvent::Finished],
            ],
        ));
        let mut registry = crate::agent_host::AgentDefinitionRegistry::new();
        registry.load_yaml(&format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: test }}\nspec:\n  features:\n    - mcp: {{ exposeTools: {explicit}, skills: true, resources: true, defaultPermissions: true }}\n{}", if explicit { "  permissions:\n    - feature: mcp\n      permission: allow\n" } else { "" })).unwrap();
        let command = test_stdio_server_command();
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.path().to_owned(),
            FileCredentialStore::new(root.path().join("credentials.json")),
            registry.resolve_agent("test").unwrap(),
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
        })
        .with_mcp_connection_resolver(move |_| {
            let command = command.clone();
            Box::pin(async move {
                Ok(vec![McpResolvedConnection {
                    key: "origin".into(),
                    kind: McpConnectionKind::Stdio,
                    command,
                    url: String::new(),
                }])
            })
        });
        let broker = InteractionBroker::new();
        let factory =
            factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions));
        let mut create = SessionCreateRequest::new(SessionId::generate());
        create.inference_settings = Some(test_inference_settings());
        let id = sessions.create(create).await?.id().clone();
        let target = ConcreteToolTarget::new("origin", "delayed_write_marker").unwrap();
        let record = sessions.store().load(&id).await.unwrap();
        let (runtime, dispatcher) = factory
            .runtime_for_record(&record, sessions.clone())
            .await
            .unwrap();
        let executor =
            SessionAuthorizedToolExecutor::new(sessions.clone(), id.clone(), runtime, dispatcher);
        let schemas = executor
            .schemas(vec![target.clone()], CancellationToken::new())
            .await;
        if explicit {
            assert_eq!(schemas.unwrap().len(), 1);
        } else {
            assert_eq!(
                schemas.unwrap_err(),
                crate::agent_host::authorized_tools::AuthorizedToolError::Denied
            );
        }
        let marker = root.path().join("direct-effect");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            executor.execute(
                AuthorizedToolRequest {
                    target: target.clone(),
                    call: ToolCall::new(
                        "direct",
                        target.permission_name(),
                        json!({"value":"direct","path":marker,"delay_ms":0}),
                    ),
                },
                CancellationToken::new(),
            ),
        )
        .await
        .expect("direct finishes");
        if !explicit {
            assert!(result.unwrap().is_error);
            assert!(!marker.exists());
        } else {
            assert!(!result.unwrap().is_error);
            assert!(marker.exists());
            fs::remove_file(&marker)?;
            executor.cleanup().await.unwrap();
            let owner = sessions
                .submit_prompt(
                    id.clone(),
                    "activate",
                    waltr_agent::RunConflictPolicy::RejectIfActive,
                )
                .await
                .unwrap()
                .handle;
            let mut owner = tokio::spawn(async move { owner.wait().await });
            let activation = tokio::select! {
                pending = crate::host_tests::pending(&broker, &id) => pending,
                result = &mut owner => panic!("activation owner ended before consent: {result:?}"),
            };
            crate::host_tests::respond(
                &broker,
                &activation,
                InteractionResponse::Permission(PermissionResponse::AllowOnce),
            )
            .unwrap();
            let call = tokio::select! {
                pending = crate::host_tests::pending(&broker, &id) => pending,
                result = &mut owner => panic!("effect owner ended before consent: {result:?}"),
            };
            assert_ne!(activation.pending[0].id, call.pending[0].id);
            crate::host_tests::respond(
                &broker,
                &call,
                InteractionResponse::Permission(PermissionResponse::Deny {
                    message: Some("not approved".into()),
                    mode: PermissionDenyMode::ReturnToolResult,
                }),
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), owner)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!marker.exists());
            assert!(broker.snapshot(&id).pending.is_empty());
        }
        executor.cleanup().await.unwrap();
        drop(executor);
        tokio::time::timeout(Duration::from_secs(5), sessions.shutdown())
            .await
            .expect("shutdown joins");
    }
    Ok(())
}

fn http_app_credential(endpoint: &str, token: &str) -> crate::auth::OAuthCredential {
    crate::auth::OAuthCredential::mcp(
        crate::auth::OAuthTokenSet::bearer(token, None, None),
        crate::auth::McpOAuthContext {
            resource: endpoint.into(),
            issuer: "https://issuer.example".into(),
            registration: crate::auth::McpOAuthRegistration {
                method: crate::auth::McpOAuthRegistrationMethod::Dynamic,
                client_id: "client".into(),
                redirect_uri: "https://waltr.example/callback".into(),
                token_endpoint_auth_method: "none".into(),
                created_at_ms: None,
                expires_at_ms: None,
            },
            endpoints: crate::auth::McpOAuthEndpoints {
                authorization: "https://issuer.example/authorize".into(),
                token: "https://issuer.example/token".into(),
                revocation: None,
            },
        },
    )
}

#[tokio::test]
async fn mcp_apps_anonymous_authority_change_during_io_never_publishes()
-> Result<(), Box<dyn std::error::Error>> {
    for operation in ["call", "open", "provider", "connection"] {
        let temp = tempfile::tempdir()?;
        let marker = temp.path().join("started");
        let server = HttpMcpTestServer::start_with_app_read_marker(
            None,
            None,
            (operation == "open").then_some(marker.as_path()),
        )
        .await?;
        let arguments = if operation == "provider" {
            json!({"value":"original", "execution_marker":marker, "delay_ms":500})
        } else {
            json!({"value":"original"})
        };
        let provider = Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "anonymous-apps-race",
            vec![
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "original",
                    "http_test__app_marker",
                    arguments,
                ))],
                vec![CompletionEvent::Finished],
            ],
        )));
        let sessions = mcp_sessions(provider.clone()).await;
        let key = AuthTarget::mcp_server("http_test").credential_key();
        let replace = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("operation started at the anonymous source");
            if operation == "connection" {
                let mut connection = http_runtime_server(&server);
                connection.url.push_str("/replaced");
                sessions.set_connections(vec![connection]);
            } else {
                sessions
                    .credentials
                    .write(&key, http_app_credential(&server.endpoint, "token-b"))
                    .await
                    .unwrap();
            }
        };
        let initial = run_session_prompt(&sessions, "app", vec![http_runtime_server(&server)]);
        let session = if operation == "provider" {
            let (session, ()) = tokio::join!(initial, replace);
            let error = session
                .err()
                .expect("authority replacement during an admitted effect is uncertain");
            assert!(error.to_string().contains("uncertain"), "{error}");
            let id = sessions.list().await?.into_iter().next().unwrap().id;
            let record = sessions.store().load(&id).await?;
            assert!(record.has_unsettled_host_dispatch());
            let session = test_session_from_timeline(record.timeline_events().cloned().collect());
            assert!(
                session
                    .timeline
                    .iter()
                    .all(|event| event.metadata.get("mcp_app").is_none())
            );
            session
        } else {
            let session = initial.await?;
            let id = session.timeline[0].session_id.clone();
            let before = sessions.store().load(&id).await?;
            let token = tokio_util::sync::CancellationToken::new;
            if operation == "call" || operation == "connection" {
                let app = sessions
                    .apps
                    .open_session_mcp_app(&id, "original", token())
                    .await?;
                let pending = sessions.apps.call_session_mcp_app_tool(
                    &app.handle,
                    "app_only",
                    json!({"value":"do-not-publish", "execution_marker":marker, "delay_ms":500}),
                    token(),
                );
                let (result, ()) = tokio::join!(pending, replace);
                assert_eq!(
                    result.unwrap_err(),
                    crate::agent_host::mcp::session_apps::McpAppError::Stale
                );
            } else {
                let pending = sessions.apps.open_session_mcp_app(&id, "original", token());
                let (result, ()) = tokio::join!(pending, replace);
                assert_eq!(
                    result.unwrap_err(),
                    crate::agent_host::mcp::session_apps::McpAppError::Stale
                );
            }
            let after = sessions.store().load(&id).await?;
            assert_eq!(
                before
                    .timeline_events()
                    .filter(|event| event
                        .channel
                        .as_ref()
                        .is_none_or(|channel| channel.as_str() != "mcp_app_audit"))
                    .cloned()
                    .collect::<Vec<_>>(),
                after
                    .timeline_events()
                    .filter(|event| event
                        .channel
                        .as_ref()
                        .is_none_or(|channel| channel.as_str() != "mcp_app_audit"))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            assert!(
                after
                    .timeline_events()
                    .filter(|event| event.cursor > before.timeline_cursor_high_water())
                    .all(|event| match &event.content {
                        TimelineContent::Json { value } => !value
                            .get("result")
                            .is_some_and(|result| result.to_string().contains("do-not-publish")),
                        _ => true,
                    })
            );
            session
        };
        let id = session.timeline[0].session_id.clone();
        if operation != "connection" && operation != "provider" {
            assert_eq!(
                sessions
                    .apps
                    .open_session_mcp_app(
                        &id,
                        "original",
                        tokio_util::sync::CancellationToken::new()
                    )
                    .await
                    .unwrap_err(),
                crate::agent_host::mcp::session_apps::McpAppError::Stale
            );
        }
        assert_eq!(
            provider.request_prompts().len(),
            if operation == "provider" { 1 } else { 2 }
        );
        sessions.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn mcp_apps_credential_authority_refresh_replacement_and_legacy_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    for initial in ["authorized", "anonymous", "legacy"] {
        let server = HttpMcpTestServer::start().await?;
        let provider = Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "credential-apps",
            vec![
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "original",
                    "http_test__app_marker",
                    json!({"value":"original"}),
                ))],
                vec![CompletionEvent::Finished],
            ],
        )));
        let sessions = mcp_sessions(provider.clone()).await;
        let key = AuthTarget::mcp_server("http_test").credential_key();
        let mut credential = http_app_credential(&server.endpoint, "token-a");
        if initial == "legacy" {
            credential.authority_id = None;
        }
        if initial != "anonymous" {
            sessions.credentials.write(&key, credential.clone()).await?;
        }
        let session =
            run_session_prompt(&sessions, "app", vec![http_runtime_server(&server)]).await?;
        assert!(!session.tool_results[0].is_error);
        let id = session.timeline[0].session_id.clone();
        let token = tokio_util::sync::CancellationToken::new;
        if initial == "legacy" {
            assert!(
                session
                    .timeline
                    .iter()
                    .all(|event| event.metadata.get("mcp_app").is_none())
            );
            assert_eq!(
                sessions
                    .apps
                    .open_session_mcp_app(&id, "original", token())
                    .await
                    .unwrap_err(),
                crate::agent_host::mcp::session_apps::McpAppError::Stale
            );
        } else {
            let app = sessions
                .apps
                .open_session_mcp_app(&id, "original", token())
                .await?;
            if initial == "authorized" {
                credential.token.access_token =
                    oauth2::AccessToken::new("refreshed-token-a".into());
                sessions.credentials.write(&key, credential.clone()).await?;
                let refreshed = sessions
                    .apps
                    .call_session_mcp_app_tool(
                        &app.handle,
                        "app_only",
                        json!({"value":"refresh"}),
                        token(),
                    )
                    .await?;
                assert_eq!(refreshed.raw["_meta"]["private"], "app-secret");
                assert_eq!(
                    sessions
                        .apps
                        .open_session_mcp_app(&id, "original", token())
                        .await?
                        .association
                        .source,
                    app.association.source
                );
                let before = sessions.store().load(&id).await?;
                let pending = sessions.apps.call_session_mcp_app_tool(
                    &app.handle,
                    "app_only",
                    json!({"value":"do-not-publish", "delay_ms":200}),
                    token(),
                );
                let replace = async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let replacement = http_app_credential(&server.endpoint, "token-b");
                    sessions.credentials.write(&key, replacement).await.unwrap();
                };
                let (result, ()) = tokio::join!(pending, replace);
                assert_eq!(
                    result.unwrap_err(),
                    crate::agent_host::mcp::session_apps::McpAppError::Stale
                );
                let after = sessions.store().load(&id).await?;
                assert_eq!(
                    before
                        .timeline_events()
                        .filter(|event| event
                            .channel
                            .as_ref()
                            .is_none_or(|channel| channel.as_str() != "mcp_app_audit"))
                        .cloned()
                        .collect::<Vec<_>>(),
                    after
                        .timeline_events()
                        .filter(|event| event
                            .channel
                            .as_ref()
                            .is_none_or(|channel| channel.as_str() != "mcp_app_audit"))
                        .cloned()
                        .collect::<Vec<_>>()
                );
                assert!(
                    after
                        .timeline_events()
                        .filter(|event| event.cursor > before.timeline_cursor_high_water())
                        .all(|event| match &event.content {
                            TimelineContent::Json { value } =>
                                !value.get("result").is_some_and(|result| result
                                    .to_string()
                                    .contains("do-not-publish")),
                            _ => true,
                        })
                );
            } else {
                sessions.credentials.write(&key, credential.clone()).await?;
            }
            assert_eq!(
                sessions
                    .apps
                    .call_session_mcp_app_tool(
                        &app.handle,
                        "app_only",
                        json!({"value":"stale"}),
                        token()
                    )
                    .await
                    .unwrap_err(),
                crate::agent_host::mcp::session_apps::McpAppError::Stale
            );
            assert_eq!(
                sessions
                    .apps
                    .open_session_mcp_app(&id, "original", token())
                    .await
                    .unwrap_err(),
                crate::agent_host::mcp::session_apps::McpAppError::Stale
            );
        }
        assert_eq!(provider.request_prompts().len(), 2);
        sessions.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn mcp_apps_malformed_links_fail_closed_for_provider_and_template_invocation()
-> Result<(), Box<dyn std::error::Error>> {
    let names = ["malformed_app_only", "malformed_empty", "malformed_default"];
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("must-not-execute");
    let provider_marker = marker.clone();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "malformed-apps",
        move |request| {
            assert!(!request.prompt.contains("app-secret"));
            if request.tool_results().count() == 0 {
                [
                    "malformed_app_only",
                    "malformed_empty",
                    "malformed_default",
                    "echo_marker",
                ]
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("call-{index}"),
                        format!("stdio_test__{name}"),
                        json!({"value":"dispatch-marker", "execution_marker":provider_marker}),
                    )))
                })
                .collect()
            } else {
                vec![Ok(CompletionEvent::Finished)]
            }
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let prompt = names
        .iter()
        .map(|name| {
            format!(
                "@tool(stdio_test__{name}, {})",
                json!({"value":"template-marker", "execution_marker":marker})
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let session = run_session_prompt(
        &sessions,
        &prompt,
        vec![stdio_runtime_server(&test_stdio_server_command())],
    )
    .await?;
    for name in names {
        assert_tool_was_not_surfaced(&provider, &format!("stdio_test__{name}"));
        let result = session
            .tool_results
            .iter()
            .find(|result| result.name == format!("stdio_test__{name}"))
            .unwrap();
        assert!(result.is_error);
        assert!(!result.content.contains("app-secret"));
    }
    assert!(
        session
            .tool_results
            .iter()
            .any(|result| result.name == "stdio_test__echo_marker"
                && !result.is_error
                && result.content.contains("dispatch-marker"))
    );
    assert!(
        session
            .timeline
            .iter()
            .all(|event| event.metadata.get("mcp_app").is_none())
    );
    assert!(
        !marker.exists(),
        "malformed tool was dispatched by provider or template"
    );
    sessions.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn mcp_apps_source_name_collisions_never_reconstruct_origin_and_changed_source_is_stale()
-> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(ScriptedProvider::new("apps-collision", |request| {
        if request.tool_results().next().is_none() {
            let alias = request
                .tools
                .iter()
                .find(|tool| tool.name.starts_with("a_b__app_marker__"))
                .expect("collision-safe surfaced alias")
                .name
                .clone();
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "original",
                alias,
                json!({"value":"original"}),
            )))]
        } else {
            vec![Ok(CompletionEvent::Finished)]
        }
    }));
    let sessions = mcp_sessions(provider).await;
    let mut first = stdio_runtime_server(&test_stdio_server_command());
    first.key = "a-b".into();
    let mut second = first.clone();
    second.key = "a_b".into();
    let session = run_session_prompt(&sessions, "app", vec![first.clone(), second.clone()]).await?;
    let id = session.timeline[0].session_id.clone();
    let app = sessions
        .apps
        .open_session_mcp_app(&id, "original", tokio_util::sync::CancellationToken::new())
        .await?;
    let source_server = app.association.server.clone();
    assert!(["a-b", "a_b"].contains(&source_server.as_str()));
    sessions
        .apps
        .call_session_mcp_app_tool(
            &app.handle,
            "app_only",
            json!({"value":"bound"}),
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    let record = sessions.store().load(&id).await?;
    assert!(record.timeline_events().any(|event| matches!(&event.content, TimelineContent::Json { value } if value["server"] == source_server && value["tool"] == "app_only")));
    assert_eq!(
        sessions
            .apps
            .call_session_mcp_app_tool(
                &app.handle,
                "a_b__app_only",
                json!({"value":"wrong-name"}),
                tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Denied
    );
    if source_server == first.key {
        first.command.push_str(" --changed-source");
    } else {
        second.command.push_str(" --changed-source");
    }
    sessions.set_connections(vec![first, second]);
    assert_eq!(
        sessions
            .apps
            .call_session_mcp_app_tool(
                &app.handle,
                "app_only",
                json!({"value":"stale"}),
                tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Stale
    );
    sessions.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn mcp_apps_preserve_raw_snapshot_refresh_without_inference_and_revoke()
-> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "apps",
        |request| {
            if request.tool_results().next().is_none() {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "original-app",
                    "stdio_test__app_marker",
                    json!({"value": "original", "error": true}),
                )))]
            } else {
                vec![Ok(CompletionEvent::Finished)]
            }
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "app",
        vec![stdio_runtime_server(&test_stdio_server_command())],
    )
    .await?;
    let id = session.timeline[0].session_id.clone();
    assert_tool_was_surfaced(&provider, "stdio_test__app_marker");
    assert_tool_was_surfaced(&provider, "stdio_test__model_only");
    assert_tool_was_not_surfaced(&provider, "stdio_test__app_only");
    let original = sessions.store().load(&id).await?;
    let original_result = original.timeline_events().find(|event| matches!(&event.content, TimelineContent::ToolResult { result } if result.call_id == "original-app")).unwrap().clone();
    let association = &original_result.metadata["mcp_app"];
    assert_eq!(association["server"], "stdio_test");
    assert_eq!(association["tool"], "app_marker");
    assert_eq!(association["arguments"]["value"], "original");
    assert_eq!(association["result"]["_meta"]["private"], "app-secret");
    assert_eq!(
        association["result"]["structuredContent"]["value"],
        "original"
    );
    assert_eq!(association["result"]["isError"], true);
    let replay: waltr_agent::SessionRecord =
        serde_json::from_value(serde_json::to_value(&original)?)?;
    assert_eq!(
        replay
            .timeline_events()
            .find(|event| event.item_id == original_result.item_id)
            .unwrap()
            .metadata,
        original_result.metadata
    );
    let app = sessions
        .apps
        .open_session_mcp_app(
            &id,
            "original-app",
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    assert!(app.resource.html.contains("App marker"));
    let refresh = sessions
        .apps
        .call_session_mcp_app_tool(
            &app.handle,
            "app_only",
            json!({"value":"refresh"}),
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    assert_eq!(refresh.raw["structuredContent"]["value"], "refresh");
    assert_eq!(refresh.raw["_meta"]["private"], "app-secret");
    let after = sessions.store().load(&id).await?;
    assert_eq!(after.snapshot().turns, original.snapshot().turns);
    assert_eq!(
        after
            .timeline_events()
            .find(|event| event.item_id == original_result.item_id)
            .unwrap(),
        &original_result
    );
    assert!(after.timeline_events().any(|event| {
        event
            .channel
            .as_ref()
            .is_some_and(|channel| channel.as_str() == "mcp_app_audit")
    }));
    assert_eq!(provider.request_prompts().len(), 2);
    assert_eq!(
        sessions
            .apps
            .call_session_mcp_app_tool(
                &app.handle,
                "model_only",
                json!({"value":"forbidden"}),
                tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Denied
    );
    let cancellation = tokio_util::sync::CancellationToken::new();
    let call = sessions.apps.call_session_mcp_app_tool(
        &app.handle,
        "app_only",
        json!({"value":"canceled", "delay_ms": 5000}),
        cancellation.clone(),
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(call, cancel);
    assert_eq!(
        result.unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Canceled
    );
    let unsettled = sessions
        .apps
        .call_session_mcp_app_tool(
            &app.handle,
            "app_only",
            json!({"value":"must-not-replay"}),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(unsettled, crate::agent_host::mcp::session_apps::McpAppError::Failure(ref message) if message.contains("unsettled"))
    );
    sessions.apps.close_session_mcp_app(&app.handle);
    drop(app);
    let fresh = run_session_prompt(
        &sessions,
        "app",
        vec![stdio_runtime_server(&test_stdio_server_command())],
    )
    .await?;
    let id = fresh.timeline[0].session_id.clone();
    let app = sessions
        .apps
        .open_session_mcp_app(
            &id,
            "original-app",
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    let call = sessions.apps.call_session_mcp_app_tool(
        &app.handle,
        "app_only",
        json!({"value":"closing", "delay_ms": 5000}),
        tokio_util::sync::CancellationToken::new(),
    );
    let close = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        sessions.apps.close_session_mcp_app(&app.handle);
    };
    let (result, ()) = tokio::join!(call, close);
    assert_eq!(
        result.unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Canceled
    );
    assert_eq!(
        sessions
            .apps
            .call_session_mcp_app_tool(
                &app.handle,
                "app_only",
                json!({"value":"closed"}),
                tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Canceled
    );
    assert_eq!(
        sessions
            .apps
            .open_session_mcp_app(
                &id,
                "missing-call",
                tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Stale
    );
    drop(app);
    let fresh = run_session_prompt(
        &sessions,
        "app",
        vec![stdio_runtime_server(&test_stdio_server_command())],
    )
    .await?;
    let id = fresh.timeline[0].session_id.clone();
    let mut handles = Vec::new();
    for _ in 0..32 {
        handles.push(
            sessions
                .apps
                .open_session_mcp_app(
                    &id,
                    "original-app",
                    tokio_util::sync::CancellationToken::new(),
                )
                .await?
                .handle,
        );
    }
    assert_eq!(
        sessions
            .apps
            .open_session_mcp_app(
                &id,
                "original-app",
                tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Limit
    );
    drop(handles);
    sessions.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn mcp_apps_current_permissions_and_hidden_calls_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(ScriptedProvider::from_turns(
        "apps-policy",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "original",
                "stdio_test__app_marker",
                json!({"value":"original"}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    let session = run_session_prompt(
        &sessions,
        "app",
        vec![stdio_runtime_server(&test_stdio_server_command())],
    )
    .await?;
    let id = session.timeline[0].session_id.clone();
    let app = sessions
        .apps
        .open_session_mcp_app(&id, "original", tokio_util::sync::CancellationToken::new())
        .await?;
    for level in ["ask", "deny"] {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: apps }}\nspec:\n  features: [{{ featureSetRef: {{ name: apps }} }}]\n  permissions: [{{ permissionSetRef: {{ name: apps }} }}]\n---\napiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata: {{ name: apps }}\nspec:\n  mcp: {{ exposeTools: true, defaultPermissions: true }}\n---\napiVersion: waltr.dev/v1alpha1\nkind: PermissionSet\nmetadata: {{ name: apps }}\nspec:\n  tools: [{{ name: stdio_test__app_only, permission: {level} }}]\n"
        );
        sessions
            .set_agent_definition(
                &id,
                Some(serde_json::to_value(
                    crate::agent_host::AgentDefinitionBundle::new("apps", vec![source]),
                )?),
            )
            .await?;
        let apps = sessions.apps.clone();
        let handle = app.handle.clone();
        let owner = tokio::spawn(async move {
            apps.call_session_mcp_app_tool(
                &handle,
                "app_only",
                json!({"value":"forbidden"}),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
        });
        if level == "ask" {
            let pending = super::pending(&sessions.broker, &id).await;
            assert!(!owner.is_finished());
            super::respond(
                &sessions.broker,
                &pending,
                InteractionResponse::Permission(PermissionResponse::Deny {
                    message: Some("not approved".into()),
                    mode: PermissionDenyMode::ReturnToolResult,
                }),
            )
            .unwrap();
        }
        assert!(owner.await?.is_err());
    }
    let record = sessions.store().load(&id).await?;
    let boundary = record.timeline_events().last().unwrap().cursor;
    let selected = sessions.capture_latest_completed(&id).await?.through_cursor;
    let mut mutation = waltr_agent::SessionMaskMutation::new(Some(boundary));
    mutation.add.push(waltr_agent::SessionMaskAddition::new(
        "hide-app",
        waltr_agent::ThroughCursor { cursor: selected },
        waltr_agent::ReplacementUserContent::new("masked"),
    ));
    sessions.store().mutate_masks(&id, mutation).await?;
    assert_eq!(
        sessions
            .apps
            .open_session_mcp_app(&id, "original", tokio_util::sync::CancellationToken::new())
            .await
            .unwrap_err(),
        crate::agent_host::mcp::session_apps::McpAppError::Stale
    );
    sessions.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn mcp_skills_durable_permission_workflow_binds_activation_and_cross_origin_call()
-> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_skill_consent",
        |request| {
            let event = match request.tool_results().count() {
                0 => CompletionEvent::ToolCall(ToolCall::new(
                    "load_remote",
                    "load_skill",
                    json!({"source":"mcp","server":"origin","uri":"skill://ops/inspect/SKILL.md"}),
                )),
                1 => CompletionEvent::ToolCall(ToolCall::new(
                    "remote_action",
                    "origin__echo_marker",
                    json!({"value":"approved-action"}),
                )),
                2 => CompletionEvent::ToolCall(ToolCall::new(
                    "support",
                    "read_mcp_resource",
                    json!({"server":"origin","uri":"skill://ops/inspect/references/action.txt"}),
                )),
                _ => CompletionEvent::Finished,
            };
            vec![Ok(event)]
        },
    )));
    let root = tempfile::tempdir()?.keep();
    let sandbox = TestSandbox::new(&root).await;
    let mut definition = crate::host_tests::agent_definitions::mcp_agent(false);
    definition.features.mcp.as_mut().unwrap().skills = true;
    let captured = provider.clone();
    let command = test_stdio_server_command();
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("credentials.json")),
        definition,
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), captured.clone())))
    .with_mcp_connection_resolver(move |_| {
        let command = command.clone();
        Box::pin(async move {
            Ok(vec![McpResolvedConnection {
                key: "origin".into(),
                kind: McpConnectionKind::Stdio,
                command,
                url: String::new(),
            }])
        })
    });
    let store = MemorySessionStore::new();
    let broker = InteractionBroker::new();
    let factory = factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
    let sessions = SessionCoordinator::new(store.clone());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let mut create = SessionCreateRequest::new(SessionId::generate());
    create.inference_settings = Some(test_inference_settings());
    let id = sessions.create(create).await?.id().clone();
    let owner = sessions
        .submit_prompt(
            id.clone(),
            "load remote skill",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?
        .handle;
    let record = store.load(&id).await.expect("stored session");
    let activation = crate::host_tests::pending(&broker, &id).await;
    assert!(
        matches!(&activation.pending[0].request, Interaction::Permission {identity,..} if identity.contains("skill"))
    );
    assert!(!record.timeline_events().any(|event| matches!(&event.content,TimelineContent::ToolResult {result} if result.name == "load_skill")));
    crate::host_tests::respond(
        &broker,
        &activation,
        InteractionResponse::Permission(PermissionResponse::AllowOnce),
    )
    .unwrap();
    let action = crate::host_tests::pending(&broker, &id).await;
    let record = store.load(&id).await.expect("stored session");
    assert_ne!(activation.pending[0].id, action.pending[0].id);
    assert!(
        matches!(&action.pending[0].request, Interaction::Permission {identity,..} if identity.contains("origin"))
    );
    assert!(record.timeline_events().any(|event| matches!(&event.content,TimelineContent::ToolResult {result} if result.name == "load_skill" && result.content.contains("@tool(shell) stays literal"))));
    crate::host_tests::respond(
        &broker,
        &action,
        InteractionResponse::Permission(PermissionResponse::AllowOnce),
    )
    .unwrap();
    owner.wait().await?;
    let record = store.load(&id).await.expect("stored session");
    assert!(record.timeline_events().any(|event| matches!(&event.content,TimelineContent::ToolResult {result} if result.name == "read_mcp_resource" && result.content.contains("Supporting skill file."))));
    assert!(
        record.snapshot().middleware_state["mcp-skills"]["held"][0]
            .get("approved")
            .is_none()
    );
    assert!(broker.snapshot(&id).pending.is_empty());
    assert_tool_was_surfaced(&provider, "load_skill");
    assert_tool_was_surfaced(&provider, "read_mcp_resource");
    sessions.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn mcp_skills_input_and_sandbox_ask_preserve_exact_call_approvals() -> waltr_agent::Result<()>
{
    for final_decision in 0..4 {
        let provider = Arc::new(ScriptedProvider::new(
            "skill_permission_retries",
            |request| {
                vec![Ok(match request.tool_results().count() {
                    0 => CompletionEvent::ToolCall(ToolCall::new(
                        "activate",
                        "load_skill",
                        json!({"source":"mcp","server":"origin","uri":"skill://ops/inspect/SKILL.md"}),
                    )),
                    1 => CompletionEvent::ToolCall(ToolCall::new(
                        "read-outside",
                        "read",
                        json!({"filePath":"../outside.txt"}),
                    )),
                    _ => CompletionEvent::Finished,
                })]
            },
        ));
        let parent = tempfile::tempdir()?;
        let root = parent.path().join("workspace");
        fs::create_dir(&root)?;
        fs::write(
            parent.path().join("outside.txt"),
            "sandbox-permission-protected",
        )?;
        let sandbox = TestSandbox::new(&root).await;
        let mut registry = crate::agent_host::AgentDefinitionRegistry::new();
        registry
            .load_yaml(
                r"apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec:
  features:
    - filesystem: { root: $(session.workspace.root), profile: auto, defaultPermissions: true }
    - mcp: { skills: true, defaultPermissions: true }
  permissions:
    - name: read
      permission:
        inputs:
          - match: [{ pointer: /filePath, value: { wildcard: '../*' } }]
            effect: ask
    - name: read
      permission:
        resources:
          paths: [{ match: { outsideRoot: $(session.workspace.root) }, effect: ask }]
",
            )
            .expect("load permission fixture");
        let definition = registry.resolve_agent("test").expect("resolve fixture");
        let command = test_stdio_server_command();
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("credentials.json")),
            definition,
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
        })
        .with_mcp_connection_resolver(move |_| {
            let command = command.clone();
            Box::pin(async move {
                Ok(vec![McpResolvedConnection {
                    key: "origin".into(),
                    kind: McpConnectionKind::Stdio,
                    command,
                    url: String::new(),
                }])
            })
        });
        let store = MemorySessionStore::new();
        let broker = InteractionBroker::new();
        let factory =
            factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
        let sessions = SessionCoordinator::new(store.clone());
        let sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions));
        let mut create = SessionCreateRequest::new(SessionId::generate());
        create.inference_settings = Some(test_inference_settings());
        let id = sessions.create(create).await?.id().clone();
        let owner = sessions
            .submit_prompt(
                id.clone(),
                "load and read",
                waltr_agent::RunConflictPolicy::ReplaceActive,
            )
            .await?
            .handle;
        let mut seen = BTreeSet::new();
        // Activation, input, skill-bound call, and worker claims remain distinct.
        for stage in 0..4 {
            let snapshot = crate::host_tests::pending(&broker, &id).await;
            let pending = &snapshot.pending[0];
            assert!(
                seen.insert(pending.id.clone()),
                "approval repeated at stage {stage}"
            );
            assert!(matches!(&pending.request, Interaction::Permission { .. }));
            if stage == 3 && final_decision == 3 {
                sessions.interrupt(&id).await?;
                break;
            }
            let response = if stage == 3 && final_decision != 0 {
                crate::permission::PermissionResponse::Deny {
                    message: None,
                    mode: if final_decision == 2 {
                        PermissionDenyMode::CancelWork
                    } else {
                        PermissionDenyMode::ReturnToolResult
                    },
                }
            } else {
                crate::permission::PermissionResponse::AllowOnce
            };
            crate::host_tests::respond(
                &broker,
                &snapshot,
                InteractionResponse::Permission(response),
            )
            .unwrap();
        }
        drop(owner.wait().await);
        let record = store.load(&id).await.unwrap();
        assert!(broker.snapshot(&id).pending.is_empty());
        assert!(
            record
                .snapshot()
                .middleware_state
                .get("permissions")
                .is_none()
        );
        assert!(record.journal.iter().any(|event| matches!(event, waltr_agent::SessionJournalEvent::MiddlewareStateUpdated { namespace, state } if namespace == "host-dispatch" && state["callId"] == "read-outside")));
        let results = record
            .timeline_events()
            .filter_map(|event| match &event.content {
                TimelineContent::ToolResult { result } if result.call_id == "read-outside" => {
                    Some(result)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if final_decision < 2 {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].is_error, final_decision == 1);
        }
        assert_eq!(
            results
                .iter()
                .any(|result| result.content.contains("sandbox-permission-protected")),
            final_decision == 0
        );
        sessions.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn mcp_definition_selection_is_forwarded_to_host_resolver() -> waltr_agent::Result<()> {
    for (servers, expected) in [
        (
            "{ include: [selected] }",
            McpServerSelection::Include(BTreeSet::from(["selected".to_owned()])),
        ),
        (
            "{ exclude: [ignored] }",
            McpServerSelection::Exclude(BTreeSet::from(["ignored".to_owned()])),
        ),
        (
            "{ exclude: [] }",
            McpServerSelection::Exclude(BTreeSet::new()),
        ),
    ] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let resolver_requests = Arc::clone(&requests);
        let provider = Arc::new(ScriptedProvider::from_turns(
            "mcp_selection_provider",
            vec![vec![CompletionEvent::Finished]],
        ));
        let root = tempfile::tempdir()?.keep();
        let sandbox = TestSandbox::new(&root).await;
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("credentials.json")),
            crate::host_tests::agent_definitions::mcp_agent_with_servers(servers),
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
        })
        .with_mcp_connection_resolver(move |request| {
            resolver_requests
                .lock()
                .expect("resolver requests lock")
                .push(request);
            Box::pin(async { Ok(Vec::new()) })
        });
        let broker = InteractionBroker::new();
        let factory =
            factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions));
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = Some(test_inference_settings());
        let session_id = sessions.create(request).await?.id().clone();
        let result = sessions
            .submit_prompt(
                session_id,
                "resolve selection",
                waltr_agent::RunConflictPolicy::ReplaceActive,
            )
            .await;

        if matches!(expected, McpServerSelection::Include(_)) {
            let error = result
                .err()
                .expect("an explicitly selected missing upstream must fail admission");
            assert!(matches!(error, AgentSessionRunError::Runtime(_)));
            assert!(error.to_string().contains("MCP"), "{error}");
        } else {
            result?.handle.wait().await?;
        }
        assert_eq!(
            requests.lock().expect("resolver requests lock").as_slice(),
            &[
                crate::agent_host::runtime_factory::McpConnectionResolveRequest {
                    servers: expected
                }
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_include_empty_selects_none_without_calling_host_resolver() -> waltr_agent::Result<()> {
    let called = Arc::new(AtomicBool::new(false));
    let resolver_called = Arc::clone(&called);
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_empty_selection_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let root = tempfile::tempdir()?.keep();
    let sandbox = TestSandbox::new(&root).await;
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("credentials.json")),
        crate::host_tests::agent_definitions::mcp_agent_with_servers("{ include: [] }"),
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), provider.clone())))
    .with_mcp_connection_resolver(move |_| {
        resolver_called.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(Vec::new()) })
    });
    let broker = InteractionBroker::new();
    let factory = factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(test_inference_settings());
    let session_id = sessions.create(request).await?.id().clone();
    sessions
        .submit_prompt(
            session_id,
            "no MCP",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?
        .handle
        .wait()
        .await?;

    assert!(!called.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn mcp_agent_loop_discovers_and_calls_stdio_and_streamable_http_direct_tools()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let http_server = HttpMcpTestServer::start().await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "scripted_mcp_provider",
        |request| direct_events(request.tool_results().count()),
    )));

    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "call direct MCP tools",
        vec![
            stdio_runtime_server(&stdio_command),
            http_runtime_server(&http_server),
        ],
    )
    .await?;
    session.require_tool_execution(provider.name())?;
    let context = format!(
        "{}\n{}",
        crate::host_tests::session::session_context(&provider, &session),
        provider.tool_context()
    );
    let direct_results = tool_result_text(&session);

    assert_tool_was_surfaced(&provider, "stdio_test__echo_marker");
    assert_tool_was_surfaced(&provider, "stdio_test__emit_log_marker");
    assert_tool_was_surfaced(&provider, "http_test__echo_marker");
    assert_tool_was_surfaced(&provider, "http_test__emit_log_marker");
    assert_tool_was_not_surfaced(&provider, "search_mcp_tools");
    assert_tool_was_not_surfaced(&provider, "get_mcp_tool_details");
    assert_tool_was_not_surfaced(&provider, "call_mcp_tool");
    assert_tool_call(&session, "stdio_test__echo_marker", &context);
    assert_tool_call(&session, "stdio_test__emit_log_marker", &context);
    assert_tool_call(&session, "http_test__echo_marker", &context);
    assert_tool_call(&session, "http_test__emit_log_marker", &context);
    assert!(
        direct_results.contains("stdio-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        direct_results.contains("http-mcp-tool-called-707"),
        "{context}"
    );
    assert!(
        direct_results.contains("stdio-mcp-log-emitted-818"),
        "{context}"
    );
    assert!(
        direct_results.contains("http-mcp-log-emitted-818"),
        "{context}"
    );
    assert!(session.text.contains("mcp-flow-complete"), "{context}");

    Ok(())
}

#[tokio::test]
async fn mcp_streamable_http_delivers_large_images_to_the_provider() -> waltr_agent::Result<()> {
    let http_server = HttpMcpTestServer::start().await?;
    let saw_image = Arc::new(AtomicBool::new(false));
    let provider_saw_image = saw_image.clone();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "scripted_mcp_image_provider",
        move |request| {
            let Some(result) = request.tool_results().next() else {
                return vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "call-large-image",
                    "http_test__large_image",
                    json!({}),
                )))];
            };
            assert!(matches!(
                &result.content.blocks()[1],
                ToolResultContentBlock::Image(image)
                    if image.data().len() > 64 * 1024
                        && image.media_type() == "image/png"
                        && image.decoded_bytes() == 96 * 1024
            ));
            assert!(!result.text_projection().contains("iVBOR"));
            provider_saw_image.store(true, Ordering::SeqCst);
            vec![
                Ok(CompletionEvent::Delta("image-flow-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));

    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "render image",
        vec![http_runtime_server(&http_server)],
    )
    .await?;

    assert_tool_was_surfaced(&provider, "http_test__large_image");
    assert!(saw_image.load(Ordering::SeqCst));
    assert!(session.text.contains("image-flow-complete"));
    assert!(tool_result_text(&session).contains("image/png"));
    assert!(!tool_result_text(&session).contains("iVBOR"));

    Ok(())
}

#[tokio::test]
async fn mcp_streamable_http_auth_failure_is_reported_as_auth_required() -> waltr_agent::Result<()>
{
    let http_server = HttpMcpTestServer::start_requiring_bearer("valid-token").await?;
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_auth_required_provider",
        vec![vec![CompletionEvent::Finished]],
    ));
    let sessions =
        mcp_sessions_with_connections(provider, vec![http_runtime_server(&http_server)]).await;
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let result = sessions
        .submit_prompt(
            session_id,
            "trigger mcp auth check",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await;
    let error = match result {
        Ok(submission) => submission
            .handle
            .wait()
            .await
            .expect_err("protected MCP server should require auth"),
        Err(error) => error,
    };

    assert!(
        matches!(
            error,
            AgentSessionRunError::Runtime(AgentSessionRuntimeError::Authentication {
                target: waltr_agent::AuthTarget::Host { ref namespace, ref key },
                ..
            }) if namespace == "mcp" && key == "http_test"
        ),
        "unexpected error: {error:?}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_authentication_is_the_first_blocker_before_provider_authentication()
-> waltr_agent::Result<()> {
    let http_server = HttpMcpTestServer::start_requiring_bearer("valid-token").await?;
    let expected_endpoint = http_server.endpoint.clone();
    let resolved_endpoint = expected_endpoint.clone();
    let root = tempfile::tempdir()?.keep();
    let sandbox = TestSandbox::new(&root).await;
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root,
        credentials,
        crate::host_tests::agent_definitions::mcp_agent(true),
    )
    .with_runtime_provider(|_| {
        Err(AgentSessionRuntimeError::Authentication {
            target: waltr_agent::AuthTarget::provider("configured-provider"),
            message: "provider OAuth credential is required".to_owned(),
            reason: waltr_agent::AuthenticationFailureReason::CredentialRequired,
            host_context: None,
        })
    })
    .with_mcp_connection_resolver(move |_| {
        let connection = McpResolvedConnection {
            key: "http_test".to_owned(),
            kind: McpConnectionKind::StreamableHttp,
            command: String::new(),
            url: resolved_endpoint.clone(),
        };
        Box::pin(async move { Ok(vec![connection]) })
    });
    let broker = InteractionBroker::new();
    let factory = factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let error = sessions
        .submit_prompt(
            session_id,
            "trigger both authentication blockers",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await
        .expect_err("MCP authentication should block runtime construction first");

    assert!(matches!(
        error,
        AgentSessionRunError::Runtime(AgentSessionRuntimeError::Authentication {
            target: waltr_agent::AuthTarget::Host { ref namespace, ref key },
            reason: waltr_agent::AuthenticationFailureReason::CredentialRequired,
            host_context: Some(ref context),
            ..
        }) if namespace == "mcp" && key == "http_test" && serde_json::from_value::<crate::auth::McpAuthorizationContext>(context.clone()).is_ok_and(|context| context.endpoint == expected_endpoint && context.http_status == 401)
    ));

    Ok(())
}

#[tokio::test]
async fn mcp_progress_notifications_are_forwarded_to_timeline() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_progress_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_direct_progress",
                "stdio_test__progress_marker",
                json!({ "value": "stdio-mcp-progress-result-919" }),
            ))],
            vec![
                CompletionEvent::Delta("mcp-progress-flow-complete".to_owned()),
                CompletionEvent::Finished,
            ],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    let session = run_session_prompt(
        &sessions,
        "call MCP progress tool",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert!(
        tool_result_text(&session).contains("stdio-mcp-progress-result-919"),
        "{session:#?}"
    );
    let tool_item_id = session
        .timeline
        .iter()
        .find_map(|event| match &event.content {
            TimelineContent::ToolCall { call } if call.id == "call_direct_progress" => {
                Some(event.item_id.clone())
            }
            _ => None,
        })
        .expect("tool call timeline item");
    assert!(
        session.timeline.iter().any(|event| {
            crate::host_tests::session::channel_is(event, CHANNEL_TOOL_PROGRESS_INFO)
                && event.parent_id.as_ref() == Some(&tool_item_id)
                && matches!(
                    &event.content,
                    TimelineContent::TextDelta { delta }
                        if delta.contains("stdio-mcp-progress-marker (1/2)")
                )
        }),
        "expected MCP progress timeline event: {:#?}",
        session.timeline
    );

    Ok(())
}

#[tokio::test]
async fn mcp_trace_propagation_explicit_false_is_preserved() -> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let _trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_trace_disabled_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_trace_disabled_stdio",
                "stdio_test__echo_marker",
                json!({ "value": "trace-disabled-tool-called" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions_with_trace(provider, false).await;

    let session = run_session_prompt(
        &sessions,
        "call MCP tool without trace propagation",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert!(
        !tool_result_text(&session).contains("trace:tool:present"),
        "{session:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn mcp_trace_propagation_reaches_tool_requests() -> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let _trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_trace_tool_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_trace_stdio",
                "stdio_test__echo_marker",
                json!({ "value": "trace-enabled-tool-called" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    let session = run_session_prompt(
        &sessions,
        "call MCP tool with trace propagation",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert!(
        tool_result_text(&session).contains("trace:tool:present"),
        "{session:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn mcp_trace_propagation_reaches_prompt_resource_and_completion_requests()
-> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let _trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_trace_template_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta("trace-template-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let query = "@resource(stdio_test, \"test://items/4\")".to_owned();
    let cursor = query.rfind('"').expect("query contains closing quote");
    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query,
                cursor: Some(cursor),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
            },
        )
        .await?;
    assert!(
        options
            .iter()
            .any(|option| option.label == "trace:completion:present"),
        "expected trace completion marker: {options:#?}"
    );

    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            "/stdio_test:greet_prompt({\"name\":\"Waltr\"})\nRead @resource(stdio_test, \"test://status\") before answering.",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    submission.handle.wait().await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(rendered.contains("trace:prompt:present"), "{rendered}");
    assert!(rendered.contains("trace:resource:present"), "{rendered}");
    Ok(())
}

#[tokio::test]
async fn mcp_trace_tool_request_span_is_child_of_agent_tool_call() -> waltr_agent::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let trace = TestTraceCapture::start();
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_trace_span_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call_trace_span_stdio",
                "stdio_test__echo_marker",
                json!({ "value": "trace-span-tool-called" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = mcp_sessions(provider).await;
    run_session_prompt(
        &sessions,
        "call MCP tool for span assertions",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    let spans = trace.finished_spans();
    let turn = find_span(&spans, "agent.turn", |_| true);
    let tool_call = find_span(&spans, "agent.tool_call", |_| true);
    let mcp_request = find_span(&spans, "mcp.client.request", |span| {
        span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "rpc.method" && attribute.value.as_str() == "tools/call"
        })
    });

    assert_eq!(
        tool_call.span_context.trace_id(),
        turn.span_context.trace_id()
    );
    assert_eq!(
        mcp_request.span_context.trace_id(),
        tool_call.span_context.trace_id()
    );
    assert!(
        is_descendant(&spans, tool_call, turn),
        "expected agent.tool_call under agent.turn: {spans:#?}"
    );
    assert!(
        is_descendant(&spans, mcp_request, tool_call)
            || mcp_request.parent_span_id == tool_call.span_context.span_id()
            || shares_trace_with_parent(&spans, mcp_request, tool_call),
        "expected tools/call MCP request under or within agent.tool_call trace: {spans:#?}"
    );

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_cancellation_cancels_tool_work_and_preserves_stdio_runtime() -> waltr_agent::Result<()>
{
    let temp = tempfile::tempdir()?;
    let marker = temp.path().join("mcp-cancel-marker.txt");
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_cancel_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "mcp-cancel-call",
                "stdio_test__delayed_write_marker",
                json!({
                    "path": marker.display().to_string(),
                    "value": "foo",
                    "delay_ms": 5000_u64,
                }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let store = MemorySessionStore::new();
    let sessions = mcp_sessions_with_store(store.clone(), provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            "run delayed MCP write",
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    let mut stream = sessions.watch_timeline(session_id.clone());

    loop {
        let update = stream.next().await.expect("tool call event");
        let waltr_agent::SessionTimelineEvent::Event(event) = update.event else {
            continue;
        };
        assert!(
            !crate::host_tests::session::channel_is(&event, CHANNEL_TOOL_RESULT),
            "MCP tool finished before cancellation: {event:#?}"
        );
        if crate::host_tests::session::channel_is(&event, CHANNEL_TOOL_CALL) {
            break;
        }
    }

    tokio::select! {
        event = stream.next() => panic!("MCP tool finished before cancellation: {event:#?}"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {}
    }
    sessions.interrupt(&session_id).await?;
    drop(submission.handle.wait().await);

    let remaining = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 100))
        .await?;

    assert!(
        remaining.iter().any(|event| {
            matches!(
                &event.content,
                TimelineContent::ToolResult { result }
                    if crate::host_tests::session::channel_is(event, CHANNEL_TOOL_RESULT)
                        && result.content == "interrupted"
            )
        }),
        "canceled run should persist an interrupted MCP tool result: {remaining:#?}"
    );
    assert!(
        !marker.exists() || fs::read_to_string(&marker)?.is_empty(),
        "canceled MCP tool should not write marker before the server observes cancellation"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(record.snapshot().pending_work.iter().any(|work| {
        work.id == "mcp-cancel-call"
            && matches!(
                work.status,
                PendingWorkStatus::Canceled(waltr_agent::CancelReason::SessionInterrupted)
            )
    }));
    assert!(matches!(
        record.snapshot().status,
        waltr_agent::SessionStatus::Suspended(Suspension::Interrupted { .. })
    ));
    tokio::time::sleep(Duration::from_secs(5)).await;

    let recovery_provider = Arc::new(ScriptedProvider::from_turns(
        "mcp_recovery_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "mcp-recovery-call",
                "stdio_test__echo_marker",
                json!({ "value": "stdio-after-cancel-909" }),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let recovery_sessions = mcp_sessions(recovery_provider.clone()).await;
    let recovery = run_session_prompt(
        &recovery_sessions,
        "recover MCP stdio",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    assert!(
        tool_result_text(&recovery).contains("stdio-after-cancel-909"),
        "{}",
        crate::host_tests::session::session_context(&recovery_provider, &recovery)
    );

    Ok(())
}

#[tokio::test]
async fn mcp_prompt_templates_are_discovered_from_stdio_server() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new("mcp_prompt_search_provider", |_| {
        vec![Ok(CompletionEvent::Finished)]
    }));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "stdio".to_owned(),
                limit: 20,
                kinds: vec![SessionTemplateKind::Prompt],
                target: SessionTemplateSearchTarget::Prompts,
                cursor: None,
            },
        )
        .await?;

    let status = options
        .iter()
        .find(|option| option.template == "/stdio_test:status_prompt")
        .unwrap_or_else(|| panic!("expected status prompt template: {options:#?}"));
    assert_eq!(status.metadata["promptType"], "mcp");
    assert_eq!(status.metadata["server"], "stdio_test");
    let greet = options
        .iter()
        .find(|option| option.template == "/stdio_test:greet_prompt({\"name\":\"\"})")
        .unwrap_or_else(|| panic!("expected greet prompt template: {options:#?}"));
    assert_eq!(greet.kind, SessionTemplateKind::Prompt);
    assert_eq!(greet.metadata["arguments"][0]["name"], "name");
    assert_eq!(greet.metadata["arguments"][0]["required"], true);
    assert!(
        options
            .iter()
            .all(|option| option.metadata["name"] != "review.code"),
        "dotted MCP prompt names cannot be represented by slash templates yet: {options:#?}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_prompt_template_renders_before_provider_request() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_prompt_render_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta("prompt-render-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "/stdio_test:greet_prompt({\"name\":\"Waltr\"})\nUse this prompt.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains("hello Waltr from stdio-mcp-prompt-414"),
        "{rendered}"
    );
    assert!(
        rendered
            .contains("<template-result kind=\"mcp_prompt\" source=\"stdio_test:greet_prompt\">"),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");
    assert!(session.text.contains("prompt-render-complete"));

    Ok(())
}

#[tokio::test]
async fn mcp_prompt_template_rejects_non_string_arguments() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_prompt_argument_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "/stdio_test:greet_prompt({\"name\":123})\nUse this prompt.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains(
            "<template-error>mcp prompt stdio_test:greet_prompt: argument `name` must be a string</template-error>"
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("hello 123 from stdio-mcp-prompt-414"),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");

    Ok(())
}

#[tokio::test]
async fn mcp_resource_templates_are_discovered_from_stdio_server() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new(
        "mcp_resource_search_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    ));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "status".to_owned(),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await?;

    let resource = options
        .iter()
        .find(|option| option.template == "@resource(stdio_test, \"test://status\")")
        .unwrap_or_else(|| panic!("expected status resource template: {options:#?}"));
    assert_eq!(resource.kind, SessionTemplateKind::Resource);
    assert_eq!(resource.metadata["resourceType"], "mcp");
    assert_eq!(resource.metadata["server"], "stdio_test");
    assert_eq!(resource.metadata["uri"], "test://status");
    assert_eq!(resource.metadata["mimeType"], "text/plain");

    Ok(())
}

#[tokio::test]
async fn mcp_uri_resource_templates_are_discovered_from_stdio_server() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new(
        "mcp_uri_resource_search_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    ));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query: "item".to_owned(),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
                cursor: None,
            },
        )
        .await?;

    let resource = options
        .iter()
        .find(|option| option.template == "@resource(stdio_test, \"test://items/{id}\")")
        .unwrap_or_else(|| panic!("expected item resource URI template: {options:#?}"));
    assert_eq!(resource.kind, SessionTemplateKind::Resource);
    assert_eq!(resource.metadata["resourceType"], "mcp_template");
    assert_eq!(resource.metadata["server"], "stdio_test");
    assert_eq!(resource.metadata["uriTemplate"], "test://items/{id}");
    assert_eq!(resource.metadata["mimeType"], "text/plain");

    Ok(())
}

#[tokio::test]
async fn mcp_uri_resource_template_completions_are_search_options() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(ScriptedProvider::new(
        "mcp_uri_resource_completion_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    ));
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(vec![stdio_runtime_server(&stdio_command)]);
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;

    let query = "@resource(stdio_test, \"test://items/4\")".to_owned();
    let cursor = query.rfind('"').expect("query contains closing quote");
    let options = sessions
        .search_templates(
            &session_id,
            SessionTemplateSearchRequest {
                query,
                cursor: Some(cursor),
                limit: 20,
                kinds: vec![SessionTemplateKind::Resource],
                target: SessionTemplateSearchTarget::Resources,
            },
        )
        .await?;

    let completion = options
        .iter()
        .find(|option| option.template == "@resource(stdio_test, \"test://items/42\")")
        .unwrap_or_else(|| panic!("expected item resource URI completion: {options:#?}"));
    assert_eq!(completion.kind, SessionTemplateKind::Resource);
    assert_eq!(completion.label, "42");
    assert_eq!(completion.detail, "test://items/42");
    assert_eq!(
        completion.metadata["resourceType"],
        "mcp_template_completion"
    );
    assert_eq!(completion.metadata["server"], "stdio_test");
    assert_eq!(completion.metadata["uriTemplate"], "test://items/{id}");
    assert_eq!(completion.metadata["argument"], "id");
    assert_eq!(completion.metadata["value"], "42");

    Ok(())
}

#[tokio::test]
async fn mcp_resource_template_renders_before_provider_request() -> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_resource_render_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta(
                    "resource-render-complete".to_owned(),
                )),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "Read @resource(stdio_test, \"test://status\") before answering.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains("stdio-mcp-resource-status-616"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "<template-result kind=\"mcp_resource\" source=\"stdio_test:test://status\">"
        ),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");
    assert!(session.text.contains("resource-render-complete"));

    Ok(())
}

#[tokio::test]
async fn mcp_uri_resource_template_renders_filled_uri_before_provider_request()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_uri_resource_render_provider",
        |_| {
            vec![
                Ok(CompletionEvent::Delta(
                    "resource-template-render-complete".to_owned(),
                )),
                Ok(CompletionEvent::Finished),
            ]
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "Read @resource(stdio_test, \"test://items/42\") before answering.",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    let prompts = provider.request_prompts();
    let rendered = prompts
        .first()
        .unwrap_or_else(|| panic!("expected captured provider prompt"));

    assert!(
        rendered.contains("stdio-mcp-resource-template-item-42-717"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "<template-result kind=\"mcp_resource\" source=\"stdio_test:test://items/42\">"
        ),
        "{rendered}"
    );
    assert!(session.tool_calls.is_empty(), "{session:#?}");
    assert!(session.text.contains("resource-template-render-complete"));

    Ok(())
}

#[tokio::test]
async fn mcp_resources_surface_one_generic_reader_not_per_resource_tools() -> waltr_agent::Result<()>
{
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_resource_tool_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;

    run_session_prompt(
        &sessions,
        "resources use one generic reader",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;

    assert_tool_was_not_surfaced(&provider, "stdio_test__status_resource");
    assert_tool_was_surfaced(&provider, "read_mcp_resource");

    Ok(())
}

#[tokio::test]
async fn mcp_generic_reader_reads_listed_and_filled_template_resources_as_data()
-> waltr_agent::Result<()> {
    let stdio_command = test_stdio_server_command();
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_generic_resource_reader",
        |request| {
            if request.tool_results().next().is_none() {
                ["test://status", "test://items/42"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, uri)| {
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            format!("resource-{index}"),
                            "read_mcp_resource",
                            json!({"server":"stdio_test","uri":uri}),
                        )))
                    })
                    .collect()
            } else {
                let results: Vec<_> = request.tool_results().collect();
                assert_eq!(results.len(), 2);
                for (result, marker) in results.iter().zip([
                    "stdio-mcp-resource-status-616",
                    "stdio-mcp-resource-template-item-42-717",
                ]) {
                    assert!(!result.is_error, "{result:?}");
                    let data: serde_json::Value = serde_json::from_str(&result.content).unwrap();
                    assert!(
                        data["data"]["contents"][0]["text"]
                            .as_str()
                            .unwrap()
                            .contains(marker)
                    );
                    assert_eq!(data["verified_skill_origins"], json!([]));
                }
                vec![Ok(CompletionEvent::Finished)]
            }
        },
    )));
    let sessions = mcp_sessions(provider.clone()).await;
    let session = run_session_prompt(
        &sessions,
        "read resource data",
        vec![stdio_runtime_server(&stdio_command)],
    )
    .await?;
    assert_eq!(session.tool_calls.len(), 2);
    assert_tool_was_surfaced(&provider, "read_mcp_resource");
    Ok(())
}

#[tokio::test]
async fn mcp_server_instructions_are_added_to_system_prompt() -> waltr_agent::Result<()> {
    let server = HttpMcpTestServer::start_with_instructions(
        "Prefer http marker tools for instruction-test requests.",
    )
    .await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;

    run_session_prompt(
        &sessions,
        "check MCP instructions",
        vec![http_runtime_server(&server)],
    )
    .await?;

    let system_prompt = captured_system_prompt(&provider);
    assert!(
        system_prompt.contains("MCP server instructions from `http_test`:"),
        "{system_prompt}"
    );
    assert!(
        system_prompt.contains("Prefer http marker tools for instruction-test requests."),
        "{system_prompt}"
    );

    Ok(())
}

#[tokio::test]
async fn mcp_server_instructions_are_ordered_by_server_key() -> waltr_agent::Result<()> {
    let alpha = HttpMcpTestServer::start_with_instructions("alpha instructions marker").await?;
    let beta = HttpMcpTestServer::start_with_instructions("beta instructions marker").await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_ordered_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions(provider.clone()).await;

    run_session_prompt(
        &sessions,
        "check ordered MCP instructions",
        vec![
            http_runtime_server_with_key("beta", &beta),
            http_runtime_server_with_key("alpha", &alpha),
        ],
    )
    .await?;

    let system_prompt = captured_system_prompt(&provider);
    let alpha_index = system_prompt
        .find("MCP server instructions from `alpha`:")
        .expect("alpha instructions");
    let beta_index = system_prompt
        .find("MCP server instructions from `beta`:")
        .expect("beta instructions");
    assert!(alpha_index < beta_index, "{system_prompt}");

    Ok(())
}

#[tokio::test]
async fn mcp_server_instructions_can_be_disabled_by_definition() -> waltr_agent::Result<()> {
    let server = HttpMcpTestServer::start_with_instructions("disabled instructions marker").await?;
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "mcp_disabled_instructions_provider",
        |_| vec![Ok(CompletionEvent::Finished)],
    )));
    let sessions = mcp_sessions_with_instructions_enabled(provider.clone(), false).await;

    run_session_prompt(
        &sessions,
        "check disabled MCP instructions",
        vec![http_runtime_server(&server)],
    )
    .await?;

    let system_prompt = captured_system_prompt(&provider);
    assert!(
        !system_prompt.contains("disabled instructions marker"),
        "{system_prompt}"
    );

    Ok(())
}

fn stdio_runtime_server(command: &str) -> McpResolvedConnection {
    McpResolvedConnection {
        key: "stdio_test".to_owned(),
        kind: McpConnectionKind::Stdio,
        command: command.to_owned(),
        url: String::new(),
    }
}

fn http_runtime_server(server: &HttpMcpTestServer) -> McpResolvedConnection {
    http_runtime_server_with_key("http_test", server)
}

fn http_runtime_server_with_key(key: &str, server: &HttpMcpTestServer) -> McpResolvedConnection {
    McpResolvedConnection {
        key: key.to_owned(),
        kind: McpConnectionKind::StreamableHttp,
        command: String::new(),
        url: server.endpoint.clone(),
    }
}

struct TestMcpSessions {
    sessions: SessionCoordinator<MemorySessionStore>,
    connections: Arc<Mutex<Vec<McpResolvedConnection>>>,
    credentials: crate::auth::CredentialStore,
    apps: crate::agent_host::mcp::session_apps::ServerMcpApps<MemorySessionStore>,
    broker: InteractionBroker,
    _sandbox: TestSandbox,
}

impl TestMcpSessions {
    fn set_connections(&self, connections: Vec<McpResolvedConnection>) {
        *self.connections.lock().expect("MCP connections lock") = connections;
    }
}

impl Deref for TestMcpSessions {
    type Target = SessionCoordinator<MemorySessionStore>;

    fn deref(&self) -> &Self::Target {
        &self.sessions
    }
}

async fn mcp_sessions(provider: Arc<impl LlmStream + 'static>) -> TestMcpSessions {
    mcp_sessions_with_instructions_enabled(provider, true).await
}

async fn mcp_sessions_with_trace(
    provider: Arc<impl LlmStream + 'static>,
    trace_propagation: bool,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_definition(
        MemorySessionStore::new(),
        provider,
        crate::host_tests::agent_definitions::mcp_agent_with_trace(true, Some(trace_propagation)),
    )
    .await
}

async fn mcp_sessions_with_connections(
    provider: Arc<impl LlmStream + 'static>,
    connections: Vec<McpResolvedConnection>,
) -> TestMcpSessions {
    let sessions = mcp_sessions(provider).await;
    sessions.set_connections(connections);
    sessions
}

async fn mcp_sessions_with_instructions_enabled(
    provider: Arc<impl LlmStream + 'static>,
    instructions_enabled: bool,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_instructions_enabled(
        MemorySessionStore::new(),
        provider,
        instructions_enabled,
    )
    .await
}

async fn mcp_sessions_with_store(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_instructions_enabled(store, provider, true).await
}

async fn mcp_sessions_with_store_and_instructions_enabled(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
    instructions_enabled: bool,
) -> TestMcpSessions {
    mcp_sessions_with_store_and_definition(
        store,
        provider,
        crate::host_tests::agent_definitions::mcp_agent(instructions_enabled),
    )
    .await
}

async fn mcp_sessions_with_store_and_definition(
    store: MemorySessionStore,
    provider: Arc<impl LlmStream + 'static>,
    definition: crate::agent_host::ResolvedAgent,
) -> TestMcpSessions {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let sandbox = TestSandbox::new(&root).await;
    let credentials = FileCredentialStore::new(root.join("credentials.json"));
    let factory =
        AgentSessionRuntimeFactory::new(sandbox.handle(), root, credentials.clone(), definition)
            .with_runtime_provider(move |_| {
                Ok(AgentRuntimeProvider::new(test_model(), provider.clone()))
            });
    let connections = Arc::new(Mutex::new(Vec::new()));
    let resolver_connections = Arc::clone(&connections);
    let factory = factory.with_mcp_connection_resolver(move |_| {
        let resolved = resolver_connections
            .lock()
            .expect("MCP connections lock")
            .clone();
        Box::pin(async move { Ok(resolved) })
    });
    let broker = InteractionBroker::new();
    let factory = factory.with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
    let sessions = SessionCoordinator::new(store);
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let apps =
        crate::agent_host::mcp::session_apps::ServerMcpApps::new(sessions.clone(), factory.clone());
    TestMcpSessions {
        sessions,
        apps,
        broker,
        connections,
        credentials: credentials.into(),
        _sandbox: sandbox,
    }
}

async fn run_session_prompt(
    sessions: &TestMcpSessions,
    prompt: &str,
    connections: Vec<McpResolvedConnection>,
) -> waltr_agent::Result<AgentTestSession> {
    sessions.set_connections(connections);
    run_session_prompt_with_current_connections(sessions, prompt).await
}

async fn run_session_prompt_with_current_connections(
    sessions: &TestMcpSessions,
    prompt: &str,
) -> waltr_agent::Result<AgentTestSession> {
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    sessions
        .set_inference_settings(&session_id, Some(test_inference_settings()))
        .await?;
    let submission = sessions
        .submit_prompt(
            session_id.clone(),
            prompt,
            waltr_agent::RunConflictPolicy::ReplaceActive,
        )
        .await?;
    submission.handle.wait().await?;
    let events = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 500))
        .await?;
    Ok(test_session_from_timeline(events))
}

fn test_inference_settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: OPENAI_TEST_MODEL.to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

fn test_session_from_timeline(timeline: Vec<AgentTimelineEvent>) -> AgentTestSession {
    let mut session = AgentTestSession::default();
    let mut saw_tool_result = false;
    for event in timeline {
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::host_tests::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                if saw_tool_result && !delta.is_empty() {
                    session.saw_text_after_tool_result = true;
                }
                session.text.push_str(delta);
            }
            TimelineContent::ToolCall { call }
                if crate::host_tests::session::channel_is(&event, CHANNEL_TOOL_CALL) =>
            {
                session.tool_calls.push(call.clone());
            }
            TimelineContent::ToolResult { result }
                if crate::host_tests::session::channel_is(&event, CHANNEL_TOOL_RESULT) =>
            {
                session.tool_results.push(result.clone());
                saw_tool_result = true;
                if result.content.starts_with("Tool error:") {
                    session.tool_errors.push(result.clone());
                }
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                session.finished = true;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. } => {}
        }
        session.timeline.push(event);
    }
    session
}

fn captured_system_prompt(provider: &CapturingProvider<impl LlmStream>) -> String {
    provider
        .system_prompts()
        .into_iter()
        .next()
        .flatten()
        .unwrap_or_default()
}

fn direct_events(step: usize) -> Vec<waltr_agent::Result<CompletionEvent>> {
    match step {
        0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_stdio",
            "stdio_test__echo_marker",
            json!({ "value": "stdio-mcp-tool-called-707" }),
        )))],
        1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_stdio_log",
            "stdio_test__emit_log_marker",
            json!({ "message": "stdio-mcp-log-emitted-818" }),
        )))],
        2 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_http",
            "http_test__echo_marker",
            json!({ "value": "http-mcp-tool-called-707" }),
        )))],
        3 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
            "call_direct_http_log",
            "http_test__emit_log_marker",
            json!({ "message": "http-mcp-log-emitted-818" }),
        )))],
        _ => vec![
            Ok(CompletionEvent::Delta("mcp-flow-complete".to_owned())),
            Ok(CompletionEvent::Finished),
        ],
    }
}

fn assert_tool_was_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn assert_tool_was_not_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    let tools = provider.all_tool_names();
    assert!(
        !tools.iter().any(|tool| tool == name),
        "{}",
        provider.tool_context()
    );
}

fn test_stdio_server_command() -> String {
    crate::host_tests::fixture_binary().display().to_string()
}

#[derive(Debug)]
struct HttpMcpTestServer {
    endpoint: String,
    child: Child,
}

impl HttpMcpTestServer {
    async fn start() -> waltr_agent::Result<Self> {
        Self::start_with_optional_instructions(None).await
    }

    async fn start_with_instructions(instructions: &str) -> waltr_agent::Result<Self> {
        Self::start_with_optional_instructions(Some(instructions)).await
    }

    async fn start_requiring_bearer(token: &str) -> waltr_agent::Result<Self> {
        Self::start_with_options(None, Some(token)).await
    }

    async fn start_with_optional_instructions(
        instructions: Option<&str>,
    ) -> waltr_agent::Result<Self> {
        Self::start_with_options(instructions, None).await
    }

    async fn start_with_options(
        instructions: Option<&str>,
        required_bearer: Option<&str>,
    ) -> waltr_agent::Result<Self> {
        Self::start_with_app_read_marker(instructions, required_bearer, None).await
    }

    async fn start_with_app_read_marker(
        instructions: Option<&str>,
        required_bearer: Option<&str>,
        read_marker: Option<&std::path::Path>,
    ) -> waltr_agent::Result<Self> {
        let temp = tempfile::tempdir()?;
        let address_file = temp.path().join("mcp-http-address.txt");
        let mut command = std::process::Command::new(test_stdio_server_command());
        command
            .arg("--http")
            .arg("127.0.0.1:0")
            .arg(&address_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(instructions) = instructions {
            command.env("WALTR_TEST_MCP_INSTRUCTIONS", instructions);
        }
        if let Some(required_bearer) = required_bearer {
            command.env("WALTR_TEST_MCP_REQUIRED_BEARER", required_bearer);
        }
        if let Some(read_marker) = read_marker {
            command.env("WALTR_TEST_MCP_APP_READ_MARKER", read_marker);
        }
        let mut child = command.spawn()?;
        let address = wait_for_http_address(&mut child, &address_file).await?;
        Ok(Self {
            endpoint: format!("http://{address}/mcp"),
            child,
        })
    }
}

impl Drop for HttpMcpTestServer {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

async fn wait_for_http_address(
    child: &mut Child,
    address_file: &std::path::Path,
) -> waltr_agent::Result<String> {
    for _ in 0..100 {
        if let Some(status) = child.try_wait()? {
            return Err(waltr_agent::LlmError::api(
                "mcp",
                format!("MCP HTTP test server exited before startup: {status}"),
            ));
        }
        if let Ok(address) = fs::read_to_string(address_file) {
            let address = address.trim().to_owned();
            if !address.is_empty() {
                return Ok(address);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(waltr_agent::LlmError::api(
        "mcp",
        "MCP HTTP test server did not report an address",
    ))
}

fn tool_result_text(session: &AgentTestSession) -> String {
    session
        .tool_results
        .iter()
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_tool_call(session: &AgentTestSession, name: &str, context: &str) {
    assert!(
        session.tool_calls.iter().any(|call| call.name == name),
        "{context}"
    );
    assert!(
        session
            .tool_results
            .iter()
            .any(|result| result.name == name),
        "{context}"
    );
}

#[derive(Debug)]
struct TestTraceCapture {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
    _guard: DefaultGuard,
}

impl TestTraceCapture {
    fn start() -> Self {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("waltr-agent-mcp-test");
        let subscriber =
            Registry::default().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let guard = tracing::subscriber::set_default(subscriber);

        Self {
            exporter,
            provider,
            _guard: guard,
        }
    }

    fn finished_spans(&self) -> Vec<SpanData> {
        drop(self.provider.force_flush());
        self.exporter.get_finished_spans().unwrap_or_default()
    }
}

fn find_span<'a>(
    spans: &'a [SpanData],
    name: &str,
    predicate: impl Fn(&SpanData) -> bool,
) -> &'a SpanData {
    spans
        .iter()
        .find(|span| span.name == name && predicate(span))
        .unwrap_or_else(|| panic!("expected {name} span: {spans:#?}"))
}

fn is_descendant(spans: &[SpanData], child: &SpanData, ancestor: &SpanData) -> bool {
    let parents = spans
        .iter()
        .map(|span| (span.span_context.span_id(), span.parent_span_id))
        .collect::<HashMap<_, _>>();
    let mut parent = child.parent_span_id;
    while parent != opentelemetry::trace::SpanId::INVALID {
        if parent == ancestor.span_context.span_id() {
            return true;
        }
        let Some(next_parent) = parents.get(&parent) else {
            return false;
        };
        parent = *next_parent;
    }
    false
}

fn shares_trace_with_parent(
    spans: &[SpanData],
    child: &SpanData,
    expected_parent: &SpanData,
) -> bool {
    spans.iter().any(|span| {
        span.span_context.trace_id() == expected_parent.span_context.trace_id()
            && span.span_context.span_id() == child.parent_span_id
    })
}

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}
