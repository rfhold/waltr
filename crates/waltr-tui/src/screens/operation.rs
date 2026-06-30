//! Shared helpers for tracking keyed client command operations from TUI state.

use std::collections::BTreeMap;

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSnapshot, OperationKey, OperationSnapshot,
};

/// Outcome of routing an input event into a screen section.
#[derive(Clone, Debug, PartialEq)]
pub enum SectionAction {
    /// The section handled the event; keep running.
    Continue,
    /// The section did not handle the event; let the host handle it.
    Unhandled,
    /// Dispatch a shared client command.
    ClientCommand(ClientCommand),
    /// Dispatch shared client commands in order.
    ClientCommands(Vec<ClientCommand>),
}

/// Outcome extracted from a keyed client operation snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationResult<T> {
    /// Operation is still in progress.
    Pending,
    /// Operation completed successfully.
    Ready(T),
    /// Operation failed with a user-facing message.
    Failed(String),
}

/// Runtime-local tracker for one submitted command operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperationTracker {
    pending: bool,
    key: Option<OperationKey>,
    error: Option<String>,
}

impl OperationTracker {
    /// Create an idle tracker.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: false,
            key: None,
            error: None,
        }
    }

    /// Start tracking the next matching operation key from the shared snapshot.
    pub fn begin(&mut self) {
        self.pending = true;
        self.key = None;
        self.error = None;
    }

    /// Whether this tracker is waiting for a terminal operation result.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// Last terminal operation error, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Clear pending state and error.
    pub fn clear(&mut self) {
        self.pending = false;
        self.key = None;
        self.error = None;
    }

    /// Clear only the visible error.
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Reconcile this tracker against a keyed operation collection.
    pub fn reconcile<T>(
        &mut self,
        operation: &OperationSnapshot,
        snapshot: &ClientSnapshot,
        inspect: impl FnOnce(&ClientOperationView) -> Option<OperationResult<T>>,
    ) -> Option<Result<T, String>> {
        if !self.pending {
            return None;
        }

        if self.key.is_none() {
            if operation.status != "loading" {
                return None;
            }

            self.key.clone_from(&operation.key);
        }

        let key = self.key.as_ref()?;
        let result = inspect_operation(&snapshot.operations.current, key, inspect)?;

        match result {
            OperationResult::Pending => None,
            OperationResult::Ready(value) => {
                self.clear();
                Some(Ok(value))
            }
            OperationResult::Failed(message) => {
                self.pending = false;
                self.key = None;
                self.error = Some(message.clone());
                Some(Err(message))
            }
        }
    }
}

fn inspect_operation<T>(
    operations: &BTreeMap<OperationKey, ClientOperationView>,
    key: &str,
    inspect: impl FnOnce(&ClientOperationView) -> Option<OperationResult<T>>,
) -> Option<OperationResult<T>> {
    operations.get(key).and_then(inspect)
}
