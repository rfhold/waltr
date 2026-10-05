//! Transient owner-bound MCP Apps coordination.

use crate::{Client, ClientEffect, ClientError, ClientState, ClientUpdate};
use waltr::grpc::waltr::session::v1::{
    CallSessionMcpAppToolRequest, CloseSessionMcpAppRequest, OpenSessionMcpAppRequest,
};
use serde::Serialize;
use std::collections::BTreeMap;
use ts_rs::TS;

#[derive(Clone, Default, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
/// Immutable open payload for sandbox initialization; never log or persist it.
pub struct McpAppView {
    /// Opaque server lease identity.
    pub app_id: String,
    /// Untrusted sandbox document.
    pub html: String,
    /// Original tool declaration JSON.
    pub tool_json: String,
    /// Complete original arguments JSON.
    pub arguments_json: String,
    /// Complete original result JSON, including private metadata.
    pub result_json: String,
    /// Resource metadata JSON for sandbox policy, not source stamps.
    pub resource_meta_json: String,
}

impl std::fmt::Debug for McpAppView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpAppView { protected payload }")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
/// RPC kind tracked independently by caller request identity.
#[allow(missing_docs)]
pub enum McpAppOperationKind {
    Open,
    Call,
    Close,
}

#[derive(Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
/// Transient RPC outcome. Debug deliberately excludes protected results.
pub struct McpAppOperation {
    /// Operation performed.
    pub kind: McpAppOperationKind,
    /// Current operation lifecycle.
    pub status: String,
    /// Associated server lease once known.
    pub app_id: Option<String>,
    /// Sanitized operation failure.
    pub error: Option<String>,
    /// Complete refresh result; never replaces the original open result.
    pub result_json: Option<String>,
}

impl std::fmt::Debug for McpAppOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAppOperation")
            .field("kind", &self.kind)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
/// In-memory protected Apps state, cleared with Waltr access state.
pub struct McpAppsSnapshot {
    /// Immutable open results by server lease identity.
    pub current: BTreeMap<String, McpAppView>,
    /// Operation outcomes by caller request identity (close uses an operation key).
    pub requests: BTreeMap<String, McpAppOperation>,
}

#[derive(Clone, PartialEq, Eq)]
/// RPC effect executed by the existing native/wasm transport.
#[allow(missing_docs)]
pub enum McpAppEffect {
    Open {
        request_id: String,
        request: OpenSessionMcpAppRequest,
    },
    Call {
        request_id: String,
        request: CallSessionMcpAppToolRequest,
    },
    Close {
        request_id: String,
        request: CloseSessionMcpAppRequest,
    },
}
impl std::fmt::Debug for McpAppEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpAppEffect { protected request }")
    }
}

#[derive(Clone, PartialEq)]
/// RPC completion facts. Protected payloads are omitted from Debug.
#[allow(missing_docs)]
pub enum McpAppEvent {
    Opened {
        request_id: String,
        app: McpAppView,
    },
    Called {
        request_id: String,
        app_id: String,
        result_json: String,
    },
    Closed {
        request_id: String,
    },
    Failed {
        request_id: String,
        error: ClientError,
    },
}
impl std::fmt::Debug for McpAppEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpAppEvent { protected result }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_access_generation_cannot_restore_protected_app_data() {
        let mut client = Client::new();
        client.open_session_mcp_app("session".into(), "call".into(), "open".into());
        let generation = client.state().access_generation;
        client.logout_waltr();
        client.apply_for_access_generation(
            generation,
            crate::ClientEvent::McpApp(McpAppEvent::Opened {
                request_id: "open".into(),
                app: McpAppView {
                    app_id: "old-owner".into(),
                    html: "private-html".into(),
                    ..McpAppView::default()
                },
            }),
        );
        assert!(client.state().apps.current.is_empty());
        assert!(client.state().apps.requests.is_empty());
    }
}

fn loading(kind: McpAppOperationKind, app_id: Option<String>) -> McpAppOperation {
    McpAppOperation {
        kind,
        status: "loading".into(),
        app_id,
        error: None,
        result_json: None,
    }
}

pub(crate) fn retain_completed_request(state: &mut ClientState, request_id: String) {
    let Some(operation) = state.apps.requests.get(&request_id) else {
        return;
    };
    if operation.status == "loading"
        || (operation.kind == McpAppOperationKind::Open && operation.status == "ready")
        || state.apps_completed_requests.contains(&request_id)
    {
        return;
    }
    state.apps_completed_requests.push_back(request_id);
    while state.apps_completed_requests.len() > 128 {
        if let Some(id) = state.apps_completed_requests.pop_front() {
            state.apps.requests.remove(&id);
        }
    }
}

