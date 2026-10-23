//! Pure fake delivery backend; never injects OS input.
// End-to-end hold regressions live with the synthetic capture/RPC fixture in hold.rs.
use waltr_desktop::config;

#[test]
fn typing_is_explicit_exclusive_and_strict() {
    let yaml = "version: 1\nkeybinds:\n - id: text\n   key: Ctrl+A\n   mode: hold\n   routine: {name: a, trigger: b, input: c, output: d}\n   audio: {device: default, sampleRateHz: 24000, channels: 1}\n   typing: {}\n";
    let parsed = config::parse(yaml.as_bytes()).unwrap();
    assert!(parsed.keybinds[0].typing.is_some());
    assert!(parsed.keybinds[0].command.is_none());
    for invalid in [
        yaml.replace("   typing: {}\n", ""),
        yaml.replace("typing: {}", "typing: {clipboard: true}"),
        yaml.replace(
            "typing: {}",
            "typing: {}\n   command: {argv: [cat], stdin: '{{ text }}'}",
        ),
        yaml.replace("typing: {}", "typing: null"),
    ] {
        assert!(config::parse(invalid.as_bytes()).is_err());
    }
}
