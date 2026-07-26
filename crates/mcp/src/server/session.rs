use std::{
    collections::{HashMap, VecDeque},
    fmt,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

#[cfg(any(
    feature = "streamable-http-postgres",
    feature = "streamable-http-sqlite"
))]
use std::time::UNIX_EPOCH;

use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::BoxFuture;
use crate::McpPrincipalId;

#[cfg(feature = "streamable-http-postgres")]
mod postgres;
#[cfg(feature = "streamable-http-sqlite")]
mod sqlite;

#[cfg(feature = "streamable-http-postgres")]
pub use postgres::PostgresStreamableHttpSessionStore;
#[cfg(feature = "streamable-http-sqlite")]
pub use sqlite::SqliteStreamableHttpSessionStore;

/// Identity that owns a Streamable HTTP session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StreamableHttpSessionOwner {
    /// Session created without access-token authorization.
    Anonymous,
    /// Session bound to a stable authenticated principal.
    Authenticated(McpPrincipalId),
}

impl StreamableHttpSessionOwner {
    #[cfg(any(
        feature = "streamable-http-postgres",
        feature = "streamable-http-sqlite"
    ))]
    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Anonymous => "anonymous".to_owned(),
            Self::Authenticated(principal) => {
                format!("authenticated:{}:{}", principal.as_str().len(), principal)
            }
        }
    }
}

/// Owner-bound lookup key for a Streamable HTTP session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamableHttpSessionKey {
    /// Opaque server-issued session identifier.
    pub session_id: String,
    /// Identity presenting the session identifier.
    pub owner: StreamableHttpSessionOwner,
}

/// Persisted lifecycle of a Streamable HTTP session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamableHttpSessionLifecycle {
    /// Initialize response was produced and the initialized notification is pending.
    InitializeResponded,
    /// One handler owns a leased initialization claim.
    Initializing,
    /// Initialization and its emitted events committed atomically.
    Initialized,
}

impl StreamableHttpSessionLifecycle {
    #[cfg(any(
        feature = "streamable-http-postgres",
        feature = "streamable-http-sqlite"
    ))]
    pub(crate) fn from_storage_name(value: &str) -> StreamableHttpSessionStoreResult<Self> {
        match value {
            "initialize_responded" => Ok(Self::InitializeResponded),
            "initializing" => Ok(Self::Initializing),
            "initialized" => Ok(Self::Initialized),
            _ => Err(StreamableHttpSessionStoreError::InvalidData),
        }
    }
}

/// Stable SSE event identifier scoped to one session stream.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamableHttpEventId {
    /// Random identifier fixed for the session lifetime.
    pub stream_id: String,
    /// Positive sequence for persisted events; zero is accepted as an acknowledgement cursor.
    pub sequence: u64,
}

impl fmt::Display for StreamableHttpEventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.stream_id, self.sequence)
    }
}

impl FromStr for StreamableHttpEventId {
    type Err = StreamableHttpEventIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 160 {
            return Err(StreamableHttpEventIdParseError);
        }
        let (stream_id, sequence) = value
            .split_once(':')
            .filter(|(stream_id, sequence)| {
                !stream_id.is_empty() && !sequence.is_empty() && !sequence.contains(':')
            })
            .ok_or(StreamableHttpEventIdParseError)?;
        let sequence = sequence
            .parse::<u64>()
            .ok()
            .filter(|sequence| i64::try_from(*sequence).is_ok())
            .ok_or(StreamableHttpEventIdParseError)?;
        if sequence.to_string() != value.rsplit_once(':').expect("separator was validated").1 {
            return Err(StreamableHttpEventIdParseError);
        }
        Ok(Self {
            stream_id: stream_id.to_owned(),
            sequence,
        })
    }
}

/// Error returned when an SSE event identifier is malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamableHttpEventIdParseError;

impl fmt::Display for StreamableHttpEventIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Streamable HTTP event identifier")
    }
}

impl std::error::Error for StreamableHttpEventIdParseError {}

/// Persisted Streamable HTTP SSE event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamableHttpEvent {
    /// Stable stream and sequence identifier.
    pub id: StreamableHttpEventId,
    /// JSON-RPC message carried by the event.
    pub message: Value,
}

