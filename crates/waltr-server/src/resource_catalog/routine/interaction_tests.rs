use super::*;
use crate::resource_catalog::{materialize::materialize_provider, speech::resolve_speech_model};
use waltr::grpc::waltr::config::v1 as proto;

#[test]
fn sequential_triggers_reject_stream_tasks_even_on_mixed_trigger_definitions() {
    for source in [TRANSCRIBE, CLEANUP, CONVERSATION_NOTES, SYNTHESIZE] {
        for trigger in [
            "{name: manual, manual: {displayName: Test, scope: session, inputs: []}}",
            "{name: event, event: {type: waltr.session.user-message.created}}",
        ] {
            let mut definition: RoutineDefinition = serde_yaml_ng::from_str(source).unwrap();
            definition
                .spec
                .triggers
                .push(serde_yaml_ng::from_str(trigger).unwrap());
            let error = validate_routine(&definition, "test").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("speech and collector tasks require interaction triggers"),
                "{error}"
            );
        }
    }
}

const TRANSCRIBE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/examples/routines/audio-transcribe.yaml"
));
const CLEANUP: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/examples/routines/audio-transcribe-cleanup.yaml"
));
const CONVERSATION_NOTES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/examples/routines/audio-conversation-notes.yaml"
));
const SYNTHESIZE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/examples/routines/audio-synthesize.yaml"
));
const PROVIDER: &str = r"apiVersion: waltr.dev/v1alpha1
kind: Provider
metadata:
  name: speech
spec:
  transport: googleSpeechV2
  url: http://localhost:50051
  models:
    dictation:
      name: Dictation
      type: 3
      speech_capabilities:
        unary: true
        streaming: true
      recognition:
        recognizer: projects/test/locations/global/recognizers/_
        model: chirp_3
        language_codes: [en-US]
        sample_rate_hz: 24000
        channels: 1
        interim_results: true
        automatic_punctuation: true
";

fn configuration() -> proto::Configuration {
    let mut provider = materialize_provider(PROVIDER, "providers/speech.yaml", None).unwrap();
    let diarized = materialize_provider(
        &PROVIDER
            .replace("dictation:", "diarized-dictation:")
            .replace(
                "automatic_punctuation: true",
                "automatic_punctuation: true\n        diarization: true",
            ),
        "providers/speech.yaml",
        None,
    )
    .unwrap();
    provider.models.extend(diarized.models);
    let synthesis = materialize_provider(
        r"apiVersion: waltr.dev/v1alpha1
kind: Provider
metadata:
  name: voice
spec:
  transport: googleTextToSpeechV1
  url: https://voice.example.test
  models:
    reader:
      name: Reader
      type: 4
      speech_capabilities:
        unary: true
        streaming: true
      synthesis:
        voice: en-US-Chirp3-HD-Aoede
        language_code: en-US
        sample_rate_hz: 24000
        channels: 1
",
        "providers/voice.yaml",
        None,
    )
    .unwrap();
    proto::Configuration {
        providers: [
            ("speech".into(), provider),
            ("voice".into(), synthesis),
            (
                "inference".into(),
                proto::Provider {
                    r#type: proto::ProviderType::OpenaiChatCompletions as i32,
                    models: [(
                        "cleanup".into(),
                        proto::Model {
                            r#type: proto::ModelType::Llm as i32,
                            ..Default::default()
                        },
                    )]
                    .into(),
                    ..Default::default()
                },
            ),
        ]
        .into(),
        ..Default::default()
    }
}

#[test]
fn existing_bundled_routine_schemas_remain_valid() {
    for source in [
        include_str!("../../../resources/routines/title.yaml"),
        include_str!("../../../resources/routines/compact-and-continue.yaml"),
        include_str!("../../../resources/routines/fork.yaml"),
    ] {
        let definition = materialize_routine(source, "routines/existing.yaml").unwrap();
        assert!(definition.spec.work_groups.is_empty());
        assert!(definition.spec.outputs.is_empty());
    }
}

#[test]
fn interaction_examples_parse_and_validate_explicit_models() {
    let config = configuration();
    for source in [TRANSCRIBE, CLEANUP, CONVERSATION_NOTES, SYNTHESIZE] {
        let definition = materialize_routine(source, "routines/example.yaml").unwrap();
        validate_routine_models(&definition, &config, "routines/example.yaml").unwrap();
        let serialized = serde_yaml_ng::to_string(&definition).unwrap();
        materialize_routine(&serialized, "routines/example.yaml").unwrap();
    }
    let resolved =
        resolve_speech_model(&config, "speech", "dictation", proto::ModelType::Stt, true).unwrap();
    assert_eq!(resolved.endpoint, "http://localhost:50051/");
    assert_eq!(resolved.model_key, "dictation");
    assert!(matches!(
        resolved.settings,
        crate::resource_catalog::speech::SpeechSettings::Recognition(_)
    ));
}

