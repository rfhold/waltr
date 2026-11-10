//! Process-local provenance supplied only by actual session producers.

use std::{collections::{BTreeMap, BTreeSet}, sync::{Arc, Mutex}};
use waltr_agent::{SessionId, SessionRecord};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskOrigin {
    pub parent: SessionId,
    pub incarnation: String,
    pub turn: u64,
    pub call: String,
    pub depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionRole {
    Helper,
    Ephemeral,
    Task(TaskOrigin),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SessionRoles(Arc<Mutex<Roles>>);

pub(crate) struct PrivateCreationEvents;
impl waltr_agent::SessionEventSink for PrivateCreationEvents {
    fn send(&self, _: waltr_agent::SessionEvent) -> waltr_agent::SessionEventSinkFuture {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Default)]
struct Roles {
    roles: BTreeMap<(SessionId, String), SessionRole>,
    completed: BTreeSet<(SessionId, String)>,
}

impl SessionRoles {
    pub fn register(&self, record: &SessionRecord, role: SessionRole) -> Result<(), &'static str> {
        let incarnation = record.metadata.incarnation.clone().ok_or("missing incarnation")?;
        let bounded = |value: &str, limit| !value.is_empty() && value.len() <= limit;
        if !bounded(record.id.as_str(), 256) || !bounded(&incarnation, 128) {
            return Err("invalid session identity");
        }
        if let SessionRole::Task(origin) = &role {
            if !bounded(origin.parent.as_str(), 256) || !bounded(&origin.incarnation, 128) || !bounded(&origin.call, 256) {
                return Err("invalid task origin");
            }
        }
        let mut roles = self.0.lock().map_err(|_| "provenance unavailable")?;
        let key = (record.id.clone(), incarnation);
        if let Some(existing) = roles.roles.get(&key) {
            return if existing == &role { Ok(()) } else { Err("provenance mismatch") };
        }
        if roles.roles.len() >= 4096 { return Err("provenance capacity reached"); }
        roles.roles.insert(key, role);
        Ok(())
    }
    pub fn get(&self, id: &SessionId, incarnation: Option<&str>) -> Option<SessionRole> {
        self.0.lock().expect("session provenance poisoned").roles.get(&(id.clone(), incarnation?.to_owned())).cloned()
    }
    pub fn complete_task(&self, event: &waltr_agent::SessionSemanticEvent) -> Option<TaskOrigin> {
        if !matches!(event.kind, waltr_agent::SessionSemanticKind::RunFinished) { return None; }
        let key = (event.session_id.clone(), event.incarnation.clone()?);
        let mut roles = self.0.lock().ok()?;
        let SessionRole::Task(origin) = roles.roles.get(&key)?.clone() else { return None };
        if !roles.completed.insert(key) { return None; }
        Some(origin)
    }
    pub fn excluded(&self, record: &SessionRecord) -> bool {
        self.get(&record.id, record.metadata.incarnation.as_deref()).is_some()
    }
    pub fn source_excluded(&self, record: &SessionRecord) -> bool {
        matches!(self.get(&record.id, record.metadata.incarnation.as_deref()), Some(SessionRole::Helper | SessionRole::Ephemeral))
    }
    pub fn forget(&self, id: &SessionId) {
        if let Ok(mut roles) = self.0.lock() {
            roles.roles.retain(|(session, _), _| session != id);
            roles.completed.retain(|(session, _)| session != id);
        }
    }
    pub fn clear(&self) {
        *self.0.lock().expect("session provenance poisoned") = Roles::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_values_and_recreated_ids_have_no_private_authority() {
        let roles = SessionRoles::default();
        let mut first = SessionRecord::new("source");
        first.metadata.values.insert("helperPrincipal".into(), serde_json::json!("forged"));
        first.metadata.values.insert("waltr.session.ephemeral".into(), serde_json::json!(true));
        assert!(!roles.excluded(&first));
        roles.register(&first, SessionRole::Helper).unwrap();
        assert!(roles.excluded(&first));
        let replacement = SessionRecord::new("source");
        assert!(!roles.excluded(&replacement));
        roles.forget(&first.id);
        assert!(!roles.excluded(&first));
        assert!(!SessionRoles::default().excluded(&first));
    }

    #[test]
    fn only_the_actual_child_incarnation_completes_its_bound_parent_once() {
        let roles = SessionRoles::default();
        let child = SessionRecord::new("child");
        let origin = TaskOrigin { parent: SessionId::new("parent"), incarnation: "parent-incarnation".into(), turn: 4, call: "call".into(), depth: 1 };
        roles.register(&child, SessionRole::Task(origin.clone())).unwrap();
        assert!(roles.excluded(&child));
        assert!(!roles.source_excluded(&child));
        let mut event = waltr_agent::SessionSemanticEvent { id: "finished".into(), session_id: child.id.clone(), incarnation: child.metadata.incarnation.clone(),
            revision: waltr_agent::Revision::new(1), cursor: None, kind: waltr_agent::SessionSemanticKind::ToolResultCommitted { call_id: "call".into(), tool_name: "task".into(), succeeded: true } };
        assert!(roles.complete_task(&event).is_none());
        event.kind = waltr_agent::SessionSemanticKind::RunFinished;
        event.incarnation = Some("forged-incarnation".into());
        assert!(roles.complete_task(&event).is_none());
        event.incarnation = child.metadata.incarnation.clone();
        assert_eq!(roles.complete_task(&event), Some(origin));
        event.id = "another-run".into();
        assert!(roles.complete_task(&event).is_none());
        roles.clear();
        assert!(roles.complete_task(&event).is_none());
    }
}
