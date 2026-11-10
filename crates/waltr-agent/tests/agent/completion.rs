//! Generic provider, fake-tool, completion, and durable-session coverage.
use crate::scripted_provider::ScriptedProvider;
use futures_util::StreamExt;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::*;

fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses)
}

fn inference() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".into(),
            api: ModelApi::OpenAiResponses,
            url: "https://unused.invalid".into(),
            model_id: "test".into(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

async fn session(sessions: &SessionCoordinator<MemorySessionStore>) -> SessionId {
    let mut request = SessionCreateRequest::new(SessionId::generate());
    request.inference_settings = Some(inference());
    request.start = SessionStart::Fresh {
        messages: vec![SessionStartMessage::user("perform task").unwrap()],
    };
    sessions.create(request).await.unwrap().id().clone()
}

struct Observer {
    proposals: Arc<Mutex<Vec<CompletionProposal>>>,
    reject: bool,
}

struct SizeObserver(Arc<AtomicUsize>);

#[test]
fn completion_override_rejects_zero_and_above_hard_ceiling() {
    let validation = CompletionValidation::new(
        Arc::new(SizeObserver(Arc::default())),
        2,
        Duration::from_secs(1),
    )
    .unwrap();
    for limit in [0, 32 * 1024 * 1024 + 1, usize::MAX] {
        assert!(validation.clone().with_encoded_byte_limit(limit).is_err());
    }
    assert!(validation.clone().with_encoded_byte_limit(1).is_ok());
    assert!(validation.with_encoded_byte_limit(32 * 1024 * 1024).is_ok());
    assert_eq!(MAX_COMPLETION_EVIDENCE_BYTES, 262144);
}

#[tokio::test]
async fn host_completion_override_losslessly_delivers_megabyte_and_escaped_capture() {
    let response = "é".repeat(524288);
    let capture = "\u{0000}\"\\".repeat(87381);
    let captured = capture.clone();
    let tool = AgentTool::new(
        ToolDefinition::new("capture", "capture", json!({"type":"object"})),
        move |ctx| Ok(ctx.result(captured.clone())),
    );
    let provider = Arc::new(ScriptedProvider::from_turns(
        "large",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "capture-1",
                "capture",
                json!({"text":capture}),
            ))],
            vec![CompletionEvent::Delta(response.clone())],
        ],
    ));
    let proposals = Arc::new(Mutex::new(Vec::new()));
    let runtime = AgentRuntime::builder(model(), provider)
        .with_tool(tool)
        .build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = session(&sessions).await;
    let validation = CompletionValidation::new(
        Arc::new(Observer {
            proposals: proposals.clone(),
            reject: false,
        }),
        2,
        Duration::from_secs(10),
    )
    .unwrap()
    .with_encoded_byte_limit(32 * 1024 * 1024)
    .unwrap();
    let outcome = sessions
        .start_pinned_helper(id, runtime, Some(validation), AgentRunLimits::default())
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    assert_eq!(outcome.status, SessionStatus::Finished);
    let proposals = proposals.lock().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].response, response);
    assert_eq!(proposals[0].evidence[0].call.arguments["text"], capture);
    assert_eq!(proposals[0].evidence[0].result.content, capture);
}

