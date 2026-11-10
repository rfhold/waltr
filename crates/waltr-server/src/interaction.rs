//! Live operation-bound interaction coordination. Pending work is never recovered or replayed.

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod tests;

use crate::{
    agent_host::question::QuestionInput,
    permission::{PermissionPattern, PermissionResponse},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

const MAX_PENDING: usize = 256;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Interaction {
    Permission {
        message: String,
        identity: String,
        claims: Vec<crate::permission::PermissionClaim>,
        patterns: Vec<PermissionPattern>,
    },
    Question {
        input: QuestionInput,
    },
    Form {
        request: mcp::McpElicitationRequest,
    },
    UrlConsent {
        request: mcp::McpElicitationRequest,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum InteractionResponse {
    Permission(PermissionResponse),
    Question(Vec<Vec<String>>),
    Form(mcp::McpElicitationResponse),
    UrlConsent(mcp::McpElicitationResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionError {
    Unavailable,
    Cancelled,
    Stale,
    Invalid,
    Capacity,
}

impl Interaction {
    fn validate(&self) -> Result<(), InteractionError> {
        match self {
            Self::Question { input } => input.validate().map_err(|_| InteractionError::Invalid),
            Self::Form {
                request: request @ mcp::McpElicitationRequest::Form { .. },
            } => request.validate().map_err(|_| InteractionError::Invalid),
            Self::UrlConsent {
                request: request @ mcp::McpElicitationRequest::Url { url, .. },
            } => {
                request.validate().map_err(|_| InteractionError::Invalid)?;
                let url = reqwest::Url::parse(url).map_err(|_| InteractionError::Invalid)?;
                if !matches!(url.scheme(), "https" | "http")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err(InteractionError::Invalid);
                }
                Ok(())
            }
            Self::Permission { .. } => Ok(()),
            _ => Err(InteractionError::Invalid),
        }
    }
    fn validate_response(&self, response: &InteractionResponse) -> Result<(), InteractionError> {
        bounded(response)?;
        match (self, response) {
            (
                Self::Permission { patterns, .. },
                InteractionResponse::Permission(PermissionResponse::AllowAlways {
                    patterns: selected,
                }),
            ) => {
                if selected.is_empty() || !selected.iter().all(|p| patterns.contains(p)) {
                    return Err(InteractionError::Invalid);
                }
            }
            (Self::Permission { .. }, InteractionResponse::Permission(_)) => {}
            (Self::Question { input }, InteractionResponse::Question(answers)) => {
                if answers.len() != input.questions.len() {
                    return Err(InteractionError::Invalid);
                }
                for (question, answers) in input.questions.iter().zip(answers) {
                    if answers.is_empty()
                        || (!question.multiple && answers.len() != 1)
                        || answers.iter().enumerate().any(|(i, answer)| {
                            answer.is_empty()
                                || answers[..i].contains(answer)
                                || (!question.custom
                                    && !question
                                        .options
                                        .iter()
                                        .any(|option| option.label == *answer))
                        })
                    {
                        return Err(InteractionError::Invalid);
                    }
                }
            }
            (Self::Form { request }, InteractionResponse::Form(response))
            | (Self::UrlConsent { request }, InteractionResponse::UrlConsent(response)) => request
                .validate_response(response)
                .map_err(|_| InteractionError::Invalid)?,
            _ => return Err(InteractionError::Invalid),
        }
        Ok(())
    }
}

fn bounded(value: &impl Serialize) -> Result<(), InteractionError> {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|size| *size <= MAX_BYTES)
                .ok_or_else(|| std::io::Error::other("interaction exceeds limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(0), value).map_err(|_| InteractionError::Capacity)
}

/// Constructed by the host from a durable dispatch association, never public metadata.
#[derive(Clone)]
pub struct OperationBinding {
    pub operation_id: String,
    pub destination: InteractionDestination,
    pub cancellation: CancellationToken,
}

#[derive(Clone)]
pub enum InteractionDestination {
    Session(waltr_agent::SessionId),
    External(Arc<dyn ExternalInteractionRoute>),
    Unavailable,
}

pub type InteractionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<InteractionResponse, InteractionError>> + Send + 'a>>;

/// One immutable authenticated operation route. Keys supplied by upstream peers are not authority.
pub trait ExternalInteractionRoute: Send + Sync {
    fn elicitation_capabilities(&self) -> mcp::McpElicitationCapabilities {
        mcp::McpElicitationCapabilities::default()
    }
    fn interact<'a>(
        &'a self,
        operation_id: &'a str,
        request: Interaction,
        cancellation: CancellationToken,
    ) -> InteractionFuture<'a>;
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingInteraction {
    pub id: String,
    pub operation_id: String,
    pub session_id: String,
    pub request: Interaction,
}

#[derive(Debug, Clone, Serialize)]
pub struct InteractionSnapshot {
    pub generation: String,
    pub revision: u64,
    pub pending: Vec<PendingInteraction>,
}

struct Pending {
    view: PendingInteraction,
    sender: oneshot::Sender<InteractionResponse>,
    cancellation: CancellationToken,
    bytes: usize,
}
struct State {
    generation: String,
    revision: u64,
    pending: BTreeMap<String, Pending>,
    changed: watch::Sender<u64>,
}

#[derive(Clone)]
pub struct InteractionBroker {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for InteractionBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InteractionBroker")
            .finish_non_exhaustive()
    }
}

impl Default for InteractionBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl InteractionBroker {
    pub fn new() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(State {
                generation: uuid::Uuid::new_v4().to_string(),
                revision: 0,
                pending: BTreeMap::new(),
                changed,
            })),
        }
    }

    pub async fn interact(
        &self,
        binding: &OperationBinding,
        request: Interaction,
    ) -> Result<InteractionResponse, InteractionError> {
        if binding.operation_id.is_empty()
            || binding.operation_id.len() > 256
            || binding.operation_id.chars().any(char::is_control)
        {
            return Err(InteractionError::Invalid);
        }
        if let InteractionDestination::Session(session) = &binding.destination {
            if session.as_str().is_empty() || session.as_str().len() > 256 {
                return Err(InteractionError::Invalid);
            }
        }
        bounded(&request)?;
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| InteractionError::Invalid)?
            .len();
        request.validate()?;
        if binding.cancellation.is_cancelled() {
            return Err(InteractionError::Cancelled);
        }
        match &binding.destination {
            InteractionDestination::Unavailable => Err(InteractionError::Unavailable),
            InteractionDestination::External(route) => {
                let response = tokio::select! {
                    biased;
                    _ = binding.cancellation.cancelled() => return Err(InteractionError::Cancelled),
                    response = route.interact(&binding.operation_id, request.clone(), binding.cancellation.clone()) => response?,
                };
                request.validate_response(&response)?;
                Ok(response)
            }
            InteractionDestination::Session(session) => {
                let id = uuid::Uuid::new_v4().to_string();
                let (sender, receiver) = oneshot::channel();
                {
                    let mut state = self.state.lock().expect("interaction lock poisoned");
                    if state.pending.len() >= MAX_PENDING
                        || state.pending.values().map(|p| p.bytes).sum::<usize>() + bytes
                            > 1024 * 1024
                    {
                        return Err(InteractionError::Capacity);
                    }
                    state.pending.insert(
                        id.clone(),
                        Pending {
                            view: PendingInteraction {
                                id: id.clone(),
                                operation_id: binding.operation_id.clone(),
                                session_id: session.as_str().into(),
                                request,
                            },
                            sender,
                            cancellation: binding.cancellation.clone(),
                            bytes,
                        },
                    );
                    state.publish();
                }
                // Dropping the owner future also closes unresolved work; viewers never own it.
                let _owner = PendingOwner {
                    broker: self.clone(),
                    id,
                };
                tokio::select! {
                    biased;
                    _ = binding.cancellation.cancelled() => Err(InteractionError::Cancelled),
                    response = receiver => response.map_err(|_| InteractionError::Cancelled),
                }
            }
        }
    }

    pub fn snapshot(&self, session: &waltr_agent::SessionId) -> InteractionSnapshot {
        self.state
            .lock()
            .expect("interaction lock poisoned")
            .snapshot(session)
    }

    /// Subscription registration and initial snapshot share the insertion/response lock.
    pub fn subscribe(&self, session: waltr_agent::SessionId) -> InteractionSubscription {
        let state = self.state.lock().expect("interaction lock poisoned");
        InteractionSubscription {
            initial: Some(state.snapshot(&session)),
            changes: state.changed.subscribe(),
            broker: self.clone(),
            session,
        }
    }

    /// Called only after the transport's existing session-access authorization.
    pub(crate) fn respond(
        &self,
        session: &waltr_agent::SessionId,
        generation: &str,
        id: &str,
        operation: &str,
        response: InteractionResponse,
    ) -> Result<(), InteractionError> {
        let mut state = self.state.lock().expect("interaction lock poisoned");
        if generation != state.generation {
            return Err(InteractionError::Stale);
        }
        let pending = state.pending.get(id).ok_or(InteractionError::Stale)?;
        if pending.view.session_id != session.as_str() || pending.view.operation_id != operation {
            return Err(InteractionError::Stale);
        }
        if pending.cancellation.is_cancelled() || pending.sender.is_closed() {
            return Err(InteractionError::Stale);
        }
        pending.view.request.validate_response(&response)?;
        let pending = state
            .pending
            .remove(id)
            .expect("locked pending interaction");
        let result = pending
            .sender
            .send(response)
            .map_err(|_| InteractionError::Stale);
        state.publish();
        result
    }
}

