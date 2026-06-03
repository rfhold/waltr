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

use std::{cmp::Ordering, fmt, path::PathBuf, sync::Arc};

use crate::{LlmError, Result, ToolCall};

/// Default policy for a tool or an unmatched permission claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevel {
    /// Allow without asking.
    Allow,
    /// Ask the configured permission handler.
    Ask,
    /// Deny without asking.
    Deny,
}

impl PermissionLevel {
    const fn conflict_rank(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Ask => 1,
            Self::Deny => 2,
        }
    }
}

/// Permission effect for a policy rule, optionally including a message for denials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionEffect {
    /// Permission level applied by the rule.
    pub level: PermissionLevel,
    /// Optional message used when a deny rule blocks a tool call.
    pub message: Option<String>,
}

impl PermissionEffect {
    /// Create an effect from a level.
    #[must_use]
    pub const fn new(level: PermissionLevel) -> Self {
        Self {
            level,
            message: None,
        }
    }

    /// Allow without asking.
    #[must_use]
    pub const fn allow() -> Self {
        Self::new(PermissionLevel::Allow)
    }

    /// Ask the configured permission handler.
    #[must_use]
    pub const fn ask() -> Self {
        Self::new(PermissionLevel::Ask)
    }

    /// Deny without asking.
    #[must_use]
    pub const fn deny() -> Self {
        Self::new(PermissionLevel::Deny)
    }

    /// Deny without asking, using a custom denial message.
    #[must_use]
    pub fn deny_with_message(message: impl Into<String>) -> Self {
        Self {
            level: PermissionLevel::Deny,
            message: Some(message.into()),
        }
    }
}

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

    fn best_claim_match(&self, tool_name: &str, claim: &PermissionClaim) -> Option<PolicyMatch> {
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

/// Tool selector for a policy rule group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionToolSelector {
    /// Match every tool.
    Any,
    /// Match exactly one tool name.
    Exact(String),
}

impl PermissionToolSelector {
    /// Create an exact tool selector.
    #[must_use]
    pub fn exact(tool_name: impl Into<String>) -> Self {
        Self::Exact(tool_name.into())
    }

    fn specificity(&self, tool_name: &str) -> Option<u8> {
        match self {
            Self::Any => Some(0),
            Self::Exact(exact) => {
                if exact.as_str() == tool_name {
                    Some(1)
                } else {
                    None
                }
            }
        }
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

/// Path pattern used by permission policy rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPathPattern {
    /// Match any path.
    Any,
    /// Match one exact path.
    Exact(PathBuf),
    /// Match a path prefix.
    Prefix(PathBuf),
    /// Match paths outside one root.
    OutsideRoot(PathBuf),
    /// Minimal glob-like wildcard pattern. `*` matches all paths and `prefix/*` matches a prefix.
    Wildcard(String),
}

impl PermissionPathPattern {
    fn matches(&self, path: &std::path::Path) -> Option<usize> {
        match self {
            Self::Any => Some(0),
            Self::Exact(pattern) => (path == pattern).then_some(usize::MAX),
            Self::Prefix(pattern) => path
                .starts_with(pattern)
                .then_some(pattern.as_os_str().len()),
            Self::OutsideRoot(root) => (!path.starts_with(root)).then_some(root.as_os_str().len()),
            Self::Wildcard(pattern) if pattern == "*" => Some(0),
            Self::Wildcard(pattern) if pattern.ends_with("/*") => {
                let prefix = &pattern[..pattern.len() - 2];
                path.to_str()
                    .is_some_and(|path| path.starts_with(prefix))
                    .then_some(prefix.len())
            }
            Self::Wildcard(pattern) => path
                .to_str()
                .is_some_and(|path| path == pattern)
                .then_some(pattern.len()),
        }
    }
}

/// Command pattern used by permission policy rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionCommandPattern {
    /// Match any command.
    Any,
    /// Match an exact full command string.
    Exact(String),
    /// Match a command prefix.
    Prefix(String),
    /// Match the executable name at the start of the command string.
    Name(String),
}

