//! Generic provider, fake-tool, completion, and durable-session coverage.
use crate::scripted_provider::ScriptedProvider;
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::*;

impl LlmStream for MetadataEmittingProvider {
    fn name(&self) -> &'static str {
        "metadata_provider"
    }
    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<serde_json::Value> {
        Some(json!({"wire":"projected","prompt":request.prompt}))
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let mut count = self.requests.lock().unwrap();
        let events = match *count {
            0 => vec![
                Ok(CompletionEvent::Delta("first".into())),
                Ok(CompletionEvent::ProviderMetadata(expected_turn_metadata())),
                Ok(CompletionEvent::Finished),
            ],
            1 => {
                assert_eq!(
                    request.turns.first().and_then(|t| t.metadata.as_ref()),
                    Some(&expected_turn_metadata())
                );
                vec![
                    Ok(CompletionEvent::Delta("second".into())),
                    Ok(CompletionEvent::Finished),
                ]
            }
            turn => panic!("unexpected request {turn}"),
        };
        *count += 1;
        drop(count);
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
    }
}
impl LlmStream for ForeignMetadataProvider {
    fn name(&self) -> &'static str {
        "other_provider"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        assert_eq!(
            request.turns.first().and_then(|t| t.metadata.as_ref()),
            Some(&expected_turn_metadata())
        );
        Box::pin(async {
            Ok(Box::pin(stream::iter([Ok(CompletionEvent::Finished)])) as BoxCompletionStream)
        })
    }
}
impl LlmStream for RequestOpenRetryProvider {
    fn name(&self) -> &'static str {
        "request-open-retry"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut count = self.requests.lock().unwrap();
            let request = *count;
            *count += 1;
            drop(count);
            if request == 0 {
                return Err(retryable_api_error("request rate limited"));
            }
            Ok(Box::pin(stream::iter([
                Ok(CompletionEvent::Delta("request retry recovered".into())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}
impl LlmStream for AlwaysFailingProvider {
    fn name(&self) -> &'static str {
        "always-failing"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            *self.requests.lock().unwrap() += 1;
            Err(retryable_api_error("rate limited until exhausted"))
        })
    }
}
impl LlmStream for ChronologyProvider {
    fn name(&self) -> &'static str {
        "chronology_provider"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let sequence = request
            .replay_items()
            .into_iter()
            .filter_map(|item| match item {
                CompletionReplayItem::Prompt(prompt) => Some(format!("user:{prompt}")),
                CompletionReplayItem::Turn(TurnItem::AssistantText(text)) => {
                    Some(format!("assistant:{text}"))
                }
                _ => None,
            })
            .collect();
        self.replay_sequences.lock().unwrap().push(sequence);
        let mut count = self.requests.lock().unwrap();
        let text = match *count {
            0 => "first answer",
            1 => "second answer",
            n => panic!("unexpected request {n}"),
        };
        *count += 1;
        drop(count);
        Box::pin(async move {
            Ok(Box::pin(stream::iter([
                Ok(CompletionEvent::Delta(text.into())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}
impl LlmStream for PendingSupersedeProvider {
    fn name(&self) -> &'static str {
        "pending_supersede_provider"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        self.replay_sequences.lock().unwrap().push(
            request
                .replay_items()
                .into_iter()
                .filter_map(|item| match item {
                    CompletionReplayItem::Prompt(prompt) => Some(format!("user:{prompt}")),
                    _ => None,
                })
                .collect(),
        );
        let mut count = self.requests.lock().unwrap();
        let request = *count;
        *count += 1;
        drop(count);
        Box::pin(async move {
            match request {
                0 => Ok(Box::pin(
                    stream::iter([Ok(CompletionEvent::Delta("partial".into()))])
                        .chain(stream::pending()),
                ) as BoxCompletionStream),
                1 => {
                    Ok(Box::pin(stream::iter([Ok(CompletionEvent::Finished)]))
                        as BoxCompletionStream)
                }
                n => panic!("unexpected request {n}"),
            }
        })
    }
}
impl LlmStream for ReplayThinkingProvider {
    fn name(&self) -> &'static str {
        "replay_thinking_provider"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let events = if *calls == 1 {
                vec![
                    Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: "reasoning".into(),
                        payload: json!({"type":"reasoning","id":"rs_1"}),
                    })),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "call_1",
                        "lookup",
                        json!({}),
                    ))),
                ]
            } else {
                *self.saw_replayed_thinking.lock().unwrap() =
                    request.thinking_blocks().count() == 1;
                vec![
                    Ok(CompletionEvent::Delta("done".into())),
                    Ok(CompletionEvent::Finished),
                ]
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}
impl LlmStream for ReplayAssistantTextProvider {
    fn name(&self) -> &'static str {
        "replay_assistant_text_provider"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let events = if *calls == 1 {
                vec![
                    Ok(CompletionEvent::Delta("partial ".into())),
                    Ok(CompletionEvent::Delta("answer".into())),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "call_1",
                        "lookup",
                        json!({}),
                    ))),
                ]
            } else {
                *self.saw_replayed_assistant_text.lock().unwrap() =
                    request.assistant_texts().collect::<Vec<_>>() == ["partial answer"];
                vec![Ok(CompletionEvent::Finished)]
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

#[tokio::test]
async fn provider_projection_and_turn_metadata_are_journaled() {
    let agent =
        AgentRuntime::builder(test_model(), Arc::new(MetadataEmittingProvider::default())).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "hello")
            .stream(),
    )
    .await;

