mod next_turn_tests {
    use super::*;

    struct BoundaryProvider {
        entered: Arc<Notify>,
        release: Arc<tokio::sync::Semaphore>,
        requests: StdMutex<Vec<Vec<String>>>,
        tools: bool,
        retry: bool,
    }
    impl BoundaryProvider {
        fn new(tools: bool) -> Self {
            Self {
                entered: Arc::new(Notify::new()),
                release: Arc::new(tokio::sync::Semaphore::new(0)),
                requests: StdMutex::new(Vec::new()),
                tools,
                retry: false,
            }
        }
    }
    impl LlmStream for BoundaryProvider {
        fn name(&self) -> &'static str {
            "next-turn-test"
        }
        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let items = request
                    .replay_items()
                    .into_iter()
                    .map(|item| match item {
                        CompletionReplayItem::Prompt(prompt) => format!("user:{prompt}"),
                        CompletionReplayItem::Turn(crate::TurnItem::ToolCall(call)) => {
                            format!("call:{}", call.id)
                        }
                        CompletionReplayItem::Turn(crate::TurnItem::ToolResult(result)) => {
                            format!("result:{}", result.call_id)
                        }
                        _ => "assistant".into(),
                    })
                    .collect();
                let first = {
                    let mut requests = self.requests.lock().unwrap();
                    requests.push(items);
                    requests.len() == 1
                };
                if first {
                    let mut events = vec![Ok(CompletionEvent::Delta("initial-response".into()))];
                    if self.tools {
                        for id in ["a", "b"] {
                            events.push(Ok(CompletionEvent::ToolCall(ToolCall::new(
                                id,
                                "missing",
                                json!({}),
                            ))));
                        }
                    }
                    let release = self.release.clone();
                    let entered = self.entered.clone();
                    let retry = self.retry;
                    let tail = stream::once(async move {
                        entered.notify_one();
                        release.acquire().await.unwrap().forget();
                        if retry {
                            Err(crate::LlmError::Api {
                                provider: "test",
                                message: "retry".into(),
                                status: Some(429),
                                headers: vec![("retry-after-ms".into(), "0".into())],
                                code: None,
                                body: None,
                            })
                        } else {
                            Ok(CompletionEvent::Finished)
                        }
                    });
                    Ok(Box::pin(stream::iter(events).chain(tail)) as BoxCompletionStream)
                } else {
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::Delta("final-response".into())),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                }
            })
        }
    }

    async fn session(
        provider: Arc<BoundaryProvider>,
    ) -> (SessionCoordinator<MemorySessionStore>, SessionRecord) {
        let sessions = runtime_sessions(provider).await;
        let record = sessions
            .create(create_request(
                Some(test_inference_settings()),
                Some(crate::agent::session::opaque_test_configuration("test", vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: test}\nspec: {}\n".into()])),
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record;
        (sessions, record)
    }
    async fn prompt(
        sessions: &SessionCoordinator<MemorySessionStore>,
        record: &SessionRecord,
        text: &str,
    ) -> AgentSessionRunSubmission {
        sessions
            .submit_owned_next_turn_prompt(
                record.id.clone(),
                record.metadata.incarnation.as_deref().unwrap(),
                text.into(),
                AgentRunLimits::default(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn next_turn_messages_are_fifo_after_all_tool_results_in_same_run() {
        for tools in [false, true] {
            let provider = Arc::new(BoundaryProvider::new(tools));
            let (sessions, record) = session(provider.clone()).await;
            let first = prompt(&sessions, &record, "initial").await;
            provider.entered.notified().await;
            let one = prompt(&sessions, &record, "one").await;
            let two = prompt(&sessions, &record, "two").await;
            for next in [&one, &two] {
                assert_eq!(next.disposition, AgentSessionRunDisposition::Started);
            }
            assert_eq!(
                sessions
                    .active_state(&record.id)
                    .await
                    .unwrap()
                    .queued_count,
                0
            );
            assert_eq!(
                sessions.active_state(&record.id).await.unwrap().run_id,
                first.run_id
            );
            assert!(
                matches!(sessions.store.load(&record.id).await.unwrap().snapshot().prompts.as_slice(), [initial] if initial == "initial")
            );
            provider.release.add_permits(1);
            for submission in [first, one, two] {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), submission.handle.wait())
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    SessionStatus::Finished
                );
            }
            let requests = provider.requests.lock().unwrap().clone();
            assert_eq!(requests.len(), 2, "no duplicate queued run");
            let mut expected = vec!["user:initial", "assistant"];
            if tools {
                expected.extend(["call:a", "call:b", "result:a", "result:b"]);
            }
            expected.extend(["user:one", "user:two"]);
            assert_eq!(requests[1], expected);
            let finished = sessions.store.load(&record.id).await.unwrap();
            assert_eq!(finished.snapshot().prompts, ["initial", "one", "two"]);
            assert_eq!(
                finished
                    .journal
                    .iter()
                    .filter(|event| matches!(event, SessionJournalEvent::Finished))
                    .count(),
                1
            );
            assert!(sessions.active_state(&record.id).await.is_none());
            // Idle admission starts a new ordinary run with the same durable history.
            prompt(&sessions, &record, "idle")
                .await
                .handle
                .wait()
                .await
                .unwrap();
            assert_eq!(provider.requests.lock().unwrap().len(), 3);
            sessions.shutdown().await;
        }
    }

    #[tokio::test]
    async fn next_turn_limit_is_shared_and_cancellation_joins_all_waiters() {
        for cancel in [false, true] {
            let provider = Arc::new(BoundaryProvider::new(false));
            let (sessions, record) = session(provider.clone()).await;
            let first = sessions
                .submit_prompt_with_limits(
                    record.id.clone(),
                    "initial",
                    RunConflictPolicy::NextTurnIfActive,
                    AgentRunLimits::default().with_max_provider_turns(NonZeroU64::new(1).unwrap()),
                )
                .await
                .unwrap();
            provider.entered.notified().await;
            let next = prompt(&sessions, &record, "one").await;
            if cancel {
                sessions.interrupt(&record.id).await.unwrap();
            } else {
                provider.release.add_permits(1);
            }
            for submission in [first, next] {
                let outcome =
                    tokio::time::timeout(Duration::from_secs(2), submission.handle.wait())
                        .await
                        .unwrap();
                assert!(
                    !matches!(outcome, Ok(outcome) if outcome.status == SessionStatus::Finished)
                );
            }
            assert_eq!(
                provider.requests.lock().unwrap().len(),
                1,
                "accepted prompt must not reset the run budget"
            );
            assert!(sessions.active_state(&record.id).await.is_none());
            if cancel {
                assert!(
                    sessions
                        .submit_prompt(
                            record.id.clone(),
                            "cannot supersede suspension",
                            RunConflictPolicy::NextTurnIfActive
                        )
                        .await
                        .is_err()
                );
            }
            sessions.shutdown().await;
        }
    }

    struct Template(crate::AgentRuntime);
    impl crate::AgentHostRuntimeFactory for Template {
        fn build<'a>(&'a self, _record: &'a SessionRecord) -> crate::HostRuntimeFuture<'a> {
            Box::pin(async { Ok(self.0.clone()) })
        }
    }

    struct Completing {
        entered: Arc<Notify>,
        release: Arc<tokio::sync::Semaphore>,
    }
    impl crate::CompletionObserver for Completing {
        fn observe(
            &self,
            _: crate::CompletionProposal,
            _: CancellationToken,
        ) -> crate::CompletionObserverFuture {
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                entered.notify_one();
                release.acquire().await.unwrap().forget();
                Ok(crate::CompletionDecision::Accept)
            })
        }
    }

    #[tokio::test]
    async fn next_turn_completion_validation_rejects_late_admission() {
        let provider = Arc::new(BoundaryProvider::new(false));
        let (sessions, record) = session(provider.clone()).await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let validation = crate::CompletionValidation::new(
            Arc::new(Completing {
                entered: entered.clone(),
                release: release.clone(),
            }),
            4,
            Duration::from_secs(2),
        )
        .unwrap();
        let runtime = crate::AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
            provider.clone(),
        )
        .build()
        .with_completion_validation(validation);
        let factory = sessions
            .runtime_factory
            .clone()
            .unwrap()
            .with_host_runtime_factory(Arc::new(Template(runtime)));
        let sessions = sessions.with_runtime_factory(factory);
        let first = prompt(&sessions, &record, "initial").await;
        provider.entered.notified().await;
        provider.release.add_permits(1);
        entered.notified().await;
        assert!(
            sessions
                .submit_prompt(
                    record.id.clone(),
                    "late",
                    RunConflictPolicy::NextTurnIfActive
                )
                .await
                .is_err()
        );
        release.add_permits(1);
        assert_eq!(
            first.handle.wait().await.unwrap().status,
            SessionStatus::Finished
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_eq!(
            sessions
                .store
                .load(&record.id)
                .await
                .unwrap()
                .snapshot()
                .prompts,
            ["initial"]
        );
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn next_turn_during_real_tool_waits_for_entire_batch() {
        let provider = Arc::new(BoundaryProvider::new(true));
        let (sessions, record) = session(provider.clone()).await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let tool = crate::AgentTool::new_async(
            crate::ToolDefinition::new("missing", "test", json!({})),
            {
                let entered = entered.clone();
                let release = release.clone();
                move |context| {
                    let entered = entered.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        entered.notify_one();
                        release.acquire().await.unwrap().forget();
                        Ok(context.result("done"))
                    })
                }
            },
        );
        let runtime = crate::AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
            provider.clone(),
        )
        .with_tool(tool)
        .build();
        let factory = sessions
            .runtime_factory
            .clone()
            .unwrap()
            .with_host_runtime_factory(Arc::new(Template(runtime)));
        let sessions = sessions.with_runtime_factory(factory);
        let first = prompt(&sessions, &record, "initial").await;
        provider.entered.notified().await;
        provider.release.add_permits(1);
        entered.notified().await;
        let next = prompt(&sessions, &record, "during-tool").await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_eq!(
            sessions
                .store
                .load(&record.id)
                .await
                .unwrap()
                .snapshot()
                .prompts,
            ["initial"]
        );
        release.add_permits(2);
        first.handle.wait().await.unwrap();
        next.handle.wait().await.unwrap();
        assert_eq!(
            provider.requests.lock().unwrap()[1],
            [
                "user:initial",
                "assistant",
                "call:a",
                "call:b",
                "result:a",
                "result:b",
                "user:during-tool"
            ]
        );
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn next_turn_preserves_retry_request_and_rejects_pause() {
        let mut provider = BoundaryProvider::new(false);
        provider.retry = true;
        let provider = Arc::new(provider);
        let (sessions, record) = session(provider.clone()).await;
        let first = prompt(&sessions, &record, "initial").await;
        provider.entered.notified().await;
        let next = prompt(&sessions, &record, "after-retry").await;
        provider.release.add_permits(1);
        first.handle.wait().await.unwrap();
        next.handle.wait().await.unwrap();
        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0], requests[1],
            "transport retry must keep its canonical request"
        );
        assert_eq!(requests[2].last().unwrap(), "user:after-retry");
        let pause = sessions
            .acquire_pause_reject_busy(&record.id, pause_reason())
            .await
            .unwrap();
        assert!(
            sessions
                .submit_prompt(
                    record.id.clone(),
                    "paused",
                    RunConflictPolicy::NextTurnIfActive
                )
                .await
                .is_err()
        );
        sessions.release_pause(&pause).await.unwrap();
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn next_turn_admission_bound_and_incarnation_are_enforced() {
        let provider = Arc::new(BoundaryProvider::new(false));
        let (sessions, record) = session(provider.clone()).await;
        let first = prompt(&sessions, &record, "initial").await;
        provider.entered.notified().await;
        assert!(
            sessions
                .submit_owned_next_turn_prompt(
                    record.id.clone(),
                    "stale",
                    "stale".into(),
                    AgentRunLimits::default()
                )
                .await
                .is_err()
        );
        let unrelated = runtime_sessions(provider.clone()).await;
        let unrelated = sessions
            .clone()
            .with_runtime_factory(unrelated.runtime_factory.unwrap());
        assert!(
            unrelated
                .submit_prompt(
                    record.id.clone(),
                    "foreign binding",
                    RunConflictPolicy::NextTurnIfActive
                )
                .await
                .is_err()
        );
        let mut accepted = vec![first];
        for index in 0..64 {
            accepted.push(prompt(&sessions, &record, &format!("followup-{index}")).await);
        }
        assert!(
            sessions
                .submit_prompt(
                    record.id.clone(),
                    "overflow",
                    RunConflictPolicy::NextTurnIfActive
                )
                .await
                .is_err()
        );
        sessions.shutdown().await;
        for submission in accepted {
            let outcome = tokio::time::timeout(Duration::from_secs(2), submission.handle.wait())
                .await
                .unwrap();
            assert!(!matches!(outcome, Ok(outcome) if outcome.status == SessionStatus::Finished));
        }
    }

    #[test]
    fn next_turn_completion_gate_atomically_accepts_or_rejects() {
        for _ in 0..128 {
            let input = super::super::super::next_turn::NextTurnInput::default();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let other = input.clone();
            let waiting = barrier.clone();
            let worker = std::thread::spawn(move || {
                let (tx, _rx) = oneshot::channel();
                waiting.wait();
                other.admit("accepted".into(), 1, tx)
            });
            barrier.wait();
            let prompts = input.drain(true);
            let accepted = worker.join().unwrap();
            assert_eq!(accepted, !prompts.is_empty());
            assert!(input.drain(true).is_empty());
            let (tx, _rx) = oneshot::channel();
            assert!(!input.admit("late".into(), 2, tx));
            assert_eq!(input.finish().len(), usize::from(accepted));
        }
    }

    #[tokio::test]
    async fn next_turn_does_not_overtake_the_ordinary_queue() {
        let provider = Arc::new(BoundaryProvider::new(false));
        let (sessions, record) = session(provider.clone()).await;
        let first = prompt(&sessions, &record, "initial").await;
        provider.entered.notified().await;
        let queued = sessions
            .submit_prompt(
                record.id.clone(),
                "ordinary queued",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        assert_eq!(
            queued.disposition,
            AgentSessionRunDisposition::Queued { position: 1 }
        );
        assert!(
            sessions
                .submit_prompt(
                    record.id.clone(),
                    "must not overtake",
                    RunConflictPolicy::NextTurnIfActive
                )
                .await
                .is_err()
        );
        provider.release.add_permits(1);
        first.handle.wait().await.unwrap();
        queued.handle.wait().await.unwrap();
        let record = sessions.store.load(&record.id).await.unwrap();
        assert_eq!(record.snapshot().prompts, ["initial", "ordinary queued"]);
        assert_eq!(
            record
                .journal
                .iter()
                .filter(|event| matches!(event, SessionJournalEvent::Finished))
                .count(),
            2
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        sessions.shutdown().await;
    }
}
