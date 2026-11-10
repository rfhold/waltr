//! Generic prompt fragment assembly.
/// Builder for deterministic compatibility system prompt construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptBuilder {
    fragments: Vec<String>,
}

impl SystemPromptBuilder {
    /// Create an empty system prompt builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fragments: Vec::new(),
        }
    }

    /// Append one prompt fragment. Empty fragments are ignored.
    #[must_use]
    pub fn with_fragment(mut self, fragment: impl Into<String>) -> Self {
        let fragment = fragment.into();
        let fragment = fragment.trim();
        if !fragment.is_empty() {
            self.fragments.push(fragment.to_owned());
        }
        self
    }

    /// Append deterministic date context.
    #[must_use]
    pub fn with_date(self, date: impl Into<String>) -> Self {
        self.with_fragment(format!("Current date: {}.", date.into()))
    }

    /// Build the joined system prompt.
    #[must_use]
    pub fn build(self) -> Option<String> {
        if self.fragments.is_empty() {
            None
        } else {
            Some(self.fragments.join("\n\n"))
        }
    }
}
