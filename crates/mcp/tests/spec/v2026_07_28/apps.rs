use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use mcp::{
    McpResourceResult, McpToolResult,
    apps::*,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
    },
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::super::support::SpecServer;

const URI: &str = "ui://test/dashboard";
const HTML: &str = "<!DOCTYPE html><html><head></head><body>test</body></html>";

#[test]
fn tool_metadata_precedence_defaults_and_unknown_preservation() {
    let default = tool_meta(None).unwrap();
    assert!(default.visible_to(UiVisibility::Model));
    assert!(default.visible_to(UiVisibility::App));
    let metadata = json!({
        "ui": {"resourceUri": URI, "visibility": ["app"], "future": {"keep": true}},
        "ui/resourceUri": "ui://deprecated/fallback", "vendor": {"keep": true}
    });
    let tool = mcp::parse_tool_definition(&json!({
        "name": "dashboard", "inputSchema": {"type": "object"}, "_meta": metadata
    }))
    .unwrap();
    assert_eq!(tool.meta.as_ref().unwrap(), &metadata);
    let parsed = tool_meta(tool.meta.as_ref()).unwrap();
    assert_eq!(parsed.resource_uri.as_deref(), Some(URI));
    assert!(!parsed.visible_to(UiVisibility::Model));
    assert!(parsed.visible_to(UiVisibility::App));
    assert_eq!(serde_json::to_value(parsed).unwrap(), metadata["ui"]);
    let flat = tool_meta(Some(&json!({"ui/resourceUri": URI}))).unwrap();
    assert_eq!(flat.resource_uri.as_deref(), Some(URI));
    assert_eq!(flat.visibility, default.visibility);
    let nested_without_uri = tool_meta(Some(
        &json!({"ui": {"visibility": ["model"]}, "ui/resourceUri": URI}),
    ))
    .unwrap();
    assert_eq!(nested_without_uri.resource_uri.as_deref(), Some(URI));
    assert_eq!(nested_without_uri.visibility, vec![UiVisibility::Model]);
    assert!(
        tool_meta(Some(&json!({"ui": {"visibility": []}})))
            .unwrap()
            .visibility
            .is_empty()
    );
    // A malformed deprecated value cannot override a valid nested value.
    assert!(
        tool_meta(Some(
            &json!({"ui": {"resourceUri": URI}, "ui/resourceUri": 123})
        ))
        .is_ok()
    );
}

#[test]
fn malformed_apps_do_not_disable_ordinary_tool_parsing() {
    for metadata in [
        json!({"ui": null}),
        json!({"ui": []}),
        json!({"ui": {"visibility": "app"}}),
        json!({"ui": {"visibility": ["unknown"]}}),
        json!({"ui": {"visibility": [1]}}),
        json!({"ui": {"visibility": null}}),
        json!({"ui": {"resourceUri": null}}),
        json!({"ui": {"resourceUri": "https://example.com"}}),
        json!({"ui": {"resourceUri": "ui://"}}),
        json!({"ui": {"resourceUri": 1}, "ui/resourceUri": URI}),
        json!({"ui/resourceUri": false}),
    ] {
        let tool = mcp::parse_tool_definition(
            &json!({"name": "bad-ui", "inputSchema": {}, "_meta": metadata}),
        )
        .unwrap();
        assert_eq!(tool.meta.as_ref(), Some(&metadata));
        assert!(tool_meta(tool.meta.as_ref()).is_err(), "{metadata}");
        assert!(UiToolDeclaration::try_from(&tool).is_err(), "{metadata}");
    }
    assert!(tool_meta(Some(&json!([]))).is_err());
}

