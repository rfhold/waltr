//! Authorized session/history operations; workflow policy belongs to the program.

pub mod automatic;
mod owned;

use crate::routine::{context::{ContextOperation, ContextPort, MetadataEntry, bounded}, ports::{PortError, PortFuture}};
use serde_json::{Value, json};
use std::{collections::{BTreeMap, BTreeSet}, sync::{Arc, Mutex}};
use tokio_util::sync::CancellationToken;
use waltr_agent::{ContextIdentity, MetadataPredicate, MetadataTarget, MetadataUpdate,
    ReplacementUserContent, Revision, SessionCaptureLimits, SessionContextCapture,
    SessionCoordinator, SessionCopy, SessionCreateRequest, SessionId, SessionMaskAddition,
    SessionMaskMutation, SessionMutation, SessionRecord, SessionStart, SessionStartMessage,
    SessionStore, TimelineCursor, TimelineItemId};

/// A bounded early-history pin for invocations admitted before a completed provider turn.
pub struct CoordinatorSessionContext<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    record: Arc<SessionRecord>,
    calls: owned::ContextCalls,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> CoordinatorSessionContext<S> {
    pub(crate) async fn pin_for_incarnation(sessions: SessionCoordinator<S>, id: &SessionId, incarnation: &str) -> Result<Self, PortError> {
        let context = Self::pin(sessions, id).await?;
        if context.record.metadata.incarnation.as_deref() != Some(incarnation) { return Err(PortError::Conflict); }
        Ok(context)
    }
    pub async fn pin(sessions: SessionCoordinator<S>, id: &SessionId) -> Result<Self, PortError> {
        let record = sessions.load(id).await.map_err(|_| PortError::Unavailable)?.record;
        if record.metadata.incarnation.is_none() { return Err(PortError::Conflict); }
        if record.journal.len() > 100_000 || record.timeline_events().take(4097).count() > 4096 {
            return Err(PortError::Invalid);
        }
        Ok(Self { sessions, record: Arc::new(record), calls: owned::ContextCalls::new() })
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> ContextPort for CoordinatorSessionContext<S> {
    fn check(&self, operation: &ContextOperation) -> Result<(), PortError> {
        match operation {
            ContextOperation::Snapshot(_) => Ok(()),
            ContextOperation::Metadata(args) if args.source == "effective" && args.item_id.is_none() => validate_key(&args.key),
            ContextOperation::Transact(args) if args.source == "effective" && args.masks.is_none()
                && args.predicates.iter().chain(&args.updates).all(|entry| entry.item_id.is_none()) => validate_entries(args),
            _ => Err(PortError::Denied),
        }
    }
    fn execute(&self, operation: ContextOperation, cancellation: CancellationToken) -> PortFuture<Value> {
        let checked = self.check(&operation);
        let sessions = self.sessions.clone();
        let record = self.record.clone();
        self.calls.run(Box::pin(async move {
            checked?;
            if cancellation.is_cancelled() { return Err(PortError::Denied); }
            let current = sessions.load(&record.id).await.map_err(|_| PortError::Unavailable)?.record;
            if current.metadata.incarnation != record.metadata.incarnation { return Err(PortError::Conflict); }
            if cancellation.is_cancelled() { return Err(PortError::Denied); }
            match operation {
                ContextOperation::Snapshot(_) => {
                    // Early history has no replay/copy authority. Only user text is projected.
                    let items = record.timeline_events().filter(|event| event.channel.as_ref()
                        .is_some_and(|channel| channel.as_str() == "user_text"))
                        .map(|event| timeline_item(event.clone())).collect::<Result<Vec<_>,_>>()?;
                    bounded(json!({"source":"effective","revision":current.revision.get().to_string(),
                        "items":items,"metadata":current.metadata.values,"itemMetadata":{},"masks":[]}))
                }
                ContextOperation::Metadata(args) => {
                    bounded(metadata_value(current.metadata.values.get(&args.key)))
                }
                ContextOperation::Transact(args) => {
                    let mutation = mutation(&record, None, args)?;
                    let result = sessions.mutate_session(&record.id, mutation).await.map_err(store_error)?;
                    Ok(json!({"committed":true,"revision":result.record.revision.get().to_string(),"cursor":null}))
                }
                _ => Err(PortError::Denied),
            }
        }))
    }
    fn cleanup(&self) -> PortFuture<()> { self.calls.cleanup(Box::pin(async { Ok(()) })) }
}

struct State {
    record: SessionRecord,
    identity: ContextIdentity,
    sources: BTreeMap<String, (SessionCopy, bool)>,
    destinations: BTreeMap<String, Option<SessionId>>,
    continuation: Option<TimelineCursor>,
    committed: bool,
}

pub struct CoordinatorContext<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    capture: SessionContextCapture,
    authority: String,
    original_allowed: bool,
    state: Arc<Mutex<State>>,
    gate: Arc<tokio::sync::Mutex<()>>,
    compaction: bool,
    automatic: Option<(Option<u64>, Option<u64>)>,
    pause: Arc<Mutex<Option<Arc<waltr_agent::SessionPauseLease>>>>,
    calls: owned::ContextCalls,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug for CoordinatorContext<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("CoordinatorContext").finish_non_exhaustive() }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> CoordinatorContext<S> {
    pub async fn pin(sessions: SessionCoordinator<S>, id: &SessionId, authority: &str, original_allowed: bool) -> Result<Self, PortError> {
        let capture = sessions.capture_context(id, authority, SessionCaptureLimits {
            max_encoded_bytes: 65_536, max_items: 4096, ..SessionCaptureLimits::default()
        }).await.map_err(|_| PortError::Conflict)?;
        let record = sessions.load(id).await.map_err(|_| PortError::Unavailable)?.record;
        if record.metadata.incarnation.is_none() || !capture.identity.matches(&record) { return Err(PortError::Conflict); }
        let mut sources = BTreeMap::from([("effective".into(), (capture.completed.copy.clone(), false))]);
        if original_allowed {
            sources.insert("original".into(), (SessionCopy::from_record(&record, capture.identity.boundary()).map_err(|_| PortError::Conflict)?, true));
        }
        let state = State { record, identity: capture.identity.clone(), sources, destinations: BTreeMap::new(), continuation: None, committed: false };
        Ok(Self { sessions, capture, authority: authority.into(), original_allowed,
            state: Arc::new(Mutex::new(state)), gate: Arc::new(tokio::sync::Mutex::new(())),
            compaction: false, automatic: None, pause: Arc::new(Mutex::new(None)), calls: owned::ContextCalls::new() })
    }
    pub async fn compaction(sessions: SessionCoordinator<S>, id: &SessionId, authority: &str) -> Result<Self, PortError> {
        Self::pin_paused(sessions, id, authority, false).await
    }
    pub async fn pin_paused(sessions: SessionCoordinator<S>, id: &SessionId, authority: &str, original_allowed: bool) -> Result<Self, PortError> {
        let mut context = Self::pin(sessions, id, authority, original_allowed).await?;
        context.compaction = true;
        Ok(context)
    }
    pub async fn completed_compaction(sessions: SessionCoordinator<S>, lease: waltr_agent::SessionPauseLease,
        authority: &str, snapshot: &waltr_agent::SessionRunCompletionSnapshot, context_limit: Option<u64>) -> Result<Self, PortError> {
        let lease = Arc::new(lease);
        match Self::compaction(sessions.clone(), lease.session_id(), authority).await {
            Ok(mut context) if context.capture.completed.through_cursor == lease.source_cursor()
                && context.capture.completed.copy.source_id() == &snapshot.session_id => {
                let terminal_matches = { let state = context.state.lock().map_err(|_| PortError::Uncertain)?;
                    state.record.last_timeline_cursor() == Some(snapshot.terminal_cursor)
                        && state.record.metadata.incarnation == snapshot.incarnation && state.record.revision == snapshot.revision };
                if terminal_matches {
                    context.automatic = Some((snapshot.context_tokens, context_limit));
                    *context.pause.lock().map_err(|_| PortError::Uncertain)? = Some(lease);
                    return Ok(context);
                }
            }
            _ => {}
        }
        sessions.release_pause(&lease).await.map_err(|_| PortError::Uncertain)?;
        Err(PortError::Conflict)
    }
    pub fn child_context(&self) -> Self {
        Self { sessions: self.sessions.clone(), capture: self.capture.clone(), authority: self.authority.clone(),
            original_allowed: self.original_allowed, state: self.state.clone(), gate: self.gate.clone(),
            compaction: false, automatic: None, pause: Arc::new(Mutex::new(None)), calls: owned::ContextCalls::new() }
    }
    pub fn helper_copy(&self) -> SessionCopy { self.capture.completed.copy.clone() }
    pub(crate) async fn committed_receipt(&self, invocation: crate::routine::triggers::InvocationIdentity,
        outcome: &crate::routine::service::Outcome) -> Result<crate::routine::triggers::CommittedInvocationReceipt, PortError> {
        if outcome.status != Some(crate::routine::history::Status::Succeeded) || !outcome.cleanup_complete || outcome.id.is_empty() {
            return Err(PortError::Conflict);
        }
        let _gate = self.gate.lock().await;
        let (record, identity, committed) = { let state = self.state.lock().map_err(|_| PortError::Unavailable)?;
            (state.record.clone(), state.identity.clone(), state.committed) };
        if !committed { return Err(PortError::Conflict); }
        let current = self.sessions.load(&record.id).await.map_err(|_| PortError::Unavailable)?.record;
        if current.revision != record.revision || current.metadata.incarnation != record.metadata.incarnation || !identity.matches(&current) {
            return Err(PortError::Conflict);
        }
        let note = match &outcome.result { Some(Value::String(value)) => value.clone(), value => serde_json::to_string(value).map_err(|_| PortError::Invalid)? };
        if note.len() > 8192 { return Err(PortError::Invalid); }
        Ok(crate::routine::triggers::CommittedInvocationReceipt { context: self.capture.identity.clone(), invocation,
            execution_id: outcome.id.clone(), revision: current.revision, note })
    }
    pub fn with_original_ceiling(&self, allowed: bool) -> Self {
        Self { sessions: self.sessions.clone(), capture: self.capture.clone(), authority: self.authority.clone(),
            original_allowed: self.original_allowed && allowed, state: self.state.clone(), gate: self.gate.clone(),
            compaction: self.compaction, automatic: self.automatic, pause: self.pause.clone(), calls: self.calls.clone() }
    }
    #[cfg(test)]
    pub(super) fn test_administrative_gate(&self) -> Arc<tokio::sync::Mutex<()>> { self.gate.clone() }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> ContextPort for CoordinatorContext<S> {
    fn intent(&self, operation: &ContextOperation) -> Result<Value, PortError> {
        match operation {
            ContextOperation::Start(args) => {
                let state = self.state.lock().map_err(|_| PortError::Unavailable)?;
                let id = state.destinations.get(&args.destination).and_then(Option::as_ref).ok_or(PortError::Denied)?;
                Ok(json!({"sessionId":id.as_str(),"destination":args.destination}))
            }
            ContextOperation::Continue(args) => Ok(json!({"cursor":args.cursor})),
            _ => Ok(json!({})),
        }
    }
    fn resolve_source(&self, source: &str) -> Result<SessionCopy, PortError> {
        let state = self.state.lock().map_err(|_| PortError::Unavailable)?;
        let (copy, original) = state.sources.get(source).ok_or(PortError::Denied)?;
        if *original && !self.original_allowed { return Err(PortError::Denied); }
        Ok(copy.clone())
    }
    fn prepare(&self) -> PortFuture<()> {
        let enabled = self.compaction;
        let sessions = self.sessions.clone();
        let state = self.state.clone();
        let pause = self.pause.clone();
        let automatic = self.automatic.is_some();
        Box::pin(async move {
            if !enabled { return Ok(()); }
            let (id, identity) = { let state = state.lock().map_err(|_| PortError::Unavailable)?; (state.record.id.clone(), state.identity.clone()) };
            if pause.lock().map_err(|_| PortError::Unavailable)?.is_some() {
                if !automatic { return Err(PortError::Conflict); }
            } else {
                let reason = waltr_agent::SessionCoordinationReason::new("routine.pause", None, None).map_err(|_| PortError::Invalid)?;
                let lease = sessions.acquire_pause_reject_busy(&id, reason).await.map_err(|_| PortError::Conflict)?;
                *pause.lock().map_err(|_| PortError::Uncertain)? = Some(Arc::new(lease));
            }
            let current = sessions.load(&id).await.map_err(|_| PortError::Unavailable)?.record;
            if !identity.matches(&current) { return Err(PortError::Conflict); }
            Ok(())
        })
    }
    fn snapshot_identity(&self) -> Option<ContextIdentity> { Some(self.capture.identity.clone()) }
    fn check(&self, operation: &ContextOperation) -> Result<(), PortError> {
        let source = match operation {
            ContextOperation::Original(_) => Some("original"),
            ContextOperation::History(args) => Some(args.source.as_str()),
            ContextOperation::Metadata(args) => { validate_key(&args.key)?; Some(args.source.as_str()) }
            ContextOperation::Transact(args) => { validate_entries(args)?; Some(args.source.as_str()) }
            ContextOperation::Create(args) => {
                if args.message.trim().is_empty() || args.message.len() > 32_768 { return Err(PortError::Invalid); }
                Some(args.source.as_str())
            }
            ContextOperation::Continue(_) if !self.compaction => return Err(PortError::Denied),
            ContextOperation::Continue(args) => {
                if self.state.lock().map_err(|_| PortError::Unavailable)?.continuation != Some(cursor(&args.cursor)?) { return Err(PortError::Conflict); }
                None
            }
            ContextOperation::Start(args) => {
                self.intent(&ContextOperation::Start(crate::routine::context::Start { destination:args.destination.clone() }))?;
                None
            }
            _ => None,
        };
        if let Some(source) = source { self.resolve_source(source)?; }
        Ok(())
    }
    fn execute(&self, operation: ContextOperation, cancellation: CancellationToken) -> PortFuture<Value> {
        let checked = self.check(&operation);
        let sessions = self.sessions.clone();
        let state = self.state.clone();
        let gate = self.gate.clone();
        let pause = self.pause.clone();
        let authority = self.authority.clone();
        let boundary = self.capture.identity.boundary();
        let automatic = self.automatic;
        let original_allowed = self.original_allowed;
        self.calls.run(Box::pin(async move {
            checked?;
            let _guard = gate.lock().await;
            if cancellation.is_cancelled() { return Err(PortError::Denied); }
            let (record, identity) = { let state = state.lock().map_err(|_| PortError::Unavailable)?; (state.record.clone(), state.identity.clone()) };
            let current = sessions.load(&record.id).await.map_err(|_| PortError::Unavailable)?.record;
            if !identity.matches(&current) { return Err(PortError::Conflict); }
            if cancellation.is_cancelled() { return Err(PortError::Denied); }
            let source = |handle: &str| -> Result<SessionCopy, PortError> {
                let state = state.lock().map_err(|_| PortError::Unavailable)?;
                let (copy, original) = state.sources.get(handle).ok_or(PortError::Denied)?;
                if *original && !original_allowed { return Err(PortError::Denied); }
                Ok(copy.clone())
            };
            let original_snapshot = matches!(&operation, ContextOperation::Original(_));
            let result = match operation {
                ContextOperation::Snapshot(_) | ContextOperation::Original(_) => {
                    let handle = if original_snapshot { "original" } else { "effective" };
                    let copy = source(handle)?;
                    let masks = record.active_session_masks().into_iter().map(|active| mask_value(&active.mask)).collect::<Vec<_>>();
                    json!({"source":handle,"boundary":boundary.get().to_string(),"revision":record.revision.get().to_string(),
                        "items":timeline(&copy)?,"itemMetadata":copy.item_metadata(),"metadata":record.metadata.values,
                        "masks":masks,"automatic":automatic.is_some(),"contextTokens":automatic.and_then(|facts| facts.0),
                        "contextLimit":automatic.and_then(|facts| facts.1),
                        "config":{"inheritDefinition":record.agent_definition.is_some(),"inheritInference":record.inference_settings.is_some()}})
                }
                ContextOperation::History(args) => {
                    let copy = source(&args.source)?;
                    let selected = if let Some(ids) = args.item_ids {
                        let allowed = copy.timeline().into_iter().map(|event| event.item_id).collect::<BTreeSet<_>>();
                        if ids.iter().any(|id| !allowed.contains(id)) { return Err(PortError::Denied); }
                        SessionCopy::selected(&record, boundary, &ids).map_err(|_| PortError::Invalid)?
                    } else { copy };
                    let handle = uuid::Uuid::now_v7().to_string();
                    let value = bounded(json!({"source":handle,"items":timeline(&selected)?,"itemMetadata":selected.item_metadata()}))?;
                    let mut state = state.lock().map_err(|_| PortError::Unavailable)?;
                    if state.sources.len() >= 32 { return Err(PortError::Invalid); }
                    let original = state.sources.get(&args.source).ok_or(PortError::Denied)?.1;
                    state.sources.insert(handle, (selected, original));
                    value
                }
                ContextOperation::Metadata(args) => {
                    let copy = source(&args.source)?;
                    check_item(&copy, args.item_id.as_ref())?;
                    let target = target(&record, args.item_id);
                    metadata_value(record.metadata_values(&target).map_err(|_| PortError::Invalid)?.and_then(|values| values.get(&args.key)))
                }
                ContextOperation::Transact(args) => {
                    let copy = source(&args.source)?;
                    let mutation = mutation(&record, Some((&copy, identity)), args)?;
                    let lease = pause.lock().map_err(|_| PortError::Unavailable)?.clone();
                    let outcome = if mutation.masks.is_some() && lease.is_some() {
                        sessions.apply_paused_session_mutation(lease.as_ref().unwrap(), mutation).await.map_err(|_| PortError::Uncertain)?
                    } else { sessions.mutate_session(&record.id, mutation).await.map_err(store_error)? };
                    let cursor = outcome.masks.as_ref().map(|masks| masks.cursor);
                    let next_identity = ContextIdentity::capture(&outcome.record, boundary, &authority).map_err(|_| PortError::Uncertain)?;
                    let effective = SessionCopy::from_record_effective(&outcome.record, boundary).map_err(|_| PortError::Uncertain)?;
                    let original = if original_allowed { Some(SessionCopy::from_record(&outcome.record, boundary).map_err(|_| PortError::Uncertain)?) } else { None };
                    let mut state = state.lock().map_err(|_| PortError::Uncertain)?;
                    state.record = outcome.record;
                    state.identity = next_identity;
                    state.committed = true;
                    state.sources.insert("effective".into(), (effective, false));
                    if let Some(original) = original { state.sources.insert("original".into(), (original, true)); }
                    state.continuation = if lease.is_some() { cursor } else { None };
                    json!({"committed":true,"revision":state.record.revision.get().to_string(),"cursor":cursor.map(|cursor| cursor.get().to_string()),
                        "addedMasks":outcome.masks.map(|masks| masks.added_masks.iter().map(mask_value).collect::<Vec<_>>())})
                }
                ContextOperation::Create(args) => {
                    // The validated source read above is the linearization point, not a source lock.
                    // Destination persistence uses only this invocation's immutable copy and settings.
                    let copy = source(&args.source)?;
                    let id = SessionId::generate();
                    let destination = uuid::Uuid::now_v7().to_string();
                    if state.lock().map_err(|_| PortError::Unavailable)?.destinations.len() >= 16 { return Err(PortError::Invalid); }
                    let mut request = SessionCreateRequest::new(id.clone());
                    if args.inherit_definition { request.agent_definition = record.agent_definition.clone(); }
                    if args.inherit_inference { request.inference_settings = record.inference_settings.clone(); }
                    let messages = vec![SessionStartMessage::user(args.message).map_err(|_| PortError::Invalid)?];
                    request.start = if args.copy_history { SessionStart::Copy { copy, messages } } else { SessionStart::Fresh { messages } };
                    sessions.create(request).await.map_err(|_| PortError::Uncertain)?;
                    state.lock().map_err(|_| PortError::Uncertain)?.destinations.insert(destination.clone(), Some(id.clone()));
                    json!({"created":true,"destination":destination,"sessionId":id.as_str()})
                }
                ContextOperation::Start(args) => {
                    // Consume before dispatch. An ambiguous failure is never retried.
                    let id = state.lock().map_err(|_| PortError::Unavailable)?.destinations.get_mut(&args.destination)
                        .and_then(Option::take).ok_or(PortError::Denied)?;
                    sessions.start_durable_prompt(id.clone(), waltr_agent::AgentRunLimits::default()).await.map_err(|_| PortError::Uncertain)?;
                    json!({"started":true,"sessionId":id.as_str()})
                }
                ContextOperation::Continue(args) => {
                    let cursor = cursor(&args.cursor)?;
                    let lease = pause.lock().map_err(|_| PortError::Unavailable)?.clone().ok_or(PortError::Denied)?;
                    if state.lock().map_err(|_| PortError::Unavailable)?.continuation != Some(cursor) { return Err(PortError::Conflict); }
                    state.lock().map_err(|_| PortError::Unavailable)?.continuation = None;
                    sessions.arm_paused_continuation(&lease, cursor, waltr_agent::AgentRunLimits::default()).await.map_err(|_| PortError::Uncertain)?;
                    json!({"armed":true,"cursor":args.cursor})
                }
            };
            bounded(result)
        }))
    }
    fn cleanup(&self) -> PortFuture<()> {
        let sessions = self.sessions.clone();
        let pause = self.pause.clone();
        self.calls.cleanup(Box::pin(async move {
            let lease = pause.lock().map_err(|_| PortError::Unavailable)?.clone();
            if let Some(lease) = lease {
                sessions.release_pause(&lease).await.map_err(|_| PortError::Uncertain)?;
                pause.lock().map_err(|_| PortError::Uncertain)?.take();
            }
            Ok(())
        }))
    }
}

fn cursor(value: &str) -> Result<TimelineCursor, PortError> { value.parse::<u64>().map(TimelineCursor::new).map_err(|_| PortError::Invalid) }
fn validate_key(key: &str) -> Result<(), PortError> {
    if key.is_empty() || key.len() > 128 || key.starts_with('_') || key.starts_with("waltr.") || key.starts_with("engine.") || key.chars().any(char::is_control) { Err(PortError::Invalid) } else { Ok(()) }
}
fn validate_entries(args: &crate::routine::context::Transaction) -> Result<(), PortError> {
    args.expected_revision.parse::<u64>().map_err(|_| PortError::Invalid)?;
    if args.predicates.len() + args.updates.len() > 8192 { return Err(PortError::Invalid); }
    for entry in args.predicates.iter().chain(&args.updates) { validate_key(&entry.key)?; }
    Ok(())
}
fn metadata_value(value: Option<&Value>) -> Value { match value { Some(value) => json!({"state":"present","value":value}), None => json!({"state":"absent"}) } }
fn target(record: &SessionRecord, item: Option<TimelineItemId>) -> MetadataTarget {
    match item { Some(item_id) => MetadataTarget::Item { session_id: record.id.clone(), item_id }, None => MetadataTarget::Session }
}
fn check_item(copy: &SessionCopy, item: Option<&TimelineItemId>) -> Result<(), PortError> {
    if item.is_some_and(|id| !copy.timeline().iter().any(|event| &event.item_id == id)) { Err(PortError::Denied) } else { Ok(()) }
}
fn mutation(record: &SessionRecord, source: Option<(&SessionCopy, ContextIdentity)>, args: crate::routine::context::Transaction) -> Result<SessionMutation, PortError> {
    let root_source = matches!(args.source.as_str(), "effective" | "original");
    let convert = |entry: MetadataEntry| -> Result<(MetadataTarget, String, Option<Value>), PortError> {
        if let Some((copy, _)) = &source { check_item(copy, entry.item_id.as_ref())?; }
        Ok((target(record, entry.item_id), entry.key, entry.value.into_option()))
    };
    let predicates = args.predicates.into_iter().map(|entry| convert(entry).map(|(target,key,expected)| MetadataPredicate { target,key,expected })).collect::<Result<Vec<_>,_>>()?;
    let updates = args.updates.into_iter().map(|entry| convert(entry).map(|(target,key,value)| MetadataUpdate { target,key,value })).collect::<Result<Vec<_>,_>>()?;
    let masks = if let Some(args) = args.masks {
        if !root_source { return Err(PortError::Denied); }
        let (copy, identity) = source.as_ref().ok_or(PortError::Denied)?;
        let mut masks = SessionMaskMutation::new(record.last_timeline_cursor());
        masks.expected_context = Some(identity.clone());
        masks.expected_completed_source = Some(identity.clone());
        masks.expected_active_masks = Some(args.expected.clone());
        // Compare the complete map even without the sibling-mutation mode.
        let actual = record.active_session_masks().into_iter().map(|active| (active.mask.key,active.mask.id)).collect::<BTreeMap<_,_>>();
        if args.expected.len() != actual.len() || args.expected.into_iter().collect::<BTreeMap<_,_>>() != actual { return Err(PortError::Conflict); }
        masks.subtract.keys = args.remove_keys;
        for through in args.remove_overlaps { let through = cursor(&through)?; if through > copy.boundary() { return Err(PortError::Denied); } masks.subtract.overlaps.push(waltr_agent::ThroughCursor::new(through)); }
        for addition in args.add {
            let through = cursor(&addition.through)?;
            if through > copy.boundary() { return Err(PortError::Denied); }
            let replacement = ReplacementUserContent::new(addition.replacement);
            let addition = if let Some(ids) = addition.item_ids {
                for id in &ids { check_item(copy, Some(id))?; }
                SessionMaskAddition::items(addition.key, through, ids, replacement)
            } else { SessionMaskAddition::new(addition.key, waltr_agent::ThroughCursor::new(through), replacement) };
            masks.add.push(addition);
        }
        Some(masks)
    } else { None };
    let incarnation = record.metadata.incarnation.clone().ok_or(PortError::Conflict)?;
    Ok(SessionMutation { expected_incarnation: Some(incarnation), expected_revision: Some(Revision::new(args.expected_revision.parse().map_err(|_| PortError::Invalid)?)), expected_context: source.map(|(_, identity)| identity), predicates, updates, masks })
}
fn store_error(error: waltr_agent::SessionStoreError) -> PortError {
    match error {
        waltr_agent::SessionStoreError::StaleRevision { .. }
        | waltr_agent::SessionStoreError::StaleExecution { .. }
        | waltr_agent::SessionStoreError::ExecutionCompatibility { .. }
        | waltr_agent::SessionStoreError::MaskMutation { .. } => PortError::Conflict,
        waltr_agent::SessionStoreError::MetadataMutation { error: waltr_agent::MetadataError::Precondition } => PortError::Conflict,
        waltr_agent::SessionStoreError::MetadataMutation { .. } => PortError::Invalid,
        _ => PortError::Uncertain,
    }
}

fn timeline_item(event: waltr_agent::AgentTimelineEvent) -> Result<Value, PortError> {
    let mut value = serde_json::to_value(&event).map_err(|_| PortError::Invalid)?;
    value["cursor"] = json!(event.cursor.get().to_string());
    value["turn"] = json!(event.turn.to_string());
    Ok(value)
}
fn timeline(copy: &SessionCopy) -> Result<Vec<Value>, PortError> {
    copy.timeline().into_iter().map(timeline_item).collect()
}
fn mask_value(mask: &waltr_agent::SessionMask) -> Value {
    json!({"id":mask.id,"key":mask.key,"through":mask.selector.cursor.get().to_string(),
        "itemIds":mask.item_ids,"replacement":mask.replacement.text})
}

#[cfg(test)]
mod primitives_tests;
