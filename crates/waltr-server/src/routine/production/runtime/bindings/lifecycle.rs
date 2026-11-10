use super::*;
use crate::routine::{
    adapters::context::automatic::{AutomaticCompactionAdmission, AutomaticCompactionHandler},
    history::{Origin, Ownership},
    rpc::{
        catalog::{CatalogEntry, PublicDefinition},
        invoke_authorized,
    },
    service::{ChildInvocation, InvocationHandle},
};
use std::sync::Weak;
use waltr_agent::{
    SessionEvent, SessionEventSink, SessionEventSinkFuture, SessionRunCompletionSnapshot,
};

pub(super) struct Scope<S: SessionStore + Clone + Send + Sync + 'static> {
    principal: AuthenticatedPrincipal,
    revision: String,
    inference: (String, String),
    events: Arc<Events<S>>,
    pub semantic: Arc<super::semantic::Semantic>,
    pub completion: Arc<AutomaticCompactionHandler<S>>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> Composition<S> {
    pub(super) async fn run_scope(
        &self,
        principal: AuthenticatedPrincipal,
        record: &waltr_agent::SessionRecord,
    ) -> Result<
        (
            Arc<dyn SessionEventSink>,
            Arc<dyn waltr_agent::SessionCompletionCoordinationHandler>,
        ),
        PortError,
    > {
        let _maintenance = self.scope_maintenance.lock().await;
        let retire = {
            let scopes = self.scopes.lock().map_err(|_| PortError::Uncertain)?;
            if scopes.len() >= 128 {
                scopes
                    .iter()
                    .enumerate()
                    .find(|(_, scope)| {
                        Arc::strong_count(&scope.events) == 1
                            && Arc::strong_count(&scope.completion) == 1
                    })
                    .map(|(index, scope)| (index, scope.completion.clone(), scope.semantic.clone()))
            } else {
                None
            }
        };
        if let Some((index, handler, semantic)) = retire {
            semantic.shutdown().await?;
            handler.shutdown().await?;
            self.scopes
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .remove(index);
        }
        let snapshot = self.state.snapshot();
        let settings = record
            .inference_settings
            .clone()
            .unwrap_or(default_inference(&snapshot)?);
        let inference = (
            settings.provider.key.clone(),
            settings.provider.model_id.clone(),
        );
        let mut scopes = self.scopes.lock().map_err(|_| PortError::Uncertain)?;
        if let Some(scope) = scopes.iter().find(|scope| {
            scope.principal.same_access_lease(&principal)
                && scope.revision == snapshot.etag()
                && scope.inference == inference
        }) {
            scope.semantic.watch(record)?;
            return Ok((scope.events.clone(), scope.completion.clone()));
        }
        if scopes.len() >= 128 || self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PortError::Busy);
        }
        let owner = self.me.get().cloned().ok_or(PortError::Unavailable)?;
        let mut limits = BTreeMap::new();
        if let Some(limit) = snapshot
            .configuration()
            .providers
            .get(&inference.0)
            .and_then(|provider| provider.models.get(&inference.1))
            .and_then(|model| model.limits)
            .and_then(|limits| limits.context)
            .filter(|limit| *limit != 0)
        {
            limits.insert(inference.clone(), u64::from(limit));
        }
        let completion = Arc::new(AutomaticCompactionHandler::new(
            self.sessions.clone(),
            principal_id(&principal),
            limits,
            Arc::new(Compaction {
                owner: owner.clone(),
                principal: principal.clone(),
            }),
        )?.with_session_roles(self.factory.session_roles.clone()));
        let semantic = Arc::new(super::semantic::Semantic::new(
            owner.clone(),
            principal.clone(),
            self.sessions.clone(),
            record,
        )?);
        let events = Arc::new(Events {
            owner,
            principal: principal.clone(),
            semantic: semantic.clone(),
        });
        scopes.push(Scope {
            principal,
            revision: snapshot.etag().to_owned(),
            inference,
            events: events.clone(),
            semantic,
            completion: completion.clone(),
        });
        Ok((events, completion))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn semantic(
        &self,
        principal: AuthenticatedPrincipal,
        source: SessionId,
        name: &str,
        expected_revision: Option<&str>,
        entrypoint: &str,
        origin: Origin,
        input: Value,
        context: Option<Arc<CoordinatorContext<S>>>,
    ) -> Result<InvocationHandle, PortError> {
        let definition = self
            .state
            .snapshot()
            .scripts()
            .get(name)
            .cloned()
            .ok_or(PortError::Invalid)?;
        if expected_revision.is_some_and(|revision| revision != definition.revision()) {
            return Err(PortError::Conflict);
        }
        let authorized = self
            .authorize_context(
                StartRequest {
                    authenticated: principal.clone(),
                    transport: None,
                    principal: principal_id(&principal),
                    definition: definition.clone(),
                    entrypoint: entrypoint.into(),
                    source_session: Some(source.as_str().into()),
                },
                context,
            )
            .await?;
        let public = serde_json::to_value(PublicDefinition::capture(&CatalogEntry {
            definition: definition.clone(),
            offers: BTreeMap::new(),
        })?)
        .map_err(|_| PortError::Invalid)?;
        let child = invoke_authorized(
            &self.service,
            ChildInvocation {
                definition,
                entrypoint: entrypoint.into(),
                caller: Caller {
                    principal: principal_id(&principal),
                    origin,
                    ownership: Ownership::Detached,
                    correlation: uuid::Uuid::new_v4().to_string(),
                },
                input,
            },
            authorized,
            public,
            None,
        )?;
        child.admitted.await
    }
}
struct Events<S: SessionStore + Clone + Send + Sync + 'static> {
    owner: Weak<Composition<S>>,
    principal: AuthenticatedPrincipal,
    semantic: Arc<super::semantic::Semantic>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> SessionEventSink for Events<S> {
    fn semantic(&self, event: waltr_agent::SessionSemanticEvent) {
        if let Some(owner) = self.owner.upgrade() {
            let role = owner.factory.session_roles.get(&event.session_id, event.incarnation.as_deref());
            if role.is_none() { self.semantic.publish(event); }
            else if owner.factory.session_roles.complete_task(&event).is_some() {
                self.semantic.publish(event);
            }
        }
    }
    fn send(&self, event: SessionEvent) -> SessionEventSinkFuture {
        let owner = self.owner.clone();
        let principal = self.principal.clone();
        let SessionEvent::UserMessageCreated { session_id, incarnation, user_message_count, .. } = &event;
        let predecessor = if *user_message_count == 1 {
            if let Some((owner, incarnation)) = owner.upgrade().zip(incarnation.as_ref()) {
                if owner.factory.session_roles.get(session_id, Some(incarnation)).is_some() {
                    return Box::pin(async { Ok(()) });
                }
                let (send, receive) = tokio::sync::oneshot::channel();
                let Ok(mut predecessors) = owner.completion_predecessors.lock() else {
                    return Box::pin(async { Err("Routine owner unavailable".to_owned()) });
                };
                predecessors.retain(|(id, _), _| id != session_id.as_str());
                if predecessors.len() >= 128 {
                    return Box::pin(async { Err("Routine completion owners are busy".to_owned()) });
                }
                predecessors.insert((session_id.as_str().into(), incarnation.clone()), receive);
                Some(send)
            } else { None }
        } else { None };
        Box::pin(async move {
            let SessionEvent::UserMessageCreated {
                session_id,
                incarnation,
                user_message_count,
                ..
            } = event;
            if user_message_count != 1 {
                return Ok(());
            }
            let owner = owner
                .upgrade()
                .ok_or_else(|| "Routine admission stopped".to_owned())?;
            let record = owner.sessions.load(&session_id).await.map_err(|_| "source unavailable".to_owned())?.record;
            if incarnation.is_none() || incarnation != record.metadata.incarnation || owner.factory.session_roles.excluded(&record) { return Ok(()); }
            let title = owner
                .semantic(
                    principal,
                    session_id.clone(),
                    "title",
                    None,
                    "first-user-message",
                    Origin::Event,
                    Value::Null,
                    None,
                )
                .await
                .map_err(|_| "Routine title admission failed".to_owned())?;
            if let Some(predecessor) = predecessor { drop(predecessor.send(title)); }
            Ok(())
        })
    }
}
struct Compaction<S: SessionStore + Clone + Send + Sync + 'static> {
    owner: Weak<Composition<S>>,
    principal: AuthenticatedPrincipal,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> AutomaticCompactionAdmission<S>
    for Compaction<S>
{
    fn settle(&self, mut snapshot: SessionRunCompletionSnapshot) -> PortFuture<SessionRunCompletionSnapshot> {
        let owner = self.owner.clone();
        Box::pin(async move {
            let owner = owner.upgrade().ok_or(PortError::Unavailable)?;
            let key = (snapshot.session_id.as_str().to_owned(), snapshot.incarnation.clone().ok_or(PortError::Conflict)?);
            let predecessor = owner.completion_predecessors.lock().map_err(|_| PortError::Uncertain)?.remove(&key);
            if let Some(predecessor) = predecessor {
                let predecessor = predecessor.await.map_err(|_| PortError::Unavailable)?;
                if !predecessor.join().await.cleanup_complete { return Err(PortError::Uncertain); }
                let record = owner.sessions.load(&snapshot.session_id).await.map_err(|_| PortError::Unavailable)?.record;
                if record.metadata.incarnation != snapshot.incarnation || record.last_timeline_cursor() != Some(snapshot.terminal_cursor) {
                    return Err(PortError::Conflict);
                }
                snapshot.revision = record.revision;
            }
            Ok(snapshot)
        })
    }
    fn admit(
        &self,
        context: Arc<CoordinatorContext<S>>,
        snapshot: SessionRunCompletionSnapshot,
    ) -> PortFuture<InvocationHandle> {
        let owner = self.owner.clone();
        let principal = self.principal.clone();
        Box::pin(async move {
            owner
                .upgrade()
                .ok_or(PortError::Unavailable)?
                .semantic(
                    principal,
                    snapshot.session_id,
                    "compact-and-continue",
                    None,
                    "context-pressure",
                    Origin::Lifecycle,
                    serde_json::json!({}),
                    Some(context),
                )
                .await
        })
    }
}
