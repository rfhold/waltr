//! Permission rules.

use std::cmp::Ordering;

use super::{
    PermissionClaim, PermissionCommandPattern, PermissionEffect, PermissionOperation,
    PermissionPathPattern, PermissionResource, PermissionToolSelector,
};

/// Policy rules for one tool selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionToolRule {
    /// Tool selector for this rule group.
    pub selector: PermissionToolSelector,
    /// Whole-tool permission.
    pub tool: Option<PermissionEffect>,
    /// Operation class rules.
    pub classes: Vec<PermissionClassRule>,
    /// Directory/path rules.
    pub directories: Vec<PermissionDirectoryRule>,
    /// Command rules.
    pub commands: Vec<PermissionCommandRule>,
}

impl PermissionToolRule {
    /// Create an empty rule group.
    #[must_use]
    pub const fn new(selector: PermissionToolSelector) -> Self {
        Self {
            selector,
            tool: None,
            classes: Vec::new(),
            directories: Vec::new(),
            commands: Vec::new(),
        }
    }

    pub(in crate::permission) fn best_claim_match(
        &self,
        tool_name: &str,
        claim: &PermissionClaim,
    ) -> Option<PolicyMatch> {
        let selector_specificity = self.selector.specificity(tool_name)?;
        let mut best = self
            .classes
            .iter()
            .filter(|rule| rule.class == claim.operation)
            .map(|rule| PolicyMatch {
                selector_specificity,
                pattern_specificity: 1,
                effect: rule.effect.clone(),
            })
            .max();

        if let Some(path) = claim.directory_path() {
            best = best.max(
                self.directories
                    .iter()
                    .filter_map(|rule| {
                        rule.pattern
                            .matches(path)
                            .map(|pattern_specificity| PolicyMatch {
                                selector_specificity,
                                pattern_specificity,
                                effect: rule.effect.clone(),
                            })
                    })
                    .max(),
            );
        }

        if let PermissionResource::ShellCommand(command) = &claim.resource {
            best = best.max(
                self.commands
                    .iter()
                    .filter_map(|rule| {
                        rule.pattern
                            .matches(command)
                            .map(|pattern_specificity| PolicyMatch {
                                selector_specificity,
                                pattern_specificity,
                                effect: rule.effect.clone(),
                            })
                    })
                    .max(),
            );
        }

        best
    }
}

/// Class policy rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionClassRule {
    /// Operation class.
    pub class: PermissionOperation,
    /// Permission effect.
    pub effect: PermissionEffect,
}

/// Directory policy rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDirectoryRule {
    /// Path pattern.
    pub pattern: PermissionPathPattern,
    /// Permission effect.
    pub effect: PermissionEffect,
}

/// Command policy rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionCommandRule {
    /// Command pattern.
    pub pattern: PermissionCommandPattern,
    /// Permission effect.
    pub effect: PermissionEffect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::permission) struct PolicyMatch {
    pub(in crate::permission) selector_specificity: u8,
    pub(in crate::permission) pattern_specificity: usize,
    pub(in crate::permission) effect: PermissionEffect,
}

impl Ord for PolicyMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.selector_specificity
            .cmp(&other.selector_specificity)
            .then_with(|| self.pattern_specificity.cmp(&other.pattern_specificity))
            .then_with(|| {
                self.effect
                    .level
                    .conflict_rank()
                    .cmp(&other.effect.level.conflict_rank())
            })
    }
}

impl PartialOrd for PolicyMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
