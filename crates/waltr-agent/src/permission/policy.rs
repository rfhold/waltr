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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    rules: Vec<PermissionToolRule>,
    input_rules: Vec<CompiledInputRule>,
    fallback: PermissionLevel,
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
        }
    }

    /// Create the default filesystem policy for one root.
    #[must_use]
    pub fn filesystem_default(root: impl Into<PathBuf>) -> Self {
        Self::filesystem_external(root, PermissionEffect::deny())
    }

    /// Create a filesystem policy with configurable external path handling.
    #[must_use]
    pub fn filesystem_external(root: impl Into<PathBuf>, external: PermissionEffect) -> Self {
        let root = normalize_policy_path(root.into());
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
            .map_or(self.fallback, |matched| matched.effect.level)
    }

    /// Evaluate whole-tool permission, preserving policy messages.
    #[must_use]
    pub fn evaluate_tool_effect(&self, tool_name: &str) -> PermissionEffect {
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
            .map_or_else(
                || PermissionEffect::new(self.fallback),
                |matched| matched.effect,
            )
    }

    /// Evaluate one contextual claim.
    #[must_use]
    pub fn evaluate_claim(&self, tool_name: &str, claim: &PermissionClaim) -> PermissionLevel {
        self.rules
            .iter()
            .filter_map(|rule| rule.best_claim_match(tool_name, claim))
            .max()
            .map_or_else(
                || self.evaluate_tool(tool_name),
                |matched| matched.effect.level,
            )
    }

    /// Evaluate one contextual claim, preserving policy messages.
    #[must_use]
    pub fn evaluate_claim_effect(
        &self,
        tool_name: &str,
        claim: &PermissionClaim,
    ) -> PermissionEffect {
        self.evaluate_claim_refinement_effect(tool_name, claim)
            .unwrap_or_else(|| self.evaluate_tool_effect(tool_name))
    }

    /// Evaluate only explicit class/resource refinements, excluding the whole-tool fallback.
    #[must_use]
    pub(crate) fn evaluate_claim_refinement_effect(
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

fn normalize_policy_path(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}