#[test]
fn output_json_references_reject_nesting_and_non_output_contexts() {
    for reference in [
        "$(output.json.)",
        "$(output.json.text.nested)",
        "$(output.json.text[0])",
    ] {
        assert!(
            materialize_routine(
                &CONVERSATION_NOTES.replace("$(output.json.retained_transcript)", reference),
                "routines/notes.yaml",
            )
            .is_err()
        );
    }
    assert!(
        materialize_routine(
            &CONVERSATION_NOTES.replace(
                "$(tasks.collect-transcript.results.transcript)",
                "$(output.json.retained_transcript)",
            ),
            "routines/notes.yaml",
        )
        .is_err()
    );
    assert!(
        materialize_routine(
            &CONVERSATION_NOTES.replace(
                "$(tasks.write-notes.results.text)",
                "$(output.json.retained_transcript)",
            ),
            "routines/notes.yaml",
        )
        .is_err()
    );
}

fn invalid_change(source: &str, change: impl FnOnce(&mut RoutineDefinition)) {
    let mut definition = materialize_routine(source, "routines/example.yaml").unwrap();
    change(&mut definition);
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    assert!(
        materialize_routine(&source, "routines/example.yaml").is_err(),
        "unexpectedly accepted: {source}"
    );
}

#[test]
fn interaction_rejects_invalid_ports_references_formats_and_policies() {
    invalid_change(TRANSCRIBE, |definition| {
        let trigger = definition.spec.triggers[0].interaction.as_mut().unwrap();
        trigger.input_ports.push(trigger.input_ports[0].clone());
    });
    invalid_change(TRANSCRIBE, |definition| {
        definition.spec.triggers[0]
            .interaction
            .as_mut()
            .unwrap()
            .input_ports[0]
            .required = false;
    });
    invalid_change(TRANSCRIBE, |definition| {
        definition.spec.triggers[0]
            .interaction
            .as_mut()
            .unwrap()
            .input_ports[0]
            .format = RoutineStreamFormat::Text;
    });
    invalid_change(TRANSCRIBE, |definition| {
        definition.spec.outputs[0].port = "missing".into();
    });
    invalid_change(TRANSCRIBE, |definition| {
        definition.spec.tasks[0]
            .task_spec
            .transcribe
            .as_mut()
            .unwrap()
            .input = RoutineStreamRef::TaskOutput("missing".into());
    });
    invalid_change(TRANSCRIBE, |definition| {
        definition.spec.tasks[0]
            .task_spec
            .transcribe
            .as_mut()
            .unwrap()
            .model
            .model = "$(params.model)".into();
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[1]
            .task_spec
            .collect
            .as_mut()
            .unwrap()
            .max_bytes = 0;
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.work_groups[0].on_busy = RoutineWorkGroupBusy::Queue;
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.work_groups[0].max_queued = Some(1);
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[2].work_group = Some("missing".into());
    });
    invalid_change(CLEANUP, |definition| {
        definition
            .spec
            .work_groups
            .push(definition.spec.work_groups[0].clone());
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.params.push(RoutineParam {
            name: "required".into(),
            r#type: RoutineResultType::String,
            required: true,
            default: None,
            validation: None,
        });
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[2]
            .task_spec
            .session_template
            .as_mut()
            .unwrap()
            .user_prompt
            .text = "$(tasks.transcribe.output)".into();
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[2]
            .task_spec
            .session_template
            .as_mut()
            .unwrap()
            .user_prompt
            .text = "$(interaction.inputs.audio)".into();
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[2]
            .run_after
            .push("collect-transcript".into());
    });
    for value in [
        "prefix $(tasks.transcribe.output)",
        "$(params.text)",
        "$(tasks.transcribe.results.text)",
        "$(interaction.inputs.audio.bad)",
    ] {
        assert!(RoutineStreamRef::try_from(value.to_owned()).is_err());
    }
    assert!(
        materialize_routine(
            &TRANSCRIBE.replace(
                "required: true",
                "required: true\n            streams: single"
            ),
            "test"
        )
        .is_err()
    );
    assert!(
        materialize_routine(&TRANSCRIBE.replace("channels: 1", "channels: 0"), "test").is_err()
    );
}

