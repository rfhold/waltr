//! Errors without request or audio payload logging.
use std::fmt;

/// Adapter failure. Upstream status, metadata and details remain available.
#[derive(Debug)]
pub enum Error {
    /// Invalid caller input or resolved settings.
    Invalid(&'static str),
    /// Transport setup failure.
    Transport(tonic::transport::Error),
    /// Unmodified upstream gRPC status.
    Status(Box<tonic::Status>),
    /// Explicit cancellation.
    Cancelled,
    /// Whole-operation deadline elapsed.
    Deadline,
}
impl From<tonic::Status> for Error {
    fn from(value: tonic::Status) -> Self {
        Self::Status(Box::new(value))
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid audio request: {reason}"),
            Self::Transport(_) => f.write_str("audio transport failed"),
            Self::Status(status) => write!(f, "upstream gRPC status: {}", status.code()),
            Self::Cancelled => f.write_str("audio operation cancelled"),
            Self::Deadline => f.write_str("audio operation deadline elapsed"),
        }
    }
}
impl std::error::Error for Error {}
