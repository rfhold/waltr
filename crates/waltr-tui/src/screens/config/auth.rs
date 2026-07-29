//! Generic outbound authentication coordination for the Config surface.

use std::collections::BTreeMap;

use waltr_client::{
    AuthStatusSnapshot, ClientAuthTarget, ClientCommand, ClientOperationView, ClientSnapshot,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::screens::{OperationResult, OperationTracker, SectionAction};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthOperationKind {
    Begin,
    Complete,
    Cancel,
    Refresh,
    Logout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackedAuthOperation {
    kind: AuthOperationKind,
    target: ClientAuthTarget,
    tracker: OperationTracker,
}

impl TrackedAuthOperation {
    fn new(kind: AuthOperationKind, target: ClientAuthTarget) -> Self {
        let mut tracker = OperationTracker::new();
        tracker.begin();
        Self {
            kind,
            target,
            tracker,
        }
    }

    fn reconcile(&mut self, snapshot: &ClientSnapshot) -> Option<Result<(), String>> {
        let target = &self.target;
        match self.kind {
            AuthOperationKind::Begin => {
                self.tracker
                    .reconcile(&snapshot.auth.begin, snapshot, |operation| {
                        let ClientOperationView::AuthBegin {
                            status,
                            error,
                            target: operation_target,
                            ..
                        } = operation
                        else {
                            return None;
                        };
                        (operation_target == target).then(|| {
                            operation_result(status, error.as_deref(), "authorization failed")
                        })
                    })
            }
            AuthOperationKind::Refresh => {
                self.tracker
                    .reconcile(&snapshot.auth.refresh, snapshot, |operation| {
                        let ClientOperationView::AuthRefresh {
                            status,
                            error,
                            target: operation_target,
                        } = operation
                        else {
                            return None;
                        };
                        (operation_target == target).then(|| {
                            operation_result(status, error.as_deref(), "credential refresh failed")
                        })
                    })
            }
            AuthOperationKind::Complete => {
                self.tracker
                    .reconcile(&snapshot.auth.complete, snapshot, |operation| {
                        let ClientOperationView::AuthComplete { status, error } = operation else {
                            return None;
                        };
                        Some(operation_result(
                            status,
                            error.as_deref(),
                            "authorization completion failed",
                        ))
                    })
            }
            AuthOperationKind::Cancel => {
                self.tracker
                    .reconcile(&snapshot.auth.cancel, snapshot, |operation| {
                        let ClientOperationView::AuthCancel { status, error } = operation else {
                            return None;
                        };
                        Some(operation_result(
                            status,
                            error.as_deref(),
                            "authorization cancellation failed",
                        ))
                    })
            }
            AuthOperationKind::Logout => {
                self.tracker
                    .reconcile(&snapshot.auth.logout, snapshot, |operation| {
                        let ClientOperationView::AuthLogout {
                            status,
                            error,
                            target: operation_target,
                        } = operation
                        else {
                            return None;
                        };
                        (operation_target == target).then(|| {
                            operation_result(status, error.as_deref(), "credential reset failed")
                        })
                    })
            }
        }
    }
}

/// Config-owned state for provider and upstream MCP authentication actions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigAuthState {
    input: String,
    explicit_target: Option<ClientAuthTarget>,
    reset_target: Option<ClientAuthTarget>,
    operations: Vec<TrackedAuthOperation>,
}

impl ConfigAuthState {
    pub(crate) const fn new() -> Self {
        Self {
            input: String::new(),
            explicit_target: None,
            reset_target: None,
            operations: Vec::new(),
        }
    }

    pub(crate) fn input(&self) -> &str {
        &self.input
    }

    pub(crate) const fn target(&self) -> Option<&ClientAuthTarget> {
        self.explicit_target.as_ref()
    }

    pub(crate) const fn input_mut(&mut self) -> &mut String {
        &mut self.input
    }

    pub(crate) fn clear_input(&mut self) {
        self.input.clear();
    }

    pub(crate) fn dismiss_attempt(&mut self) {
        self.input.clear();
        self.explicit_target = None;
    }

    pub(crate) fn complete_attempt(&mut self, target: &ClientAuthTarget) {
        if self
            .explicit_target
            .as_ref()
            .is_some_and(|current| targets_match(current, target))
        {
            self.dismiss_attempt();
        }
    }

    pub(crate) fn reset_matches(&self, target: &ClientAuthTarget) -> bool {
        self.reset_target
            .as_ref()
            .is_some_and(|reset| targets_match(reset, target))
    }

    pub(crate) const fn has_reset(&self) -> bool {
        self.reset_target.is_some()
    }

    pub(crate) fn pending_for(&self, target: &ClientAuthTarget) -> bool {
        self.operations.iter().any(|operation| {
            targets_match(&operation.target, target) && operation.tracker.is_pending()
        })
    }

    pub(crate) fn error_for(&self, target: &ClientAuthTarget) -> Option<&str> {
        self.operations
            .iter()
            .rev()
            .filter(|operation| targets_match(&operation.target, target))
            .find_map(|operation| operation.tracker.error())
    }

    pub(crate) fn dismiss_reset(&mut self) -> bool {
        self.reset_target.take().is_some()
    }

    pub(crate) fn retry_begin(&mut self, target: ClientAuthTarget) -> Option<ClientCommand> {
        if self.pending_for(&target) {
            return None;
        }
        self.track(AuthOperationKind::Begin, target.clone());
        Some(ClientCommand::BeginAuth { target })
    }

    pub(crate) fn begin_complete(&mut self, target: ClientAuthTarget) {
        self.track(AuthOperationKind::Complete, target);
    }

    pub(crate) fn begin_cancel(&mut self, target: ClientAuthTarget) {
        self.track(AuthOperationKind::Cancel, target);
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        target: Option<ClientAuthTarget>,
        statuses: &BTreeMap<String, AuthStatusSnapshot>,
    ) -> SectionAction {
        if let Some(reset_target) = self.reset_target.clone() {
            if self.pending_for(&reset_target) {
                return SectionAction::Continue;
            }
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.track(AuthOperationKind::Logout, reset_target.clone());
                    SectionAction::ClientCommand(ClientCommand::LogoutAuth {
                        target: reset_target,
                    })
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.reset_target = None;
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            };
        }

        let Some(target) = target else {
            return SectionAction::Unhandled;
        };
        if self.pending_for(&target) {
            return SectionAction::Continue;
        }
        let status = status_for(statuses, &target);
        match key.code {
            KeyCode::Char('a') if status.is_some_and(status_can_authorize) => {
                self.track(AuthOperationKind::Begin, target.clone());
                self.explicit_target = Some(target.clone());
                SectionAction::ClientCommand(ClientCommand::BeginAuth { target })
            }
            KeyCode::Char('f') if status.is_some_and(|status| status.can_refresh) => {
                self.track(AuthOperationKind::Refresh, target.clone());
                SectionAction::ClientCommand(ClientCommand::RefreshAuth { target })
            }
            KeyCode::Char('r') if status.is_some_and(status_can_reset) => {
                self.reset_target = Some(target);
                SectionAction::Continue
            }
            KeyCode::Char('a' | 'f' | 'r') => SectionAction::Continue,
            _ => SectionAction::Unhandled,
        }
    }

    pub(crate) fn reconcile(&mut self, snapshot: &ClientSnapshot) -> bool {
        let mut status_stale = false;
        let mut dismiss_attempt = false;
        self.operations.retain_mut(|operation| {
            let result = operation.reconcile(snapshot);
            if result.as_ref().is_some_and(Result::is_ok) {
                status_stale |= matches!(
                    operation.kind,
                    AuthOperationKind::Refresh | AuthOperationKind::Logout
                );
                if operation.kind == AuthOperationKind::Logout
                    && self
                        .reset_target
                        .as_ref()
                        .is_some_and(|target| targets_match(target, &operation.target))
                {
                    self.reset_target = None;
                }
                dismiss_attempt |= matches!(
                    operation.kind,
                    AuthOperationKind::Complete | AuthOperationKind::Cancel
                );
                return false;
            }
            true
        });
        if dismiss_attempt {
            self.dismiss_attempt();
        }
        if self
            .reset_target
            .as_ref()
            .is_some_and(|target| !target_exists(snapshot, target))
        {
            self.reset_target = None;
        }
        status_stale
    }

    fn track(&mut self, kind: AuthOperationKind, target: ClientAuthTarget) {
        self.operations
            .retain(|operation| !targets_match(&operation.target, &target));
        self.operations
            .push(TrackedAuthOperation::new(kind, target));
    }
}