#[test]
fn live_and_completion_edges_form_one_acyclic_graph() {
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[0].run_after.push("cleanup".into());
    });
    invalid_change(CLEANUP, |definition| {
        definition.spec.tasks[1].run_after.push("transcribe".into());
    });
    let source = r"apiVersion: waltr.dev/v1alpha1
kind: Routine
metadata: {name: cycle}
spec:
  description: Invalid live stream cycle.
  triggers:
    - name: speak
      interaction:
        inputPorts:
          - name: text
            required: true
            format: {type: text}
        outputPorts: []
  tasks:
    - name: transcribe
      taskSpec:
        transcribe:
          model: {provider: speech, model: dictation}
          input: $(tasks.synthesize.output)
    - name: synthesize
      taskSpec:
        synthesize:
          model: {provider: voice, model: reader}
          input: $(tasks.transcribe.output)
          format: {type: audio, encoding: pcmS16le, sampleRateHz: 24000, channels: 1}
";
    assert!(materialize_routine(source, "test").is_err());
}

const LIVE_CHAIN: &str = r"apiVersion: waltr.dev/v1alpha1
kind: Routine
metadata: {name: live-chain}
spec:
  description: Three concurrent live stream tasks.
  triggers:
    - name: dictate
      interaction:
        inputPorts:
          - name: audio
            required: true
            format: {type: audio, encoding: pcmS16le, sampleRateHz: 24000, channels: 1}
        outputPorts: []
  tasks:
    - name: recognize
      taskSpec:
        transcribe:
          model: {provider: speech, model: dictation}
          input: $(interaction.inputs.audio)
    - name: speak
      taskSpec:
        synthesize:
          model: {provider: voice, model: reader}
          input: $(tasks.recognize.output)
          format: {type: audio, encoding: pcmS16le, sampleRateHz: 24000, channels: 1}
    - name: recognize-again
      taskSpec:
        transcribe:
          model: {provider: speech, model: dictation}
          input: $(tasks.speak.output)
";

fn add_queue_group(definition: &mut RoutineDefinition) {
    definition.spec.work_groups.push(RoutineWorkGroup {
        name: "serial".into(),
        on_busy: RoutineWorkGroupBusy::Queue,
        max_queued: Some(8),
    });
}

#[test]
fn live_dependencies_cannot_share_a_queue_group() {
    for consumer in [1, 2] {
        let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
        add_queue_group(&mut definition);
        definition.spec.tasks[0].work_group = Some("serial".into());
        definition.spec.tasks[consumer].work_group = Some("serial".into());
        let source = serde_yaml_ng::to_string(&definition).unwrap();
        let error = materialize_routine(&source, "test")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&definition.spec.tasks[consumer].name),
            "{error}"
        );
        assert!(error.contains("live-stream component"), "{error}");
        assert!(error.contains("queue work group `serial`"), "{error}");
    }
}

#[test]
fn queue_groups_allow_independent_tasks_and_completion_successors() {
    let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
    add_queue_group(&mut definition);
    let mut first = independent_completion_task();
    first.work_group = Some("serial".into());
    let mut second = first.clone();
    second.name = "other".into();
    definition.spec.tasks.extend([first, second]);
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    materialize_routine(&source, "test").unwrap();

    // Completion waits precede group admission, so this successor cannot hold the lease early.
    definition.spec.tasks[0].work_group = Some("serial".into());
    definition.spec.tasks[3]
        .run_after
        .push("recognize-again".into());
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    materialize_routine(&source, "test").unwrap();

    let mut definition = materialize_routine(CLEANUP, "test").unwrap();
    add_queue_group(&mut definition);
    definition.spec.tasks[1].work_group = Some("serial".into());
    definition.spec.tasks[2].work_group = Some("serial".into());
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    materialize_routine(&source, "test").unwrap();
}

#[test]
fn live_dependencies_preserve_reject_and_interrupt_group_policies() {
    for policy in [
        RoutineWorkGroupBusy::Reject,
        RoutineWorkGroupBusy::Interrupt,
    ] {
        let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
        add_queue_group(&mut definition);
        definition.spec.work_groups[0].on_busy = policy;
        definition.spec.work_groups[0].max_queued = None;
        for task in &mut definition.spec.tasks {
            task.work_group = Some("serial".into());
        }
        let source = serde_yaml_ng::to_string(&definition).unwrap();
        materialize_routine(&source, "test").unwrap();
    }
}