#[test]
fn declarations_retain_core_data_and_do_not_compare_read_metadata() {
    let tool = mcp::parse_tool_definition(&json!({
        "name": "dashboard", "inputSchema": {}, "_meta": {"ui": {"visibility": ["app"]}}
    }))
    .unwrap();
    let declaration = UiToolDeclaration::try_from(&tool).unwrap();
    assert!(std::ptr::eq(declaration.tool, &tool));
    assert!(declaration.ui.visible_to(UiVisibility::App));
    assert_eq!(declaration.ui.resource_uri, None);
    let resource = mcp::parse_resource_definition(&json!({
        "name": "dashboard", "uri": URI, "mimeType": UI_MIME_TYPE,
        "_meta": {"ui": {"prefersBorder": false}, "vendor": 1}
    }))
    .unwrap();
    let declaration = UiResourceDeclaration::try_from(&resource).unwrap();
    assert!(std::ptr::eq(declaration.resource, &resource));
    assert_eq!(declaration.ui.prefers_border, Some(false));
    let decoded = decode_resource(&read(content()), URI, 1024).unwrap();
    assert_ne!(declaration.ui, decoded.ui);
    let mut invalid = resource.clone();
    invalid.meta = Some(json!({"ui": null}));
    assert!(matches!(
        UiResourceDeclaration::try_from(&invalid),
        Err(AppsError::Metadata(_))
    ));
    invalid.meta = None;
    invalid.mime_type = None;
    assert_eq!(
        UiResourceDeclaration::try_from(&invalid).unwrap_err(),
        AppsError::MimeType
    );
}

#[test]
fn resource_security_metadata_is_typed_and_strict() {
    let raw = json!({"ui": {
        "csp": {"connectDomains": ["https://api.example.com", "wss://socket.example.com:8443"],
            "resourceDomains": ["https://*.example.com"], "frameDomains": [], "future": "keep"},
        "permissions": {"camera": {}, "clipboardWrite": {}, "future": {}},
        "domain": "host-defined", "prefersBorder": false, "future": {"keep": true}
    }, "vendor": "keep"});
    assert_eq!(
        serde_json::to_value(resource_meta(Some(&raw)).unwrap()).unwrap(),
        raw["ui"]
    );
    assert_eq!(resource_meta(None).unwrap(), UiResourceMeta::default());
    for ui in [
        json!(null),
        json!({"csp": true}),
        json!({"csp": null}),
        json!({"csp": {"connectDomains": null}}),
        json!({"csp": {"connectDomains": "https://api.example.com"}}),
        json!({"csp": {"frameDomains": [123]}}),
        json!({"permissions": []}),
        json!({"permissions": {"camera": true}}),
        json!({"permissions": {"camera": null}}),
        json!({"prefersBorder": "true"}),
        json!({"domain": 1}),
    ] {
        assert!(resource_meta(Some(&json!({"ui": ui}))).is_err(), "{ui}");
    }
    for source in [
        "*",
        "'unsafe-inline'",
        "https://example.com;script-src *",
        "https://example.com/path",
        "https://user:pass@example.com",
        "javascript:alert(1)",
        "https://example.com?x=1",
        "https://example.com\n",
        "https://foo.*.com",
        "https://example.com/../",
        "https:////example.com",
    ] {
        assert!(
            resource_meta(Some(&json!({"ui": {"csp": {"resourceDomains": [source]}}}))).is_err(),
            "{source}"
        );
    }
}

fn read(content: Value) -> McpResourceResult {
    McpResourceResult {
        raw: json!({"contents": [content], "_meta": {"outer": "keep"}}),
    }
}

fn content() -> Value {
    json!({"uri": URI, "mimeType": UI_MIME_TYPE, "text": HTML,
        "_meta": {"ui": {"prefersBorder": true}, "vendor": {"secret": "keep"}}})
}

#[test]
fn resource_text_and_blob_decoding_preserves_raw_metadata() {
    let result = read(content());
    let snapshot = result.clone();
    let decoded = decode_resource(&result, URI, HTML.len()).unwrap();
    assert_eq!(decoded.html, HTML);
    assert_eq!(decoded.uri, URI);
    assert_eq!(decoded.ui.prefers_border, Some(true));
    assert_eq!(decoded.meta.unwrap()["vendor"], json!({"secret": "keep"}));
    assert_eq!(result, snapshot);
    let mut blob = content();
    blob.as_object_mut().unwrap().remove("text");
    blob["blob"] = json!(STANDARD.encode(HTML));
    assert_eq!(
        decode_resource(&read(blob), URI, HTML.len()).unwrap().html,
        HTML
    );
    assert_eq!(
        decode_resource(&result, URI, HTML.len() - 1),
        Err(AppsError::TooLarge)
    );
}

