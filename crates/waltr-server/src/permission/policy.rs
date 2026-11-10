//! Permission policy assembly.

use std::path::PathBuf;

use super::{
    PermissionClaim, PermissionClassRule, PermissionCommandPattern, PermissionCommandRule,
    PermissionDirectoryRule, PermissionEffect, PermissionInputRule, PermissionLevel,
    PermissionOperation, PermissionPathPattern, PermissionToolRule, PermissionToolSelector,
    input::{
        CompiledInputRule, InputRuleEvaluation, PermissionInputEvaluation, denied_input_evaluation,
        matched_input_evaluation,
    },
};
use crate::permission::rule::PolicyMatch;

/// Permission policy for tools and contextual permission claims.
#[derive(Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    rules: Vec<PermissionToolRule>,
    input_rules: Vec<CompiledInputRule>,
    fallback: PermissionLevel,
    restrictions: Vec<Self>,
}

impl std::fmt::Debug for PermissionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PermissionPolicy");
        debug
            .field("rules", &self.rules)
            .field("input_rules", &self.input_rules)
            .field("fallback", &self.fallback);
        if !self.restrictions.is_empty() {
            debug.field("restrictions", &self.restrictions);
        }
        debug.finish()
    }
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::new(PermissionLevel::Allow)
    }
}

