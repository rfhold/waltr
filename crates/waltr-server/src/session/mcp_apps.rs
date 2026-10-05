use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use waltr_agent::{McpAppError, McpAppHandle, SessionId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;
use tonic::{Request, Status};

pub(super) const TIMEOUT: Duration = Duration::from_secs(30);
const TTL: Duration = Duration::from_secs(900);
const MAX_APPS: usize = 128;

pub(super) type Owner = Option<(String, String)>;

pub(super) fn owner<T>(request: &Request<T>) -> Owner {
    request
        .extensions()
        .get::<crate::access_auth::AuthenticatedPrincipal>()
        .map(|principal| {
            (
                principal.issuer().to_owned(),
                principal.subject().to_owned(),
            )
        })
}

#[derive(Debug, Clone)]
pub(super) struct Entry {
    pub owner: Owner,
    pub session: SessionId,
    _call_id: String,
    pub handle: McpAppHandle,
    pub cancellation: tokio_util::sync::CancellationToken,
    expires: Instant,
}

#[derive(Debug, Default)]
pub(super) struct Registry(Mutex<HashMap<String, OwnedEntry>>);

#[derive(Debug)]
struct OwnedEntry {
    entry: Entry,
    _guard: tokio_util::sync::DropGuard,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        let registry = Arc::new(Self::default());
        let weak = Arc::downgrade(&registry);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                registry
                    .0
                    .lock()
                    .unwrap()
                    .retain(|_, owned| owned.entry.expires > Instant::now());
            }
        });
        registry
    }

    pub fn insert(
        &self,
        owner: Owner,
        session: SessionId,
        call_id: String,
        handle: McpAppHandle,
    ) -> Result<String, Status> {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|_, owned| owned.entry.expires > Instant::now());
        if entries.len() >= MAX_APPS {
            return Err(Status::resource_exhausted("MCP app limit"));
        }
        let id = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        entries.insert(
            id.clone(),
            OwnedEntry {
                entry: Entry {
                    owner,
                    session,
                    _call_id: call_id,
                    handle,
                    cancellation,
                    expires: Instant::now() + TTL,
                },
                _guard: guard,
            },
        );
        Ok(id)
    }

    pub fn get(&self, id: &str, owner: &Owner) -> Result<Entry, Status> {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|_, owned| owned.entry.expires > Instant::now());
        entries
            .get(id)
            .map(|owned| &owned.entry)
            .filter(|entry| &entry.owner == owner)
            .cloned()
            .ok_or_else(|| Status::failed_precondition("MCP app stale"))
    }

    pub fn remove(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub(super) fn status(error: McpAppError) -> Status {
    match error {
        McpAppError::Busy => Status::failed_precondition("MCP app busy"),
        McpAppError::Stale => Status::failed_precondition("MCP app stale"),
        McpAppError::Canceled => Status::cancelled("MCP app canceled"),
        McpAppError::ApprovalRequired => Status::permission_denied("MCP app approval required"),
        McpAppError::Denied => Status::permission_denied("MCP app denied"),
        McpAppError::Limit => Status::resource_exhausted("MCP app limit"),
        McpAppError::Failure(_) => Status::unavailable("MCP app unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errors_do_not_expose_upstream_details() {
        assert_eq!(
            status(McpAppError::Failure("credential-secret".into())).message(),
            "MCP app unavailable"
        );
        assert_eq!(
            status(McpAppError::ApprovalRequired).code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            status(McpAppError::Busy).code(),
            tonic::Code::FailedPrecondition
        );
    }
}