#[test]
fn invalid_resource_results_are_rejected() {
    for (field, value, expected) in [
        ("uri", json!("ui://different"), AppsError::Uri),
        ("mimeType", json!("text/html"), AppsError::MimeType),
        ("mimeType", Value::Null, AppsError::MimeType),
        ("text", json!(123), AppsError::Content),
        (
            "text",
            json!("<div>fragment</div>"),
            AppsError::HtmlDocument,
        ),
        ("blob", json!(STANDARD.encode(HTML)), AppsError::Content),
    ] {
        let mut raw = content();
        raw[field] = value;
        assert_eq!(
            decode_resource(&read(raw), URI, DEFAULT_MAX_HTML_BYTES),
            Err(expected)
        );
    }
    for (blob, max, expected) in [
        ("%%%".to_owned(), 1024, AppsError::Base64),
        (STANDARD.encode([0xff]), 1024, AppsError::Utf8),
        (STANDARD.encode(HTML), HTML.len() - 1, AppsError::TooLarge),
        (STANDARD.encode(HTML), 1, AppsError::TooLarge),
    ] {
        assert_eq!(
            decode_resource(
                &read(json!({"uri": URI, "mimeType": UI_MIME_TYPE, "blob": blob})),
                URI,
                max
            ),
            Err(expected)
        );
    }
    let mut missing = content();
    missing.as_object_mut().unwrap().remove("text");
    assert_eq!(
        decode_resource(&read(missing), URI, 1024),
        Err(AppsError::Content)
    );
    for raw in [
        json!({}),
        json!({"contents": []}),
        json!({"contents": [content(), content()]}),
    ] {
        assert_eq!(
            decode_resource(&McpResourceResult { raw }, URI, 1024),
            Err(AppsError::Content)
        );
    }
    assert_eq!(
        decode_resource(&read(content()), "https://example.com", 1024),
        Err(AppsError::Uri)
    );
    let mut raw = content();
    raw["_meta"]["ui"]["csp"] = json!("unsafe");
    assert!(matches!(
        decode_resource(&read(raw), URI, 1024),
        Err(AppsError::Metadata(_))
    ));
}

#[test]
fn capability_requires_mime_types_and_lifecycle_uses_separate_revision() {
    assert_eq!(ui_capability(&json!({})).unwrap(), None);
    for capability in [
        json!({}),
        json!({"mimeTypes": null}),
        json!({"mimeTypes": UI_MIME_TYPE}),
        json!({"mimeTypes": [123]}),
        json!(null),
    ] {
        assert!(ui_capability(&json!({"extensions": {UI_EXTENSION: capability}})).is_err());
    }
    let capability = ui_capability(
        &json!({"extensions": {UI_EXTENSION: {"mimeTypes": [UI_MIME_TYPE], "future": true}}}),
    )
    .unwrap()
    .unwrap();
    assert!(capability.supports_html());
    assert_eq!(capability.extra["future"], true);
    assert!(
        !ui_capability(&json!({"extensions": {UI_EXTENSION: {"mimeTypes": []}}}))
            .unwrap()
            .unwrap()
            .supports_html()
    );
    let params = json!({"protocolVersion": UI_PROTOCOL_VERSION, "appInfo": {"name": "view", "version": "1"}, "appCapabilities": {"availableDisplayModes": ["inline"]}});
    let typed: UiInitializeParams = serde_json::from_value(params.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), params);
    assert_ne!(UI_PROTOCOL_VERSION, mcp::protocol::MCP_PROTOCOL_VERSION);
    let result = json!({"protocolVersion": UI_PROTOCOL_VERSION, "hostInfo": {"name": "host", "version": "1"}, "hostCapabilities": {"serverTools": {}, "sandbox": {"permissions": {"camera": {}}, "csp": {"connectDomains": []}}}, "hostContext": {"theme": "dark", "containerDimensions": {"width": 400}, "styles": {"variables": {"--font-sans": "serif"}}}});
    let typed: UiInitializeResult = serde_json::from_value(result.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), result);
    let input = json!({"arguments": {"city": "test"}});
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<UiToolInputParams>(input.clone()).unwrap())
            .unwrap(),
        input
    );
    let raw = json!({"content": [{"type": "text", "text": "fallback"}], "structuredContent": {"data": 1}, "_meta": {"private": "keep"}, "future": "keep"});
    let tool = McpToolResult { raw: raw.clone() };
    assert_eq!(
        serde_json::to_value(UiToolResultParams::try_from(&tool).unwrap()).unwrap(),
        raw
    );
    assert_eq!(tool.raw, raw);
    for structured in [
        Value::Null,
        json!(1),
        json!("value"),
        json!([1, 2]),
        json!(true),
    ] {
        let raw = json!({"content": [], "structuredContent": structured});
        let tool = McpToolResult { raw: raw.clone() };
        assert_eq!(
            serde_json::to_value(UiToolResultParams::try_from(&tool).unwrap()).unwrap(),
            raw
        );
    }
    assert_eq!(
        serde_json::to_value(UiResourceTeardownParams::default()).unwrap(),
        json!({})
    );
}

