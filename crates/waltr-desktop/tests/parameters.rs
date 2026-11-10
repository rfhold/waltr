//! Schema-derived native parameter defaults.
use waltr_client::{RoutineContract, interaction::wire};
use waltr_desktop::interaction::default_parameters;

#[test]
fn closed_schema_defaults_cover_nested_required_values() {
    let contract = RoutineContract::from_wire(&wire::ValueContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: br#"{"type":"object","properties":{"optional":{"type":"string","maxLength":12},"nested":{"type":"object","properties":{"flag":{"type":"boolean"},"number":{"type":"number","min":3,"max":6},"list":{"type":"array","maxItems":2,"items":{"type":"string","maxLength":2}},"nothing":{"type":"nullable","schema":{"type":"string","maxLength":2}}},"required":["flag","number","list","nothing"]}},"required":["nested"]}"#.to_vec(),
    }).unwrap();
    let defaults = default_parameters(&contract).unwrap();
    contract.validate(&defaults).unwrap();
    let value: serde_json::Value = serde_json::from_str(defaults.as_str()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"nested":{"flag":false,"number":3,"list":[],"nothing":null}})
    );
}
