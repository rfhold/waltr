//! Embedded web asset responses.

use axum::{
    Extension,
    body::Body,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

#[derive(Debug)]
struct WebAsset {
    path: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

#[derive(Clone)]
pub(crate) struct AppsBootstrap(String);

impl AppsBootstrap {
    pub(crate) fn new(value: String) -> std::io::Result<Self> {
        let url = url::Url::parse(&value).map_err(std::io::Error::other)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/container.html"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(std::io::Error::other("invalid public App container URL"));
        }
        Ok(Self(url.to_string()))
    }
}

pub async fn serve_root(bootstrap: Option<Extension<AppsBootstrap>>) -> Response {
    serve_web_asset_path("/", bootstrap.as_ref().map(|value| &value.0))
}

pub async fn serve_path(
    Path(path): Path<String>,
    bootstrap: Option<Extension<AppsBootstrap>>,
) -> Response {
    serve_web_asset_path(
        &format!("/{path}"),
        bootstrap.as_ref().map(|value| &value.0),
    )
}

fn serve_web_asset_path(path: &str, bootstrap: Option<&AppsBootstrap>) -> Response {
    if let Some(asset) = asset_for_path(path) {
        return response_for_asset(asset, bootstrap);
    }

    if !path.rsplit('/').next().unwrap_or_default().contains('.')
        && let Some(asset) = asset_for_path("/index.html")
    {
        return response_for_asset(asset, bootstrap);
    }

    StatusCode::NOT_FOUND.into_response()
}

fn asset_for_path(path: &str) -> Option<&'static WebAsset> {
    let path = if path == "/" { "/index.html" } else { path };
    WEB_ASSETS.iter().find(|asset| asset.path == path)
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn response_for_asset(asset: &'static WebAsset, bootstrap: Option<&AppsBootstrap>) -> Response {
    if asset.path == "/index.html"
        && let Some(bootstrap) = bootstrap
    {
        let html = std::str::from_utf8(asset.bytes).expect("bundled index HTML must be UTF-8");
        let meta = format!(
            "<meta name=\"waltr-mcp-apps-container-url\" content=\"{}\">",
            escape_attribute(&bootstrap.0)
        );
        let Some(head) = html.find("<head>") else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let mut document = String::with_capacity(html.len() + meta.len());
        document.push_str(&html[..head + 6]);
        document.push_str(&meta);
        document.push_str(&html[head + 6..]);
        return Response::builder()
            .header(header::CONTENT_TYPE, asset.content_type)
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(document))
            .expect("bootstrap HTML response should be valid");
    }
    Response::builder()
        .header(header::CONTENT_TYPE, asset.content_type)
        .body(Body::from(asset.bytes))
        .expect("static web asset response should be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_applies_to_root_and_spa_fallback_only() {
        let bootstrap = AppsBootstrap::new("http://127.0.0.1:12345/container.html".into()).unwrap();
        for path in ["/", "/index.html", "/sessions/example"] {
            let response = serve_web_asset_path(path, Some(&bootstrap));
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(html.contains("<head><meta name=\"waltr-mcp-apps-container-url\" content=\"http://127.0.0.1:12345/container.html\">"));
        }
        assert_eq!(
            serve_web_asset_path("/apps/container.html", Some(&bootstrap)).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(escape_attribute("&\"'<x>"), "&amp;&quot;&#39;&lt;x&gt;");
        for value in [
            "javascript:alert(1)",
            "https://user:password@host/container.html",
            "https://host/container.html?secret=x",
        ] {
            assert!(AppsBootstrap::new(value.into()).is_err());
        }
    }
}
