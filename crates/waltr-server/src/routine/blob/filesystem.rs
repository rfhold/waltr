//! Private descriptor-rooted Unix storage with fixed image signature checks.
use super::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::FilesystemBlobStore;

/// Host topology, selected by application composition rather than path existence.
#[derive(Debug, Clone)]
pub enum WorkspaceRoot {
    /// Workspace on this host; require directory-descriptor identity checks.
    Local(std::path::PathBuf),
    /// Absolute worker identity verified by the private socket handshake.
    SocketWorker(std::path::PathBuf),
}

/// Filesystem configuration is host-only, never part of the domain port.
pub struct FilesystemConfig {
    /// Existing empty 0700 directory, outside the worker workspace and its mounts.
    pub private_root: std::path::PathBuf,
    /// Workspace exclusion boundary and explicit host topology.
    pub workspace_root: WorkspaceRoot,
    /// Process-monotonic lease clock.
    pub clock: Arc<dyn Clock>,
}
impl std::fmt::Debug for FilesystemConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemConfig")
            .finish_non_exhaustive()
    }
}

/// Unix-only adapter; other platforms explicitly fail construction.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct FilesystemBlobStore;
#[cfg(not(unix))]
impl FilesystemBlobStore {
    /// No false claim of portable private-filesystem enforcement.
    pub fn new(_config: FilesystemConfig) -> Result<Self, BlobError> {
        Err(BlobError::Unsupported)
    }
}
