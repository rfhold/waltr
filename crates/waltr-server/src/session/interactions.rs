//! Interaction RPCs use the same authenticated session admission as ordinary session operations.

use crate::{
    interaction::{
        Interaction, InteractionBroker, InteractionError, InteractionResponse, InteractionSnapshot,
    },
    permission::{PermissionDenyMode, PermissionResponse},
};
use std::{pin::Pin, sync::Arc};
use tonic::{Request, Response, Status};
use waltr::grpc::waltr::session::v1 as wire;
use waltr_agent::{SessionId, SessionStore};

pub struct InteractionService<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: Arc<super::SessionService<S>>,
    broker: InteractionBroker,
}

impl<S: SessionStore + Clone + Send + Sync + 'static> InteractionService<S> {
    pub fn new(sessions: Arc<super::SessionService<S>>, broker: InteractionBroker) -> Self {
        Self { sessions, broker }
    }
    async fn authorize<T>(&self, request: &Request<T>, session: &str) -> Result<SessionId, Status> {
        if session.is_empty() || session.len() > 256 {
            return Err(Status::invalid_argument("invalid session id"));
        }
        let id = SessionId::new(session);
        let principal = request
            .extensions()
            .get::<crate::access_auth::AuthenticatedPrincipal>()
            .cloned();
        let sessions = self.sessions.request_sessions(principal, &id).await?;
        sessions
            .load(&id)
            .await
            .map_err(super::error::agent_error_to_status)?;
        Ok(id)
    }
}

#[tonic::async_trait]
impl<S: SessionStore + Clone + Send + Sync + 'static>
    wire::interaction_service_server::InteractionService for InteractionService<S>
{
    async fn get_interactions(
        &self,
        request: Request<wire::GetInteractionsRequest>,
    ) -> Result<Response<wire::GetInteractionsResponse>, Status> {
        let session = self
            .authorize(&request, &request.get_ref().session_id)
            .await?;
        Ok(Response::new(wire::GetInteractionsResponse {
            snapshot: Some(snapshot(self.broker.snapshot(&session))),
        }))
    }
    type WatchInteractionsStream = Pin<
        Box<
            dyn futures_util::Stream<Item = Result<wire::WatchInteractionsResponse, Status>> + Send,
        >,
    >;
    async fn watch_interactions(
        &self,
        request: Request<wire::WatchInteractionsRequest>,
    ) -> Result<Response<Self::WatchInteractionsStream>, Status> {
        let session = self
            .authorize(&request, &request.get_ref().session_id)
            .await?;
        let subscription = self.broker.subscribe(session);
        // Revocation ends observation, never the underlying operation.
        let revoked = request
            .extensions()
            .get::<crate::access_auth::AuthenticatedPrincipal>()
            .map(|p| p.access_ended());
        let stream = futures_util::stream::try_unfold(
            (subscription, revoked),
            |(mut subscription, mut revoked)| async move {
                let next = if let Some(revoked) = &mut revoked {
                    tokio::select! { biased; _ = revoked.as_mut() => Err(Status::permission_denied("session access ended"))?,
                    next = subscription.next() => next }
                } else {
                    subscription.next().await
                };
                Ok(next.map(|next| {
                    (
                        wire::WatchInteractionsResponse {
                            snapshot: Some(snapshot(next)),
                        },
                        (subscription, revoked),
                    )
                }))
            },
        );
        Ok(Response::new(Box::pin(stream)))
    }
    async fn respond_interaction(
        &self,
        request: Request<wire::RespondInteractionRequest>,
    ) -> Result<Response<wire::RespondInteractionResponse>, Status> {
        let session = self
            .authorize(&request, &request.get_ref().session_id)
            .await?;
        let input = request.into_inner();
        let response = response(
            input
                .response
                .ok_or_else(|| Status::invalid_argument("response is required"))?,
        )?;
        self.broker
            .respond(
                &session,
                &input.generation,
                &input.id,
                &input.operation_id,
                response,
            )
            .map_err(status)?;
        Ok(Response::new(wire::RespondInteractionResponse {}))
    }
}

fn status(error: InteractionError) -> Status {
    match error {
        InteractionError::Invalid => Status::invalid_argument("invalid interaction response"),
        InteractionError::Stale => {
            Status::failed_precondition("interaction is stale or already resolved")
        }
        InteractionError::Capacity => Status::resource_exhausted("interaction limit exceeded"),
        InteractionError::Cancelled => Status::cancelled("interaction owner cancelled"),
        InteractionError::Unavailable => {
            Status::failed_precondition("interaction route is unavailable")
        }
    }
}

fn response(
    input: wire::respond_interaction_request::Response,
) -> Result<InteractionResponse, Status> {
    use wire::respond_interaction_request::Response as R;
    Ok(match input {
        R::Permission(p) => {
            use wire::permission_interaction_response::Decision;
            let patterns = if p.patterns_json.is_empty() {
                Vec::new()
            } else {
                if p.patterns_json.len() > 64 * 1024 {
                    return Err(Status::resource_exhausted("patterns too large"));
                }
                serde_json::from_str(&p.patterns_json)
                    .map_err(|_| Status::invalid_argument("invalid patterns"))?
            };
            let decision = Decision::try_from(p.decision)
                .map_err(|_| Status::invalid_argument("invalid decision"))?;
            if decision != Decision::AllowAlways && !patterns.is_empty() {
                return Err(Status::invalid_argument("unexpected patterns"));
            }
            InteractionResponse::Permission(match decision {
                Decision::AllowOnce => PermissionResponse::AllowOnce,
                Decision::AllowAlways => PermissionResponse::AllowAlways { patterns },
                Decision::Deny | Decision::CancelWork => PermissionResponse::Deny {
                    message: p.message,
                    mode: if decision == Decision::CancelWork {
                        PermissionDenyMode::CancelWork
                    } else {
                        PermissionDenyMode::ReturnToolResult
                    },
                },
                _ => return Err(Status::invalid_argument("decision is required")),
            })
        }
        R::Question(q) => {
            InteractionResponse::Question(q.answers.into_iter().map(|a| a.values).collect())
        }
        R::Form(f) => InteractionResponse::Form(elicitation_response(f)?),
        R::UrlConsent(u) => InteractionResponse::UrlConsent(elicitation_response(u)?),
    })
}