#[test]
fn bounded_fanout_requires_concurrent_eligibility() {
    for shared_input in [false, true] {
        let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
        let (first, second) = if shared_input {
            let mut sibling = definition.spec.tasks[0].clone();
            sibling.name = "sibling".into();
            definition.spec.tasks.truncate(1);
            definition.spec.tasks.push(sibling);
            (0, 1)
        } else {
            let mut collector = materialize_routine(CLEANUP, "test")
                .unwrap()
                .spec
                .tasks
                .remove(1);
            collector.name = "collector".into();
            collector.task_spec.collect.as_mut().unwrap().stream =
                RoutineStreamRef::TaskOutput("recognize".into());
            let synthesis = definition.spec.tasks[1].clone();
            definition.spec.tasks.truncate(1);
            definition.spec.tasks.extend([collector, synthesis]);
            (1, 2)
        };
        let source = serde_yaml_ng::to_string(&definition).unwrap();
        materialize_routine(&source, "test").unwrap();
        for queue in [false, true] {
            let mut invalid = definition.clone();
            if queue {
                add_queue_group(&mut invalid);
                invalid.spec.tasks[first].work_group = Some("serial".into());
                invalid.spec.tasks[second].work_group = Some("serial".into());
            } else {
                let dependency = invalid.spec.tasks[first].name.clone();
                invalid.spec.tasks[second].run_after.push(dependency);
            }
            let source = serde_yaml_ng::to_string(&invalid).unwrap();
            let error = materialize_routine(&source, "test")
                .unwrap_err()
                .to_string();
            assert!(error.contains("same live-stream component"), "{error}");
            assert!(error.contains(&invalid.spec.tasks[second].name), "{error}");
            if queue {
                assert!(error.contains("queue work group `serial`"), "{error}");
            }
        }
    }
}

#[test]
fn separate_input_components_allow_shared_queue_groups_but_not_completion_cycles() {
    let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
    definition.spec.tasks.truncate(2);
    let interaction = definition.spec.triggers[0].interaction.as_mut().unwrap();
    let mut port = interaction.input_ports[0].clone();
    port.name = "other-audio".into();
    interaction.input_ports.push(port);
    let mut producer = definition.spec.tasks[0].clone();
    producer.name = "other-producer".into();
    producer.task_spec.transcribe.as_mut().unwrap().input =
        RoutineStreamRef::InteractionInput("other-audio".into());
    let mut consumer = definition.spec.tasks[1].clone();
    consumer.name = "other-consumer".into();
    consumer.task_spec.synthesize.as_mut().unwrap().input =
        RoutineStreamRef::TaskOutput("other-producer".into());
    definition.spec.tasks.extend([producer, consumer]);
    add_queue_group(&mut definition);
    definition.spec.tasks[0].work_group = Some("serial".into());
    definition.spec.tasks[2].work_group = Some("serial".into());
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    materialize_routine(&source, "test").unwrap();
    definition.spec.tasks[1]
        .run_after
        .push("other-producer".into());
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    materialize_routine(&source, "test").unwrap();
    definition.spec.tasks[3].run_after.push("recognize".into());
    let source = serde_yaml_ng::to_string(&definition).unwrap();
    let error = materialize_routine(&source, "test")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cycle between live-stream components"),
        "{error}"
    );
}

fn independent_completion_task() -> RoutineTask {
    let mut task = materialize_routine(CLEANUP, "test")
        .unwrap()
        .spec
        .tasks
        .remove(2);
    task.name = "barrier".into();
    task.work_group = None;
    task.run_after.clear();
    task.task_spec
        .session_template
        .as_mut()
        .unwrap()
        .user_prompt
        .text = "Independent work.".into();
    task
}