#[tokio::test]
async fn host_completion_override_exact_encoded_limits_at_both_enforcement_points() {
    let response = "é\u{0000}\"\\".repeat(30000);
    let mut sizes = None;
    for mode in 0..5 {
        let calls = Arc::new(AtomicUsize::new(0));
        let effects = calls.clone();
        let tool = AgentTool::new(
            ToolDefinition::new("effect", "effect", json!({"type":"object"})),
            move |ctx| {
                effects.fetch_add(1, Ordering::SeqCst);
                Ok(ctx.result("\u{0000}".repeat(50000)))
            },
        );
        let provider = Arc::new(ScriptedProvider::from_turns(
            "boundary",
            vec![
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "effect-1",
                    "effect",
                    json!({}),
                ))],
                vec![CompletionEvent::Delta(response.clone())],
            ],
        ));
        let runtime = AgentRuntime::builder(model(), provider)
            .with_tool(tool)
            .build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let proposals = Arc::new(Mutex::new(Vec::new()));
        let limit = match (mode, sizes) {
            (0, _) => 32 * 1024 * 1024,
            (1, Some((_, proposal))) => proposal,
            (2, Some((_, proposal))) => proposal - 1,
            (3, Some((evidence, _))) => evidence,
            (4, Some((evidence, _))) => evidence - 1,
            _ => unreachable!(),
        };
        let validation = CompletionValidation::new(
            Arc::new(Observer {
                proposals: proposals.clone(),
                reject: false,
            }),
            2,
            Duration::from_secs(10),
        )
        .unwrap()
        .with_encoded_byte_limit(limit)
        .unwrap();
        drop(
            sessions
                .start_pinned_helper(
                    id.clone(),
                    runtime.clone(),
                    Some(validation),
                    AgentRunLimits::default(),
                )
                .await
                .unwrap()
                .handle
                .wait()
                .await,
        );
        let record = sessions.store().load(&id).await.unwrap();
        let proposals = proposals.lock().unwrap();
        if mode == 0 {
            sizes = Some((
                serde_json::to_vec(&proposals[0].evidence[0]).unwrap().len(),
                serde_json::to_vec(&proposals[0]).unwrap().len(),
            ));
        }
        assert_eq!(
            record.snapshot().status == SessionStatus::Finished,
            mode < 2,
            "mode={mode}"
        );
        assert_eq!(proposals.len(), usize::from(mode < 2));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let results = record
            .timeline_events()
            .filter(|event| {
                event
                    .channel
                    .as_ref()
                    .is_some_and(|channel| channel.as_str() == "tool_result")
            })
            .count();
        assert_eq!(results, usize::from(mode != 4), "mode={mode}");
        drop(proposals);
        if mode >= 2 {
            let resumed = sessions
                .resume(&runtime, id, ResumeInput::Continue)
                .stream()
                .collect::<Vec<_>>()
                .await;
            assert!(resumed.iter().any(Result::is_err));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
impl CompletionObserver for SizeObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        _: CancellationToken,
    ) -> CompletionObserverFuture {
        self.0.store(
            serde_json::to_vec(&proposal).unwrap().len(),
            Ordering::SeqCst,
        );
        Box::pin(async { Ok(CompletionDecision::Accept) })
    }
}