impl Client {
    /// Open a durable tool result's app using a unique caller-local request id.
    pub fn open_session_mcp_app(
        &mut self,
        session_id: String,
        call_id: String,
        request_id: String,
    ) -> ClientUpdate {
        if !self.state().permits_protected_requests()
            || self.state().apps.requests.contains_key(&request_id)
        {
            return ClientUpdate::default();
        }
        self.apps_state_mut()
            .requests
            .insert(request_id.clone(), loading(McpAppOperationKind::Open, None));
        ClientUpdate::with_effect(ClientEffect::McpApp(McpAppEffect::Open {
            request_id,
            request: OpenSessionMcpAppRequest {
                session_id,
                call_id,
            },
        }))
    }
    /// Invoke an allowed app tool without inference or timeline mutation.
    pub fn call_session_mcp_app_tool(
        &mut self,
        app_id: String,
        name: String,
        arguments_json: String,
        request_id: String,
    ) -> ClientUpdate {
        if !self.state().permits_protected_requests()
            || self.state().apps.requests.contains_key(&request_id)
        {
            return ClientUpdate::default();
        }
        let valid = self.state().apps.current.contains_key(&app_id);
        let mut operation = loading(McpAppOperationKind::Call, Some(app_id.clone()));
        if !valid {
            operation.status = "failed".into();
            operation.error = Some("MCP app is closed or unavailable".into());
        }
        self.apps_state_mut()
            .requests
            .insert(request_id.clone(), operation);
        if !valid {
            self.retain_completed_app_request(request_id);
            return ClientUpdate::default();
        }
        ClientUpdate::with_effect(ClientEffect::McpApp(McpAppEffect::Call {
            request_id,
            request: CallSessionMcpAppToolRequest {
                app_id,
                name,
                arguments_json,
            },
        }))
    }
    /// Cancel presentation interest without dropping the in-flight open response.
    pub fn cancel_session_mcp_app_open(&mut self, request_id: String) -> ClientUpdate {
        let ready_app_id = self
            .state()
            .apps
            .requests
            .get(&request_id)
            .filter(|op| op.kind == McpAppOperationKind::Open && op.status == "ready")
            .and_then(|op| op.app_id.clone());
        if let Some(app_id) = ready_app_id {
            return self.close_session_mcp_app(app_id);
        }
        if let Some(operation) = self.apps_state_mut().requests.get_mut(&request_id) {
            if operation.kind == McpAppOperationKind::Open && operation.status == "loading" {
                operation.status = "canceled".into();
            }
        }
        self.retain_completed_app_request(request_id);
        ClientUpdate::default()
    }
    /// Clear all local payloads for a lease before the best-effort server close.
    pub fn close_session_mcp_app(&mut self, app_id: String) -> ClientUpdate {
        if !self.state().permits_protected_requests() {
            return ClientUpdate::default();
        }
        let request_id = format!("mcp-app-close:{}", self.next_app_operation_key());
        self.apps_state_mut().current.remove(&app_id);
        self.apps_state_mut()
            .requests
            .retain(|_, op| op.app_id.as_ref() != Some(&app_id));
        self.apps_state_mut().requests.insert(
            request_id.clone(),
            loading(McpAppOperationKind::Close, Some(app_id.clone())),
        );
        ClientUpdate::with_effect(ClientEffect::McpApp(McpAppEffect::Close {
            request_id,
            request: CloseSessionMcpAppRequest { app_id },
        }))
    }
}

pub(crate) fn reduce(state: &mut ClientState, event: McpAppEvent) -> ClientUpdate {
    let request_id = match &event {
        McpAppEvent::Opened { request_id, .. }
        | McpAppEvent::Called { request_id, .. }
        | McpAppEvent::Closed { request_id }
        | McpAppEvent::Failed { request_id, .. } => request_id,
    };
    let current = state.apps.requests.get(request_id).is_some_and(|op| {
        op.status == "loading"
            && match &event {
                McpAppEvent::Opened { .. } => op.kind == McpAppOperationKind::Open,
                McpAppEvent::Called { app_id, .. } => {
                    op.kind == McpAppOperationKind::Call
                        && op.app_id.as_ref() == Some(app_id)
                        && state.apps.current.contains_key(app_id)
                }
                McpAppEvent::Closed { .. } => op.kind == McpAppOperationKind::Close,
                McpAppEvent::Failed { .. } => true,
            }
    });
    if !current {
        if let McpAppEvent::Opened { app, .. } = event {
            if state.apps.current.contains_key(&app.app_id) {
                return ClientUpdate::default();
            }
            return ClientUpdate::with_effect(ClientEffect::McpApp(McpAppEffect::Close {
                request_id: String::new(),
                request: CloseSessionMcpAppRequest { app_id: app.app_id },
            }));
        }
        return ClientUpdate::default();
    }
    let request_id = request_id.clone();
    let operation = state.apps.requests.get_mut(&request_id).unwrap();
    operation.status = "ready".into();
    match event {
        McpAppEvent::Opened { app, .. } => {
            operation.app_id = Some(app.app_id.clone());
            state.apps.current.insert(app.app_id.clone(), app);
        }
        McpAppEvent::Called {
            app_id,
            result_json,
            ..
        } => {
            if operation.app_id.as_ref() == Some(&app_id)
                && state.apps.current.contains_key(&app_id)
            {
                operation.result_json = Some(result_json);
            }
        }
        McpAppEvent::Closed { .. } => {}
        McpAppEvent::Failed { error, .. } => {
            operation.status = "failed".into();
            operation.error = Some(
                if error.is_waltr_login_required() {
                    "Waltr login required"
                } else {
                    "MCP app operation failed"
                }
                .into(),
            );
        }
    }
    retain_completed_request(state, request_id);
    ClientUpdate::default()
}