/// Capacity, expiry, batch, and lease policy enforced by a session store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamableHttpSessionStoreConfig {
    /// Fixed absolute lifetime assigned when a session is created.
    pub session_ttl: Duration,
    /// Maximum unexpired sessions across all owners.
    pub max_sessions: usize,
    /// Maximum unexpired sessions for one authenticated principal.
    pub max_sessions_per_principal: usize,
    /// Maximum unacknowledged events in one session.
    pub max_events_per_session: usize,
    /// Maximum serialized bytes across unacknowledged events in one session.
    pub max_pending_bytes_per_session: usize,
    /// Maximum serialized size of one event.
    pub max_event_bytes: usize,
    /// Maximum events claimed by one delivery.
    pub delivery_batch_size: usize,
    /// Maximum expired sessions removed during admission cleanup.
    pub cleanup_batch_size: usize,
    /// Time during which one delivery claim excludes another.
    pub delivery_lease: Duration,
    /// Time during which one initialization handler owns its claim.
    pub initialization_claim_lease: Duration,
}

impl Default for StreamableHttpSessionStoreConfig {
    fn default() -> Self {
        Self {
            session_ttl: Duration::from_hours(24),
            max_sessions: 10_000,
            max_sessions_per_principal: 32,
            max_events_per_session: 1_024,
            max_pending_bytes_per_session: 8 * 1024 * 1024,
            max_event_bytes: 1024 * 1024,
            delivery_batch_size: 128,
            cleanup_batch_size: 128,
            delivery_lease: Duration::from_secs(30),
            initialization_claim_lease: Duration::from_mins(1),
        }
    }
}

impl StreamableHttpSessionStoreConfig {
    fn validate(&self) -> StreamableHttpSessionStoreResult<()> {
        if self.session_ttl.is_zero()
            || self.max_sessions == 0
            || self.max_sessions_per_principal == 0
            || self.max_events_per_session == 0
            || self.max_pending_bytes_per_session == 0
            || self.max_event_bytes == 0
            || self.delivery_batch_size == 0
            || self.cleanup_batch_size == 0
            || self.delivery_lease.is_zero()
            || self.initialization_claim_lease.is_zero()
            || !duration_milliseconds_fit_i64(self.session_ttl)
            || !duration_milliseconds_fit_i64(self.delivery_lease)
            || !duration_milliseconds_fit_i64(self.initialization_claim_lease)
        {
            return Err(StreamableHttpSessionStoreError::InvalidData);
        }
        Ok(())
    }
}

fn duration_milliseconds_fit_i64(duration: Duration) -> bool {
    i64::try_from(duration.as_millis()).is_ok()
}

#[derive(Clone, Debug)]
/// Parameters for creating a session.
pub struct StreamableHttpCreateSessionRequest {
    /// New owner-bound session key.
    pub key: StreamableHttpSessionKey,
    /// Negotiated MCP protocol version.
    pub protocol_version: String,
    /// Operation time used for expiry decisions.
    pub now: SystemTime,
}

#[derive(Clone, Debug)]
/// Parameters shared by operations on an existing session.
pub struct StreamableHttpSessionRequest {
    /// Owner-bound session key.
    pub key: StreamableHttpSessionKey,
    /// Protocol version presented by the client, when present.
    pub presented_protocol_version: Option<String>,
    /// Operation time used for expiry and lease decisions.
    pub now: SystemTime,
}

#[derive(Clone, Debug)]
/// Parameters for atomically completing initialization.
pub struct StreamableHttpCompleteInitializationRequest {
    /// Existing session parameters.
    pub session: StreamableHttpSessionRequest,
    /// Initialization claim being completed.
    pub claim_id: String,
    /// Events emitted by the initialization handler.
    pub messages: Vec<Value>,
}

#[derive(Clone, Debug)]
/// Parameters for releasing a failed initialization claim.
pub struct StreamableHttpAbandonInitializationRequest {
    /// Existing session parameters.
    pub session: StreamableHttpSessionRequest,
    /// Initialization claim being released.
    pub claim_id: String,
}

#[derive(Clone, Debug)]
/// Parameters for atomically appending events.
pub struct StreamableHttpAppendEventsRequest {
    /// Existing session parameters.
    pub session: StreamableHttpSessionRequest,
    /// JSON messages to append in order.
    pub messages: Vec<Value>,
}