struct CaptureServer {
    seen: Arc<Mutex<Vec<Value>>>,
}
impl ServerHandler for CaptureServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("apps-spec", "1")
    }
    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().resources()
    }
    fn read_resource(
        &self,
        _uri: String,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpResourceResult>> {
        self.seen.lock().unwrap().push(json!({
            "caps": context.client_capabilities(),
            "ui": context.ui_capability().unwrap(),
            "protocol": context.meta_value(mcp::protocol::MCP_PROTOCOL_VERSION_META_KEY),
        }));
        Box::pin(async { Ok(read(content())) })
    }
}
struct ClientHandler;
impl McpClientHandler for ClientHandler {}

#[tokio::test]
async fn client_apps_opt_in_merges_skills_in_stateless_request_metadata() -> mcp::Result<()> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = SpecServer::start(Arc::new(CaptureServer { seen: seen.clone() })).await?;
    for apps in [false, true] {
        for skills in [false, true] {
            let mut options = McpClientOptions::new("apps-client", "1", Duration::from_secs(2));
            assert!(!options.apps);
            if apps {
                options = options.with_apps();
            }
            if skills {
                options = options.with_skills();
            }
            let client = McpClient::connect(
                "apps-spec",
                McpTransportConfig::StreamableHttp {
                    endpoint: server.endpoint(),
                    bearer_token: None,
                },
                Arc::new(ClientHandler),
                options,
                CancellationToken::new(),
            )
            .await?;
            // Each request independently advertises the same capability, no session handshake.
            for _ in 0..2 {
                let result = client
                    .read_app_resource(URI, 1024, CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(result.html, HTML);
            }
            client.shutdown().await?;
        }
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 8);
    for (index, item) in seen.iter().enumerate() {
        let apps = index >= 4;
        let skills = (index / 2) % 2 == 1;
        assert_eq!(item["caps"]["extensions"].get(UI_EXTENSION).is_some(), apps);
        assert_eq!(
            item["caps"]["extensions"]
                .get(mcp::skills::SKILLS_EXTENSION)
                .is_some(),
            skills
        );
        assert_eq!(item["caps"]["roots"], json!({}));
        assert_eq!(item["protocol"], mcp::protocol::MCP_PROTOCOL_VERSION);
        if apps {
            assert_eq!(item["ui"]["mimeTypes"], json!([UI_MIME_TYPE]));
        } else {
            assert_eq!(item["ui"], Value::Null);
        }
    }
    Ok(())
}

#[tokio::test]
async fn typed_app_read_preserves_decode_and_request_errors() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(CaptureServer {
        seen: Arc::default(),
    }))
    .await?;
    let client = McpClient::connect(
        "apps-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(ClientHandler),
        McpClientOptions::new("apps-client", "1", Duration::from_secs(2)).with_apps(),
        CancellationToken::new(),
    )
    .await?;
    assert!(matches!(
        client
            .read_app_resource(URI, 1, CancellationToken::new())
            .await,
        Err(ReadAppResourceError::Apps(AppsError::TooLarge))
    ));
    assert!(matches!(
        client
            .read_app_resource("ui://test/other", 1024, CancellationToken::new())
            .await,
        Err(ReadAppResourceError::Apps(AppsError::Uri))
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        client.read_app_resource(URI, 1024, cancelled).await,
        Err(ReadAppResourceError::Request(_))
    ));
    client.shutdown().await?;
    Ok(())
}
