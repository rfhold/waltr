//! Strict configuration contract tests without environment mutation.
use waltr_desktop::{
    command::CommandSpec,
    config::{self, ConfigError},
};
use std::path::Path;

const VALID: &str = "version: 1\nkeybinds:\n  - id: dictate-copy\n    key: Ctrl+Alt+Space\n    mode: hold\n    routine: { name: audio-transcribe-cleanup, trigger: dictate, input: audio, output: clean-text }\n    audio: { device: default, sampleRateHz: 24000, channels: 1 }\n    command: { argv: [pbcopy], stdin: '{{ text }}', timeoutSeconds: 10 }\n";

#[test]
fn documented_typing_example_uses_cleanup_routine() {
    let config =
        config::parse(include_str!("../../../docs/examples/client.yaml").as_bytes()).unwrap();
    assert_eq!(config.keybinds.len(), 1);
    let binding = &config.keybinds[0];
    assert!(binding.typing.is_some());
    assert!(binding.command.is_none());
    assert_eq!(binding.routine.name, "audio-transcribe-cleanup");
    assert_eq!(binding.routine.trigger, "dictate");
    assert_eq!(binding.routine.input, "audio");
    assert_eq!(binding.routine.output, "clean-text");
}

#[test]
fn strict_schema() {
    let config = config::parse(VALID.as_bytes()).unwrap();
    assert_eq!(config.keybinds.len(), 1);
    assert_eq!(
        config.keybinds[0].command.as_ref().unwrap().timeout_seconds,
        10
    );
    for invalid in [
        VALID.replace("version: 1", "version: 2"),
        VALID.replace("version: 1", "version: 1\ncredentials: secret"),
        VALID.replace("mode: hold", "mode: toggle"),
        VALID.replace("device: default", "device: microphone"),
        VALID.replace("device: default", "device: 'id:'"),
        VALID.replace("channels: 1", "channels: 0"),
        VALID.replace("24000", "0"),
        VALID.replace("Ctrl+Alt+Space", "Ctrl+Ctrl+Space"),
        VALID.replace("Ctrl+Alt+Space", "Alt+Ctrl+Space"),
        VALID.replace("Ctrl+Alt+Space", "Ctrl+"),
        VALID.replace("Ctrl+Alt+Space", "Ctrl+F25"),
        VALID.replace("trigger: dictate", "trigger: dictate, token: secret"),
        VALID.replace("timeoutSeconds: 10", "timeoutSeconds: 0"),
        VALID.replace("timeoutSeconds: 10", "timeoutSeconds: 301"),
        VALID.replace("timeoutSeconds: 10", "timeoutSeconds: 1.5"),
        VALID.replace("{{ text }}", "{{ unknown }}"),
        VALID.replace("[pbcopy]", "['{{ text }}']"),
        format!("{VALID}{}", VALID.split_once("keybinds:\n").unwrap().1),
        VALID.replace("version: 1", "version: 1\nversion: 1"),
    ] {
        assert!(
            config::parse(invalid.as_bytes()).is_err(),
            "accepted invalid schema"
        );
    }
    for device in ["id:42", "name:USB microphone"] {
        assert!(
            config::parse(
                VALID
                    .replace("device: default", &format!("device: '{device}'"))
                    .as_bytes()
            )
            .is_ok()
        );
    }
}

#[test]
fn paths_and_bounded_loading() {
    assert_eq!(
        config::config_path(Some(Path::new("/xdg")), Some(Path::new("/home/u"))).unwrap(),
        Path::new("/xdg/waltr/client.yaml")
    );
    assert_eq!(
        config::config_path(None, Some(Path::new("/home/u"))).unwrap(),
        Path::new("/home/u/.config/waltr/client.yaml")
    );
    assert_eq!(
        config::config_path(Some(Path::new("")), Some(Path::new("/home/u"))).unwrap(),
        Path::new("/home/u/.config/waltr/client.yaml")
    );
    assert!(config::config_path(Some(Path::new("relative")), Some(Path::new("/home/u"))).is_err());
    assert!(config::config_path(None, None).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.yaml");
    assert!(config::load_path(&path).unwrap().keybinds.is_empty());
    std::fs::write(&path, "not: [valid").unwrap();
    assert!(matches!(
        config::load_path(&path),
        Err(ConfigError::Syntax { .. })
    ));
    std::fs::write(&path, vec![b' '; config::MAX_CONFIG_BYTES + 1]).unwrap();
    assert!(matches!(
        config::load_path(&path),
        Err(ConfigError::TooLarge)
    ));
    std::fs::write(&path, VALID).unwrap();
    assert_eq!(config::load_path(&path).unwrap().keybinds.len(), 1);
    assert!(config::parse(VALID.replace("dictate-copy", &"x".repeat(257)).as_bytes()).is_err());
    let entries = VALID.split_once("keybinds:\n").unwrap().1;
    assert!(
        config::parse(format!("version: 1\nkeybinds:\n{}", entries.repeat(65)).as_bytes()).is_err()
    );
}

#[test]
fn templates_and_redaction() {
    for template in [
        "{{text}}",
        "{{ text",
        "text }}",
        "{{ unknown }}",
        "{{ {{ text }} }}",
    ] {
        let spec = CommandSpec {
            argv: vec!["echo".into()],
            stdin: template.into(),
            timeout_seconds: 10,
        };
        assert!(spec.validate().is_err());
    }
    let spec: CommandSpec = serde_yaml_ng::from_str("argv: [cat]\nstdin: '{{ text }}'").unwrap();
    assert_eq!(spec.timeout_seconds, 10);
    assert!(!format!("{spec:?}").contains("{{ text }}"));
    let error = config::parse(b"version: secret-sensitive-value").unwrap_err();
    assert!(!format!("{error:?}").contains("secret-sensitive-value"));
}
