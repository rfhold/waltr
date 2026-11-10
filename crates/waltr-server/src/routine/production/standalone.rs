//! Explicit standalone Routine configuration; embedding hosts retain their own authority.
use super::{HelperInputAuthority, RoutineAuthority};
use crate::permission::{
    PermissionEffect, PermissionLevel, PermissionPolicy, PermissionToolSelector,
};
use crate::routine::media::speech::SynthesisSelection;
use std::{collections::BTreeMap, io};

/// The standalone binary snapshots only these Routine settings.
pub const VARIABLES: [&str; 9] = [
    "WALTR_ROUTINE_STT_PROVIDER",
    "WALTR_ROUTINE_STT_MODEL",
    "WALTR_ROUTINE_TTS_PROVIDER",
    "WALTR_ROUTINE_TTS_MODEL",
    "WALTR_ROUTINE_TTS_CHOICES",
    "WALTR_ROUTINE_HELPER_INPUTS",
    "WALTR_ROUTINE_MEDIA_TOOLS",
    "WALTR_ROUTINE_STT_SNAPSHOT_PROVIDER",
    "WALTR_ROUTINE_STT_SNAPSHOT_MODEL",
];

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInput {
    provider: String,
    model: String,
    text: bool,
    max_body_bytes: usize,
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// Parse a supplied environment snapshot without reading ambient state.
/// Catalog kind and streaming support are checked at invocation admission.
pub fn speech_authority(values: &BTreeMap<String, String>) -> io::Result<RoutineAuthority> {
    let pair = |provider: &str, model: &str| match (values.get(provider), values.get(model)) {
        (None, None) => Ok(None),
        (Some(p), Some(m))
            if [p, m].iter().all(|s| {
                !s.trim().is_empty()
                    && s.len() <= 256
                    && s.trim() == s.as_str()
                    && !s.chars().any(char::is_control)
            }) =>
        {
            Ok(Some((p.clone(), m.clone())))
        }
        _ => Err(io::Error::other(format!(
            "{provider} and {model} require paired nonempty identities of at most 256 bytes without surrounding whitespace or control characters"
        ))),
    };
    let recognition = pair(VARIABLES[0], VARIABLES[1])?;
    let recognition_snapshot = pair(VARIABLES[7], VARIABLES[8])?;
    let synthesis = pair(VARIABLES[2], VARIABLES[3])?;
    let synthesis_choices: Vec<SynthesisSelection> = match values.get(VARIABLES[4]) {
        None => Vec::new(),
        Some(raw) => {
            let invalid = || {
                io::Error::other(
                    "WALTR_ROUTINE_TTS_CHOICES requires a nonempty JSON array of at most 128 valid choices for WALTR_ROUTINE_TTS_MODEL (at most 131072 bytes)",
                )
            };
            if synthesis.is_none() || raw.len() > 131072 {
                return Err(invalid());
            }
            let choices: Vec<SynthesisSelection> =
                serde_json::from_str(raw).map_err(|_| invalid())?;
            if choices.is_empty()
                || choices.len() > 128
                || choices.iter().any(|choice| {
                    choice.validate().is_err()
                        || synthesis
                            .as_ref()
                            .is_none_or(|(_, model)| model != &choice.model)
                })
            {
                return Err(invalid());
            }
            choices
        }
    };
    let mut helper_inputs = Vec::new();
    if let Some(raw) = values.get(VARIABLES[5]) {
        let invalid = || {
            io::Error::other(
                "WALTR_ROUTINE_HELPER_INPUTS requires a nonempty JSON array of at most 128 unique exact provider/model text inputs with text:true and max_body_bytes in 1..=33554432 (at most 131072 bytes)",
            )
        };
        if raw.len() > 131072 {
            return Err(invalid());
        }
        let entries: Vec<TextInput> = serde_json::from_str(raw).map_err(|_| invalid())?;
        let mut identities = std::collections::BTreeSet::new();
        if entries.is_empty() || entries.len() > 128 {
            return Err(invalid());
        }
        for entry in entries {
            if !valid_identity(&entry.provider)
                || !valid_identity(&entry.model)
                || !entry.text
                || !(1..=32 * 1024 * 1024).contains(&entry.max_body_bytes)
                || !identities.insert((entry.provider.clone(), entry.model.clone()))
            {
                return Err(invalid());
            }
            helper_inputs.push(HelperInputAuthority {
                provider: entry.provider,
                model: entry.model,
                capabilities: waltr_agent::TransientModelCapabilities {
                    png: false,
                    jpeg: false,
                    webp: false,
                    pdf: false,
                    text: true,
                    max_body_bytes: entry.max_body_bytes,
                },
            });
        }
    }
    let media_restriction = match values.get(VARIABLES[6]) {
        None => None,
        Some(raw) => {
            let invalid = || {
                io::Error::other(
                    "WALTR_ROUTINE_MEDIA_TOOLS requires a nonempty JSON array of unique allowlisted builtin capture tool names: _helper_capture (at most 131072 bytes)",
                )
            };
            if raw.len() > 131072 {
                return Err(invalid());
            }
            let names: Vec<String> = serde_json::from_str(raw).map_err(|_| invalid())?;
            let mut seen = std::collections::BTreeSet::new();
            let mut policy = PermissionPolicy::new(PermissionLevel::Deny);
            if names.is_empty() || names.len() > 128 {
                return Err(invalid());
            }
            for name in names {
                if name != "_helper_capture" || !seen.insert(name.clone()) {
                    return Err(invalid());
                }
                policy = policy.with_tool_rule(
                    PermissionToolSelector::exact(name),
                    PermissionEffect::allow(),
                );
            }
            Some(policy)
        }
    };
    Ok(RoutineAuthority {
        recognition,
        recognition_snapshot,
        synthesis,
        synthesis_choices,
        helper_inputs,
        media_restriction,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn values(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }
    #[test]
    fn snapshot_selection_is_paired_and_value_redacted() {
        let provider = "WALTR_ROUTINE_STT_SNAPSHOT_PROVIDER";
        let model = "WALTR_ROUTINE_STT_SNAPSHOT_MODEL";
        assert!(speech_authority(&values(&[(provider, "secret-sentinel")])).is_err());
        assert!(VARIABLES.contains(&provider) && VARIABLES.contains(&model));
        for (p, m) in [
            ("", "model"),
            ("provider", " model"),
            ("provider", "bad\nmodel"),
        ] {
            let error = speech_authority(&values(&[(provider, p), (model, m)])).unwrap_err();
            assert!(!error.to_string().contains("secret-sentinel"));
        }
    }
    #[test]
    fn absent_speech_grants_no_authority() {
        let authority = speech_authority(&BTreeMap::new()).unwrap();
        assert!(
            authority.recognition.is_none()
                && authority.recognition_snapshot.is_none()
                && authority.synthesis.is_none()
        );
        assert!(authority.code.is_empty() && authority.targets.is_empty());
        assert!(authority.helper_inputs.is_empty() && authority.media_restriction.is_none());
    }
    #[test]
    fn pairs_are_independent_and_partial_empty_or_invalid_pairs_fail_without_values() {
        for pair in [0, 2, 7] {
            for (p, m) in [
                (Some("secret-sentinel"), None),
                (None, Some("model")),
                (Some(""), Some("model")),
                (Some("provider"), Some(" ")),
                (Some(" provider"), Some("model")),
                (Some("provider"), Some("bad\nmodel")),
                (Some("provider"), Some("x".repeat(257).as_str())),
            ] {
                let mut map = BTreeMap::new();
                if let Some(p) = p {
                    map.insert(VARIABLES[pair].into(), p.into());
                }
                if let Some(m) = m {
                    map.insert(VARIABLES[pair + 1].into(), m.into());
                }
                let error = speech_authority(&map).unwrap_err().to_string();
                assert!(!error.contains("secret-sentinel"));
            }
            let map = values(&[
                (VARIABLES[pair], "provider"),
                (VARIABLES[pair + 1], "model"),
            ]);
            let authority = speech_authority(&map).unwrap();
            assert_eq!(
                if pair == 0 {
                    authority.recognition
                } else if pair == 2 {
                    authority.synthesis
                } else {
                    authority.recognition_snapshot
                },
                Some(("provider".into(), "model".into()))
            );
        }
    }
    #[test]
    fn tts_choices_are_bounded_exact_and_do_not_grant_other_authority() {
        let mut map = values(&[(VARIABLES[2], "tts"), (VARIABLES[3], "reader")]);
        assert!(speech_authority(&map).unwrap().synthesis_choices.is_empty());
        let choice =
            serde_json::json!({"model":"reader","voice":"voice","language":"en-US","rate":1.0});
        for raw in ["".into(), "[]".into(), "secret-sentinel".into(), serde_json::json!([{"model":"foreign","voice":"voice","language":"en-US","rate":1}]).to_string(), serde_json::json!([{"model":"reader","voice":"voice","language":"en-US","rate":0}]).to_string(), serde_json::json!([{"model":"reader","voice":"voice","language":"en-US","rate":1,"endpoint":"secret-sentinel"}]).to_string(), serde_json::to_string(&vec![choice.clone();129]).unwrap(), " ".repeat(131073)] {
            map.insert(VARIABLES[4].into(), raw);
            assert!(!speech_authority(&map).unwrap_err().to_string().contains("secret-sentinel"));
        }
        map.insert(VARIABLES[4].into(), serde_json::json!([choice]).to_string());
        let authority = speech_authority(&map).unwrap();
        assert_eq!(authority.synthesis_choices.len(), 1);
        assert!(
            authority.code.is_empty()
                && authority.helper_inputs.is_empty()
                && authority.media_restriction.is_none()
        );
        map.remove(VARIABLES[2]);
        map.remove(VARIABLES[3]);
        assert!(speech_authority(&map).is_err());
    }

    #[test]
    fn helper_inputs_are_strict_bounded_and_value_redacted() {
        let valid = serde_json::json!({"provider":"provider","model":"model","text":true,"max_body_bytes":2097152});
        let mut invalid = vec![
            "".into(),
            "[]".into(),
            "secret-sentinel".into(),
            " ".repeat(131073),
            serde_json::json!([valid.clone(), valid.clone()]).to_string(),
            serde_json::to_string(&vec![valid.clone(); 129]).unwrap(),
        ];
        for (key, value) in [
            ("provider", serde_json::json!("")),
            ("provider", serde_json::json!(" provider")),
            ("model", serde_json::json!("model\n")),
            ("model", serde_json::json!("x".repeat(257))),
            ("text", serde_json::json!(false)),
            ("max_body_bytes", serde_json::json!(0)),
            ("max_body_bytes", serde_json::json!(33554433u64)),
            ("max_body_bytes", serde_json::json!(-1)),
            ("max_body_bytes", serde_json::json!(1.5)),
            ("png", serde_json::json!(true)),
            ("endpoint", serde_json::json!("secret-sentinel")),
        ] {
            let mut entry = valid.clone();
            entry[key] = value;
            invalid.push(serde_json::json!([entry]).to_string());
        }
        for raw in invalid {
            let error = speech_authority(&values(&[(VARIABLES[5], &raw)]))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-sentinel"));
        }
        for ceiling in [1, 2097152, 33554432] {
            let mut entry = valid.clone();
            entry["max_body_bytes"] = serde_json::json!(ceiling);
            let authority = speech_authority(&values(&[(
                VARIABLES[5],
                &serde_json::json!([entry]).to_string(),
            )]))
            .unwrap();
            authority.validate().unwrap();
            let input = &authority.helper_inputs[0];
            assert_eq!((&*input.provider, &*input.model), ("provider", "model"));
            let cap = input.capabilities;
            assert!(cap.text && !cap.png && !cap.jpeg && !cap.webp && !cap.pdf);
            assert_eq!(cap.max_body_bytes, ceiling);
            assert!(
                authority.media_restriction.is_none()
                    && authority.code.is_empty()
                    && authority.targets.is_empty()
            );
            assert!(authority.recognition.is_none() && authority.synthesis.is_none());
        }
    }

    #[test]
    fn media_tools_allow_only_exact_builtin_capture_and_preserve_intersection() {
        for raw in [
            "",
            "[]",
            "null",
            "[\"*\"]",
            "[\"_helper_*\"]",
            "[\"shell\"]",
            "[\"mcp:waltr:_helper_capture\"]",
            "[\" _helper_capture\"]",
            "[\"_helper_capture\",\"_helper_capture\"]",
            "[\"secret-sentinel\"]",
            &" ".repeat(131073),
        ] {
            assert!(
                !speech_authority(&values(&[(VARIABLES[6], raw)]))
                    .unwrap_err()
                    .to_string()
                    .contains("secret-sentinel")
            );
        }
        let authority =
            speech_authority(&values(&[(VARIABLES[6], "[\"_helper_capture\"]")])).unwrap();
        assert!(
            authority.helper_inputs.is_empty()
                && authority.code.is_empty()
                && authority.targets.is_empty()
        );
        let policy = authority.media_restriction.unwrap();
        assert_eq!(
            policy.evaluate_tool("_helper_capture"),
            PermissionLevel::Allow
        );
        for tool in [
            "shell",
            "mcp:waltr:_helper_capture",
            "_helper_capture_suffix",
        ] {
            assert_eq!(policy.evaluate_tool(tool), PermissionLevel::Deny);
        }
        assert_eq!(
            policy
                .intersection(PermissionPolicy::new(PermissionLevel::Deny))
                .evaluate_tool("_helper_capture"),
            PermissionLevel::Deny
        );
    }
}