#[tokio::test]
async fn cumulative_evidence_count_and_encoded_byte_boundaries_fail_without_commit_or_replay() {
    let mut overhead = 0;
    for (calls, payload, accepted) in [
        (1, 0, true),
        (1, usize::MAX, true),
        (1, usize::MAX, false),
        (256, 0, true),
        (257, 0, false),
    ] {
        let payload = if payload == usize::MAX {
            MAX_COMPLETION_EVIDENCE_BYTES - overhead + usize::from(!accepted)
        } else {
            payload
        };
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let tool = AgentTool::new(
            ToolDefinition::new("effect", "effect", json!({"type":"object"})),
            move |ctx| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(ctx.result("x".repeat(payload)))
            },
        );
        let provider = Arc::new(ScriptedProvider::from_turns(
            "evidence-bound",
            vec![
                (0..calls)
                    .map(|index| {
                        CompletionEvent::ToolCall(ToolCall::new(
                            format!("call-{index}"),
                            "effect",
                            json!({}),
                        ))
                    })
                    .collect(),
                vec![CompletionEvent::Delta("candidate".into())],
            ],
        ));
        let runtime = AgentRuntime::builder(model(), provider)
            .with_tool(tool)
            .build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let encoded = Arc::new(AtomicUsize::new(0));
        let validation = CompletionValidation::new(
            Arc::new(SizeObserver(encoded.clone())),
            2,
            Duration::from_secs(10),
        )
        .unwrap();
        let result = sessions
            .start_pinned_helper(
                id.clone(),
                runtime.clone(),
                Some(validation),
                AgentRunLimits::default(),
            )
            .await
            .unwrap()
            .handle
            .wait()
            .await;
        let record = sessions.store().load(&id).await.unwrap();
        assert_eq!(
            record.snapshot().status == SessionStatus::Finished,
            accepted,
            "calls={calls},payload={payload},result={result:?}"
        );
        if calls == 1 && payload == 0 {
            overhead = encoded.load(Ordering::SeqCst);
        }
        if calls == 1 && payload > 0 && accepted {
            assert_eq!(
                encoded.load(Ordering::SeqCst),
                MAX_COMPLETION_EVIDENCE_BYTES
            );
        }
        if !accepted {
            assert_eq!(encoded.load(Ordering::SeqCst), 0);
            assert!(!record.timeline_events().any(|event| {
                event
                    .channel
                    .as_ref()
                    .is_some_and(|channel| channel.as_str() == CHANNEL_ASSISTANT_TEXT)
                    && matches!(&event.content, TimelineContent::TextSnapshot { .. })
            }));
            let before = effects.load(Ordering::SeqCst);
            let resumed = sessions
                .resume(&runtime, id, ResumeInput::Continue)
                .stream()
                .collect::<Vec<_>>()
                .await;
            assert!(resumed.iter().any(Result::is_err));
            assert_eq!(effects.load(Ordering::SeqCst), before);
        }
    }
}

#[tokio::test]
async fn exact_one_turn_rejection_never_gains_a_validation_turn() {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let provider = Arc::new(ScriptedProvider::new("one-turn", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        vec![Ok(CompletionEvent::Delta("candidate".into()))]
    }));
    struct Reject;
    impl CompletionObserver for Reject {
        fn observe(
            &self,
            proposal: CompletionProposal,
            _: CancellationToken,
        ) -> CompletionObserverFuture {
            Box::pin(async move {
                Ok(CompletionDecision::Reject {
                    feedback: CompletionFeedback {
                        proposal_id: proposal.proposal_id,
                        call_id: None,
                        message: "again".into(),
                    },
                })
            })
        }
    }
    let runtime = AgentRuntime::builder(model(), provider).build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = session(&sessions).await;
    let result = sessions
        .start_pinned_helper(
            id,
            runtime,
            Some(CompletionValidation::new(Arc::new(Reject), 1, Duration::from_secs(2)).unwrap()),
            AgentRunLimits::default(),
        )
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(matches!(result.status, SessionStatus::Failed { .. }));
    assert!(result.assistant_text.is_none());
}
impl CompletionObserver for Observer {
    fn observe(
        &self,
        proposal: CompletionProposal,
        _: CancellationToken,
    ) -> CompletionObserverFuture {
        let reject = self.reject && self.proposals.lock().unwrap().is_empty();
        self.proposals.lock().unwrap().push(proposal.clone());
        Box::pin(async move {
            Ok(if reject {
                CompletionDecision::Reject {
                    feedback: CompletionFeedback {
                        proposal_id: proposal.proposal_id,
                        call_id: Some("effect".into()),
                        message: "verify before finishing".into(),
                    },
                }
            } else {
                CompletionDecision::Accept
            })
        })
    }
}