impl PermissionCommandPattern {
    fn matches(&self, command: &str) -> Option<usize> {
        match self {
            Self::Any => Some(0),
            Self::Exact(pattern) => (command == pattern).then_some(usize::MAX),
            Self::Prefix(pattern) => command.starts_with(pattern).then_some(pattern.len()),
            Self::Name(name) => command
                .split_whitespace()
                .next()
                .is_some_and(|command_name| command_name == name)
                .then_some(name.len()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PolicyMatch {
    selector_specificity: u8,
    pattern_specificity: usize,
    effect: PermissionEffect,
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

/// Permission handler decision for an asked permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow this call only.
    AllowOnce,
    /// Allow this call and store reusable patterns for the current run.
    AllowAlways(Vec<PermissionPattern>),
    /// Deny with a message.
    Deny(String),
}

/// Abstract operation requested by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOperation {
    /// Read a resource.
    Read,
    /// Write or update a resource.
    Write,
    /// Delete a resource.
    Delete,
    /// Execute a command or program.
    Execute,
    /// Access a network resource.
    Network,
}

/// Abstract resource requested by a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResource {
    /// Concrete filesystem path.
    FilesystemPath(PathBuf),
    /// Filesystem path prefix.
    FilesystemPathPrefix(PathBuf),
    /// Working directory.
    WorkingDirectory(PathBuf),
    /// Shell command.
    ShellCommand(String),
}

/// One permission claim extracted from a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionClaim {
    /// Requested operation.
    pub operation: PermissionOperation,
    /// Requested resource.
    pub resource: PermissionResource,
    /// Human-readable summary for prompts and diagnostics.
    pub summary: String,
}

impl PermissionClaim {
    /// Create a permission claim.
    pub fn new(
        operation: PermissionOperation,
        resource: PermissionResource,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            operation,
            resource,
            summary: summary.into(),
        }
    }

    fn directory_path(&self) -> Option<&std::path::Path> {
        match &self.resource {
            PermissionResource::FilesystemPath(path)
            | PermissionResource::FilesystemPathPrefix(path)
            | PermissionResource::WorkingDirectory(path) => Some(path),
            PermissionResource::ShellCommand(_) => None,
        }
    }
}

/// Permission check context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionCheckContext<'a> {
    /// Tool name being checked.
    pub tool_name: &'a str,
    /// Claims being checked.
    pub claims: &'a [PermissionClaim],
}

/// Reusable permission approval pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPattern {
    /// Operation matched by this pattern. `None` matches any operation.
    pub operation: Option<PermissionOperation>,
    /// Resource matcher.
    pub resource: PermissionResourcePattern,
}

impl PermissionPattern {
    /// Create a filesystem prefix pattern for one operation.
    pub fn filesystem_prefix(operation: PermissionOperation, path: impl Into<PathBuf>) -> Self {
        Self {
            operation: Some(operation),
            resource: PermissionResourcePattern::FilesystemPathPrefix(path.into()),
        }
    }

    /// Return true if this pattern matches a claim.
    #[must_use]
    pub fn matches(&self, claim: &PermissionClaim) -> bool {
        if self
            .operation
            .is_some_and(|operation| operation != claim.operation)
        {
            return false;
        }
        self.resource.matches(&claim.resource)
    }
}

/// Resource matcher for reusable permission approvals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResourcePattern {
    /// Exact filesystem path.
    FilesystemPath(PathBuf),
    /// Filesystem path prefix.
    FilesystemPathPrefix(PathBuf),
    /// Working directory prefix.
    WorkingDirectoryPrefix(PathBuf),
    /// Exact shell command.
    ShellCommand(String),
    /// Shell command prefix.
    ShellCommandPrefix(String),
}

impl PermissionResourcePattern {
    fn matches(&self, resource: &PermissionResource) -> bool {
        match (self, resource) {
            (Self::FilesystemPath(pattern), PermissionResource::FilesystemPath(path)) => {
                path == pattern
            }
            (
                Self::FilesystemPathPrefix(pattern),
                PermissionResource::FilesystemPath(path)
                | PermissionResource::FilesystemPathPrefix(path),
            ) => path.starts_with(pattern),
            (Self::WorkingDirectoryPrefix(pattern), PermissionResource::WorkingDirectory(path)) => {
                path.starts_with(pattern)
            }
            (Self::ShellCommand(pattern), PermissionResource::ShellCommand(command)) => {
                command == pattern
            }
            (Self::ShellCommandPrefix(pattern), PermissionResource::ShellCommand(command)) => {
                command.starts_with(pattern)
            }
            _ => false,
        }
    }
}

