use super::*;
use crate::persistence::routine_invocation::{RoutineEventKind, RoutineInvocationStatus};

async fn completion_catalog(manual: bool) -> AgentDefinitionState {
    let catalog = state().await;
    let snapshot = catalog.snapshot();
    let (provider, model) = snapshot
        .configuration()
        .providers
        .iter()
        .find_map(|(provider, config)| {
            config
                .models
                .keys()
                .find(|model| {
                    crate::session::pinned_inference_settings(
                        provider.clone(),
                        (*model).clone(),
                        snapshot.configuration(),
                    )
                    .is_ok()
                })
                .map(|model| (provider.clone(), model.clone()))
        })
        .expect("synthetic inference model");
    let mut definition: Value =
        serde_yaml_ng::from_str(include_str!("../../../resources/routines/title.yaml")).unwrap();
    definition["spec"]["triggers"] = if manual {
        json!([{ "name": "manual", "manual": {"displayName": "Completion", "scope": "session", "inputs": [{"name": "enabled"}, {"name": "target"}]}}])
    } else {
        json!([{ "name": "event", "event": {"type": USER_MESSAGE_CREATED}}])
    };
    if manual {
        definition["spec"]["params"] = json!([{"name": "enabled", "type": "boolean", "default": false}, {"name": "target", "type": "string", "default": "source-session"}]);
    }
    let tasks = definition["spec"]["tasks"].as_array_mut().unwrap();
    tasks[0]["taskSpec"]["sessionTemplate"]
        .as_object_mut()
        .unwrap()
        .remove("inference");
    tasks[0]["taskSpec"]["sessionTemplate"]["model"] =
        json!({"provider": provider, "model": model});
    tasks[0]["taskSpec"]["sessionTemplate"]["userPrompt"]["text"] = json!("fixed prompt");
    let mut skipped = tasks[0].clone();
    skipped["name"] = json!("skipped");
    skipped["when"] = if manual {
        json!([{"input": "$(params.enabled)", "operator": "In", "values": [true]}])
    } else {
        json!([{"input": "$(event.data.userMessageCount)", "operator": "In", "values": ["2"]}])
    };
    tasks[0]["runAfter"] = json!(["skipped"]);
    tasks[1]["taskSpec"]["action"]["sessionMetadata"]["sessionId"] = json!(if manual {
        "$(params.target)"
    } else {
        "$(event.data.session.id)"
    });
    tasks.insert(0, skipped);
    let crate::resource_catalog::AgentDefinitionInspection::Resource(detail) =
        catalog.inspect(Some("routines/title.yaml")).unwrap()
    else {
        panic!("title resource")
    };
    catalog
        .replace(
            "routines/title.yaml",
            serde_yaml_ng::to_string(&definition).unwrap(),
            &detail.persisted_etag,
        )
        .await
        .unwrap();
    catalog
}

#[tokio::test]
async fn manual_and_event_completion_skip_then_model_helper_preserve_failure_fence_and_provenance()
{
    for manual in [false, true] {
        for fail in [false, true] {
            let catalog = completion_catalog(manual).await;
            let persistence = crate::persistence::Persistence::initialize(
                crate::persistence::DatabaseConfig::for_tests().unwrap(),
            )
            .await
            .unwrap();
            let store = persistence.routine_invocation_store();
            let host = Arc::new(FakeHost::with_response(if fail {
                Err("scripted failure")
            } else {
                Ok(Some("complete"))
            }));
            let bus = EventBus::new(8);
            let engine = RoutineEngine::start_with_host_and_store(
                bus.subscribe(),
                catalog,
                host.clone(),
                store.clone(),
            );
            wait_for(|| matches!(&*engine.health().borrow(), EngineHealth::Running)).await;
            let id = if manual {
                engine
                    .manual_handle()
                    .start(ManualRoutineInvocation {
                        request_id: "manual-completion".into(),
                        routine_name: "title".into(),
                        trigger_name: "manual".into(),
                        session_id: SessionId::new("source-session"),
                        inputs: BTreeMap::new(),
                    })
                    .await
                    .unwrap()
            } else {
                EventSink::send(&bus, event("event-completion", false)).unwrap();
                String::new()
            };
            let detail = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let summaries = store.list(None, 10, &BTreeMap::new()).await.unwrap();
                    if let Some(summary) = summaries.iter().find(|s| {
                        s.routine_name == "title"
                            && matches!(
                                s.status,
                                RoutineInvocationStatus::Succeeded
                                    | RoutineInvocationStatus::Failed
                            )
                    }) {
                        break store.detail(&summary.invocation_id).await.unwrap();
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if manual {
                assert_eq!(id, detail.summary.invocation_id);
            }
            assert_eq!(
                detail.summary.status,
                if fail {
                    RoutineInvocationStatus::Failed
                } else {
                    RoutineInvocationStatus::Succeeded
                }
            );
            assert!(
                detail
                    .events
                    .iter()
                    .any(|e| e.task_name.as_deref() == Some("skipped")
                        && e.kind == RoutineEventKind::TaskSkipped)
            );
            assert!(
                detail
                    .events
                    .iter()
                    .any(|e| e.task_name.as_deref() == Some("apply-title")
                        && e.kind
                            == if fail {
                                RoutineEventKind::TaskBlocked
                            } else {
                                RoutineEventKind::TaskSucceeded
                            })
            );
            assert_eq!(host.writes.lock().unwrap().len(), usize::from(!fail));
            let children = host.children.lock().unwrap();
            assert_eq!(children.len(), 1);
            let helper = &children[0];
            let metadata = &helper.metadata_values;
            assert_eq!(
                metadata["waltr.routine.invocation_id"],
                detail.summary.invocation_id
            );
            assert_ne!(
                metadata["waltr.routine.execution_id"],
                metadata["waltr.routine.invocation_id"]
            );
            let execution_id = metadata["waltr.routine.execution_id"].as_str().unwrap();
            assert_eq!(
                helper.id.as_str(),
                format!(
                    "routine_{}",
                    engine::short_digest(&format!("{execution_id}\0generate-title"))
                )
            );
            assert_eq!(metadata["waltr.routine.name"], "title");
            assert_eq!(
                metadata["waltr.routine.revision"],
                detail.summary.routine_revision
            );
            assert_eq!(metadata["waltr.routine.task"], "generate-title");
            assert_eq!(
                metadata["waltr.routine.trigger"],
                if manual { "manual" } else { "event" }
            );
            assert!(!metadata.contains_key("waltr.routine.source_session_id"));
            assert!(
                matches!(&helper.start, waltr_agent::SessionStart::Fresh { messages } if messages.len() == 1)
            );
            drop(children);
            engine.shutdown().await;
        }
    }
}
