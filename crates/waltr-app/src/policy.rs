//! Security policies shared by Tauri hooks and focused tests.

use url::Url;

/// Validate an external URL before handing it to the operating system.
pub fn external_https_url(value: &str) -> Result<Url, &'static str> {
    if value.contains('\\') || value.chars().any(char::is_whitespace) {
        return Err("invalid external URL");
    }
    let url = Url::parse(value).map_err(|_| "invalid external URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("external URL must be credential-free HTTPS without a fragment");
    }
    Ok(url)
}

/// Return whether a main-WebView navigation remains on packaged app content.
#[must_use]
pub fn allows_packaged_navigation(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }

    match url.scheme() {
        "tauri" | "asset" => url.host_str().is_none_or(|host| host == "localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost") && url.port().is_none(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_policy_accepts_only_credential_free_https() {
        for accepted in [
            "https://example.com",
            "https://example.com/path?q=value",
            "https://xn--bcher-kva.example/authorize?state=opaque",
        ] {
            assert!(external_https_url(accepted).is_ok(), "{accepted}");
        }

        for rejected in [
            "http://example.com",
            "waltr://login",
            "file:///etc/passwd",
            "data:text/html,pwned",
            "javascript:alert(1)",
            "https://user@example.com",
            "https://user:pass@example.com",
            "https://example.com/#token",
            "https://example.com\\@evil.example",
            "https://example.com%2Fevil.example",
            "//example.com/path",
            "not a url",
        ] {
            assert!(external_https_url(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn navigation_policy_allows_local_spa_routes_only() {
        for accepted in [
            "tauri://localhost/index.html",
            "tauri://localhost/sessions/one?tab=timeline",
            "https://tauri.localhost/settings",
            "http://tauri.localhost/sessions",
            "asset://localhost/index.html",
        ] {
            assert!(allows_packaged_navigation(&Url::parse(accepted).unwrap()));
        }

        for rejected in [
            "https://preview-waltr.holdenitdown.net",
            "https://tauri.localhost.evil.example",
            "https://user@tauri.localhost",
            "file:///tmp/index.html",
            "data:text/html,pwned",
            "javascript:alert(1)",
        ] {
            assert!(
                !allows_packaged_navigation(&Url::parse(rejected).unwrap()),
                "{rejected}"
            );
        }
    }
}