    let record = store.load(&session_id).await.expect("stored session");
    let request = record
        .provider_requests()
        .next()
        .expect("journaled request record");
    assert_eq!(
        request.projected_request,
        json!({ "wire": "projected", "prompt": "hello" }),
        "projected request must be the provider wire payload"
    );
    assert_ne!(request.projected_request, request.canonical_request);
    assert_eq!(
        record
            .snapshot()
            .turns
            .first()
            .and_then(|turn| turn.metadata.clone()),
        Some(expected_turn_metadata())
    );
}

#[tokio::test]
async fn provider_turn_metadata_is_forwarded_without_agent_filtering() {
    let agent =
        AgentRuntime::builder(test_model(), Arc::new(MetadataEmittingProvider::default())).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "first prompt")
            .stream(),
    )
    .await;
    drain(
        sessions
            .prompt(&agent, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;
    let foreign_agent =
        AgentRuntime::builder(test_model(), Arc::new(ForeignMetadataProvider)).build();
    drain(
        sessions
            .prompt(&foreign_agent, session_id, "third prompt")
            .stream(),
    )
    .await;
}

#[tokio::test]
async fn provider_request_open_failure_retries_from_checkpoint() {
    let provider = Arc::new(RequestOpenRetryProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_provider_retry_config(test_retry_config(3))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), "retry request")
            .stream(),
    )
    .await;

    assert_eq!(provider.requests(), 2);
    assert_eq!(assistant_text(&events), "request retry recovered");
    assert!(events.iter().any(|event| {
        event.kind == TimelineEventKind::SessionStatusChanged
            && event.status == Some(TimelineStatus::RetryingProvider)
    }));
    let record = store.load(&session_id).await.expect("stored session");
    assert_eq!(record.provider_requests().count(), 1);
}

