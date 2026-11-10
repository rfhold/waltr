//! Immutable upstream request routing into the host operation's live broker.

use crate::interaction::{Interaction, InteractionBroker, InteractionResponse, OperationBinding};
use mcp::{
    McpElicitationCapabilities, McpElicitationFuture, McpElicitationRequest,
    McpElicitationResponse, client::McpClientHandler,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct McpOperation {
    broker: InteractionBroker,
    binding: OperationBinding,
}

impl McpOperation {
    pub(crate) fn operation_id(&self) -> &str {
        &self.binding.operation_id
    }
    pub fn new(broker: InteractionBroker, binding: OperationBinding) -> Self {
        Self { broker, binding }
    }

    pub(crate) fn for_context(
        policy: &dyn super::super::dispatcher::CapabilityPolicy,
        context: &waltr_agent::ToolExecutionContext,
    ) -> Option<Self> {
        let (broker, binding) = policy.interaction_binding(context)?;
        Some(Self::new(broker, binding))
    }

    pub fn capabilities(&self) -> McpElicitationCapabilities {
        if self.binding.cancellation.is_cancelled() {
            return McpElicitationCapabilities::default();
        }
        match &self.binding.destination {
            crate::interaction::InteractionDestination::Session(_) => McpElicitationCapabilities {
                form: true,
                url: true,
            },
            crate::interaction::InteractionDestination::External(route) => {
                route.elicitation_capabilities()
            }
            crate::interaction::InteractionDestination::Unavailable => {
                McpElicitationCapabilities::default()
            }
        }
    }

    pub(crate) fn client(&self, client: &mcp::client::McpClient) -> mcp::client::McpClient {
        client.for_operation(Arc::new(self.clone()), self.capabilities())
    }
}

impl McpClientHandler for McpOperation {
    fn elicit(
        &self,
        _upstream_key: String,
        request: McpElicitationRequest,
        cancellation: CancellationToken,
    ) -> McpElicitationFuture {
        let this = self.clone();
        Box::pin(async move {
            let modes = this.capabilities();
            if matches!(&request, McpElicitationRequest::Form { .. } if !modes.form)
                || matches!(&request, McpElicitationRequest::Url { .. } if !modes.url)
            {
                return Err(mcp::Error::protocol(
                    "MCP input mode unavailable for this operation",
                ));
            }
            let interaction = match &request {
                McpElicitationRequest::Form { .. } => Interaction::Form {
                    request: request.clone(),
                },
                McpElicitationRequest::Url { .. } => Interaction::UrlConsent {
                    request: request.clone(),
                },
            };
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(mcp::Error::protocol("MCP input operation cancelled")),
                response = this.broker.interact(&this.binding, interaction) => response
                    .map_err(|_| mcp::Error::protocol("MCP input route unavailable or cancelled"))?,
            };
            let response: McpElicitationResponse = match (&request, response) {
                (McpElicitationRequest::Form { .. }, InteractionResponse::Form(response))
                | (McpElicitationRequest::Url { .. }, InteractionResponse::UrlConsent(response)) => {
                    response
                }
                _ => return Err(mcp::Error::protocol("MCP input response mode mismatch")),
            };
            request.validate_response(&response)?;
            // URL acceptance is consent only. The upstream owns completion and continuation state.
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::{
        ExternalInteractionRoute, InteractionDestination, InteractionError, InteractionFuture,
    };
    use mcp::McpElicitationAction;
    use std::sync::Mutex;

    struct Route {
        seen: Mutex<Vec<String>>,
        action: McpElicitationAction,
    }
    impl ExternalInteractionRoute for Route {
        fn elicitation_capabilities(&self) -> McpElicitationCapabilities {
            McpElicitationCapabilities {
                form: true,
                url: true,
            }
        }
        fn interact<'a>(
            &'a self,
            operation: &'a str,
            request: Interaction,
            _: CancellationToken,
        ) -> InteractionFuture<'a> {
            self.seen.lock().unwrap().push(operation.into());
            let response = McpElicitationResponse {
                action: self.action,
                content: None,
            };
            Box::pin(async move {
                Ok(match request {
                    Interaction::Form { .. } => InteractionResponse::Form(response),
                    Interaction::UrlConsent { .. } => InteractionResponse::UrlConsent(response),
                    _ => return Err(InteractionError::Invalid),
                })
            })
        }
    }
    fn operation(id: &str, destination: InteractionDestination) -> McpOperation {
        McpOperation::new(
            InteractionBroker::new(),
            OperationBinding {
                operation_id: id.into(),
                destination,
                cancellation: CancellationToken::new(),
            },
        )
    }
    fn form() -> McpElicitationRequest {
        McpElicitationRequest::parse(&serde_json::json!({"message":"Choose", "requestedSchema":{"type":"object","properties":{}}})).unwrap()
    }
    #[tokio::test]
    async fn external_routes_must_explicitly_advertise_supported_input_modes() {
        struct NoInput;
        impl ExternalInteractionRoute for NoInput {
            fn interact<'a>(
                &'a self,
                _: &'a str,
                _: Interaction,
                _: CancellationToken,
            ) -> InteractionFuture<'a> {
                panic!("unsupported mode must never reach the authenticated route")
            }
        }
        let operation = operation(
            "no-input",
            InteractionDestination::External(Arc::new(NoInput)),
        );
        assert_eq!(
            operation.capabilities(),
            McpElicitationCapabilities::default()
        );
        assert!(
            operation
                .elicit("peer-key".into(), form(), CancellationToken::new())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn session_answer_resumes_only_the_original_future_and_callback_cancel_removes_pending() {
        let broker = InteractionBroker::new();
        let session = waltr_agent::SessionId::generate();
        let unrelated = waltr_agent::SessionId::generate();
        let operation = McpOperation::new(
            broker.clone(),
            OperationBinding {
                operation_id: "durable-invocation".into(),
                destination: InteractionDestination::Session(session.clone()),
                cancellation: CancellationToken::new(),
            },
        );
        let task = tokio::spawn(operation.elicit(
            "untrusted-key".into(),
            form(),
            CancellationToken::new(),
        ));
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let snapshot = broker.snapshot(&session);
                if !snapshot.pending.is_empty() {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(broker.snapshot(&unrelated).pending.is_empty());
        let pending = &snapshot.pending[0];
        assert_eq!(pending.operation_id, "durable-invocation");
        let response = InteractionResponse::Form(McpElicitationResponse {
            action: McpElicitationAction::Decline,
            content: None,
        });
        assert_eq!(
            broker.respond(
                &unrelated,
                &snapshot.generation,
                &pending.id,
                &pending.operation_id,
                response.clone()
            ),
            Err(InteractionError::Stale)
        );
        broker
            .respond(
                &session,
                &snapshot.generation,
                &pending.id,
                &pending.operation_id,
                response,
            )
            .unwrap();
        assert_eq!(
            task.await.unwrap().unwrap().action,
            McpElicitationAction::Decline
        );
        assert!(broker.snapshot(&session).pending.is_empty());
        let callback_cancel = CancellationToken::new();
        let task =
            tokio::spawn(operation.elicit("same-key".into(), form(), callback_cancel.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while broker.snapshot(&session).pending.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        callback_cancel.cancel();
        assert!(task.await.unwrap().is_err());
        assert!(broker.snapshot(&session).pending.is_empty());
    }
    #[tokio::test]
    async fn concurrent_peer_keys_cannot_select_another_authenticated_operation() {
        let a = Arc::new(Route {
            seen: Mutex::default(),
            action: McpElicitationAction::Decline,
        });
        let b = Arc::new(Route {
            seen: Mutex::default(),
            action: McpElicitationAction::Cancel,
        });
        let a_operation = operation(
            "authenticated-a",
            InteractionDestination::External(a.clone()),
        );
        let b_operation = operation(
            "authenticated-b",
            InteractionDestination::External(b.clone()),
        );
        let (a_response, b_response) = tokio::join!(
            a_operation.elicit("same-peer-key".into(), form(), CancellationToken::new()),
            b_operation.elicit("same-peer-key".into(), form(), CancellationToken::new())
        );
        assert_eq!(a_response.unwrap().action, McpElicitationAction::Decline);
        assert_eq!(b_response.unwrap().action, McpElicitationAction::Cancel);
        assert_eq!(*a.seen.lock().unwrap(), ["authenticated-a"]);
        assert_eq!(*b.seen.lock().unwrap(), ["authenticated-b"]);
    }
    #[tokio::test]
    async fn absent_and_cancelled_routes_fail_without_synthetic_session_or_broadcast() {
        let absent = operation("missing", InteractionDestination::Unavailable);
        assert_eq!(absent.capabilities(), McpElicitationCapabilities::default());
        assert!(
            absent
                .elicit("key".into(), form(), CancellationToken::new())
                .await
                .is_err()
        );
        let route = Arc::new(Route {
            seen: Mutex::default(),
            action: McpElicitationAction::Decline,
        });
        let cancelled = operation("cancelled", InteractionDestination::External(route.clone()));
        cancelled.binding.cancellation.cancel();
        assert!(
            cancelled
                .elicit("key".into(), form(), CancellationToken::new())
                .await
                .is_err()
        );
        assert!(route.seen.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn url_acceptance_is_only_typed_consent_and_unsafe_urls_never_reach_route() {
        let route = Arc::new(Route {
            seen: Mutex::default(),
            action: McpElicitationAction::Accept,
        });
        let operation = operation("consent", InteractionDestination::External(route.clone()));
        let url = |url: &str| McpElicitationRequest::Url {
            message: "Connect".into(),
            url: url.into(),
        };
        let response = operation
            .elicit(
                "peer".into(),
                url("https://example.test/connect"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.action, McpElicitationAction::Accept);
        assert!(response.content.is_none());
        assert!(
            operation
                .elicit(
                    "peer".into(),
                    url("https://user:secret@example.test/connect"),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(route.seen.lock().unwrap().len(), 1);
    }
}
