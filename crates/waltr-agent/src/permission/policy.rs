//! Permission policy assembly.

use std::path::PathBuf;

use super::{
    PermissionClaim, PermissionClassRule, PermissionCommandPattern, PermissionCommandRule,
    PermissionDirectoryRule, PermissionEffect, PermissionLevel, PermissionOperation,
    PermissionPathPattern, PermissionToolRule, PermissionToolSelector,
};
use crate::permission::rule::PolicyMatch;

/// Permission policy for tools and contextual permission claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    rules: Vec<PermissionToolRule>,
    fallback: PermissionLevel,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::new(PermissionLevel::Allow)
    }
}

impl PermissionPolicy {
    /// Create an empty policy with a fallback level.
    #[must_use]
    pub const fn new(fallback: PermissionLevel) -> Self {
        Self {
            rules: Vec::new(),
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
            .map_or(self.fallback, |matched| matched.effect.level)
    }

    /// Evaluate one contextual claim, preserving policy messages.
    #[must_use]
    pub fn evaluate_claim_effect(
        &self,
        tool_name: &str,
        claim: &PermissionClaim,
    ) -> PermissionEffect {
        self.rules
            .iter()
            .filter_map(|rule| rule.best_claim_match(tool_name, claim))
            .max()
            .map_or_else(
                || PermissionEffect::new(self.fallback),
                |matched| matched.effect,
            )
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
