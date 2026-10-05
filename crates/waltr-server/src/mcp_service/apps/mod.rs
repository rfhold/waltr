//! Explicit, compiled-in tool-linked HTML resources, separate from tool execution.

use mcp::{McpResourceResult, apps, server::ServerContext};
use serde_json::json;

pub(super) mod status;
include!(concat!(env!("OUT_DIR"), "/served_app_assets.rs"));

pub(super) static RESOURCES: &[AppResource] = &[status::APP];

pub(super) struct AppResource {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub html: Option<&'static str>,
}

impl AppResource {
    pub fn supported(&self, context: &ServerContext) -> bool {
        self.html.is_some()
            && context
                .ui_capability()
                .ok()
                .flatten()
                .is_some_and(|capability| capability.supports_html())
    }

    pub fn descriptor(&self) -> mcp::McpResourceDefinition {
        mcp::parse_resource_definition(&json!({
            "uri": self.uri,
            "name": self.name,
            "description": self.description,
            "mimeType": apps::UI_MIME_TYPE
        }))
        .expect("static app resource declaration")
    }

    pub fn resource(&self) -> McpResourceResult {
        McpResourceResult::new(json!({"contents": [{
            "uri": self.uri,
            "mimeType": apps::UI_MIME_TYPE,
            "text": self.html.expect("app is available before resource reads"),
            "_meta": {"ui": {"csp": {
                "connectDomains": [], "resourceDomains": [], "frameDomains": [], "baseUriDomains": []
            }, "permissions": {}}}
        }]}))
    }
}

pub(super) fn advertise(capabilities: &mut mcp::server::ServerCapabilities) {
    if RESOURCES.iter().any(|app| app.html.is_some()) {
        capabilities.extensions.as_mut().unwrap()[apps::UI_EXTENSION] = json!({});
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_static_resource_uses_the_same_publication_without_production_registration() {
        let fixture = AppResource {
            uri: "ui://waltr/second-fixture",
            name: "Second fixture",
            description: "Test-only publication",
            html: Some(include_str!(
                "../../../tests/fixtures/apps/second/index.html"
            )),
        };
        assert_eq!(fixture.descriptor().uri, fixture.uri);
        let decoded = apps::decode_resource(
            &fixture.resource(),
            fixture.uri,
            apps::DEFAULT_MAX_HTML_BYTES,
        )
        .unwrap();
        assert!(decoded.html.contains("Second app build fixture"));
        assert!(decoded.ui.csp.unwrap().resource_domains.unwrap().is_empty());
        assert!(RESOURCES.iter().all(|app| app.uri != fixture.uri));
    }
}
