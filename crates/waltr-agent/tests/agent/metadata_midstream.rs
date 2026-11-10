//! Public metadata transactions must not invalidate live execution appends.

use futures_util::{FutureExt, StreamExt, stream};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use waltr_agent::*;

const WAIT: Duration = Duration::from_secs(5);
const OUTPUT: &str = "ordinary assistant response";

struct FirstResponseBarrierProvider {
    requests: AtomicUsize,
    dispatched: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

impl LlmStream for FirstResponseBarrierProvider {
    fn name(&self) -> &'static str {
        "metadata-midstream-barrier"
    }

    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            assert_eq!(self.requests.fetch_add(1, Ordering::SeqCst), 0, "no resend");
            self.dispatched
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let release = self.release.lock().await.take().unwrap();
            tokio::time::timeout(WAIT, release)
                .await
                .expect("first-response barrier release timed out")
                .expect("barrier owner dropped before releasing response");
            Ok(Box::pin(stream::iter([
                Ok(CompletionEvent::Delta(OUTPUT.into())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

enum ConcurrentWrite {
    Metadata,
    ExecutionJournal,
}

async fn run_with_midstream_write(
    write: ConcurrentWrite,
) -> (Vec<waltr_agent::Result<AgentTimelineEvent>>, SessionRecord) {
    let store = MemorySessionStore::new();
    let id = SessionId::new("metadata-midstream");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let (dispatched_tx, dispatched_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let provider = Arc::new(FirstResponseBarrierProvider {
        requests: AtomicUsize::new(0),
        dispatched: Mutex::new(Some(dispatched_tx)),
        release: tokio::sync::Mutex::new(Some(release_rx)),
    });
    let runtime = AgentRuntime::builder(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider.clone(),
    )
    .build();
    let sessions = SessionCoordinator::new(store.clone());
    let mut summaries = sessions.watch_sessions();
    let run = sessions.prompt(&runtime, id.clone(), "respond normally");
    let collect = run.stream().collect::<Vec<_>>();
    let mutate = async {
        tokio::time::timeout(WAIT, dispatched_rx)
            .await
            .expect("provider dispatch barrier timed out")
            .expect("provider ended before dispatch barrier");
        let before = store.load(&id).await.unwrap();
        assert!(matches!(
            before.journal.last(),
            Some(SessionJournalEvent::ProviderRequestRecorded { .. })
        ));
        assert_eq!(
            before
                .journal
                .iter()
                .filter(|event| matches!(
                    event,
                    SessionJournalEvent::ProviderRequestRecorded { .. }
                ))
                .count(),
            1
        );
        assert!(
            !before
                .timeline_events()
                .any(|event| matches!(&event.content, TimelineContent::TextDelta { .. }))
        );
        assert!(before.metadata.incarnation.is_some());
        let committed = match write {
            ConcurrentWrite::Metadata => {
                let outcome = store
                    .mutate(
                        &id,
                        SessionMutation {
                            expected_revision: Some(before.revision),
                            expected_incarnation: before.metadata.incarnation.clone(),
                            predicates: vec![MetadataPredicate {
                                target: MetadataTarget::Session,
                                key: "title".into(),
                                expected: None,
                            }],
                            updates: vec![MetadataUpdate {
                                target: MetadataTarget::Session,
                                key: "title".into(),
                                value: Some(json!("Concurrent title")),
                            }],
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(outcome.record.journal, before.journal);
                assert_eq!(
                    outcome.record.metadata_value("title"),
                    Some(&json!("Concurrent title"))
                );
                outcome.record.revision
            }
            ConcurrentWrite::ExecutionJournal => {
                store
                    .append_journal(
                        &id,
                        before.execution_token().unwrap(),
                        vec![SessionJournalEvent::MiddlewareStateUpdated {
                            namespace: "concurrent-execution".into(),
                            state: json!({}),
                        }],
                    )
                    .await
                    .unwrap()
                    .revision
            }
        };
        assert_eq!(committed, before.revision.next());
        eprintln!(
            "barrier: durable last=ProviderRequestRecorded, no assistant delta; concurrent commit {:?} -> {:?}",
            before.revision, committed
        );
        release_tx.send(()).unwrap();
    };
    // Both futures are owned here; no detached driver or timing-based interleaving.
    let (events, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(collect, mutate)
    })
    .await
    .expect("bounded run and mutation did not finish");
    let record = store.load(&id).await.unwrap();
    let mut last_revision = Revision::ZERO;
    let mut committed_title_seen = false;
    while let Some(Some(update)) = summaries.next().now_or_never() {
        if let SessionListEvent::Upsert(summary) = update.event {
            assert!(summary.revision >= last_revision);
            last_revision = summary.revision;
            if summary.metadata_values.get("title") == Some(&json!("Concurrent title")) {
                committed_title_seen = true;
            }
            if committed_title_seen {
                assert_eq!(
                    summary.metadata_values.get("title"),
                    Some(&json!("Concurrent title"))
                );
            }
        }
    }
    if matches!(write, ConcurrentWrite::Metadata) {
        assert!(
            committed_title_seen,
            "committed title must reach live summaries"
        );
        assert_eq!(last_revision, record.revision);
    }
    let requests = provider.requests.load(Ordering::SeqCst);
    eprintln!(
        "provider requests={requests}; final revision={:?}; last journal={:?}",
        record.revision,
        record.journal.last()
    );
    for error in events.iter().filter_map(|event| event.as_ref().err()) {
        eprintln!("run error: {error:?}");
    }
    assert_eq!(
        requests, 1,
        "a storage conflict must never resend inference"
    );
    assert_eq!(
        record
            .journal
            .iter()
            .filter(|event| matches!(event, SessionJournalEvent::ProviderRequestRecorded { .. }))
            .count(),
        1
    );
    (events, record)
}

#[tokio::test]
async fn public_metadata_transaction_between_dispatch_and_first_delta_preserves_live_run() {
    let (events, record) = run_with_midstream_write(ConcurrentWrite::Metadata).await;
    assert_eq!(
        record.metadata_value("title"),
        Some(&json!("Concurrent title"))
    );
    assert!(
        events.iter().all(Result::is_ok),
        "metadata-only transaction must not reject live append: {events:?}"
    );
    let text = record
        .timeline_events()
        .filter_map(|event| match &event.content {
            TimelineContent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, OUTPUT);
    assert!(
        record
            .journal
            .iter()
            .any(|event| matches!(event, SessionJournalEvent::TurnFinished { turn: 0 }))
    );
    assert!(
        record
            .journal
            .iter()
            .any(|event| matches!(event, SessionJournalEvent::Finished))
    );
    assert!(
        record
            .timeline_events()
            .any(|event| matches!(&event.content,
        TimelineContent::TextSnapshot { text } if text == OUTPUT))
    );
    assert!(record.timeline_events().any(|event| event.kind
        == TimelineEventKind::SessionStatusChanged
        && event.status == Some(TimelineStatus::Finished)));
    assert_eq!(record.snapshot().status, SessionStatus::Finished);
}

#[tokio::test]
async fn concurrent_execution_journal_append_between_dispatch_and_first_delta_rejects_without_resend()
 {
    let (events, record) = run_with_midstream_write(ConcurrentWrite::ExecutionJournal).await;
    let errors = events
        .iter()
        .filter_map(|event| event.as_ref().err())
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].to_string().contains("stale session execution"),
        "expected execution CAS rejection: {:?}",
        errors[0]
    );
    assert!(record.journal.iter().any(|event| matches!(event,
        SessionJournalEvent::MiddlewareStateUpdated { namespace, .. }
            if namespace == "concurrent-execution")));
    assert!(!record.journal.iter().any(|event| matches!(
        event,
        SessionJournalEvent::TurnFinished { .. } | SessionJournalEvent::Finished
    )));
    assert!(
        !record
            .timeline_events()
            .any(|event| matches!(&event.content, TimelineContent::TextDelta { .. }))
    );
}
