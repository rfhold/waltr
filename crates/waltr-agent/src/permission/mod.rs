//! Generic agent permission types and matching.

mod claim;
mod engine;
mod handler;
mod level;
mod pattern;
mod policy;
mod request;
mod rule;
mod selector;

pub use claim::{PermissionClaim, PermissionOperation, PermissionResource};
pub use engine::PermissionEngine;
pub use handler::{PermissionDecision, PermissionHandler, SharedPermissionHandler};
pub use level::{PermissionEffect, PermissionLevel};
pub use pattern::{
    PermissionCommandPattern, PermissionPathPattern, PermissionPattern, PermissionResourcePattern,
};
pub use policy::PermissionPolicy;
pub use request::{PermissionCheckContext, PermissionRequest};
pub use rule::{
    PermissionClassRule, PermissionCommandRule, PermissionDirectoryRule, PermissionToolRule,
};
pub use selector::PermissionToolSelector;

pub(in crate::permission) use request::{denial_message, policy_denial_message};

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::ToolCall;

    use super::{
        PermissionClaim, PermissionCommandPattern, PermissionDecision, PermissionEffect,
        PermissionEngine, PermissionLevel, PermissionOperation, PermissionPathPattern,
        PermissionPattern, PermissionPolicy, PermissionResource, PermissionToolSelector,
    };

    #[test]
    fn exact_tool_selector_overrides_global_selector() {
        let policy = PermissionPolicy::default()
            .with_directory_rule(
                PermissionToolSelector::Any,
                PermissionPathPattern::OutsideRoot("/tmp/root".into()),
                PermissionEffect::deny(),
            )
            .with_directory_rule(
                PermissionToolSelector::exact("read"),
                PermissionPathPattern::Any,
                PermissionEffect::allow(),
            );
        let claim = PermissionClaim::new(
            PermissionOperation::Read,
            PermissionResource::FilesystemPath("/tmp/secret".into()),
            "read /tmp/secret",
        );

        assert_eq!(
            policy.evaluate_claim("read", &claim),
            PermissionLevel::Allow
        );
        assert_eq!(
            policy.evaluate_claim("write", &claim),
            PermissionLevel::Deny
        );
    }

    #[test]
    fn exact_tool_deny_hides_tool() {
        let policy = PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("delete"),
            PermissionEffect::deny(),
        );

        assert_eq!(policy.evaluate_tool("delete"), PermissionLevel::Deny);
        assert_eq!(policy.evaluate_tool("read"), PermissionLevel::Allow);
    }

    #[test]
    fn same_specificity_conflict_prefers_deny_then_ask() {
        let ask_over_allow = PermissionPolicy::default()
            .with_class_rule(
                PermissionToolSelector::Any,
                PermissionOperation::Write,
                PermissionEffect::allow(),
            )
            .with_class_rule(
                PermissionToolSelector::Any,
                PermissionOperation::Write,
                PermissionEffect::ask(),
            );
        let deny_over_ask = ask_over_allow.clone().with_class_rule(
            PermissionToolSelector::Any,
            PermissionOperation::Write,
            PermissionEffect::deny(),
        );
        let claim = PermissionClaim::new(
            PermissionOperation::Write,
            PermissionResource::FilesystemPath("/tmp/root/file.txt".into()),
            "write /tmp/root/file.txt",
        );

        assert_eq!(
            ask_over_allow.evaluate_claim("write", &claim),
            PermissionLevel::Ask
        );
        assert_eq!(
            deny_over_ask.evaluate_claim("write", &claim),
            PermissionLevel::Deny
        );
    }

    #[test]
    fn class_directory_and_command_rules_evaluate_claims() {
        let policy = PermissionPolicy::default()
            .with_class_rule(
                PermissionToolSelector::Any,
                PermissionOperation::Read,
                PermissionEffect::allow(),
            )
            .with_class_rule(
                PermissionToolSelector::Any,
                PermissionOperation::Write,
                PermissionEffect::ask(),
            )
            .with_class_rule(
                PermissionToolSelector::Any,
                PermissionOperation::Delete,
                PermissionEffect::deny(),
            )
            .with_directory_rule(
                PermissionToolSelector::Any,
                PermissionPathPattern::Prefix("/tmp/project".into()),
                PermissionEffect::allow(),
            )
            .with_command_rule(
                PermissionToolSelector::exact("bash"),
                PermissionCommandPattern::Name("git".to_owned()),
                PermissionEffect::deny(),
            );

        let write = PermissionClaim::new(
            PermissionOperation::Write,
            PermissionResource::FilesystemPath("/tmp/other/file.txt".into()),
            "write /tmp/other/file.txt",
        );
        let delete = PermissionClaim::new(
            PermissionOperation::Delete,
            PermissionResource::FilesystemPath("/tmp/other/file.txt".into()),
            "delete /tmp/other/file.txt",
        );
        let working_directory = PermissionClaim::new(
            PermissionOperation::Execute,
            PermissionResource::WorkingDirectory("/tmp/project".into()),
            "workdir /tmp/project",
        );
        let git = PermissionClaim::new(
            PermissionOperation::Execute,
            PermissionResource::ShellCommand("git status".to_owned()),
            "git status",
        );

        assert_eq!(policy.evaluate_claim("write", &write), PermissionLevel::Ask);
        assert_eq!(
            policy.evaluate_claim("delete", &delete),
            PermissionLevel::Deny
        );
        assert_eq!(
            policy.evaluate_claim("bash", &working_directory),
            PermissionLevel::Allow
        );
        assert_eq!(policy.evaluate_claim("bash", &git), PermissionLevel::Deny);
    }

    #[test]
    fn policy_denial_messages_are_preserved() {
        let mut engine = PermissionEngine::new(
            PermissionPolicy::default().with_command_rule(
                PermissionToolSelector::exact("bash"),
                PermissionCommandPattern::Name("git".to_owned()),
                PermissionEffect::deny_with_message("do not interact with git"),
            ),
            None,
        );
        let call = ToolCall::new("call_1", "bash", json!({ "command": "git status" }));
        let claim = PermissionClaim::new(
            PermissionOperation::Execute,
            PermissionResource::ShellCommand("git status".to_owned()),
            "git status",
        );

        let error = engine
            .check(&call, &[claim])
            .expect_err("git command should be denied");

        assert!(
            error
                .to_string()
                .contains("permission denied: do not interact with git: git status"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn filesystem_default_does_not_use_parent_directory_deny_rules() {
        let policy = PermissionPolicy::filesystem_default("/tmp/root");
        let explicit_read_allow = policy.with_directory_rule(
            PermissionToolSelector::exact("read"),
            PermissionPathPattern::Any,
            PermissionEffect::allow(),
        );
        let outside = PermissionClaim::new(
            PermissionOperation::Read,
            PermissionResource::FilesystemPath("/tmp/secret".into()),
            "read /tmp/secret",
        );

        assert_eq!(
            explicit_read_allow.evaluate_claim("read", &outside),
            PermissionLevel::Allow
        );
    }

    #[test]
    fn allow_always_pattern_is_reused() {
        let mut engine = PermissionEngine::new(
            PermissionPolicy::new(PermissionLevel::Ask),
            Some(std::sync::Arc::new(|request| {
                assert_eq!(request.claims.len(), 1);
                PermissionDecision::AllowAlways(vec![PermissionPattern::filesystem_prefix(
                    PermissionOperation::Read,
                    "/tmp/waltr-shared",
                )])
            })),
        );
        let call = ToolCall::new("call_1", "read", json!({}));
        let claim = PermissionClaim::new(
            PermissionOperation::Read,
            PermissionResource::FilesystemPath("/tmp/waltr-shared/a.txt".into()),
            "read /tmp/waltr-shared/a.txt",
        );

        engine
            .check(&call, &[claim])
            .expect("first check asks and stores approval");
        assert_eq!(engine.patterns().len(), 1);

        let reused_claim = PermissionClaim::new(
            PermissionOperation::Read,
            PermissionResource::FilesystemPath("/tmp/waltr-shared/nested/b.txt".into()),
            "read /tmp/waltr-shared/nested/b.txt",
        );
        engine
            .check(&call, &[reused_claim])
            .expect("stored approval is reused");
    }
}
