use super::*;
use crate::agent_host::{
    Capability, authorized_tools::ConcreteToolTarget, dispatcher::UnconfiguredPolicy,
    middleware::chain::MiddlewareChain,
};
use crate::permission::{
    PermissionEffect, PermissionLevel, PermissionPolicy, PermissionToolSelector,
};

#[tokio::test]
async fn scoped_app_effect_does_not_publish_raw_results_changed_by_after_middleware() {
    use crate::agent_host::middleware::{AgentMiddleware, HookOutcome, context::ToolResultContext};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use waltr_agent::{MemorySessionStore, SessionCreateRequest, ToolDefinition};
    struct Projection(bool);
    impl AgentMiddleware for Projection {
        async fn after_tool_call(
            &self,
            context: &mut ToolResultContext<'_>,
        ) -> waltr_agent::Result<HookOutcome> {
            if self.0 {
                context.result.content = waltr_agent::ToolResultContent::text("redacted");
            }
            Ok(HookOutcome::Continue)
        }
    }
    for redacted in [false, true] {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone());
        let session = SessionId::generate();
        sessions
            .create(SessionCreateRequest::new(session.clone()))
            .await
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let effects = count.clone();
        let capability = Capability::new_async(
            ToolDefinition::new("app-effect", "app", json!({"type":"object"})),
            move |context| {
                effects.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(context.result("original raw result")) })
            },
        );
        let mut chain = MiddlewareChain::new();
        chain.push(Projection(redacted));
        let dispatcher = CapabilityDispatcher::new(
            vec![capability.clone()],
            Some(session.clone()),
            crate::permission::ServerPermissionPolicy::sessions(
                crate::interaction::InteractionBroker::new(),
            ),
        )
        .unwrap()
        .with_context(chain, PermissionPolicy::new(PermissionLevel::Allow));
        let effect = dispatcher.scoped_effect(capability).unwrap();
        let record = store.load(&session).await.unwrap();
        let audit_session = session.clone();
        let result = sessions
            .execute_idle_host_tool(
                &session,
                record.revision,
                effect,
                ToolCall::new("app-call", "app-effect", json!({})),
                move |_| {
                    PendingTimelineEvent::new(
                        audit_session,
                        0,
                        PendingTimelineItemId::New,
                        TimelineEventKind::ItemFinished,
                        Some(TimelineChannel::from("mcp_app_audit")),
                        TimelineContent::Json {
                            value: json!({"known":true}),
                        },
                    )
                },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.is_err(), redacted);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let record = store.load(&session).await.unwrap();
        assert_eq!(
            record
                .timeline_events()
                .filter(|event| event
                    .channel
                    .as_ref()
                    .is_some_and(|channel| channel.as_str() == "mcp_app_audit"))
                .count(),
            usize::from(!redacted)
        );
        sessions.shutdown().await;
    }
}

#[test]
fn app_source_requires_the_original_exact_origin_declaration_link_and_current_policy() {
    let target = ConcreteToolTarget::new("upstream", "lookup").unwrap();
    let association = McpAppAssociation {
        version: 1,
        server: "upstream".into(),
        tool: "lookup".into(),
        resource_uri: "ui://original".into(),
        source: "authorization-incarnation".into(),
        declaration: json!({"name":"lookup","inputSchema":{"type":"object"}}),
        arguments: json!({"original":true}),
        result: json!({"structuredContent":{"original":true}}),
    };
    let mut capability = Capability::new_async(
        waltr_agent::ToolDefinition::new("visible_alias", "lookup", json!({"type":"object"})),
        |_| Box::pin(async { panic!("source validation must be effect-free") }),
    )
    .with_concrete_target(target.clone());
    capability.mcp_app_source = Some(super::super::apps::McpAppSource {
        server: association.server.clone(),
        tool: association.tool.clone(),
        source: association.source.clone(),
        declaration: association.declaration.clone(),
        ui: mcp::apps::tool_meta(Some(
            &json!({"ui":{"resourceUri":association.resource_uri}}),
        ))
        .unwrap(),
    });
    let policy = PermissionPolicy::new(PermissionLevel::Deny).with_tool_rule(
        PermissionToolSelector::exact("visible_alias"),
        PermissionEffect::allow(),
    );
    let dispatcher = |policy| {
        CapabilityDispatcher::new(vec![capability.clone()], None, Arc::new(UnconfiguredPolicy))
            .unwrap()
            .with_context(MiddlewareChain::new(), policy)
    };
    assert_eq!(
        validate_source(&dispatcher(policy.clone()), &association),
        Ok(PermissionPolicy::new(PermissionLevel::Allow))
    );
    for (effect, expected) in [
        (
            PermissionEffect::ask(),
            Ok(PermissionPolicy::new(PermissionLevel::Ask)),
        ),
        (PermissionEffect::deny(), Err(McpAppError::Denied)),
    ] {
        let restricted = policy.clone().with_tool_rule(
            PermissionToolSelector::exact(target.permission_name()),
            effect,
        );
        assert_eq!(
            validate_source(&dispatcher(restricted), &association),
            expected
        );
    }
    for mutation in 0..5 {
        let mut changed = association.clone();
        match mutation {
            0 => changed.version = 2,
            1 => changed.server = "other".into(),
            2 => changed.source = "other-authorization".into(),
            3 => changed.declaration = json!({"changed":true}),
            _ => changed.resource_uri = "ui://other".into(),
        }
        assert_eq!(
            validate_source(&dispatcher(policy.clone()), &changed),
            Err(McpAppError::Stale)
        );
    }
}
