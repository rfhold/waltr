use super::*;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

pub(super) struct QueuedResponse {
    pub response: Result<wire::InteractResponse, Status>,
    _bytes: Option<OwnedSemaphorePermit>,
}

impl QueuedResponse {
    pub fn control(response: Result<wire::InteractResponse, Status>) -> Self {
        Self {
            response,
            _bytes: None,
        }
    }
}

pub(super) fn bytes(response: &wire::InteractResponse) -> usize {
    match response.event.as_ref() {
        Some(wire::interact_response::Event::OutputChunk(chunk)) => match chunk.content.as_ref() {
            Some(wire::interaction_output_chunk::Content::Audio(data)) => data.len(),
            Some(wire::interaction_output_chunk::Content::Text(text)) => {
                text.text.len().saturating_add(text.segment_id.len())
            }
            None => 0,
        },
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk() -> wire::InteractResponse {
        response(wire::interact_response::Event::OutputChunk(
            wire::InteractionOutputChunk {
                stream_id: "out".into(),
                content: Some(wire::interaction_output_chunk::Content::Audio(vec![1; 4])),
            },
        ))
    }

    #[tokio::test]
    async fn queued_byte_credit_survives_dequeue_until_yield_or_drop() {
        let limits = InteractionLimits {
            edge_bytes: 8,
            chunk_bytes: 4,
            ..InteractionLimits::default()
        };
        let (tx, mut rx) = mpsc::channel(8);
        let credits = Arc::new(Semaphore::new(8));
        let cancel = CancellationToken::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        send(&tx, &credits, chunk(), &cancel, &limits, deadline)
            .await
            .unwrap();
        send(&tx, &credits, chunk(), &cancel, &limits, deadline)
            .await
            .unwrap();
        let pending = send(&tx, &credits, chunk(), &cancel, &limits, deadline);
        tokio::pin!(pending);
        assert!(futures_util::poll!(&mut pending).is_pending());
        let queued = rx.recv().await.unwrap();
        assert_eq!(credits.available_permits(), 0);
        assert!(futures_util::poll!(&mut pending).is_pending());
        drop(queued);
        pending.await.unwrap();
        assert_eq!(credits.available_permits(), 0);
        drop(rx);
        assert_eq!(credits.available_permits(), 8);
    }

    #[tokio::test]
    async fn queue_wait_is_cancellation_aware_with_bytes_and_slots_saturated() {
        let limits = InteractionLimits {
            edge_bytes: 4,
            chunk_bytes: 4,
            queued_chunks: 1,
            ..InteractionLimits::default()
        };
        let (tx, _rx) = mpsc::channel(1);
        let credits = Arc::new(Semaphore::new(4));
        let cancel = CancellationToken::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        send(&tx, &credits, chunk(), &cancel, &limits, deadline)
            .await
            .unwrap();
        let pending = send(&tx, &credits, chunk(), &cancel, &limits, deadline);
        tokio::pin!(pending);
        assert!(futures_util::poll!(&mut pending).is_pending());
        cancel.cancel();
        assert_eq!(pending.await.unwrap_err().code, "cancelled");
    }
}

pub(super) async fn send(
    tx: &mpsc::Sender<QueuedResponse>,
    credits: &Arc<Semaphore>,
    response: wire::InteractResponse,
    cancel: &CancellationToken,
    limits: &InteractionLimits,
    invocation_deadline: tokio::time::Instant,
) -> Result<(), interaction::Error> {
    let size = bytes(&response);
    if size > limits.chunk_bytes || size > limits.edge_bytes {
        return Err(interaction::Error::new(
            "resource_exhausted",
            "response chunk exceeds queue byte limit",
        ));
    }
    let deadline = (tokio::time::Instant::now() + Duration::from_secs(limits.queue_wait_seconds))
        .min(invocation_deadline);
    tokio::select! { biased;
        () = cancel.cancelled() => Err(interaction::Error::new("cancelled", "interaction response cancelled")),
        () = tx.closed() => Err(interaction::Error::new("cancelled", "interaction response dropped")),
        () = tokio::time::sleep_until(deadline) => Err(interaction::Error::new(if deadline == invocation_deadline { "deadline_exceeded" } else { "resource_exhausted" }, "interaction response queue wait expired")),
        result = async {
            let permit = credits.clone().acquire_many_owned(size as u32).await.map_err(|_| interaction::Error::new("cancelled", "response queue closed"))?;
            let slot = tx.reserve().await.map_err(|_| interaction::Error::new("cancelled", "interaction response dropped"))?;
            slot.send(QueuedResponse { response: Ok(response), _bytes: Some(permit) });
            Ok(())
        } => result,
    }
}
