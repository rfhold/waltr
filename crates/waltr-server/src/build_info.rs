//! Compile-time server build metadata.

/// Server package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sanitized source revision supplied by the container build.
pub fn revision() -> String {
    sanitize(option_env!("WALTR_BUILD_REVISION").unwrap_or("unknown"))
}

fn sanitize(value: &str) -> String {
    let revision: String = value
        .chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if revision.is_empty() {
        "unknown".to_owned()
    } else {
        revision
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn revision_is_safe_for_startup_logs() {
        assert_eq!(sanitize("abc123-dirty"), "abc123-dirty");
        assert_eq!(sanitize("rev\n\u{1b}[31msecret"), "rev___31msecret");
        assert_eq!(sanitize(""), "unknown");
        assert_eq!(sanitize(&"a".repeat(129)).len(), 128);
    }
}
