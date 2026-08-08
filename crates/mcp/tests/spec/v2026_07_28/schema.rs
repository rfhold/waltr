use mcp::{McpPromptList, McpServerCapabilities, McpToolList};
use serde_json::json;

#[test]
fn capabilities_parse_known_flags_extensions_and_preserve_unknown_values() {
    let raw = json!({
        "tools": { "listChanged": true },
        "prompts": {},
        "resources": { "subscribe": false, "listChanged": true },
        "completions": {},
        "logging": {},
        "extensions": { "example.test/feature": { "enabled": true } },
        "futureCapability": { "opaque": 7 }
    });
    let capabilities = McpServerCapabilities::parse(Some(&raw));
    assert!(capabilities.tools.unwrap().list_changed);
    assert!(!capabilities.prompts.unwrap().list_changed);
    let resources = capabilities.resources.unwrap();
    assert!(!resources.subscribe);
    assert!(resources.list_changed);
    assert!(capabilities.completions);
    assert!(capabilities.logging);
    assert_eq!(
        capabilities.extensions.unwrap()["example.test/feature"]["enabled"],
        true
    );
    assert_eq!(capabilities.raw["futureCapability"]["opaque"], 7);
}

#[test]
fn absent_capability_flags_default_to_false_without_inventing_features() {
    let capabilities = McpServerCapabilities::parse(Some(&json!({})));
    assert!(capabilities.tools.is_none());
    assert!(capabilities.prompts.is_none());
    assert!(capabilities.resources.is_none());
    assert!(!capabilities.completions);
    assert!(!capabilities.logging);
    assert!(capabilities.extensions.is_none());
}

#[test]
fn tool_schema_preserves_boolean_composition_metadata_and_icon_arrays() {
    let list = McpToolList::parse(&json!({
        "tools": [{
            "name": "composed",
            "title": "Composed",
            "icons": [{ "src": "https://example.test/tool.svg", "sizes": ["16x16", "any"], "theme": "dark" }],
            "inputSchema": { "allOf": [true, { "type": "object" }] },
            "outputSchema": false,
            "_meta": { "example.test/opaque": [1, 2] }
        }]
    })).unwrap();
    let tool = &list.tools[0];
    assert_eq!(tool.input_schema["allOf"][0], true);
    assert_eq!(tool.output_schema, Some(json!(false)));
    assert_eq!(
        tool.meta.as_ref().unwrap()["example.test/opaque"],
        json!([1, 2])
    );
    assert_eq!(tool.icons[0].sizes.as_ref().unwrap(), &["16x16", "any"]);
    assert_eq!(format!("{:?}", tool.icons[0].theme), "Some(Dark)");
}

#[test]
fn tool_without_required_input_schema_is_rejected() {
    assert!(McpToolList::parse(&json!({ "tools": [{ "name": "broken" }] })).is_err());
}

#[test]
fn invalid_icon_theme_rejects_the_containing_definition() {
    assert!(
        McpToolList::parse(&json!({
            "tools": [{
                "name": "broken",
                "inputSchema": { "type": "object" },
                "icons": [{ "src": "https://example.test/icon", "theme": "sepia" }]
            }]
        }))
        .is_err()
    );
}

#[test]
fn prompt_argument_title_and_opaque_metadata_round_trip_through_parser() {
    let list = McpPromptList::parse(&json!({
        "prompts": [{
            "name": "deploy",
            "arguments": [{ "name": "target", "title": "Target environment", "required": true }],
            "_meta": { "example.test/prompt": "opaque" }
        }]
    }))
    .unwrap();
    assert_eq!(
        list.prompts[0].arguments[0].title.as_deref(),
        Some("Target environment")
    );
    assert_eq!(
        list.prompts[0].meta.as_ref().unwrap()["example.test/prompt"],
        "opaque"
    );
}