#[tokio::test]
async fn rejection_is_same_helper_durable_feedback_and_original_effect_once() {
    let requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = requests.clone();
    let provider = Arc::new(ScriptedProvider::new("validation", move |request| {
        let turn = observed_requests.fetch_add(1, Ordering::SeqCst);
        let events = match turn {
            0 => vec![CompletionEvent::ToolCall(ToolCall::new(
                "effect",
                "effect",
                json!({}),
            ))],
            1 => vec![CompletionEvent::Delta("candidate".into())],
            2 => {
                assert!(request.prompt.contains("verify before finishing"));
                assert_eq!(
                    request
                        .tool_results()
                        .filter(|result| result.call_id == "effect")
                        .count(),
                    1
                );
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "verify",
                    "verify",
                    json!({}),
                ))]
            }
            3 => vec![CompletionEvent::Delta("accepted".into())],
            _ => panic!("unexpected provider turn"),
        };
        events
            .into_iter()
            .chain([CompletionEvent::Finished])
            .map(Ok)
            .collect()
    }));
    let effects = Arc::new(AtomicUsize::new(0));
    let effect_count = effects.clone();
    let runtime = AgentRuntime::builder(model(), provider)
        .with_tool(AgentTool::new(
            ToolDefinition::new("effect", "effect", json!({"type":"object"})),
            move |context| {
                effect_count.fetch_add(1, Ordering::SeqCst);
                Ok(context.result("original"))
            },
        ))
        .with_tool(AgentTool::new(
            ToolDefinition::new("verify", "verify", json!({"type":"object"})),
            |context| Ok(context.result("verified")),
        ))
        .build();
    let proposals = Arc::new(Mutex::new(Vec::new()));
    let validation = CompletionValidation::new(
        Arc::new(Observer {
            proposals: proposals.clone(),
            reject: true,
        }),
        4,
        Duration::from_secs(3),
    )
    .unwrap();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = session(&sessions).await;
    let outcome = sessions
        .start_pinned_helper(
            id.clone(),
            runtime,
            Some(validation),
            AgentRunLimits::default(),
        )
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    assert_eq!(outcome.assistant_text.as_deref(), Some("accepted"));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let proposals = proposals.lock().unwrap();
    assert_eq!(proposals.len(), 2);
    assert_eq!(proposals[0].session_id, proposals[1].session_id);
    assert_eq!(proposals[0].evidence[0], proposals[1].evidence[0]);
    assert_eq!(proposals[0].evidence[0].result.content, "original");
    let record = store.load(&id).await.unwrap();
    assert!(
        record
            .snapshot()
            .prompts
            .iter()
            .any(|prompt| prompt.contains("verify before finishing"))
    );
    assert_eq!(record.snapshot().status, SessionStatus::Finished);
}

struct InvalidObserver;

impl CompletionObserver for InvalidObserver {
    fn observe(&self, _: CompletionProposal, _: CancellationToken) -> CompletionObserverFuture {
        Box::pin(async { Err(CompletionObserverError::Invalid) })
    }
}

struct RejectObserver;

struct InvalidFeedbackObserver(u8);
impl CompletionObserver for InvalidFeedbackObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        _: CancellationToken,
    ) -> CompletionObserverFuture {
        let mode = self.0;
        Box::pin(async move {
            if mode == 0 {
                return Err(CompletionObserverError::Failed);
            }
            Ok(CompletionDecision::Reject {
                feedback: CompletionFeedback {
                    proposal_id: if mode == 1 {
                        "wrong-proposal".into()
                    } else {
                        proposal.proposal_id
                    },
                    call_id: (mode == 2).then(|| "missing-call".into()),
                    message: if mode == 3 {
                        "x".repeat(MAX_COMPLETION_FEEDBACK_BYTES + 1)
                    } else {
                        "correct".into()
                    },
                },
            })
        })
    }
}

#[tokio::test]
async fn failed_observer_and_malformed_feedback_never_commit_or_continue() {
    for mode in 0..4 {
        let turns = Arc::new(AtomicUsize::new(0));
        let count = turns.clone();
        let runtime = AgentRuntime::builder(
            model(),
            Arc::new(ScriptedProvider::new("invalid-feedback", move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                vec![
                    Ok(CompletionEvent::Delta("candidate".into())),
                    Ok(CompletionEvent::Finished),
                ]
            })),
        )
        .build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let validation = CompletionValidation::new(
            Arc::new(InvalidFeedbackObserver(mode)),
            4,
            Duration::from_secs(1),
        )
        .unwrap();
        let run = sessions
            .start_pinned_helper(
                id.clone(),
                runtime,
                Some(validation),
                AgentRunLimits::default(),
            )
            .await
            .unwrap();
        assert!(run.handle.wait().await.is_err());
        assert_eq!(turns.load(Ordering::SeqCst), 1);
        assert!(matches!(
            sessions.store().load(&id).await.unwrap().snapshot().status,
            SessionStatus::Failed { .. }
        ));
    }
}

