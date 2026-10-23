use super::*;
use crate::persistence::routine_invocation::{
    RoutineAdmission, RoutineEventKind, RoutineInvocationStatus, RoutineInvocationStore,
    RoutineTaskDescriptor, RoutineTriggerKind,
};
use crate::session::SessionHost;

pub(super) async fn record(request: &mut Request) -> RoutineInvocationStore {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    store
        .admit(RoutineAdmission {
            invocation_id: request.id.clone(),
            execution_id: request.id.clone(),
            routine_name: request.routine.name().into(),
            routine_revision: request.routine.source_etag().into(),
            trigger_name: request.trigger.clone(),
            trigger_kind: RoutineTriggerKind::Interaction,
            correlation_id: request.id.clone(),
            metadata: BTreeMap::new(),
            source_cursor: None,
            declared_tasks: request
                .routine
                .definition()
                .spec
                .tasks
                .iter()
                .map(|t| RoutineTaskDescriptor {
                    name: t.name.clone(),
                    task_type: if t.task_spec.collect.is_some() {
                        "collect"
                    } else if t.task_spec.transcribe.is_some() {
                        "transcribe"
                    } else {
                        "session_template"
                    }
                    .into(),
                })
                .collect(),
            admitted_at_ms: super::super::super::lifecycle::now_ms(),
        })
        .await
        .unwrap();
    let (wakeups, _) = tokio::sync::broadcast::channel(8);
    request.recorder = Some(super::super::super::lifecycle::LifecycleRecorder::new(
        store.clone(),
        request.id.clone(),
        wakeups,
    ));
    store
}

#[tokio::test]
async fn concurrent_graph_records_one_success_and_helper_reference() {
    let server = fixtures::SpeechServer::start().await;
    let mut request = request(CLEANUP).await;
    Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("speech")
        .unwrap()
        .url = server.endpoint.clone();
    let store = record(&mut request).await;
    let id = request.id.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    release.notify_one();
    let (host, _sandbox) = fixtures::Host::new(release).await;
    let mut interaction = start_sessions(request, host.clone()).unwrap();
    drop(next(&mut interaction.events).await);
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .send(Chunk::Audio(vec![0; 4]))
        .await
        .unwrap();
    interaction
        .inputs
        .get_mut("audio")
        .unwrap()
        .close()
        .await
        .unwrap();
    loop {
        if let Event::Completed(result) = next(&mut interaction.events).await {
            assert!(result.is_ok(), "{result:?}");
            break;
        }
    }
    interaction.join().await;
    let detail = store.detail(&id).await.unwrap();
    assert_eq!(detail.summary.status, RoutineInvocationStatus::Succeeded);
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|e| e.kind == RoutineEventKind::TaskSucceeded)
            .count(),
        3
    );
    assert!(detail.events.iter().any(|e| {
        e.session_references
            .as_ref()
            .is_some_and(|r| r.contains_key("helper"))
    }));
    let helper_id = detail
        .events
        .iter()
        .find_map(|event| event.session_references.as_ref()?.get("helper"))
        .unwrap();
    let helper = host
        .load(&waltr_agent::SessionId::new(helper_id))
        .await
        .unwrap();
    for (key, value) in [
        ("name", "audio-transcribe-cleanup"),
        ("revision", detail.summary.routine_revision.as_str()),
        ("trigger", "dictate"),
        ("task", "cleanup"),
        ("invocation_id", id.as_str()),
        ("execution_id", id.as_str()),
    ] {
        assert_eq!(
            helper.record.metadata_value(&format!("waltr.routine.{key}")),
            Some(&Value::String(value.to_owned()))
        );
    }
    host.shutdown().await;
}

struct Fail;
impl Adapters for Fail {
    fn execute<'a>(&'a self, _: &'a RoutineTask, _: &'a mut TaskContext) -> AdapterFuture<'a> {
        Box::pin(async { Err(Error::new("provider", "scripted failure")) })
    }
}
#[tokio::test]
async fn concurrent_failure_records_tasks_before_atomic_invocation_failure() {
    let mut request = request(CLEANUP).await;
    let store = record(&mut request).await;
    let id = request.id.clone();
    let mut interaction = start(request, Arc::new(Fail)).unwrap();
    loop {
        if let Event::Completed(result) = next(&mut interaction.events).await {
            assert!(
                matches!(
                    result,
                    Err(Error {
                        code: "provider",
                        ..
                    })
                ),
                "{result:?}"
            );
            break;
        }
    }
    interaction.join().await;
    let detail = store.detail(&id).await.unwrap();
    assert_eq!(detail.summary.status, RoutineInvocationStatus::Failed);
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|e| e.kind == RoutineEventKind::InvocationFailed)
            .count(),
        1
    );
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|e| e.kind == RoutineEventKind::TaskFailed)
            .count(),
        1
    );
}