impl PermissionPolicy {
    pub(crate) fn identity(&self) -> String {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in format!("{self:?}").bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    /// Create an empty policy with a fallback level.
    #[must_use]
    pub const fn new(fallback: PermissionLevel) -> Self {
        Self {
            rules: Vec::new(),
            input_rules: Vec::new(),
            fallback,
            restrictions: Vec::new(),
        }
    }

    /// Create the default filesystem policy for a worker-reported canonical root.
    #[must_use]
    pub fn filesystem_default(root: impl Into<PathBuf>) -> Self {
        Self::filesystem_external(root, PermissionEffect::deny())
    }

    /// Create a filesystem policy with configurable external path handling.
    /// The root must be the worker's canonical root; this does no host filesystem I/O.
    #[must_use]
    pub fn filesystem_external(root: impl Into<PathBuf>, external: PermissionEffect) -> Self {
        let root = root.into();
        Self::default()
            .with_directory_rule(
                PermissionToolSelector::Any,
                PermissionPathPattern::Prefix(root.clone()),
                PermissionEffect::allow(),
            )
            .with_directory_rule(
                PermissionToolSelector::Any,
                PermissionPathPattern::OutsideRoot(root),
                external,
            )
    }

    /// Add another policy's rules after this policy's rules.
    #[must_use]
    pub fn merged(mut self, other: Self) -> Self {
        self.rules.extend(other.rules);
        let offset = self.input_rules.len();
        self.input_rules
            .extend(other.input_rules.into_iter().map(|mut rule| {
                rule.position += offset;
                rule
            }));
        self.fallback = other.fallback;
        self.restrictions.extend(other.restrictions);
        self
    }

    /// Narrow this policy without allowing either policy's refinements to loosen the other.
    /// Each component evaluates its own rules before deny > ask > allow is combined.
    #[must_use]
    pub fn intersection(mut self, restriction: Self) -> Self {
        self.restrictions.push(restriction);
        self
    }

    /// Add a whole-tool rule.
    #[must_use]
    pub fn with_tool_rule(
        mut self,
        selector: PermissionToolSelector,
        effect: PermissionEffect,
    ) -> Self {
        self.rule_mut(selector).tool = Some(effect);
        self
    }

    /// Add a class rule.
    #[must_use]
    pub fn with_class_rule(
        mut self,
        selector: PermissionToolSelector,
        class: PermissionOperation,
        effect: PermissionEffect,
    ) -> Self {
        self.rule_mut(selector)
            .classes
            .push(PermissionClassRule { class, effect });
        self
    }

    /// Add a directory rule.
    #[must_use]
    pub fn with_directory_rule(
        mut self,
        selector: PermissionToolSelector,
        pattern: PermissionPathPattern,
        effect: PermissionEffect,
    ) -> Self {
        self.rule_mut(selector)
            .directories
            .push(PermissionDirectoryRule { pattern, effect });
        self
    }

    /// Add a command rule.
    #[must_use]
    pub fn with_command_rule(
        mut self,
        selector: PermissionToolSelector,
        pattern: PermissionCommandPattern,
        effect: PermissionEffect,
    ) -> Self {
        self.rule_mut(selector)
            .commands
            .push(PermissionCommandRule { pattern, effect });
        self
    }

    /// Add a strict nested-input rule at the next composed position.
    #[must_use]
    pub fn with_input_rule(
        mut self,
        selector: PermissionToolSelector,
        rule: PermissionInputRule,
    ) -> Self {
        let position = self.input_rules.len();
        self.input_rules.push(CompiledInputRule {
            selector,
            rule,
            position,
        });
        self
    }

    /// Evaluate nested input rules against final tool arguments.
    ///
    /// `None` means no input rule matched and callers must retain the whole-tool baseline.
    #[must_use]
    pub(crate) fn evaluate_input(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Option<PermissionInputEvaluation> {
        if self.restrictions.is_empty() {
            return self.base_input(tool_name, arguments);
        }
        let own = self.base_input(tool_name, arguments);
        let others = self
            .restrictions
            .iter()
            .map(|policy| (policy, policy.evaluate_input(tool_name, arguments)))
            .collect::<Vec<_>>();
        if own.is_none() && others.iter().all(|(_, evaluation)| evaluation.is_none()) {
            return None;
        }
        let mut result = own.unwrap_or_else(|| PermissionInputEvaluation {
            effect: self.base_tool_effect(tool_name),
            claims: Vec::new(),
        });
        for (policy, evaluation) in others {
            let evaluation = evaluation.unwrap_or_else(|| PermissionInputEvaluation {
                effect: policy.evaluate_tool_effect(tool_name),
                claims: Vec::new(),
            });
            result.effect = restrictive(result.effect, evaluation.effect);
            result.claims.extend(evaluation.claims);
        }
        Some(result)
    }

    fn base_input(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Option<PermissionInputEvaluation> {
        let mut best = None;
        for rule in &self.input_rules {
            match rule.evaluate(tool_name, arguments) {
                InputRuleEvaluation::Missing => {}
                InputRuleEvaluation::Aggregate => return Some(denied_input_evaluation()),
                InputRuleEvaluation::Match(matched) => best = best.max(Some(matched)),
            }
        }
        best.map(|matched| matched_input_evaluation(tool_name, matched))
    }

    /// Evaluate whole-tool permission.
    #[must_use]
    pub fn evaluate_tool(&self, tool_name: &str) -> PermissionLevel {
        self.evaluate_tool_effect(tool_name).level
    }

    /// Evaluate whole-tool permission, preserving policy messages.
    #[must_use]
    pub fn evaluate_tool_effect(&self, tool_name: &str) -> PermissionEffect {
        self.restrictions
            .iter()
            .fold(self.base_tool_effect(tool_name), |effect, policy| {
                restrictive(effect, policy.evaluate_tool_effect(tool_name))
            })
    }

    /// Apply alias admission and any explicit exact-target ceiling together.
    #[must_use]
    pub fn evaluate_target_tool_effect(&self, identity: &str, alias: &str) -> PermissionEffect {
        let effect = restrictive(
            self.base_tool_effect(alias),
            self.explicit_tool_effect(identity)
                .unwrap_or_else(PermissionEffect::allow),
        );
        self.restrictions.iter().fold(effect, |effect, policy| {
            restrictive(effect, policy.evaluate_target_tool_effect(identity, alias))
        })
    }

    fn base_tool_effect(&self, tool_name: &str) -> PermissionEffect {
        self.explicit_tool_effect(tool_name)
            .unwrap_or_else(|| PermissionEffect::new(self.fallback))
    }

    fn explicit_tool_effect(&self, tool_name: &str) -> Option<PermissionEffect> {
        self.rules
            .iter()
            .filter_map(|rule| {
                let selector_specificity = rule.selector.specificity(tool_name)?;
                rule.tool.clone().map(|effect| PolicyMatch {
                    selector_specificity,
                    pattern_specificity: 0,
                    effect,
                })
            })
            .max()
            .map(|matched| matched.effect)
    }

    /// Evaluate one contextual claim.
    #[must_use]
    pub fn evaluate_claim(&self, tool_name: &str, claim: &PermissionClaim) -> PermissionLevel {
        self.evaluate_claim_effect(tool_name, claim).level
    }

    /// Evaluate one contextual claim, preserving policy messages.
    #[must_use]
    pub fn evaluate_claim_effect(
        &self,
        tool_name: &str,
        claim: &PermissionClaim,
    ) -> PermissionEffect {
        let own = self
            .base_claim_refinement(tool_name, claim)
            .unwrap_or_else(|| self.base_tool_effect(tool_name));
        self.restrictions.iter().fold(own, |effect, policy| {
            restrictive(effect, policy.evaluate_claim_effect(tool_name, claim))
        })
    }

    /// Evaluate only explicit class/resource refinements, excluding the whole-tool fallback.
    #[must_use]
    pub(crate) fn evaluate_claim_refinement_effect(
        &self,
        tool_name: &str,
        claim: &PermissionClaim,
    ) -> Option<PermissionEffect> {
        if self.restrictions.is_empty() {
            return self.base_claim_refinement(tool_name, claim);
        }
        if self.base_claim_refinement(tool_name, claim).is_none()
            && self.restrictions.iter().all(|policy| {
                policy
                    .evaluate_claim_refinement_effect(tool_name, claim)
                    .is_none()
            })
        {
            return None;
        }
        Some(self.evaluate_claim_effect(tool_name, claim))
    }

    fn base_claim_refinement(
        &self,
        tool_name: &str,
        claim: &PermissionClaim,
    ) -> Option<PermissionEffect> {
        self.rules
            .iter()
            .filter_map(|rule| rule.best_claim_match(tool_name, claim))
            .max()
            .map(|matched| matched.effect)
    }

    fn rule_mut(&mut self, selector: PermissionToolSelector) -> &mut PermissionToolRule {
        if let Some(index) = self.rules.iter().position(|rule| rule.selector == selector) {
            return &mut self.rules[index];
        }
        self.rules.push(PermissionToolRule::new(selector));
        self.rules.last_mut().expect("rule just pushed")
    }
}

fn restrictive(left: PermissionEffect, right: PermissionEffect) -> PermissionEffect {
    if right.level.conflict_rank() > left.level.conflict_rank() {
        right
    } else {
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionResource;

    #[test]
    fn intersection_preserves_each_complete_policy_and_denial_message() {
        let levels = [
            PermissionLevel::Allow,
            PermissionLevel::Ask,
            PermissionLevel::Deny,
        ];
        let claim = PermissionClaim::new(
            PermissionOperation::Write,
            PermissionResource::FilesystemPath("/worker/a".into()),
            "write",
        );
        for left in levels {
            for right in levels {
                let a = PermissionPolicy::new(left).with_tool_rule(
                    PermissionToolSelector::exact("other"),
                    PermissionEffect::allow(),
                );
                let b = PermissionPolicy::new(PermissionLevel::Allow)
                    .with_tool_rule(PermissionToolSelector::Any, PermissionEffect::new(right));
                let narrowed = a.intersection(b);
                let expected = if left.conflict_rank() >= right.conflict_rank() {
                    left
                } else {
                    right
                };
                assert_eq!(narrowed.evaluate_tool("mcp_configure"), expected);
                assert_eq!(narrowed.evaluate_claim("mcp_configure", &claim), expected);
            }
        }
        let deny = PermissionPolicy::new(PermissionLevel::Allow).with_tool_rule(
            PermissionToolSelector::Any,
            PermissionEffect::deny_with_message("caller denied"),
        );
        let refined = PermissionPolicy::new(PermissionLevel::Deny)
            .with_tool_rule(
                PermissionToolSelector::exact("write"),
                PermissionEffect::allow(),
            )
            .with_directory_rule(
                PermissionToolSelector::Any,
                PermissionPathPattern::Prefix("/worker".into()),
                PermissionEffect::allow(),
            );
        let intersection = refined.intersection(deny);
        assert_eq!(
            intersection
                .evaluate_tool_effect("write")
                .message
                .as_deref(),
            Some("caller denied")
        );
        assert_eq!(
            intersection.evaluate_claim("write", &claim),
            PermissionLevel::Deny
        );
    }

    #[test]
    fn intersection_input_refinement_cannot_replace_other_component_fallback() {
        use crate::permission::{
            PermissionInputCondition, PermissionInputScalar, PermissionInputValueMatcher,
            PermissionJsonPointer,
        };
        use serde_json::json;
        let rule = PermissionInputRule::new(
            vec![PermissionInputCondition {
                pointer: PermissionJsonPointer::parse("/options/dryRun").unwrap(),
                value: PermissionInputValueMatcher::Exact(PermissionInputScalar::Bool(true)),
            }],
            PermissionEffect::allow(),
        )
        .unwrap();
        let own = PermissionPolicy::new(PermissionLevel::Ask)
            .with_input_rule(PermissionToolSelector::exact("mcp_configure"), rule);
        for level in [
            PermissionLevel::Allow,
            PermissionLevel::Ask,
            PermissionLevel::Deny,
        ] {
            let narrowed = own.clone().intersection(PermissionPolicy::new(level));
            assert_eq!(
                narrowed
                    .evaluate_input("mcp_configure", &json!({"options":{"dryRun":true}}))
                    .unwrap()
                    .effect
                    .level,
                level
            );
            assert!(
                narrowed
                    .evaluate_input("mcp_configure", &json!({}))
                    .is_none()
            );
            assert_eq!(
                narrowed.evaluate_tool("mcp_configure"),
                if level == PermissionLevel::Deny {
                    level
                } else {
                    PermissionLevel::Ask
                }
            );
        }
        let denied = own.intersection(PermissionPolicy::new(PermissionLevel::Deny));
        assert_eq!(
            denied
                .evaluate_input("mcp_configure", &json!({"options":{"dryRun":true}}))
                .unwrap()
                .effect
                .level,
            PermissionLevel::Deny
        );
    }

    #[test]
    fn policy_uses_worker_canonical_root_without_local_filesystem() {
        let actual = PathBuf::from("/worker/workspace");
        let policy = PermissionPolicy::filesystem_external(&actual, PermissionEffect::ask());
        let inside = PermissionClaim::new(
            PermissionOperation::Write,
            PermissionResource::FilesystemPath(actual.join("note")),
            "inside",
        );
        let outside = PermissionClaim::new(
            PermissionOperation::Write,
            PermissionResource::FilesystemPath(PathBuf::from("/host/workspace/other")),
            "outside",
        );
        assert_eq!(
            policy.evaluate_claim("write", &inside),
            PermissionLevel::Allow
        );
        assert_eq!(
            policy.evaluate_claim("write", &outside),
            PermissionLevel::Ask
        );
    }
}