fn elicitation_response(
    input: wire::ElicitationInteractionResponse,
) -> Result<mcp::McpElicitationResponse, Status> {
    use wire::elicitation_interaction_response::Action;
    let action = match Action::try_from(input.action) {
        Ok(Action::Accept) => mcp::McpElicitationAction::Accept,
        Ok(Action::Decline) => mcp::McpElicitationAction::Decline,
        Ok(Action::Cancel) => mcp::McpElicitationAction::Cancel,
        _ => return Err(Status::invalid_argument("elicitation action is required")),
    };
    let content = input
        .content_json
        .map(|json| {
            if json.len() > 64 * 1024 {
                return Err(Status::resource_exhausted("form content too large"));
            }
            serde_json::from_str(&json)
                .map_err(|_| Status::invalid_argument("invalid form content"))
        })
        .transpose()?;
    Ok(mcp::McpElicitationResponse { action, content })
}

fn snapshot(snapshot: InteractionSnapshot) -> wire::InteractionSnapshot {
    wire::InteractionSnapshot {
        generation: snapshot.generation,
        revision: snapshot.revision,
        pending: snapshot
            .pending
            .into_iter()
            .map(|pending| {
                use wire::pending_interaction::Request as R;
                let request = match pending.request {
                    Interaction::Permission {
                        message,
                        identity,
                        claims,
                        patterns,
                    } => R::Permission(wire::PermissionInteraction {
                        message,
                        identity,
                        claims_json: serde_json::to_string(&claims).expect("claims serialize"),
                        suggested_patterns_json: serde_json::to_string(&patterns)
                            .expect("patterns serialize"),
                    }),
                    Interaction::Question { input } => R::Question(wire::QuestionInteraction {
                        questions: input
                            .questions
                            .into_iter()
                            .map(|q| wire::InteractionQuestion {
                                question: q.question,
                                header: q.header,
                                multiple: q.multiple,
                                custom: q.custom,
                                options: q
                                    .options
                                    .into_iter()
                                    .map(|o| wire::InteractionQuestionOption {
                                        label: o.label,
                                        description: o.description,
                                    })
                                    .collect(),
                            })
                            .collect(),
                    }),
                    Interaction::Form {
                        request:
                            mcp::McpElicitationRequest::Form {
                                message,
                                requested_schema,
                            },
                    } => R::Form(wire::FormInteraction {
                        message,
                        requested_schema_json: serde_json::to_string(&requested_schema)
                            .expect("schema serializes"),
                    }),
                    Interaction::UrlConsent {
                        request: mcp::McpElicitationRequest::Url { message, url },
                    } => R::UrlConsent(wire::UrlConsentInteraction { message, url }),
                    _ => unreachable!("broker validates interaction mode"),
                };
                wire::PendingInteraction {
                    id: pending.id,
                    operation_id: pending.operation_id,
                    session_id: pending.session_id,
                    request: Some(request),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_responses_require_typed_decisions_and_valid_json() {
        use wire::respond_interaction_request::Response as R;
        let invalid = response(R::Permission(wire::PermissionInteractionResponse {
            decision: 0,
            patterns_json: String::new(),
            message: None,
        }))
        .unwrap_err();
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        let invalid = response(R::Form(wire::ElicitationInteractionResponse {
            action: wire::elicitation_interaction_response::Action::Accept as i32,
            content_json: Some("not json".into()),
        }))
        .unwrap_err();
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        assert!(matches!(
            response(R::UrlConsent(wire::ElicitationInteractionResponse {
                action: wire::elicitation_interaction_response::Action::Decline as i32,
                content_json: None,
            }))
            .unwrap(),
            InteractionResponse::UrlConsent(_)
        ));
    }

    #[test]
    fn wire_snapshot_retains_generation_and_exact_operation_session_binding() {
        let wire = snapshot(InteractionSnapshot {
            generation: "process-generation".into(),
            revision: 7,
            pending: vec![crate::interaction::PendingInteraction {
                id: "pending".into(),
                operation_id: "operation".into(),
                session_id: "actual-session".into(),
                request: Interaction::UrlConsent {
                    request: mcp::McpElicitationRequest::Url {
                        message: "Consent only".into(),
                        url: "https://example.com/".into(),
                    },
                },
            }],
        });
        assert_eq!(wire.generation, "process-generation");
        assert_eq!(wire.revision, 7);
        assert_eq!(wire.pending[0].session_id, "actual-session");
        assert_eq!(wire.pending[0].operation_id, "operation");
        assert!(matches!(
            wire.pending[0].request,
            Some(wire::pending_interaction::Request::UrlConsent(_))
        ));
    }
}
