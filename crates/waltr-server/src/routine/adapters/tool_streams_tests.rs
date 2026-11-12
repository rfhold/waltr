use super::*;
use serde_json::json;
use std::time::Duration;
use waltr_agent::{AgentTool, ToolCall, ToolExecutionContext};

fn selection() -> HelperSelection {
    HelperSelection::Named {
        name: "writer".into(),
    }
}
fn streams(calls: usize, bytes: usize) -> Arc<ToolStreams> {
    ToolStreams::new(BTreeMap::from([("segments".into(), serde_json::from_value(json!({
        "selection":{"kind":"named","name":"writer"}, "tool":"speak", "description":"Queue speech",
        "contract":{"schemaVersion":"waltr.value/v1", "schema":{"type":"object","properties":{"text":{"type":"string","maxLength":256}},"required":["text"]}},
        "maxCalls":calls,"maxBytes":bytes
    })).unwrap())]))
}
fn call(tool: &AgentTool, id: &str, text: &str) -> waltr_agent::ToolFuture {
    tool.call(ToolExecutionContext::new(ToolCall::new(
        id,
        "speak",
        json!({"text":text}),
    )))
}

#[tokio::test]
async fn tool_streams_admission_progress_order_close_drain_and_no_replay() {
    let streams = streams(2, 1024);
    let mut producer = streams.claim(&selection()).unwrap();
    let tool = producer.tools(CancellationToken::new()).remove(0).tool;
    assert!(!call(&tool, "one", "first").await.unwrap().is_error);
    // The tool and the helper can proceed while the consumer is busy with another effect.
    assert!(!call(&tool, "two", "second").await.unwrap().is_error);
    assert!(!streams.drained());
    producer.complete();
    drop(producer);
    assert!(
        !streams.drained(),
        "normal producer completion must retain accepted calls"
    );
    for (id, text) in [("one", "first"), ("two", "second")] {
        assert_eq!(
            streams
                .next("segments", CancellationToken::new())
                .await
                .unwrap()
                .unwrap(),
            json!({"callId":id,"tool":"speak","arguments":{"text":text}})
        );
    }
    assert_eq!(
        streams
            .next("segments", CancellationToken::new())
            .await
            .unwrap(),
        None
    );
    assert!(streams.drained());
    assert!(matches!(
        streams.claim(&selection()),
        Err(PortError::Conflict)
    ));
    assert!(
        streams
            .next("foreign", CancellationToken::new())
            .await
            .is_err()
    );
    let fresh = ToolStreams::new(BTreeMap::from([(
        "segments".into(),
        streams.queues["segments"].declaration.clone(),
    )]));
    assert!(
        fresh.queues["segments"]
            .state
            .lock()
            .unwrap()
            .queue
            .is_empty()
    );
    streams.cancel();
    streams.cancel();
    assert!(streams.was_drained());
}

#[tokio::test]
async fn tool_streams_count_and_bytes_backpressure_wake_after_receive() {
    for (count, bytes, text) in [
        (1, 1024, "small"),
        (16, 120, "012345678901234567890123456789"),
    ] {
        let streams = streams(count, bytes);
        let mut producer = streams.claim(&selection()).unwrap();
        let tool = producer.tools(CancellationToken::new()).remove(0).tool;
        assert!(!call(&tool, "a", text).await.unwrap().is_error);
        let mut blocked = call(&tool, "b", text);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut blocked)
                .await
                .is_err()
        );
        assert_eq!(
            streams.queues["segments"].state.lock().unwrap().queue.len(),
            1
        );
        streams
            .next("segments", CancellationToken::new())
            .await
            .unwrap();
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), blocked)
                .await
                .unwrap()
                .unwrap()
                .is_error
        );
        producer.complete();
        drop(producer);
        streams
            .next("segments", CancellationToken::new())
            .await
            .unwrap();
        assert!(streams.drained());
    }
}

#[tokio::test]
async fn tool_streams_schema_byte_rejection_and_producer_failure_are_terminal() {
    let streams = streams(1, 100);
    let producer = streams.claim(&selection()).unwrap();
    let tool = producer.tools(CancellationToken::new()).remove(0).tool;
    let invalid = tool
        .call(ToolExecutionContext::new(ToolCall::new(
            "bad",
            "speak",
            json!({"text":false}),
        )))
        .await
        .unwrap();
    assert!(invalid.is_error);
    assert!(
        call(&tool, "oversize", &"x".repeat(80))
            .await
            .unwrap()
            .is_error
    );
    assert!(
        streams.queues["segments"]
            .state
            .lock()
            .unwrap()
            .queue
            .is_empty()
    );
    drop(producer);
    assert_eq!(
        streams.next("segments", CancellationToken::new()).await,
        Err(PortError::Uncertain)
    );
    assert!(!streams.drained());
}

#[tokio::test]
async fn tool_streams_cancellation_wakes_blocked_send_and_receive_and_closes_state() {
    let streams = streams(1, 1024);
    let producer = streams.claim(&selection()).unwrap();
    let cancel = CancellationToken::new();
    let tool = producer.tools(cancel.clone()).remove(0).tool;
    call(&tool, "first", "first").await.unwrap();
    let mut blocked = call(&tool, "blocked", "blocked");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut blocked)
            .await
            .is_err()
    );
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .unwrap()
            .unwrap()
            .is_error
    );
    streams
        .next("segments", CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let mut next = Box::pin(streams.next("segments", cancel.clone()));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut next)
            .await
            .is_err()
    );
    cancel.cancel();
    assert!(next.await.is_err());
    streams.cancel();
    assert!(call(&tool, "late", "late").await.unwrap().is_error);
    assert_eq!(streams.queues["segments"].state.lock().unwrap().bytes, 0);
    assert!(
        streams.queues["segments"]
            .state
            .lock()
            .unwrap()
            .queue
            .is_empty()
    );
    assert!(!streams.drained());
}
