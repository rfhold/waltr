use super::*;
use crate::routine::{
    history::{Origin, Ownership, Status},
    rpc::{
        catalog::{CatalogEntry, PublicDefinition},
        invoke_authorized,
    },
    service::ChildInvocation,
};

// Constructed only inside the already-authorized manage-context handler. Never serialized or cloned.
pub(super) struct ApprovedContextAdmission {
    session: SessionId,
    incarnation: String,
    principal: String,
    revision: String,
    routine: String,
    entrypoint: String,
    call: String,
    policy: PermissionPolicy,
    cancellation: CancellationToken,
}

impl ApprovedContextAdmission {
    pub(super) fn consume(self, request: &StartRequest, record: &waltr_agent::SessionRecord) -> Result<PermissionPolicy, PortError> {
        if request.source_session.as_deref() != Some(self.session.as_str())
            || record.id != self.session
            || record.metadata.incarnation.as_deref() != Some(self.incarnation.as_str())
            || request.principal != self.principal
            || request.definition.revision() != self.revision
            || request.definition.envelope().name != self.routine
            || request.entrypoint != self.entrypoint
            || self.call.is_empty()
            || self.cancellation.is_cancelled()
            || self.policy.evaluate_tool("manage-context") == PermissionLevel::Deny
        { return Err(PortError::Denied); }
        Ok(self.policy)
    }
}

pub(super) fn original_source_allowed(capabilities: &BTreeSet<String>, policy: &PermissionPolicy, approved: bool) -> bool {
    capabilities.contains("context.original")
        && (approved || policy.evaluate_tool("manage-context") == PermissionLevel::Allow)
}

