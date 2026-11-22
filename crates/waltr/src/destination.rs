//! Destination selector for the typed Waltr API.

/// Name of the ASCII gRPC metadata field. Absence selects the entry server.
pub const HEADER: &str = "waltr-destination";

/// Explicit entry-server selector.
pub const LOCAL: &str = "local";

/// Metadata key in an entry server's Remote authorization status.
pub const ROUTE_ID: &str = "remoteRouteId";

/// Whether an authorization status contains a well-formed route ID.
#[must_use]
pub fn is_route_id(id: &str) -> bool {
    id.len() == 32
        && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Split a pinned selector obtained from the entry server.
#[must_use]
pub fn pinned(selector: &str) -> Option<(&str, &str)> {
    let (name, id) = selector.split_once('@')?;
    (is_remote_name(name) && is_route_id(id)).then_some((name, id))
}

/// Whether `name` can name a Remote resource.
#[must_use]
pub fn is_remote_name(name: &str) -> bool {
    !matches!(name, "" | "." | ".." | LOCAL)
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