#[derive(Clone, Debug)]
/// Parameters for acknowledging and claiming a delivery batch.
pub struct StreamableHttpPrepareDeliveryRequest {
    /// Existing session parameters.
    pub session: StreamableHttpSessionRequest,
    /// Last event durably consumed by the client.
    pub acknowledged_event_id: Option<StreamableHttpEventId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Public persisted state returned by session inspection.
pub struct StreamableHttpSession {
    /// Current initialization lifecycle.
    pub lifecycle: StreamableHttpSessionLifecycle,
    /// Negotiated MCP protocol version.
    pub protocol_version: String,
    /// Stable identifier shared by retries of initialization.
    pub initialization_operation_id: String,
    /// Fixed absolute session expiry.
    pub expires_at: SystemTime,
    /// Number of unacknowledged events.
    pub pending_event_count: usize,
    /// Serialized bytes held by unacknowledged events.
    pub pending_event_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of session creation.
pub enum StreamableHttpCreateSession {
    /// Session was inserted.
    Created(StreamableHttpSession),
    /// The generated session identifier already exists.
    AlreadyExists,
    /// A global or per-principal session cap was reached.
    CapacityExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of session inspection.
pub enum StreamableHttpInspectSession {
    /// Matching unexpired session state.
    Session(StreamableHttpSession),
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of claiming initialization.
pub enum StreamableHttpClaimInitialization {
    /// A fresh initialization lease was acquired.
    Claimed {
        /// Identifier required to complete or abandon the claim.
        claim_id: String,
        /// Stable operation identifier shared by reclaimed attempts.
        operation_id: String,
    },
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
    /// Session lifecycle cannot be initialized.
    LifecycleMismatch,
    /// Another unexpired initialization claim exists.
    InitializationInProgress {
        /// Remaining lease duration.
        retry_after: Duration,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of completing initialization.
pub enum StreamableHttpCompleteInitialization {
    /// Lifecycle and emitted events committed together.
    Completed(Vec<StreamableHttpEvent>),
    /// This exact claim was already committed.
    AlreadyCompleted,
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
    /// Claim or lifecycle does not match.
    LifecycleMismatch,
    /// Emitted events exceed session capacity.
    CapacityExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of abandoning initialization.
pub enum StreamableHttpAbandonInitialization {
    /// Claim was released.
    Abandoned,
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
    /// Claim or lifecycle does not match.
    LifecycleMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of appending events.
pub enum StreamableHttpAppendEvents {
    /// Events were appended with contiguous identifiers.
    Events(Vec<StreamableHttpEvent>),
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
    /// Session is not initialized.
    LifecycleMismatch,
    /// Event count or byte capacity would be exceeded.
    CapacityExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Outcome of preparing resumable delivery.
pub enum StreamableHttpPrepareDelivery {
    /// Non-destructively leased event batch.
    Events {
        /// Identifier of this delivery lease.
        delivery_id: String,
        /// Ordered events in the lease.
        events: Vec<StreamableHttpEvent>,
    },
    /// No unacknowledged events are available.
    Idle {
        /// Sequence-zero cursor used to prime a new SSE stream.
        priming_event_id: StreamableHttpEventId,
    },
    /// Another unexpired delivery lease exists.
    DeliveryBusy {
        /// Remaining lease duration.
        retry_after: Duration,
    },
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
    /// Session is not initialized.
    LifecycleMismatch,
    /// Acknowledgement cursor is malformed for this session state.
    InvalidCursor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Outcome of deleting a session.
pub enum StreamableHttpDeleteSession {
    /// Session and events were deleted.
    Deleted,
    /// Session is absent, expired, or owned by another identity.
    NotFound,
    /// Presented and negotiated protocol versions differ.
    ProtocolVersionMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Counts returned by bounded expired-session cleanup.
pub struct StreamableHttpCleanupExpired {
    /// Number of sessions removed.
    pub sessions_deleted: usize,
    /// Number of events removed by cascading deletes.
    pub events_deleted: usize,
}

/// Error returned by a Streamable HTTP session store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamableHttpSessionStoreError {
    /// Backing persistence could not complete the operation.
    Unavailable,
    /// Persisted or supplied data violates the store contract.
    InvalidData,
}

impl fmt::Display for StreamableHttpSessionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("Streamable HTTP session store unavailable"),
            Self::InvalidData => {
                formatter.write_str("Streamable HTTP session store contains invalid data")
            }
        }
    }
}

impl std::error::Error for StreamableHttpSessionStoreError {}

/// Result returned by session store operations.
pub type StreamableHttpSessionStoreResult<T> =
    std::result::Result<T, StreamableHttpSessionStoreError>;

/// Owner-bound, bounded persistence contract for Streamable HTTP session state.
pub trait StreamableHttpSessionStore: Send + Sync + 'static {
    /// Return the capacity and lease policy enforced by this store.
    fn config(&self) -> StreamableHttpSessionStoreConfig;
    /// Create a bounded owner-bound session.
    fn create_session(
        &self,
        request: StreamableHttpCreateSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCreateSession>>;
    /// Inspect an owner-bound unexpired session without mutation.
    fn inspect_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpInspectSession>>;
    /// Acquire or reclaim an initialization lease.
    fn claim_initialization(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpClaimInitialization>>;
    /// Atomically commit initialization and emitted events.
    fn complete_initialization(
        &self,
        request: StreamableHttpCompleteInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCompleteInitialization>>;
    /// Release an initialization claim after handler failure.
    fn abandon_initialization(
        &self,
        request: StreamableHttpAbandonInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAbandonInitialization>>;
    /// Atomically append an ordered event batch.
    fn append_events(
        &self,
        request: StreamableHttpAppendEventsRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAppendEvents>>;
    /// Apply an acknowledgement and non-destructively lease a delivery batch.
    fn prepare_delivery(
        &self,
        request: StreamableHttpPrepareDeliveryRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery>>;
    /// Delete an owner-bound session and its events.
    fn delete_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpDeleteSession>>;
    /// Delete up to `limit` expired sessions in deterministic order.
    fn cleanup_expired(
        &self,
        now: SystemTime,
        limit: usize,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCleanupExpired>>;
}

#[derive(Clone, Debug)]
/// Process-local implementation of the session store contract.
pub struct InMemoryStreamableHttpSessionStore {
    state: Arc<Mutex<InMemoryState>>,
    config: StreamableHttpSessionStoreConfig,
}

impl Default for InMemoryStreamableHttpSessionStore {
    fn default() -> Self {
        Self::new(StreamableHttpSessionStoreConfig::default())
            .expect("default Streamable HTTP session store config is valid")
    }
}

impl InMemoryStreamableHttpSessionStore {
    /// Create an empty in-memory store with explicit policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the policy is invalid.
    pub fn new(config: StreamableHttpSessionStoreConfig) -> StreamableHttpSessionStoreResult<Self> {
        config.validate()?;
        Ok(Self {
            state: Arc::new(Mutex::new(InMemoryState::default())),
            config,
        })
    }
}

#[derive(Debug, Default)]
struct InMemoryState {
    sessions: HashMap<String, InMemorySession>,
}

#[derive(Debug)]
struct InMemorySession {
    owner: StreamableHttpSessionOwner,
    protocol_version: String,
    lifecycle: StreamableHttpSessionLifecycle,
    operation_id: String,
    claim: Option<Lease>,
    completed_claim_id: Option<String>,
    stream_id: String,
    next_sequence: u64,
    delivered_sequence: u64,
    acknowledged_sequence: u64,
    events: VecDeque<StoredEvent>,
    delivery: Option<DeliveryLease>,
    updated_at: SystemTime,
    expires_at: SystemTime,
}

#[derive(Debug)]
struct StoredEvent {
    event: StreamableHttpEvent,
    bytes: usize,
}

#[derive(Debug)]
struct Lease {
    id: String,
    expires_at: SystemTime,
}

#[derive(Debug)]
struct DeliveryLease {
    through: u64,
    expires_at: SystemTime,
}

impl InMemorySession {
    fn public(&self) -> StreamableHttpSession {
        StreamableHttpSession {
            lifecycle: self.lifecycle,
            protocol_version: self.protocol_version.clone(),
            initialization_operation_id: self.operation_id.clone(),
            expires_at: self.expires_at,
            pending_event_count: self.events.len(),
            pending_event_bytes: self.events.iter().map(|event| event.bytes).sum(),
        }
    }
}

impl StreamableHttpSessionStore for InMemoryStreamableHttpSessionStore {
    fn config(&self) -> StreamableHttpSessionStoreConfig {
        self.config.clone()
    }

    fn create_session(
        &self,
        request: StreamableHttpCreateSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCreateSession>> {
        let state = Arc::clone(&self.state);
        let config = self.config.clone();
        Box::pin(async move {
            let expires_at = request
                .now
                .checked_add(config.session_ttl)
                .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
            let mut state = state.lock().await;
            cleanup_memory(&mut state, request.now, config.cleanup_batch_size);
            if state.sessions.contains_key(&request.key.session_id) {
                return Ok(StreamableHttpCreateSession::AlreadyExists);
            }
            let active_sessions = state
                .sessions
                .values()
                .filter(|session| session.expires_at > request.now)
                .count();
            if active_sessions >= config.max_sessions {
                return Ok(StreamableHttpCreateSession::CapacityExceeded);
            }
            if let StreamableHttpSessionOwner::Authenticated(principal) = &request.key.owner {
                let count = state
                    .sessions
                    .values()
                    .filter(|session| {
                        session.expires_at > request.now
                            && session.owner
                                == StreamableHttpSessionOwner::Authenticated(principal.clone())
                    })
                    .count();
                if count >= config.max_sessions_per_principal {
                    return Ok(StreamableHttpCreateSession::CapacityExceeded);
                }
            }
            let session = InMemorySession {
                owner: request.key.owner,
                protocol_version: request.protocol_version,
                lifecycle: StreamableHttpSessionLifecycle::InitializeResponded,
                operation_id: fresh_id(),
                claim: None,
                completed_claim_id: None,
                stream_id: fresh_id(),
                next_sequence: 1,
                delivered_sequence: 0,
                acknowledged_sequence: 0,
                events: VecDeque::new(),
                delivery: None,
                updated_at: request.now,
                expires_at,
            };
            let public = session.public();
            state.sessions.insert(request.key.session_id, session);
            drop(state);
            Ok(StreamableHttpCreateSession::Created(public))
        })
    }

    fn inspect_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpInspectSession>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let state = state.lock().await;
            let Some(session) = memory_session(&state, &request) else {
                return Ok(StreamableHttpInspectSession::NotFound);
            };
            if !version_matches(session, &request) {
                return Ok(StreamableHttpInspectSession::ProtocolVersionMismatch);
            }
            Ok(StreamableHttpInspectSession::Session(session.public()))
        })
    }

    fn claim_initialization(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpClaimInitialization>> {
        let state = Arc::clone(&self.state);
        let lease_duration = self.config.initialization_claim_lease;
        Box::pin(async move {
            let mut state = state.lock().await;
            let Some(session) = memory_session_mut(&mut state, &request) else {
                return Ok(StreamableHttpClaimInitialization::NotFound);
            };
            if !version_matches(session, &request) {
                return Ok(StreamableHttpClaimInitialization::ProtocolVersionMismatch);
            }
            if session.lifecycle == StreamableHttpSessionLifecycle::Initializing
                && let Some(claim) = &session.claim
                && claim.expires_at > request.now
            {
                return Ok(
                    StreamableHttpClaimInitialization::InitializationInProgress {
                        retry_after: duration_until(request.now, claim.expires_at),
                    },
                );
            }
            if !matches!(
                session.lifecycle,
                StreamableHttpSessionLifecycle::InitializeResponded
                    | StreamableHttpSessionLifecycle::Initializing
            ) {
                return Ok(StreamableHttpClaimInitialization::LifecycleMismatch);
            }
            let claim_id = fresh_id();
            session.lifecycle = StreamableHttpSessionLifecycle::Initializing;
            session.claim = Some(Lease {
                id: claim_id.clone(),
                expires_at: request
                    .now
                    .checked_add(lease_duration)
                    .ok_or(StreamableHttpSessionStoreError::InvalidData)?,
            });
            session.updated_at = request.now;
            Ok(StreamableHttpClaimInitialization::Claimed {
                claim_id,
                operation_id: session.operation_id.clone(),
            })
        })
    }

    fn complete_initialization(
        &self,
        request: StreamableHttpCompleteInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCompleteInitialization>> {
        let state = Arc::clone(&self.state);
        let config = self.config.clone();
        Box::pin(async move {
            let serialized = serialize_messages(request.messages, &config)?;
            let mut state = state.lock().await;
            let Some(session) = memory_session_mut(&mut state, &request.session) else {
                return Ok(StreamableHttpCompleteInitialization::NotFound);
            };
            if !version_matches(session, &request.session) {
                return Ok(StreamableHttpCompleteInitialization::ProtocolVersionMismatch);
            }
            if session.lifecycle == StreamableHttpSessionLifecycle::Initialized
                && session.completed_claim_id.as_deref() == Some(&request.claim_id)
            {
                return Ok(StreamableHttpCompleteInitialization::AlreadyCompleted);
            }
            if session.lifecycle != StreamableHttpSessionLifecycle::Initializing
                || session.claim.as_ref().map(|claim| claim.id.as_str())
                    != Some(request.claim_id.as_str())
                || session
                    .claim
                    .as_ref()
                    .is_some_and(|claim| claim.expires_at <= request.session.now)
            {
                return Ok(StreamableHttpCompleteInitialization::LifecycleMismatch);
            }
            let Some(events) = append_memory(session, serialized, &config)? else {
                return Ok(StreamableHttpCompleteInitialization::CapacityExceeded);
            };
            session.lifecycle = StreamableHttpSessionLifecycle::Initialized;
            session.claim = None;
            session.completed_claim_id = Some(request.claim_id);
            session.updated_at = request.session.now;
            Ok(StreamableHttpCompleteInitialization::Completed(events))
        })
    }

    fn abandon_initialization(
        &self,
        request: StreamableHttpAbandonInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAbandonInitialization>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut state = state.lock().await;
            let Some(session) = memory_session_mut(&mut state, &request.session) else {
                return Ok(StreamableHttpAbandonInitialization::NotFound);
            };
            if !version_matches(session, &request.session) {
                return Ok(StreamableHttpAbandonInitialization::ProtocolVersionMismatch);
            }
            if session.lifecycle != StreamableHttpSessionLifecycle::Initializing
                || session.claim.as_ref().map(|claim| claim.id.as_str())
                    != Some(request.claim_id.as_str())
            {
                return Ok(StreamableHttpAbandonInitialization::LifecycleMismatch);
            }
            session.lifecycle = StreamableHttpSessionLifecycle::InitializeResponded;
            session.claim = None;
            session.updated_at = request.session.now;
            Ok(StreamableHttpAbandonInitialization::Abandoned)
        })
    }

    fn append_events(
        &self,
        request: StreamableHttpAppendEventsRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAppendEvents>> {
        let state = Arc::clone(&self.state);
        let config = self.config.clone();
        Box::pin(async move {
            let serialized = serialize_messages(request.messages, &config)?;
            let mut state = state.lock().await;
            let Some(session) = memory_session_mut(&mut state, &request.session) else {
                return Ok(StreamableHttpAppendEvents::NotFound);
            };
            if !version_matches(session, &request.session) {
                return Ok(StreamableHttpAppendEvents::ProtocolVersionMismatch);
            }
            if session.lifecycle != StreamableHttpSessionLifecycle::Initialized {
                return Ok(StreamableHttpAppendEvents::LifecycleMismatch);
            }
            let Some(events) = append_memory(session, serialized, &config)? else {
                return Ok(StreamableHttpAppendEvents::CapacityExceeded);
            };
            if !events.is_empty() {
                session.updated_at = request.session.now;
            }
            Ok(StreamableHttpAppendEvents::Events(events))
        })
    }

    fn prepare_delivery(
        &self,
        request: StreamableHttpPrepareDeliveryRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery>> {
        let state = Arc::clone(&self.state);
        let config = self.config.clone();
        Box::pin(async move {
            let mut state = state.lock().await;
            let Some(session) = memory_session_mut(&mut state, &request.session) else {
                return Ok(StreamableHttpPrepareDelivery::NotFound);
            };
            if !version_matches(session, &request.session) {
                return Ok(StreamableHttpPrepareDelivery::ProtocolVersionMismatch);
            }
            if session.lifecycle != StreamableHttpSessionLifecycle::Initialized {
                return Ok(StreamableHttpPrepareDelivery::LifecycleMismatch);
            }
            if request.acknowledged_event_id.is_none()
                && session.delivery.is_none()
                && session.next_sequence - 1 <= session.delivered_sequence
            {
                return Ok(StreamableHttpPrepareDelivery::Idle {
                    priming_event_id: StreamableHttpEventId {
                        stream_id: session.stream_id.clone(),
                        sequence: 0,
                    },
                });
            }
            prepare_memory_delivery(session, request, &config)
        })
    }

    fn delete_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpDeleteSession>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut state = state.lock().await;
            let Some(session) = memory_session(&state, &request) else {
                return Ok(StreamableHttpDeleteSession::NotFound);
            };
            if !version_matches(session, &request) {
                return Ok(StreamableHttpDeleteSession::ProtocolVersionMismatch);
            }
            state.sessions.remove(&request.key.session_id);
            drop(state);
            Ok(StreamableHttpDeleteSession::Deleted)
        })
    }

    fn cleanup_expired(
        &self,
        now: SystemTime,
        limit: usize,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCleanupExpired>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut state = state.lock().await;
            Ok(cleanup_memory(&mut state, now, limit))
        })
    }
}

fn memory_session<'a>(
    state: &'a InMemoryState,
    request: &StreamableHttpSessionRequest,
) -> Option<&'a InMemorySession> {
    state
        .sessions
        .get(&request.key.session_id)
        .filter(|session| session.owner == request.key.owner && session.expires_at > request.now)
}

fn memory_session_mut<'a>(
    state: &'a mut InMemoryState,
    request: &StreamableHttpSessionRequest,
) -> Option<&'a mut InMemorySession> {
    state
        .sessions
        .get_mut(&request.key.session_id)
        .filter(|session| session.owner == request.key.owner && session.expires_at > request.now)
}

fn version_matches(session: &InMemorySession, request: &StreamableHttpSessionRequest) -> bool {
    request
        .presented_protocol_version
        .as_ref()
        .is_none_or(|version| *version == session.protocol_version)
}

pub fn serialize_messages(
    messages: Vec<Value>,
    config: &StreamableHttpSessionStoreConfig,
) -> StreamableHttpSessionStoreResult<Vec<(Value, String, usize)>> {
    messages
        .into_iter()
        .map(|message| {
            let serialized = serde_json::to_string(&message)
                .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
            let bytes = serialized.len();
            if bytes == 0 || bytes > config.max_event_bytes {
                return Err(StreamableHttpSessionStoreError::InvalidData);
            }
            Ok((message, serialized, bytes))
        })
        .collect()
}

fn append_memory(
    session: &mut InMemorySession,
    messages: Vec<(Value, String, usize)>,
    config: &StreamableHttpSessionStoreConfig,
) -> StreamableHttpSessionStoreResult<Option<Vec<StreamableHttpEvent>>> {
    let bytes = messages.iter().map(|(_, _, bytes)| bytes).sum::<usize>();
    let current_bytes = session
        .events
        .iter()
        .map(|event| event.bytes)
        .sum::<usize>();
    if session.events.len().saturating_add(messages.len()) > config.max_events_per_session
        || current_bytes.saturating_add(bytes) > config.max_pending_bytes_per_session
    {
        return Ok(None);
    }
    let count =
        u64::try_from(messages.len()).map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    let next = session
        .next_sequence
        .checked_add(count)
        .filter(|next| *next <= i64::MAX as u64 + 1)
        .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
    let mut events = Vec::with_capacity(messages.len());
    for (sequence, (message, _, bytes)) in (session.next_sequence..next).zip(messages) {
        let event = StreamableHttpEvent {
            id: StreamableHttpEventId {
                stream_id: session.stream_id.clone(),
                sequence,
            },
            message,
        };
        session.events.push_back(StoredEvent {
            event: event.clone(),
            bytes,
        });
        events.push(event);
    }
    session.next_sequence = next;
    Ok(Some(events))
}

fn prepare_memory_delivery(
    session: &mut InMemorySession,
    request: StreamableHttpPrepareDeliveryRequest,
    config: &StreamableHttpSessionStoreConfig,
) -> StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery> {
    if let Some(cursor) = request.acknowledged_event_id {
        let sequence_out_of_range = cursor.sequence < session.acknowledged_sequence
            || cursor.sequence > session.delivered_sequence;
        if cursor.stream_id != session.stream_id || sequence_out_of_range {
            return Ok(StreamableHttpPrepareDelivery::InvalidCursor);
        }
        if cursor.sequence > session.acknowledged_sequence {
            session.acknowledged_sequence = cursor.sequence;
            while session
                .events
                .front()
                .is_some_and(|event| event.event.id.sequence <= cursor.sequence)
            {
                session.events.pop_front();
            }
            session.updated_at = request.session.now;
        }
    }
    if session.delivery.as_ref().is_some_and(|delivery| {
        delivery.through <= session.acknowledged_sequence
            || delivery.expires_at <= request.session.now
    }) {
        session.delivery = None;
        session.updated_at = request.session.now;
    }
    if let Some(delivery) = &session.delivery {
        return Ok(StreamableHttpPrepareDelivery::DeliveryBusy {
            retry_after: duration_until(request.session.now, delivery.expires_at),
        });
    }
    let events = session
        .events
        .iter()
        .filter(|event| event.event.id.sequence > session.acknowledged_sequence)
        .take(config.delivery_batch_size)
        .map(|event| event.event.clone())
        .collect::<Vec<_>>();
    let Some(_) = events.first() else {
        return Ok(StreamableHttpPrepareDelivery::Idle {
            priming_event_id: StreamableHttpEventId {
                stream_id: session.stream_id.clone(),
                sequence: 0,
            },
        });
    };
    let through = events.last().expect("nonempty delivery batch").id.sequence;
    let delivery_id = fresh_id();
    session.delivered_sequence = session.delivered_sequence.max(through);
    session.delivery = Some(DeliveryLease {
        through,
        expires_at: request
            .session
            .now
            .checked_add(config.delivery_lease)
            .ok_or(StreamableHttpSessionStoreError::InvalidData)?,
    });
    session.updated_at = request.session.now;
    Ok(StreamableHttpPrepareDelivery::Events {
        delivery_id,
        events,
    })
}

fn cleanup_memory(
    state: &mut InMemoryState,
    now: SystemTime,
    limit: usize,
) -> StreamableHttpCleanupExpired {
    let mut expired = state
        .sessions
        .iter()
        .filter(|(_, session)| session.expires_at <= now)
        .map(|(id, session)| (id.clone(), session.expires_at))
        .collect::<Vec<_>>();
    expired.sort_by_key(|(id, expires_at)| (*expires_at, id.clone()));
    let mut events_deleted = 0;
    let mut sessions_deleted = 0;
    for (id, _) in expired.into_iter().take(limit) {
        if let Some(session) = state.sessions.remove(&id) {
            sessions_deleted += 1;
            events_deleted += session.events.len();
        }
    }
    StreamableHttpCleanupExpired {
        sessions_deleted,
        events_deleted,
    }
}

pub fn fresh_id() -> String {
    Uuid::now_v7().to_string()
}

#[cfg(any(
    feature = "streamable-http-postgres",
    feature = "streamable-http-sqlite"
))]
pub fn system_time_ms(time: SystemTime) -> StreamableHttpSessionStoreResult<i64> {
    let milliseconds = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?
        .as_millis();
    i64::try_from(milliseconds).map_err(|_| StreamableHttpSessionStoreError::InvalidData)
}

#[cfg(any(
    feature = "streamable-http-postgres",
    feature = "streamable-http-sqlite"
))]
pub fn system_time_from_ms(ms: i64) -> StreamableHttpSessionStoreResult<SystemTime> {
    let ms = u64::try_from(ms).map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    UNIX_EPOCH
        .checked_add(Duration::from_millis(ms))
        .ok_or(StreamableHttpSessionStoreError::InvalidData)
}