pub(super) fn status_for<'a>(
    statuses: &'a BTreeMap<String, AuthStatusSnapshot>,
    target: &ClientAuthTarget,
) -> Option<&'a AuthStatusSnapshot> {
    let prefix = if target.kind == "mcpServer" {
        "mcp"
    } else {
        "provider"
    };
    statuses.get(&format!("{prefix}:{}", target.key))
}

pub(super) fn status_can_reset(status: &AuthStatusSnapshot) -> bool {
    matches!(
        status.kind.as_str(),
        "oauthConfigured" | "expired" | "authorizationRequired"
    )
}

pub(super) fn status_can_authorize(status: &AuthStatusSnapshot) -> bool {
    status.kind != "apiKeyConfigured"
        && status
            .supported_methods
            .iter()
            .any(|method| method.starts_with("oauth"))
}

pub fn targets_match(left: &ClientAuthTarget, right: &ClientAuthTarget) -> bool {
    left.kind == right.kind && left.key == right.key
}

fn target_exists(snapshot: &ClientSnapshot, target: &ClientAuthTarget) -> bool {
    let Some(config) = snapshot.config.current.as_ref() else {
        return true;
    };
    if target.kind == "mcpServer" {
        config.mcp_servers.contains_key(&target.key)
    } else {
        config.providers.contains_key(&target.key)
    }
}

fn operation_result(
    status: &'static str,
    error: Option<&str>,
    fallback: &str,
) -> OperationResult<()> {
    match status {
        "ready" => OperationResult::Ready(()),
        "failed" => OperationResult::Failed(error.unwrap_or(fallback).to_owned()),
        _ => OperationResult::Pending,
    }
}