#[tokio::test]
async fn provider_stream_failure_rolls_back_partial_output_before_retry() {
    let requests = Mutex::new(0);
    let provider = Arc::new(ScriptedProvider::new("retry-stream", move |_| {
        let mut requests = requests.lock().expect("request count");
        let request = *requests;
        *requests += 1;
        drop(requests);
        match request {
            0 => vec![
                Ok(CompletionEvent::Delta("partial".to_owned())),
                Err(retryable_api_error("stream rate limited")),
            ],
            1 => vec![
                Ok(CompletionEvent::Delta("final".to_owned())),
                Ok(CompletionEvent::Finished),
            ],
            _ => panic!("unexpected provider request {request}"),
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_provider_retry_config(test_retry_config(3))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = collect_events(
        sessions
            .prompt(&agent, session_id.clone(), "retry stream")
            .stream(),
    )
    .await;

    assert_eq!(assistant_text(&events), "partialfinal");
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert_eq!(snapshot.assistant_texts().collect::<Vec<_>>(), ["final"]);
    assert!(matches!(snapshot.status, SessionStatus::Finished));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn copied_context_is_replayed_once_across_retry_and_next_turn() {
    let sequences = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(0));
    let provider = Arc::new(ScriptedProvider::new("copy-retry", {
        let sequences = Arc::clone(&sequences);
        let requests = Arc::clone(&requests);
        move |request| {
            sequences
                .lock()
                .expect("sequence lock")
                .push(copy_replay_sequence(request));
            let mut requests = requests.lock().expect("request lock");
            let request = *requests;
            *requests += 1;
            drop(requests);
            match request {
                0 => vec![Err(retryable_api_error("retry copied request"))],
                1 => vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "missing-call",
                        "missing",
                        json!({}),
                    ))),
                    Ok(CompletionEvent::Finished),
                ],
                2 => vec![
                    Ok(CompletionEvent::Delta("done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ],
                request => panic!("unexpected provider request {request}"),
            }
        }
    }));
    let agent = AgentRuntime::builder(test_model(), provider)
        .with_provider_retry_config(test_retry_config(3))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let source_id = SessionId::new("copy-source");
    let mut source_request = SessionCreateRequest::new(source_id.clone());
    source_request.start = SessionStart::Fresh {
        messages: vec![SessionStartMessage::user("source prompt").unwrap()],
    };
    let source = store.create(source_request).await.unwrap();
    store
        .append_timeline(
            &source_id,
            source.execution_token().unwrap(),
            vec![PendingTimelineEvent::assistant_text_snapshot(
                source_id.clone(),
                0,
                PendingTimelineItemId::New,
                "source answer",
            )],
        )
        .await
        .unwrap();
    store
        .append_journal(
            &source_id,
            store
                .load(&source_id)
                .await
                .unwrap()
                .execution_token()
                .unwrap(),
            vec![waltr_agent::SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .unwrap();
    let source = store.load(&source_id).await.unwrap();
    let session_id = SessionId::new("copied-replay");
    let mut create = SessionCreateRequest::new(session_id.clone());
    create.start = SessionStart::Copy {
        copy: SessionCopy::from_record(&source, waltr_agent::TimelineCursor::new(2)).unwrap(),
        messages: vec![SessionStartMessage::user("destination prompt").unwrap()],
    };

    let created = store.create(create).await.expect("copied session");

    assert_eq!(created.revision, waltr_agent::Revision::ZERO);
    assert_eq!(created.timeline_cursor_high_water().get(), 3);
    assert_eq!(
        created.snapshot().prompts,
        ["source prompt", "destination prompt"]
    );
    assert_eq!(created.snapshot().prompt_turns[1].turn, 1);

    drain(
        sessions
            .resume(&agent, session_id.clone(), ResumeInput::Continue)
            .stream(),
    )
    .await;

    let sequences = sequences.lock().expect("sequence lock").clone();
    assert_eq!(sequences.len(), 3);
    for sequence in &sequences {
        assert_eq!(
            sequence
                .iter()
                .filter(|item| item.as_str() == "user:source prompt")
                .count(),
            1
        );
        assert_eq!(
            sequence
                .iter()
                .filter(|item| item.as_str() == "assistant:source answer")
                .count(),
            1
        );
        assert_eq!(
            sequence
                .iter()
                .filter(|item| item.as_str() == "user:destination prompt")
                .count(),
            1
        );
    }
    assert_eq!(sequences[0], sequences[1]);
    assert!(
        sequences[2]
            .iter()
            .any(|item| item == "tool-call:missing-call")
    );
    assert!(
        sequences[2]
            .iter()
            .any(|item| item == "tool-result:missing-call")
    );
    let snapshot = store.load(&session_id).await.unwrap().snapshot();
    assert_eq!(snapshot.prompt_turns[0].prompt, "source prompt");
    assert_eq!(snapshot.prompt_turns[1].prompt, "destination prompt");
    assert_eq!(
        snapshot.turns[0].assistant_texts().collect::<Vec<_>>(),
        ["source answer"]
    );
}

#[tokio::test]
async fn provider_retry_exhaustion_records_failed_status() {
    let provider = Arc::new(AlwaysFailingProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_provider_retry_config(test_retry_config(2))
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "retry exhausted")
        .stream();
    let mut final_error = None;
    while let Some(event) = stream.next().await {
        if let Err(error) = event {
            final_error = Some(error);
        }
    }

    assert!(final_error.is_some());
    assert_eq!(provider.requests(), 2);
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert!(matches!(snapshot.status, SessionStatus::Failed { .. }));
}

#[tokio::test]
async fn durable_multi_prompt_request_replays_user_prompts_in_order() {
    let provider = Arc::new(ChronologyProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone()).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "first prompt")
            .stream(),
    )
    .await;
    drain(
        sessions
            .prompt(&agent, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;

    assert_eq!(
        provider.replay_sequences(),
        vec![
            vec!["user:first prompt".to_owned()],
            vec![
                "user:first prompt".to_owned(),
                "assistant:first answer".to_owned(),
                "user:second prompt".to_owned(),
            ],
        ]
    );
    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert_eq!(
        snapshot
            .prompt_turns
            .iter()
            .map(|prompt| (prompt.turn, prompt.prompt.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "first prompt"), (1, "second prompt")]
    );
}

#[tokio::test]
async fn superseded_prompt_closes_turn_and_keeps_distinct_user_timeline_items() {
    let provider = Arc::new(PendingSupersedeProvider::default());
    let agent = AgentRuntime::builder(test_model(), provider.clone()).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let mut first = sessions
        .prompt(&agent, session_id.clone(), "first prompt")
        .stream();
    while provider.replay_sequences().is_empty() {
        first
            .next()
            .await
            .expect("first prompt event")
            .expect("first prompt event");
    }

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "second prompt")
            .stream(),
    )
    .await;
    drop(first);

    assert_eq!(
        provider.replay_sequences(),
        vec![
            vec!["user:first prompt".to_owned()],
            vec![
                "user:first prompt".to_owned(),
                "user:second prompt".to_owned(),
            ],
        ]
    );

    let record = store.load(&session_id).await.expect("stored session");
    let snapshot = record.snapshot();
    assert_eq!(
        snapshot
            .prompt_turns
            .iter()
            .map(|prompt| (prompt.turn, prompt.prompt.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "first prompt"), (1, "second prompt")]
    );

    let user_events = store
        .timeline(&session_id, TimelineQuery::forward(None, 20))
        .await
        .expect("timeline events")
        .into_iter()
        .filter(|event| crate::session::channel_is(event, CHANNEL_USER_TEXT))
        .collect::<Vec<_>>();
    assert_eq!(user_events.len(), 2);
    assert_eq!(user_events[0].turn, 0);
    assert_eq!(user_events[1].turn, 1);
    assert_ne!(user_events[0].item_id, user_events[1].item_id);
    assert!(matches!(
        &user_events[0].content,
        TimelineContent::TextSnapshot { text } if text == "first prompt"
    ));
    assert!(matches!(
        &user_events[1].content,
        TimelineContent::TextSnapshot { text } if text == "second prompt"
    ));
}

#[tokio::test]
async fn durable_session_timeline_ranges_preserve_cursor_order() {
    let agent =
        AgentRuntime::builder(test_model(), Arc::new(MetadataEmittingProvider::default())).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    drain(
        sessions
            .prompt(&agent, session_id.clone(), "range prompt")
            .stream(),
    )
    .await;

    let all = store
        .timeline(&session_id, TimelineQuery::forward(None, 10))
        .await
        .expect("timeline events");
    assert!(
        all.len() >= 2,
        "expected content and finished status events"
    );
    assert!(
        all.windows(2)
            .all(|events| events[0].cursor < events[1].cursor)
    );
    assert!(matches!(
        &all[0].content,
        TimelineContent::TextSnapshot { text } if text == "range prompt"
    ));
    let first_assistant = all
        .iter()
        .find(|event| crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT))
        .expect("assistant event");
    assert!(matches!(
        &first_assistant.content,
        TimelineContent::TextDelta { delta } if delta == "first"
    ));
    assert_eq!(
        all.last().map(|event| (event.kind, event.status)),
        Some((
            TimelineEventKind::SessionStatusChanged,
            Some(TimelineStatus::Finished)
        ))
    );

    let after_first = store
        .timeline(&session_id, TimelineQuery::forward(Some(all[0].cursor), 10))
        .await
        .expect("timeline after first cursor");
    assert_eq!(after_first, all[1..].to_vec());

    let before_last = store
        .timeline(
            &session_id,
            TimelineQuery::backward(all.last().map(|event| event.cursor), 10),
        )
        .await
        .expect("timeline before last cursor");
    assert_eq!(before_last, all[..all.len() - 1].to_vec());

    let limited = store
        .timeline(&session_id, TimelineQuery::forward(None, 1))
        .await
        .expect("limited timeline range");
    assert_eq!(limited, vec![all[0].clone()]);
}

#[tokio::test]
async fn assistant_and_thinking_deltas_use_stable_items_and_reconstruct_text() {
    let provider = ScriptedProvider::from_turns(
        "streaming_text_provider",
        vec![vec![
            CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "think-1 ".to_owned(),
            }),
            CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "think-2".to_owned(),
            }),
            CompletionEvent::Delta("answer-1 ".to_owned()),
            CompletionEvent::Delta("answer-2".to_owned()),
            CompletionEvent::Finished,
        ]],
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider)).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;

    let events = sessions
        .prompt(&agent, session_id.clone(), "stream text")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    let assistant_events = events
        .iter()
        .filter(|event| crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT))
        .collect::<Vec<_>>();
    let thinking_events = events
        .iter()
        .filter(|event| crate::session::channel_is(event, CHANNEL_THINKING))
        .collect::<Vec<_>>();

    assert_eq!(assistant_events.len(), 3);
    assert_eq!(thinking_events.len(), 2);
    assert!(
        assistant_events
            .iter()
            .all(|event| event.item_id == assistant_events[0].item_id)
    );
    assert!(
        thinking_events
            .iter()
            .all(|event| event.item_id == thinking_events[0].item_id)
    );
    assert_eq!(assistant_events[0].kind, TimelineEventKind::ContentDelta);
    assert_eq!(assistant_events[1].kind, TimelineEventKind::ContentDelta);
    assert_eq!(assistant_events[2].kind, TimelineEventKind::ItemFinished);
    assert!(matches!(
        &assistant_events[2].content,
        TimelineContent::TextSnapshot { text } if text == "answer-1 answer-2"
    ));
    assert!(
        thinking_events
            .iter()
            .all(|event| event.kind == TimelineEventKind::ContentDelta)
    );

    let assistant_text = store
        .timeline_item_text(&session_id, &assistant_events[0].item_id)
        .await
        .expect("assistant item text");
    let thinking_text = store
        .timeline_item_text(&session_id, &thinking_events[0].item_id)
        .await
        .expect("thinking item text");

    assert_eq!(assistant_text.text, "answer-1 answer-2");
    assert_eq!(thinking_text.text, "think-1 think-2");
}