#[cfg(any(
    feature = "streamable-http-postgres",
    feature = "streamable-http-sqlite"
))]
pub fn duration_ms(duration: Duration) -> StreamableHttpSessionStoreResult<i64> {
    i64::try_from(duration.as_millis()).map_err(|_| StreamableHttpSessionStoreError::InvalidData)
}

pub fn duration_until(now: SystemTime, future: SystemTime) -> Duration {
    future.duration_since(now).unwrap_or_default()
}

#[cfg(any(
    feature = "streamable-http-postgres",
    feature = "streamable-http-sqlite"
))]
pub fn persisted_event(
    stream_id: &str,
    sequence: i64,
    message: &str,
) -> StreamableHttpSessionStoreResult<StreamableHttpEvent> {
    let sequence = u64::try_from(sequence)
        .ok()
        .filter(|sequence| *sequence > 0)
        .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
    let message =
        serde_json::from_str(message).map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    Ok(StreamableHttpEvent {
        id: StreamableHttpEventId {
            stream_id: stream_id.to_owned(),
            sequence,
        },
        message,
    })
}

#[cfg(any(
    feature = "streamable-http-postgres",
    feature = "streamable-http-sqlite"
))]
pub const fn migration_error(
    error: &sqlx::migrate::MigrateError,
) -> StreamableHttpSessionStoreError {
    use sqlx::migrate::MigrateError;
    match error {
        MigrateError::VersionMissing(_)
        | MigrateError::VersionMismatch(_)
        | MigrateError::VersionNotPresent(_)
        | MigrateError::VersionTooOld(_, _)
        | MigrateError::VersionTooNew(_, _)
        | MigrateError::Dirty(_) => StreamableHttpSessionStoreError::InvalidData,
        _ => StreamableHttpSessionStoreError::Unavailable,
    }
}
