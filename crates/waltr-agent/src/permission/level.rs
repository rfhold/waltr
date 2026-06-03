//! Permission levels and effects.

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
    pub(in crate::permission) const fn conflict_rank(self) -> u8 {
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