#[tokio::test]
async fn tool_progress_streams_between_call_preview_and_result() {
    let provider = ScriptedProvider::from_turns(
        "progress_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "progress-call",
                "progress_tool",
                json!({}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    );
    let tool = AgentTool::new_async(
        ToolDefinition::new(
            "progress_tool",
            "Emit progress before finishing.",
            json!({}),
        ),
        |context| {
            Box::pin(async move {
                context.progress().info("progress-started")?;
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(context.result("progress-finished"))
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(tool)
        .build();

    let events = agent
        .prompt("run progress tool")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    let call_index = events
        .iter()
        .position(|event| crate::session::channel_is(event, CHANNEL_TOOL_CALL))
        .expect("tool-call preview");
    let progress_index = events
        .iter()
        .position(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_PROGRESS_INFO)
                && matches!(&event.content, TimelineContent::TextDelta { delta } if delta == "progress-started")
        })
        .expect("tool progress");
    let result_index = events
        .iter()
        .position(|event| crate::session::channel_is(event, CHANNEL_TOOL_RESULT))
        .expect("tool result");

    assert!(call_index < progress_index);
    assert!(progress_index < result_index);
    assert_eq!(
        events[progress_index].parent_id,
        Some(events[call_index].item_id.clone())
    );
    assert_eq!(
        events[result_index].parent_id,
        Some(events[call_index].item_id.clone())
    );
    assert!(events[call_index].cursor < events[progress_index].cursor);
    assert!(events[progress_index].cursor < events[result_index].cursor);
}

#[tokio::test]
async fn provider_turn_can_execute_multiple_tool_calls() {
    let requests = Arc::new(Mutex::new(0usize));
    let provider = ScriptedProvider::new("multi_tool_provider", {
        let requests = requests.clone();
        move |request| {
            let mut count = requests.lock().expect("request count");
            let index = *count;
            *count += 1;
            drop(count);

            match index {
                0 => vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "multi-call-1",
                        "multi_tool",
                        json!({ "value": 1 }),
                    ))),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "multi-call-2",
                        "multi_tool",
                        json!({ "value": 2 }),
                    ))),
                    Ok(CompletionEvent::Finished),
                ],
                1 => {
                    let results = request.tool_results().collect::<Vec<_>>();
                    assert_eq!(results.len(), 2, "provider should receive both results");
                    assert_eq!(results[0].call_id, "multi-call-1");
                    assert_eq!(results[0].content, "result-multi-call-1");
                    assert_eq!(results[1].call_id, "multi-call-2");
                    assert_eq!(results[1].content, "result-multi-call-2");
                    vec![Ok(CompletionEvent::Finished)]
                }
                _ => panic!("unexpected provider request {index}"),
            }
        }
    });
    let tool = AgentTool::new(
        ToolDefinition::new("multi_tool", "Return the call id.", json!({})),
        |context| Ok(context.result(format!("result-{}", context.call().id))),
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(tool)
        .build();

    let events = agent
        .prompt("run multiple tools")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    let call_ids = events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolCall { call }
                if crate::session::channel_is(event, CHANNEL_TOOL_CALL) =>
            {
                Some(call.id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let result_ids = events
        .iter()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolResult { result }
                if crate::session::channel_is(event, CHANNEL_TOOL_RESULT) =>
            {
                Some(result.call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(call_ids, vec!["multi-call-1", "multi-call-2"]);
    assert_eq!(result_ids, vec!["multi-call-1", "multi-call-2"]);
    assert_eq!(*requests.lock().expect("request count"), 2);
}

#[tokio::test]
async fn interrupting_multi_tool_turn_preserves_finished_results() {
    let provider = ScriptedProvider::from_turns(
        "multi_tool_interrupt_provider",
        vec![vec![
            CompletionEvent::ToolCall(ToolCall::new("finished-call", "fast_tool", json!({}))),
            CompletionEvent::ToolCall(ToolCall::new("pending-call", "slow_tool", json!({}))),
            CompletionEvent::Finished,
        ]],
    );
    let fast_tool = AgentTool::new(
        ToolDefinition::new("fast_tool", "Finish immediately.", json!({})),
        |context| Ok(context.result("fast-result")),
    );
    let slow_tool = AgentTool::new_async(
        ToolDefinition::new("slow_tool", "Wait for cancellation.", json!({})),
        |context| {
            Box::pin(async move {
                context.cancellation_token().cancelled().await;
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(context.result("should-not-finish"))
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(fast_tool)
        .with_tool(slow_tool)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = create_session_id(&sessions).await;
    let cancellation = CancellationToken::new();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "run partial batch")
        .with_cancellation_token(cancellation.clone())
        .stream();

    let mut saw_finished_result = false;
    while let Some(event) = stream.next().await {
        let event = event.expect("agent event");
        if let TimelineContent::ToolResult { result } = &event.content
            && result.call_id == "finished-call"
        {
            saw_finished_result = true;
            cancellation.cancel();
            break;
        }
    }
    assert!(
        saw_finished_result,
        "first tool should finish before cancellation"
    );
    drain(stream).await;

    let snapshot = store
        .load(&session_id)
        .await
        .expect("stored session")
        .snapshot();
    assert_eq!(snapshot.turn, 1, "interrupted batch must close turn");
    assert!(snapshot.pending_work.iter().any(|work| {
        work.id == "finished-call"
            && matches!(
                &work.status,
                waltr_agent::PendingWorkStatus::Finished(result)
                    if result.content == "fast-result"
            )
    }));
    assert!(snapshot.pending_work.iter().any(|work| {
        work.id == "pending-call"
            && matches!(
                work.status,
                waltr_agent::PendingWorkStatus::Canceled(
                    waltr_agent::CancelReason::SessionInterrupted
                )
            )
    }));
}

#[tokio::test]
async fn cancellation_token_stops_running_tool_without_finishing_result() {
    let provider = ScriptedProvider::from_turns(
        "cancel_tool_provider",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "cancel-call",
                "cancel_tool",
                json!({}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    );
    let tool = AgentTool::new_async(
        ToolDefinition::new("cancel_tool", "Wait for cancellation.", json!({})),
        |context| {
            Box::pin(async move {
                context.cancellation_token().cancelled().await;
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(context.result("should-not-finish"))
            })
        },
    );
    let agent = AgentRuntime::builder(test_model(), Arc::new(provider))
        .with_tool(tool)
        .build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("created session")
        .id()
        .clone();
    let cancellation = CancellationToken::new();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "run cancel tool")
        .with_cancellation_token(cancellation.clone())
        .stream();

    let first = stream
        .next()
        .await
        .expect("tool call event")
        .expect("tool call event");
    assert!(crate::session::channel_is(&first, CHANNEL_TOOL_CALL));
    cancellation.cancel();
    let remaining = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("agent event"))
        .collect::<Vec<_>>();

    assert!(
        remaining
            .iter()
            .all(|event| !crate::session::channel_is(event, CHANNEL_TOOL_RESULT)),
        "canceled run should not finish the running tool: {remaining:#?}"
    );
    let record = store.load(&session_id).await.expect("stored session");
    assert!(record.snapshot().pending_work.iter().any(|work| {
        work.id == "cancel-call"
            && matches!(
                work.status,
                waltr_agent::PendingWorkStatus::Canceled(
                    waltr_agent::CancelReason::SessionInterrupted
                )
            )
    }));
    assert!(matches!(
        record.snapshot().status,
        waltr_agent::SessionStatus::Suspended(waltr_agent::Suspension::Interrupted { .. })
    ));
}

#[tokio::test]
async fn user_prompt_keeps_non_file_at_mentions_literal() {
    let provider = Arc::new(ScriptedProvider::new("user-template-mentions", |request| {
        assert!(request.prompt.contains("foo@example.com"), "{request:?}");
        assert!(request.prompt.contains("@username"), "{request:?}");
        assert!(!request.prompt.contains("<template-error>"), "{request:?}");
        assert!(!request.prompt.contains("<template-result"), "{request:?}");
        vec![Ok(CompletionEvent::Finished)]
    }));
    let agent = AgentRuntime::builder(test_model(), provider).build();

    drain(
        agent
            .prompt("email foo@example.com and mention @username")
            .stream(),
    )
    .await;
}

#[tokio::test]
async fn agent_replays_thinking_blocks_on_tool_continuation() {
    let provider = Arc::new(ReplayThinkingProvider::default());
    let tool = AgentTool::new(
        ToolDefinition::new(
            "lookup",
            "Look up test data.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |call| Ok(call.result("tool output")),
    );
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_tool(tool)
        .build();

    drain(agent.prompt("use tool").stream()).await;

    assert!(provider.saw_replayed_thinking());
}

#[tokio::test]
async fn agent_replays_assistant_text_on_tool_continuation() {
    let provider = Arc::new(ReplayAssistantTextProvider::default());
    let tool = AgentTool::new(
        ToolDefinition::new(
            "lookup",
            "Look up test data.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        |call| Ok(call.result("tool output")),
    );
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_tool(tool)
        .build();

    drain(agent.prompt("use tool").stream()).await;

    assert!(provider.saw_replayed_assistant_text());
}

fn test_model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
}

async fn create_session_id<S>(
    sessions: &waltr_agent::SessionCoordinator<S>,
) -> waltr_agent::SessionId
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .expect("created session")
        .id()
        .clone()
}

async fn drain<S>(mut stream: S)
where
    S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    while let Some(event) = stream.next().await {
        event.expect("agent event");
    }
}

async fn collect_events<S>(mut stream: S) -> Vec<AgentTimelineEvent>
where
    S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
{
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("agent event"));
    }
    events
}

fn copy_replay_sequence(request: &CompletionRequest<'_>) -> Vec<String> {
    request
        .replay_items()
        .into_iter()
        .map(|item| match item {
            CompletionReplayItem::Prompt(prompt) => format!("user:{prompt}"),
            CompletionReplayItem::Turn(TurnItem::AssistantText(text)) => {
                format!("assistant:{text}")
            }
            CompletionReplayItem::Turn(TurnItem::ToolCall(call)) => {
                format!("tool-call:{}", call.id)
            }
            CompletionReplayItem::Turn(TurnItem::ToolResult(result)) => {
                format!("tool-result:{}", result.call_id)
            }
            CompletionReplayItem::Turn(TurnItem::Thinking(_)) => "thinking".to_owned(),
        })
        .collect()
}

fn assistant_text(events: &[AgentTimelineEvent]) -> String {
    events
        .iter()
        .filter_map(|event| {
            if crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT)
                && let TimelineContent::TextDelta { delta } = &event.content
            {
                return Some(delta.as_str());
            }
            None
        })
        .collect()
}

fn test_retry_config(max_attempts: u32) -> ProviderRetryConfig {
    ProviderRetryConfig {
        max_attempts,
        initial_delay: Duration::ZERO,
        fallback_delay_cap: Duration::ZERO,
        provider_delay_cap: Duration::ZERO,
        ..ProviderRetryConfig::default()
    }
}

fn retryable_api_error(message: &str) -> LlmError {
    LlmError::Api {
        provider: "test",
        message: message.to_owned(),
        status: Some(429),
        headers: Vec::new(),
        code: None,
        body: None,
    }
}

#[derive(Default)]
struct RequestOpenRetryProvider {
    requests: Mutex<u32>,
}

impl RequestOpenRetryProvider {
    fn requests(&self) -> u32 {
        *self.requests.lock().expect("request count")
    }
}

#[derive(Default)]
struct AlwaysFailingProvider {
    requests: Mutex<u32>,
}

impl AlwaysFailingProvider {
    fn requests(&self) -> u32 {
        *self.requests.lock().expect("request count")
    }
}

#[derive(Default)]
struct ChronologyProvider {
    requests: Mutex<usize>,
    replay_sequences: Mutex<Vec<Vec<String>>>,
}

#[derive(Default)]
struct PendingSupersedeProvider {
    requests: Mutex<usize>,
    replay_sequences: Mutex<Vec<Vec<String>>>,
}

impl ChronologyProvider {
    fn replay_sequences(&self) -> Vec<Vec<String>> {
        self.replay_sequences
            .lock()
            .expect("replay sequence lock")
            .clone()
    }
}

impl PendingSupersedeProvider {
    fn replay_sequences(&self) -> Vec<Vec<String>> {
        self.replay_sequences
            .lock()
            .expect("replay sequence lock")
            .clone()
    }
}

fn expected_turn_metadata() -> ProviderTurnMetadata {
    ProviderTurnMetadata {
        provider: "metadata_provider".to_owned(),
        url: "memory://metadata-provider".to_owned(),
        model: "metadata-model".to_owned(),
        usage: Some(TokenUsage {
            input_tokens: Some(11),
            output_tokens: Some(3),
            ..Default::default()
        }),
        cost: None,
        resume: json!({ "response_id": "resp-1" }),
    }
}

#[derive(Default)]
struct MetadataEmittingProvider {
    requests: Mutex<usize>,
}

struct ForeignMetadataProvider;

#[derive(Default)]
struct ReplayThinkingProvider {
    calls: Mutex<usize>,
    saw_replayed_thinking: Mutex<bool>,
}

impl ReplayThinkingProvider {
    fn saw_replayed_thinking(&self) -> bool {
        *self.saw_replayed_thinking.lock().expect("lock")
    }
}

#[derive(Default)]
struct ReplayAssistantTextProvider {
    calls: Mutex<usize>,
    saw_replayed_assistant_text: Mutex<bool>,
}

impl ReplayAssistantTextProvider {
    fn saw_replayed_assistant_text(&self) -> bool {
        *self.saw_replayed_assistant_text.lock().expect("lock")
    }
}
