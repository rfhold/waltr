//! Experimental MCP Events (draft SEP-3415). Application replay and authorization stay in the host.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Experimental extension identifier.
pub const EVENTS_EXTENSION: &str = "io.modelcontextprotocol/events";

/// One event definition advertised by a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEventDefinition {
    /// Stable event type name.
    pub name: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Application filter schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Map<String, Value>>,
    /// Application payload schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_schema: Option<serde_json::Map<String, Value>>,
    /// Supported delivery modes (for example, push).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Vec<String>>,
}

/// One discovery page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEventList {
    /// Advertised definitions.
    pub events: Vec<McpEventDefinition>,
    /// Opaque pagination cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Parameters opening a request-scoped event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEventStreamRequest {
    /// Event type name.
    pub name: String,
    /// Application-defined filters.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub arguments: Option<serde_json::Map<String, Value>>,
    /// Opaque application replay cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum replay age in milliseconds.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_age_ms: Option<u64>,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Structured recoverable or terminal stream error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpEventError {
    /// JSON-RPC-style error code.
    pub code: i64,
    /// Error description.
    pub message: String,
    /// Application-defined detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// One typed stream notification. Terminated is terminal, unlike Error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpEventMessage {
    /// Stream admission and replay-gap report.
    Active {
        /// Application replay position, absent for non-replayable events.
        cursor: Option<String>,
        /// Whether replay skipped unavailable events.
        #[serde(default)]
        truncated: bool,
    },
    /// Application event; data may be any JSON value.
    Event {
        /// Application event identifier.
        #[serde(rename = "eventId")]
        event_id: String,
        /// Event type name.
        name: String,
        /// Application timestamp string.
        timestamp: String,
        /// Application payload.
        data: Value,
        /// Application replay position.
        cursor: Option<String>,
    },
    /// Application has checked up to this cursor.
    Heartbeat {
        /// Last checked application replay position.
        cursor: Option<String>,
    },
    /// Recoverable failure; the stream remains open.
    Error {
        /// Recoverable failure detail.
        error: McpEventError,
    },
    /// Terminal failure; consumers must stop this request.
    Terminated {
        /// Terminal failure detail.
        error: McpEventError,
    },
}

impl McpEventMessage {
    pub(crate) fn notification(&self) -> crate::Result<(String, Value)> {
        let mut value =
            serde_json::to_value(self).map_err(|e| crate::Error::protocol(e.to_string()))?;
        let kind = value
            .as_object_mut()
            .and_then(|v| v.remove("type"))
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| crate::Error::protocol("invalid event message"))?;
        Ok((format!("notifications/events/{kind}"), value))
    }

    pub(crate) fn parse(method: &str, mut params: Value) -> crate::Result<Self> {
        if method == "notifications/events/event" && params.get("data").is_none() {
            return Err(crate::Error::protocol("event notification omitted data"));
        }
        let kind = method
            .strip_prefix("notifications/events/")
            .ok_or_else(|| crate::Error::protocol("invalid event notification"))?;
        params
            .as_object_mut()
            .ok_or_else(|| crate::Error::protocol("invalid event params"))?
            .insert("type".into(), Value::String(kind.into()));
        serde_json::from_value(params)
            .map_err(|e| crate::Error::protocol(format!("invalid event notification: {e}")))
    }
}

/// Bounded client stream. Call shutdown to cancel and join its owned request.
#[derive(Debug)]
pub struct McpEventStream {
    pub(crate) receiver: tokio::sync::mpsc::Receiver<crate::Result<McpEventMessage>>,
    pub(crate) cancellation: tokio_util::sync::CancellationToken,
    pub(crate) task: Option<tokio::task::JoinHandle<()>>,
    pub(crate) completion: Option<tokio::sync::oneshot::Receiver<crate::Result<()>>>,
    pub(crate) finished: bool,
}

impl McpEventStream {
    /// Receive the next notification. Silence for sixty seconds cancels the request.
    pub async fn recv(&mut self) -> Option<crate::Result<McpEventMessage>> {
        if self.finished {
            return None;
        }
        let Ok(message) =
            tokio::time::timeout(std::time::Duration::from_mins(1), self.receiver.recv()).await
        else {
            self.finished = true;
            self.cancellation.cancel();
            self.receiver.close();
            return Some(Err(crate::Error::protocol(
                "MCP event stream idle for 60 seconds",
            )));
        };
        if matches!(
            &message,
            Some(Ok(McpEventMessage::Terminated { .. }) | Err(_))
        ) {
            self.finished = true;
            self.cancellation.cancel();
            self.receiver.close();
        }
        if message.is_some() {
            return message;
        }
        self.finished = true;
        match self.completion.take() {
            Some(completion) => match completion.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(Err(error)),
                Err(_) => Some(Err(crate::Error::protocol("MCP event request task closed"))),
            },
            None => None,
        }
    }

    /// Cancel the request and join local cleanup; does not shut down the client.
    pub async fn shutdown(&mut self) {
        self.cancellation.cancel();
        self.finished = true;
        self.receiver.close();
        if let Some(task) = self.task.take() {
            drop(task.await);
        }
    }
}

impl Drop for McpEventStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn heartbeat_resets_idle_deadline_and_timeout_is_terminal() {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let (_completion_tx, completion) = tokio::sync::oneshot::channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let token = cancellation.clone();
        let mut stream = McpEventStream {
            receiver,
            cancellation,
            completion: Some(completion),
            task: None,
            finished: false,
        };
        let task = tokio::spawn(async move {
            assert!(matches!(
                stream.recv().await.unwrap().unwrap(),
                McpEventMessage::Heartbeat { .. }
            ));
            let error = stream.recv().await.unwrap().unwrap_err();
            assert!(error.to_string().contains("idle"));
            assert!(stream.recv().await.is_none());
        });
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        sender
            .send(Ok(McpEventMessage::Heartbeat { cursor: None }))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        assert!(!task.is_finished());
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        task.await.unwrap();
        assert!(token.is_cancelled());
    }

    #[test]
    fn nullable_cursors_and_payload_validation_match_wire() {
        assert_eq!(
            McpEventMessage::parse("notifications/events/active", serde_json::json!({})).unwrap(),
            McpEventMessage::Active {
                cursor: None,
                truncated: false
            }
        );
        assert!(
            McpEventMessage::parse(
                "notifications/events/event",
                serde_json::json!({"eventId":"e", "name":"n", "timestamp":"t"})
            )
            .is_err()
        );
        assert!(McpEventMessage::parse("notifications/events/event", serde_json::json!({"eventId":"e", "name":"n", "timestamp":"t", "data":null, "cursor":null})).is_ok());
    }
}