pub(super) fn provide<S: SessionStore + Clone + Send + Sync + 'static>(
    owner: std::sync::Weak<Composition<S>>,
    principal: AuthenticatedPrincipal,
    record: &waltr_agent::SessionRecord,
    _runtime: &waltr_agent::AgentRuntime,
    policy: crate::permission::PermissionPolicy,
) -> Result<Vec<crate::agent_host::Capability>, waltr_agent::AgentSessionRuntimeError> {
    let incarnation = record.metadata.incarnation.clone().ok_or_else(|| {
        waltr_agent::AgentSessionRuntimeError::FailedPrecondition(
            "session incarnation required".into(),
        )
    })?;
    let mut tools = Vec::new();
    for (name, routine, entrypoint, schema) in [
        (
            "manage-context",
            "context-management",
            "session-tool",
            serde_json::json!({"type":"object","properties":{"instruction":{"type":"string","maxLength":8192}},"required":["instruction"],"additionalProperties":false}),
        ),
        (
            "configure-waltr",
            "configuration",
            "tool",
            serde_json::json!({"type":"object","properties":{"prompt":{"type":"string","maxLength":12288}},"required":["prompt"],"additionalProperties":false}),
        ),
    ] {
        let owner = owner.clone();
        let principal = principal.clone();
        let session = record.id.clone();
        let incarnation = incarnation.clone();
        let policy = policy.clone();
        tools.push(waltr_agent::AgentTool::new_async(waltr_agent::ToolDefinition {
            name: name.into(), description: format!("Run the bounded {routine} Routine against this session under normal permissions."), input_schema: schema,
        }, move |context| {
            let owner = owner.clone(); let principal = principal.clone(); let session = session.clone(); let incarnation = incarnation.clone(); let policy = policy.clone();
            Box::pin(async move {
                let run = async {
                    if context.call().name != name || context.session_id() != Some(&session) || context.session_incarnation() != Some(incarnation.as_str()) { return Err(PortError::Denied); }
                    let owner = owner.upgrade().ok_or(PortError::Unavailable)?;
                    let current = owner.sessions.load(&session).await.map_err(|_| PortError::Denied)?.record;
                    if current.metadata.incarnation.as_deref() != Some(incarnation.as_str()) { return Err(PortError::Denied); }
                    let definition = owner.state.snapshot().scripts().get(routine).cloned().ok_or(PortError::Unavailable)?;
                    let approved = if name == "manage-context" {
                        Some(ApprovedContextAdmission { session: session.clone(), incarnation: incarnation.clone(), principal: principal_id(&principal), revision: definition.revision().to_string(), routine: definition.envelope().name.clone(), entrypoint: entrypoint.into(), call: context.call().id.clone(), policy: policy.clone(), cancellation: context.cancellation_token().clone() })
                    } else { None };
                    let authorized = owner.authorize_context_policy(StartRequest { authenticated: principal.clone(), transport: None, principal: principal_id(&principal), definition: definition.clone(), entrypoint: entrypoint.into(), source_session: Some(session.as_str().into()) }, None, Some(policy), approved).await?;
                    let public = serde_json::to_value(PublicDefinition::capture(&CatalogEntry { definition: definition.clone(), offers: BTreeMap::new() })?).map_err(|_| PortError::Invalid)?;
                    let child = invoke_authorized(&owner.service, ChildInvocation { definition, entrypoint: entrypoint.into(), caller: Caller { principal: principal_id(&principal), origin: Origin::Tool, ownership: Ownership::Tool, correlation: context.call().id.clone() }, input: context.call().arguments.clone() }, authorized, public, None)?;
                    let handle = child.admitted.await?;
                    let outcome = tokio::select! { biased; () = context.cancellation_token().cancelled() => { handle.cancel(); handle.join().await }, outcome = handle.join() => outcome };
                    if outcome.status != Some(Status::Succeeded) || !outcome.cleanup_complete { return Err(PortError::Uncertain); }
                    Ok(outcome.result.unwrap_or(Value::Null))
                }.await;
                match run {
                    Ok(value) => Ok(waltr_agent::ToolResult::new(context.call().id.clone(), context.call().name.clone(), value.to_string())),
                    Err(error) => Err(waltr_agent::LlmError::Protocol { provider: "routine", message: format!("Routine tool failed: {error:?}; interrupted effects must not be replayed") }),
                }
            })
        }).into());
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approved_ask_evidence_is_consumed_only_for_exact_call_target_and_incarnation() {
        let engine = waltr_script::Engine::new(1).unwrap();
        let definition = Definition::parse(&serde_json::json!({
            "apiVersion":"waltr.routine/v1","kind":"Routine","name":"context-fixture","description":"Admission evidence contract",
            "spec":{"source":"export default async function () { return null; }","entrypoints":{"run":{"trigger":"manual"}},
                "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
                "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},"capabilities":["context.original"]}
        }).to_string(), &engine).unwrap();
        let authenticated = AuthenticatedPrincipal::local_process(&crate::local_admission::ValidatedLocalOrigin(Arc::new(CancellationToken::new())));
        let store = waltr_agent::MemorySessionStore::new();
        let record = store.create(SessionCreateRequest::new(SessionId::generate())).await.unwrap();
        let mut request = StartRequest { principal:principal_id(&authenticated), authenticated, transport:None,
            definition, entrypoint:"run".into(), source_session:Some(record.id.as_str().into()) };
        let evidence = || ApprovedContextAdmission { session:record.id.clone(), incarnation:record.metadata.incarnation.clone().unwrap(),
            principal:request.principal.clone(), routine:request.definition.envelope().name.clone(), revision:request.definition.revision().into(),
            entrypoint:"run".into(), call:"exact-approved-call".into(), policy:PermissionPolicy::new(PermissionLevel::Ask), cancellation:CancellationToken::new() };
        let caps = BTreeSet::from(["context.original".into()]);
        let approved = evidence().consume(&request, &record).unwrap();
        assert!(original_source_allowed(&caps, &approved, true));
        assert!(!original_source_allowed(&caps, &approved, false), "direct Ask has no live approval");
        assert!(!original_source_allowed(&caps, &PermissionPolicy::new(PermissionLevel::Deny), false));
        assert!(original_source_allowed(&caps, &PermissionPolicy::new(PermissionLevel::Allow), false));
        assert!(!original_source_allowed(&BTreeSet::new(), &approved, true), "approval does not add capabilities");
        for fault in 0..8 {
            let mut changed = evidence();
            match fault {
                0 => changed.session = SessionId::generate(),
                1 => changed.incarnation = "other-incarnation".into(),
                2 => changed.principal = "other-principal".into(),
                3 => changed.revision = "other-revision".into(),
                4 => changed.entrypoint = "another-entrypoint".into(),
                5 => changed.routine = "configuration".into(),
                6 => changed.cancellation.cancel(),
                _ => changed.call.clear(),
            }
            assert!(matches!(changed.consume(&request, &record), Err(PortError::Denied)));
        }
        let fixed = evidence();
        request.source_session = Some(SessionId::generate().as_str().into());
        assert!(matches!(fixed.consume(&request, &record), Err(PortError::Denied)));
    }
}