impl State {
    fn publish(&mut self) {
        self.revision += 1;
        self.changed.send_replace(self.revision);
    }
    fn snapshot(&self, session: &waltr_agent::SessionId) -> InteractionSnapshot {
        InteractionSnapshot {
            generation: self.generation.clone(),
            revision: self.revision,
            pending: self
                .pending
                .values()
                .filter(|p| {
                    p.view.session_id == session.as_str()
                        && !p.cancellation.is_cancelled()
                        && !p.sender.is_closed()
                })
                .map(|p| p.view.clone())
                .collect(),
        }
    }
}

struct PendingOwner {
    broker: InteractionBroker,
    id: String,
}
impl Drop for PendingOwner {
    fn drop(&mut self) {
        let mut state = self.broker.state.lock().expect("interaction lock poisoned");
        if state.pending.remove(&self.id).is_some() {
            state.publish();
        }
    }
}

pub struct InteractionSubscription {
    initial: Option<InteractionSnapshot>,
    changes: watch::Receiver<u64>,
    broker: InteractionBroker,
    session: waltr_agent::SessionId,
}
impl InteractionSubscription {
    /// Each event is a full replacement snapshot. Slow consumers coalesce, never miss pending state.
    pub async fn next(&mut self) -> Option<InteractionSnapshot> {
        if let Some(initial) = self.initial.take() {
            return Some(initial);
        }
        self.changes.changed().await.ok()?;
        Some(self.broker.snapshot(&self.session))
    }
}
