//! Embedded web asset responses.

use axum::{
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

pub async fn serve_root() -> Response {
    serve_web_asset_path("/")
}

pub async fn serve_path(Path(path): Path<String>) -> Response {
    serve_web_asset_path(&format!("/{path}"))
}

fn serve_web_asset_path(path: &str) -> Response {
    if let Some(asset) = asset_for_path(path) {
        return response_for_asset(asset);
    }

    if !path.rsplit('/').next().unwrap_or_default().contains('.')
        && let Some(asset) = asset_for_path("/index.html")
    {
        return response_for_asset(asset);
    }

    StatusCode::NOT_FOUND.into_response()
}

fn asset_for_path(path: &str) -> Option<&'static WebAsset> {
    let path = if path == "/" { "/index.html" } else { path };
    WEB_ASSETS.iter().find(|asset| asset.path == path)
}

fn response_for_asset(asset: &'static WebAsset) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, asset.content_type)
        .body(Body::from(asset.bytes))
        .expect("static web asset response should be valid")
}
