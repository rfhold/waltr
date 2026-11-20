//! Codified permission policy and private approvals for the original live operation.

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

use super::{
    PermissionClaim, PermissionDenyMode, PermissionLevel, PermissionPattern, PermissionPolicy,
    PermissionResource, PermissionResourcePattern, PermissionResponse,
};
use crate::{
    agent_host::dispatcher::{CapabilityPolicy, PolicyFuture},
    interaction::{
        Interaction, InteractionBroker, InteractionDestination, InteractionError,
        InteractionFuture, InteractionResponse, OperationBinding,
    },
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use waltr_agent::{SessionId, ToolExecutionContext};

#[derive(Default)]
struct Approvals {
    patterns: BTreeMap<(String, String, String, String), Vec<PermissionPattern>>,
    once: BTreeMap<String, Vec<String>>,
}

/// Admission never derives authority from session metadata or an upstream request key.
pub struct ServerPermissionPolicy {
    broker: InteractionBroker,
    approvals: Mutex<Approvals>,
    routine: Option<RoutinePermissionBinding>,
}

type SourceCheck =
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync;

struct RoutinePermissionBinding {
    operation: OperationBinding,
    session: SessionId,
    incarnation: String,
    current: Arc<SourceCheck>,
}

impl ServerPermissionPolicy {
    pub fn sessions(broker: InteractionBroker) -> Arc<Self> {
        Arc::new(Self {
            broker,
            approvals: Mutex::default(),
            routine: None,
        })
    }

    /// Use the normal permission policy with a fresh cache and an already authorized live
    /// source route. This binding is usable only by the matching durable Routine operation.
    pub(crate) fn routine(
        normal: &dyn CapabilityPolicy,
        context: &ToolExecutionContext,
        incarnation: String,
        current: Arc<SourceCheck>,
    ) -> Option<Arc<Self>> {
        let session = context.session_id()?.clone();
        let dispatch = context.host_dispatch()?;
        let (broker, operation) = normal.interaction_binding(context)?;
        if incarnation.is_empty()
            || operation.operation_id != dispatch.invocation_id
            || !matches!(&operation.destination, InteractionDestination::Session(id) if *id == session)
            || operation.cancellation.is_cancelled()
        {
            return None;
        }
        Some(Arc::new(Self {
            broker,
            approvals: Mutex::default(),
            routine: Some(RoutinePermissionBinding {
                operation,
                session,
                incarnation,
                current,
            }),
        }))
    }

    pub(crate) async fn routine_current(&self, context: &ToolExecutionContext) -> bool {
        if let Some(routine) = &self.routine {
            self.binding(context).is_ok()
                && (routine.current)().await
                && !context.is_cancelled()
                && !routine.operation.cancellation.is_cancelled()
        } else {
            false
        }
    }

    fn binding(
        &self,
        context: &ToolExecutionContext,
    ) -> Result<OperationBinding, InteractionError> {
        let operation = context
            .host_dispatch()
            .ok_or(InteractionError::Unavailable)?;
        if let Some(routine) = &self.routine {
            return if operation.invocation_id == routine.operation.operation_id
                && context.session_id() == Some(&routine.session)
                && !context.is_cancelled()
                && !routine.operation.cancellation.is_cancelled()
            {
                Ok(routine.operation.clone())
            } else {
                Err(InteractionError::Unavailable)
            };
        }
        let destination = InteractionDestination::Session(
            context
                .session_id()
                .ok_or(InteractionError::Unavailable)?
                .clone(),
        );
        Ok(OperationBinding {
            operation_id: operation.invocation_id.clone(),
            destination,
            cancellation: context.cancellation_token().clone(),
        })
    }
    pub fn retire_session(&self, session: &waltr_agent::SessionId) {
        self.approvals
            .lock()
            .expect("permission lock poisoned")
            .patterns
            .retain(|(id, _, _, _), _| id != session.as_str());
        crate::agent_host::middleware::mcp_skills::retire_session_consents(session);
    }
}

impl CapabilityPolicy for ServerPermissionPolicy {
    fn interaction_binding(
        &self,
        context: &ToolExecutionContext,
    ) -> Option<(InteractionBroker, OperationBinding)> {
        let binding = self.binding(context).ok()?;
        if matches!(binding.destination, InteractionDestination::Unavailable) {
            return None;
        }
        Some((self.broker.clone(), binding))
    }
    fn authorize<'a>(
        &'a self,
        context: &'a ToolExecutionContext,
        identity: &'a str,
        policy: &'a PermissionPolicy,
        claims: &'a [PermissionClaim],
    ) -> PolicyFuture<'a> {
        Box::pin(async move {
            let binding = match self.binding(context) {
                Ok(binding) => binding,
                Err(_) => {
                    return Ok(Some(
                        "permission operation has no interaction destination".into(),
                    ));
                }
            };
            if context.is_cancelled() {
                return Ok(Some("permission operation cancelled".into()));
            }
            if self.routine.is_some() && !self.routine_current(context).await {
                return Ok(Some("Routine source authority is no longer current".into()));
            }
            let call = context.call();
            let input = policy.evaluate_input(&call.name, &call.arguments);
            let tool_effect = policy.evaluate_target_tool_effect(identity, &call.name);
            let tool = tool_effect.level;
            if tool == PermissionLevel::Deny {
                return Ok(Some(
                    tool_effect
                        .message
                        .unwrap_or_else(|| "permission denied by server policy".into()),
                ));
            }
            if let Some(input) = input
                .as_ref()
                .filter(|input| input.effect.level == PermissionLevel::Deny)
            {
                return Ok(Some(input.effect.message.clone().unwrap_or_else(|| {
                    "permission denied: tool input does not satisfy policy".into()
                })));
            }
            let denied = claims
                .iter()
                .filter_map(|claim| {
                    let effect = policy.evaluate_claim_effect(&call.name, claim);
                    (effect.level == PermissionLevel::Deny).then(|| (claim.clone(), effect))
                })
                .collect::<Vec<_>>();
            if !denied.is_empty() {
                return Ok(Some(super::policy_denial_message(&denied)));
            }
            let Some(incarnation) = self
                .routine
                .as_ref()
                .map(|routine| routine.incarnation.as_str())
                .or_else(|| context.session_incarnation())
            else {
                return Ok(Some(
                    "permission operation has no durable session incarnation".into(),
                ));
            };
            let key = (
                context
                    .session_id()
                    .expect("session binding")
                    .as_str()
                    .to_owned(),
                incarnation.to_owned(),
                identity.to_owned(),
                policy.identity(),
            );
            let patterns = self
                .approvals
                .lock()
                .expect("permission lock poisoned")
                .patterns
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let mut ask = claims
                .iter()
                .filter(|claim| {
                    policy
                        .evaluate_claim_refinement_effect(&call.name, claim)
                        .is_some_and(|e| e.level == PermissionLevel::Ask)
                })
                .cloned()
                .collect::<Vec<_>>();
            if let Some(input) = &input {
                if input.effect.level == PermissionLevel::Ask {
                    ask.extend(input.claims.clone());
                }
            }
            ask.retain(|claim| !patterns.iter().any(|pattern| pattern.matches(claim)));
            if tool != PermissionLevel::Ask && ask.is_empty() {
                return Ok(None);
            }
            let receipt = if self.routine.is_some() {
                // Discovery and full dispatch may build different rule sets. Reuse consent
                // only for the same alias, arguments and requested claims in this operation.
                serde_json::to_string(&(identity, &call.name, &call.arguments, &ask))?
            } else {
                serde_json::to_string(&(identity, policy.identity(), &call.arguments, &ask))?
            };
            if self
                .approvals
                .lock()
                .expect("permission lock poisoned")
                .once
                .get(&binding.operation_id)
                .is_some_and(|receipts| receipts.contains(&receipt))
            {
                return Ok(None);
            }
            let suggested = if self.routine.is_some() {
                Vec::new()
            } else {
                suggested_patterns(&ask)
            };
            let response = self
                .broker
                .interact(
                    &binding,
                    Interaction::Permission {
                        message: format!("Allow {}?", call.name),
                        identity: identity.into(),
                        claims: ask,
                        patterns: suggested,
                    },
                )
                .await;
            if self.routine.is_some() && !self.routine_current(context).await {
                return Ok(Some("Routine source authority is no longer current".into()));
            }
            match response {
                Ok(InteractionResponse::Permission(response)) if !context.is_cancelled() => {
                    match response {
                        PermissionResponse::AllowOnce | PermissionResponse::AllowAlways { .. } => {
                            let mut approvals =
                                self.approvals.lock().expect("permission lock poisoned");
                            if approvals.once.len() >= 256
                                && !approvals.once.contains_key(&binding.operation_id)
                            {
                                return Ok(Some("permission approval capacity reached".into()));
                            }
                            if let PermissionResponse::AllowAlways { patterns } = response
                                && self.routine.is_none()
                            {
                                if approvals.patterns.len() >= 4096
                                    && !approvals.patterns.contains_key(&key)
                                {
                                    return Ok(Some(
                                        "session permission approval capacity reached".into(),
                                    ));
                                }
                                let stored = approvals.patterns.entry(key).or_default();
                                if stored.len() + patterns.len() > 256 {
                                    return Ok(Some(
                                        "session permission pattern capacity reached".into(),
                                    ));
                                }
                                for pattern in patterns {
                                    if !stored.contains(&pattern) {
                                        stored.push(pattern);
                                    }
                                }
                            }
                            let receipts = approvals.once.entry(binding.operation_id).or_default();
                            if receipts.len() >= 32 {
                                return Ok(Some(
                                    "operation permission approval capacity reached".into(),
                                ));
                            }
                            receipts.push(receipt);
                            Ok(None)
                        }
                        PermissionResponse::Deny { message, mode } => {
                            if mode == PermissionDenyMode::CancelWork {
                                binding.cancellation.cancel();
                            }
                            Ok(Some(message.unwrap_or_else(|| "permission denied".into())))
                        }
                    }
                }
                _ => Ok(Some(
                    "permission interaction unavailable or cancelled".into(),
                )),
            }
        })
    }
    fn interact<'a>(
        &'a self,
        context: &'a ToolExecutionContext,
        request: Interaction,
    ) -> InteractionFuture<'a> {
        Box::pin(async move {
            if self.routine.is_some() && !self.routine_current(context).await {
                return Err(InteractionError::Unavailable);
            }
            let response = self
                .broker
                .interact(&self.binding(context)?, request)
                .await?;
            if self.routine.is_some() && !self.routine_current(context).await {
                return Err(InteractionError::Unavailable);
            }
            Ok(response)
        })
    }
    fn finished(&self, operation_id: &str) {
        self.approvals
            .lock()
            .expect("permission lock poisoned")
            .once
            .remove(operation_id);
    }
    fn retire_session(&self, session: &waltr_agent::SessionId) {
        ServerPermissionPolicy::retire_session(self, session);
    }
}

fn suggested_patterns(claims: &[PermissionClaim]) -> Vec<PermissionPattern> {
    let mut patterns = Vec::new();
    for claim in claims {
        let resource = match &claim.resource {
            PermissionResource::FilesystemPath(path) => {
                PermissionResourcePattern::FilesystemPathPrefix(
                    path.parent().unwrap_or(path).to_owned(),
                )
            }
            PermissionResource::FilesystemPathPrefix(path) => {
                PermissionResourcePattern::FilesystemPathPrefix(path.clone())
            }
            PermissionResource::WorkingDirectory(path) => {
                PermissionResourcePattern::WorkingDirectoryPrefix(path.clone())
            }
            PermissionResource::ShellCommand(command) => {
                PermissionResourcePattern::ShellCommand(command.clone())
            }
            PermissionResource::ToolInput {
                tool,
                pointer,
                value,
            } => PermissionResourcePattern::ToolInput {
                tool: tool.clone(),
                pointer: pointer.clone(),
                value: value.clone(),
            },
        };
        let pattern = PermissionPattern {
            operation: Some(claim.operation),
            resource,
        };
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}