impl CompletionObserver for RejectObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        _: CancellationToken,
    ) -> CompletionObserverFuture {
        Box::pin(async move {
            Ok(CompletionDecision::Reject {
                feedback: CompletionFeedback {
                    proposal_id: proposal.proposal_id,
                    call_id: None,
                    message: "not yet".into(),
                },
            })
        })
    }
}

struct PendingObserver;
impl CompletionObserver for PendingObserver {
    fn observe(&self, _: CompletionProposal, _: CancellationToken) -> CompletionObserverFuture {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn permanent_rejection_obeys_total_turn_cap_and_never_commits() {
    let turns = Arc::new(AtomicUsize::new(0));
    let count = turns.clone();
    let provider = Arc::new(ScriptedProvider::new("reject", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        vec![
            Ok(CompletionEvent::Delta("preview".into())),
            Ok(CompletionEvent::Finished),
        ]
    }));
    let runtime = AgentRuntime::builder(model(), provider).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = session(&sessions).await;
    let validation =
        CompletionValidation::new(Arc::new(RejectObserver), 2, Duration::from_secs(2)).unwrap();
    let outcome = sessions
        .start_pinned_helper(
            id.clone(),
            runtime,
            Some(validation),
            AgentRunLimits::default(),
        )
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    assert!(matches!(outcome.status, SessionStatus::Failed { .. }));
    assert!(outcome.assistant_text.is_none());
    assert_eq!(turns.load(Ordering::SeqCst), 2);
    assert!(matches!(
        store.load(&id).await.unwrap().snapshot().status,
        SessionStatus::Failed { .. }
    ));
}

#[tokio::test]
async fn pending_validator_is_bounded_by_deadline_and_cancellation() {
    for cancel in [false, true] {
        let provider = Arc::new(ScriptedProvider::from_turns(
            "pending",
            vec![vec![
                CompletionEvent::Delta("preview".into()),
                CompletionEvent::Finished,
            ]],
        ));
        let runtime = AgentRuntime::builder(model(), provider).build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let owner = SessionExecutionOwner::new(sessions, id);
        let token = CancellationToken::new();
        let validation =
            CompletionValidation::new(Arc::new(PendingObserver), 2, Duration::from_millis(50))
                .unwrap();
        let run = owner.helper(
            runtime,
            validation,
            AgentRunLimits::default(),
            token.clone(),
        );
        let trigger = async {
            if cancel {
                tokio::time::sleep(Duration::from_millis(10)).await;
                token.cancel();
            }
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(run, trigger) })
                .await
                .unwrap();
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn invalid_observer_never_commits_candidate_and_restart_does_not_replay() {
    let provider = Arc::new(ScriptedProvider::from_turns(
        "invalid",
        vec![vec![
            CompletionEvent::Delta("uncommitted".into()),
            CompletionEvent::Finished,
        ]],
    ));
    let runtime = AgentRuntime::builder(model(), provider).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = session(&sessions).await;
    let validation =
        CompletionValidation::new(Arc::new(InvalidObserver), 2, Duration::from_secs(2)).unwrap();
    assert!(
        sessions
            .start_pinned_helper(
                id.clone(),
                runtime.clone(),
                Some(validation),
                AgentRunLimits::default()
            )
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .is_err()
    );
    let snapshot = store.load(&id).await.unwrap().snapshot();
    assert!(matches!(snapshot.status, SessionStatus::Failed { .. }));
    let events = sessions
        .resume(&runtime, id, ResumeInput::Continue)
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(Result::is_err));
}
