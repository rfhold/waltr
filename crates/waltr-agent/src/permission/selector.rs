//! Permission selectors.

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

    pub(in crate::permission) fn specificity(&self, tool_name: &str) -> Option<u8> {
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