#[test]
fn live_consumers_reject_completion_waits_on_any_upstream_producer() {
    for indirect_completion in [false, true] {
        let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
        let dependency = if indirect_completion {
            let mut barrier = independent_completion_task();
            barrier.run_after.push("recognize".into());
            definition.spec.tasks.push(barrier);
            "barrier"
        } else {
            "recognize"
        };
        definition.spec.tasks[2].run_after.push(dependency.into());
        let source = serde_yaml_ng::to_string(&definition).unwrap();
        let error = materialize_routine(&source, "test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("live-stream component"), "{error}");
    }
}

#[test]
fn live_chains_and_independent_completion_dependencies_remain_valid() {
    let mut definition = materialize_routine(LIVE_CHAIN, "test").unwrap();
    validate_routine_models(&definition, &configuration(), "test").unwrap();
    definition.spec.tasks.push(independent_completion_task());
    definition.spec.tasks[2].run_after.push("barrier".into());
    validate_routine(&definition, "test").unwrap();
    materialize_routine(CLEANUP, "test").unwrap();
}

#[test]
fn speech_transport_validation_is_distinct_and_has_no_auth_or_catalog() {
    for source in [
        PROVIDER.replace("type: 3", "type: 4"),
        PROVIDER.replace("googleSpeechV2", "openaiChatCompletions"),
        PROVIDER.replace("channels: 1", "channels: 0"),
        PROVIDER.replace(
            "automatic_punctuation: true",
            "automatic_punctuation: true\n        typo: true",
        ),
        PROVIDER.replace("  models:", "  headers: {Authorization: secret}\n  models:"),
        PROVIDER.replace(
            "  models:",
            "  catalog: {provider: google, include: ['*']}\n  models:",
        ),
        PROVIDER.replace("  models:", "  oauth: {}\n  models:"),
        PROVIDER.replace("http://localhost:50051", "http://secret@localhost:50051"),
    ] {
        assert!(
            materialize_provider(&source, "providers/speech.yaml", None).is_err(),
            "unexpectedly accepted: {source}"
        );
    }
    let definition = materialize_routine(TRANSCRIBE, "test").unwrap();
    let mut config = configuration();
    config
        .providers
        .get_mut("speech")
        .unwrap()
        .models
        .get_mut("dictation")
        .unwrap()
        .recognition
        .as_mut()
        .unwrap()
        .sample_rate_hz = 16000;
    assert!(validate_routine_models(&definition, &config, "test").is_err());
    let config = configuration();
    assert!(resolve_speech_model(&config, "voice", "reader", proto::ModelType::Stt, true).is_err());
    assert!(
        resolve_speech_model(&config, "inference", "cleanup", proto::ModelType::Stt, true).is_err()
    );
    let cleanup = materialize_routine(CLEANUP, "test").unwrap();
    let mut wrong_inference = configuration();
    wrong_inference
        .providers
        .get_mut("inference")
        .unwrap()
        .models
        .get_mut("cleanup")
        .unwrap()
        .r#type = proto::ModelType::Stt as i32;
    assert!(validate_routine_models(&cleanup, &wrong_inference, "test").is_err());
    let mut unary_only = configuration();
    unary_only
        .providers
        .get_mut("speech")
        .unwrap()
        .models
        .get_mut("dictation")
        .unwrap()
        .speech_capabilities
        .as_mut()
        .unwrap()
        .streaming = false;
    assert!(validate_routine_models(&definition, &unary_only, "test").is_err());
    assert!(
        resolve_speech_model(
            &unary_only,
            "speech",
            "dictation",
            proto::ModelType::Stt,
            false
        )
        .is_ok()
    );
}

#[test]
fn work_groups_and_server_limits_are_explicit_and_bounded() {
    for policy in [
        RoutineWorkGroupBusy::Queue,
        RoutineWorkGroupBusy::Reject,
        RoutineWorkGroupBusy::Interrupt,
    ] {
        let mut definition = materialize_routine(CLEANUP, "test").unwrap();
        definition.spec.work_groups[0].on_busy = policy;
        definition.spec.work_groups[0].max_queued =
            (policy == RoutineWorkGroupBusy::Queue).then_some(8);
        validate_routine(&definition, "test").unwrap();
    }
    let limits = InteractionLimits::default();
    assert!(limits.validate().is_ok());
    assert_eq!(limits.complete_request_bytes, 256 * 1024 * 1024);
    assert_eq!(limits.chunk_bytes, 1024 * 1024);
    assert_eq!(limits.edge_bytes, 2 * 1024 * 1024);
    assert_eq!(limits.queued_chunks, 8);
    assert_eq!(limits.active_interactions, 16);
    assert_eq!(limits.active_tasks_per_interaction, 8);
    assert_eq!(limits.queue_wait_seconds, 30);
    assert_eq!(limits.input_seconds, 3900);
    assert_eq!(limits.invocation_seconds, 5400);
    assert_eq!(limits.collector_bytes, 1024 * 1024);
    let notes = materialize_routine(CONVERSATION_NOTES, "test").unwrap();
    assert_eq!(
        notes.spec.tasks[1]
            .task_spec
            .collect
            .as_ref()
            .unwrap()
            .max_bytes,
        262144
    );
    assert!(
        InteractionLimits {
            chunk_bytes: 0,
            ..limits.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        InteractionLimits {
            chunk_bytes: usize::MAX,
            ..limits.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        InteractionLimits {
            input_seconds: 5401,
            ..limits
        }
        .validate()
        .is_err()
    );
}
