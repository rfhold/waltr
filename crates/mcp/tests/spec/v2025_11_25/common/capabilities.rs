use mcp::McpServerCapabilities;
use serde_json::json;

#[test]
fn server_capabilities_parse_known_feature_objects() {
    let value = json!({
        "tools": { "listChanged": true },
        "prompts": { "listChanged": true },
        "resources": { "subscribe": true, "listChanged": true },
        "completions": {},
        "logging": {}
    });

    let capabilities = McpServerCapabilities::parse(Some(&value));

    assert!(capabilities.tools.expect("tools capability").list_changed);
    assert!(
        capabilities
            .prompts
            .expect("prompts capability")
            .list_changed
    );
    let resources = capabilities.resources.expect("resources capability");
    assert!(resources.subscribe);
    assert!(resources.list_changed);
    assert!(capabilities.completions);
    assert!(capabilities.logging);
}

#[test]
fn server_capabilities_default_absent_optional_flags_to_false() {
    let value = json!({
        "tools": {},
        "prompts": {},
        "resources": {}
    });

    let capabilities = McpServerCapabilities::parse(Some(&value));

    assert!(!capabilities.tools.expect("tools capability").list_changed);
    assert!(
        !capabilities
            .prompts
            .expect("prompts capability")
            .list_changed
    );
    let resources = capabilities.resources.expect("resources capability");
    assert!(!resources.subscribe);
    assert!(!resources.list_changed);
    assert!(!capabilities.completions);
    assert!(!capabilities.logging);
}

#[test]
fn server_capabilities_preserve_raw_object_including_unknown_capabilities() {
    let value = json!({
        "experimental": { "vendor.example/feature": {} },
        "customCapability": { "enabled": true },
        "tools": { "listChanged": false }
    });

    let capabilities = McpServerCapabilities::parse(Some(&value));

    assert_eq!(capabilities.raw, value);
    assert_eq!(
        capabilities.raw["experimental"]["vendor.example/feature"],
        json!({})
    );
    assert_eq!(capabilities.raw["customCapability"]["enabled"], true);
}

#[test]
fn missing_server_capabilities_parse_as_empty_capability_set() {
    let capabilities = McpServerCapabilities::parse(None);

    assert!(capabilities.tools.is_none());
    assert!(capabilities.prompts.is_none());
    assert!(capabilities.resources.is_none());
    assert!(!capabilities.completions);
    assert!(!capabilities.logging);
    assert_eq!(capabilities.raw, serde_json::Value::Null);
}