/// Permission request passed to an ask handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Tool call being checked.
    pub tool_call: ToolCall,
    /// Claims requiring a decision.
    pub claims: Vec<PermissionClaim>,
    /// Human-readable request summary.
    pub summary: String,
}

/// Callback used to answer permission requests.
pub type PermissionHandler = dyn Fn(&PermissionRequest) -> PermissionDecision + Send + Sync;

/// Shared permission handler.
pub type SharedPermissionHandler = Arc<PermissionHandler>;

/// Run-scoped permission engine.
#[derive(Clone)]
pub struct PermissionEngine {
    patterns: Vec<PermissionPattern>,
    policy: PermissionPolicy,
    handler: Option<SharedPermissionHandler>,
}

impl PermissionEngine {
    /// Create a permission engine.
    #[must_use]
    pub fn new(policy: PermissionPolicy, handler: Option<SharedPermissionHandler>) -> Self {
        Self {
            patterns: Vec::new(),
            policy,
            handler,
        }
    }

    /// Return stored reusable patterns.
    #[must_use]
    pub fn patterns(&self) -> &[PermissionPattern] {
        &self.patterns
    }

    /// Return true if an ask handler is configured.
    #[must_use]
    pub const fn has_handler(&self) -> bool {
        self.handler.is_some()
    }

    /// Check claims for a tool call, mutating run-scoped approvals as needed.
    ///
    /// # Errors
    ///
    /// Returns an error when a claim is denied, when a claim requires `Ask` but no handler is
    /// configured, or when the handler denies the request.
    pub fn check(&mut self, tool_call: &ToolCall, claims: &[PermissionClaim]) -> Result<()> {
        let unmatched = claims
            .iter()
            .filter(|claim| !self.patterns.iter().any(|pattern| pattern.matches(claim)))
            .cloned()
            .collect::<Vec<_>>();

        if unmatched.is_empty() {
            return Ok(());
        }

        let denied = unmatched
            .iter()
            .filter_map(|claim| {
                let effect = self.policy.evaluate_claim_effect(&tool_call.name, claim);
                (effect.level == PermissionLevel::Deny).then(|| (claim.clone(), effect))
            })
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            return Err(permission_error(policy_denial_message(&denied)));
        }

        let ask = unmatched
            .iter()
            .filter(|claim| {
                self.policy
                    .evaluate_claim_effect(&tool_call.name, claim)
                    .level
                    == PermissionLevel::Ask
            })
            .cloned()
            .collect::<Vec<_>>();
        if ask.is_empty() {
            return Ok(());
        }

        let Some(handler) = &self.handler else {
            return Err(permission_error(denial_message(
                "permission denied: no permission handler configured",
                &ask,
            )));
        };
        let request = PermissionRequest {
            tool_call: tool_call.clone(),
            claims: ask.clone(),
            summary: ask
                .iter()
                .map(|claim| claim.summary.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        };
        match handler(&request) {
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowAlways(patterns) => {
                self.patterns.extend(patterns);
                Ok(())
            }
            PermissionDecision::Deny(message) => Err(permission_error(denial_message(
                &format!("permission denied: {message}"),
                &ask,
            ))),
        }
    }
}

impl fmt::Debug for PermissionEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionEngine")
            .field("patterns", &self.patterns)
            .field("policy", &self.policy)
            .field("has_handler", &self.handler.is_some())
            .finish()
    }
}

fn normalize_policy_path(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn denial_message(prefix: &str, claims: &[PermissionClaim]) -> String {
    let summaries = claims
        .iter()
        .map(|claim| claim.summary.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    if summaries.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {summaries}")
    }
}

fn policy_denial_message(denied: &[(PermissionClaim, PermissionEffect)]) -> String {
    let claims = denied
        .iter()
        .map(|(claim, _)| claim.clone())
        .collect::<Vec<_>>();
    let messages = denied
        .iter()
        .filter_map(|(_, effect)| effect.message.as_deref())
        .collect::<Vec<_>>();
    if messages.is_empty() {
        return denial_message("permission denied", &claims);
    }

    let mut unique_messages = Vec::new();
    for message in messages {
        if !unique_messages.contains(&message) {
            unique_messages.push(message);
        }
    }
    denial_message(
        &format!("permission denied: {}", unique_messages.join("; ")),
        &claims,
    )
}

fn permission_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "permission",
        message: message.into(),
    }
}

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
