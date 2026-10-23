use super::*;

fn providers(channels: u32, pitch: f32) -> Vec<(&'static str, String)> {
    [
        ("speech", "transport: googleSpeechV2\n  url: http://127.0.0.1:1\n  models:\n    dictation:\n      name: Test\n      type: 3\n      speech_capabilities: {unary: true, streaming: true}\n      recognition: {recognizer: recognizer, model: chirp_3, language_codes: [en-US], sample_rate_hz: 24000, channels: 1, interim_results: true, automatic_punctuation: true}".into()),
        ("voice", format!("transport: googleTextToSpeechV1\n  url: http://127.0.0.1:1\n  models:\n    reader:\n      name: Test\n      type: 4\n      speech_capabilities: {{unary: true, streaming: true}}\n      synthesis: {{voice: test-voice, language_code: en-US, sample_rate_hz: 24000, channels: {channels}, pitch: {pitch}}}")),
        ("inference", "transport: openaiChatCompletions\n  url: http://127.0.0.1:1/v1\n  models:\n    cleanup:\n      name: Test\n      type: 1\n      limits: {context: 65536, output: 1024}".into()),
    ].into_iter().map(|(name, spec)| {
        let path = match name { "speech" => "providers/speech.yaml", "voice" => "providers/voice.yaml", _ => "providers/inference.yaml" };
        (path, format!("apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata: {{name: {name}}}\nspec:\n  {spec}\n"))
    }).collect()
}

async fn rejects_without_history(source: &str, trigger: &str, resources: &[(&str, String)]) {
    let (service, engine, _bus, persistence, root, _) = setup_resources(
        source,
        InteractionLimits::default(),
        session_fixture::adapters(),
        resources,
    )
    .await;
    let mut identity = open();
    identity.trigger_name = trigger.into();
    let mut response = service
        .run(Request::new(wire::RunInteractionRequest {
            open: Some(identity),
            inputs: vec![],
        }))
        .await
        .unwrap()
        .into_inner();
    let Some(Out::ValidationFailure(failure)) = response.next().await.unwrap().unwrap().event
    else {
        panic!("validation failure expected")
    };
    assert!(
        failure.message.contains(if trigger == "dictate" {
            "maxTurns 1"
        } else {
            "streaming synthesis requires mono"
        }),
        "{}",
        failure.message
    );
    assert!(response.next().await.is_none());
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unsupported_static_helper_capabilities_are_rejected_without_history() {
    let source = include_str!("../../../../docs/examples/routines/audio-transcribe-cleanup.yaml")
        .replace("name: audio-transcribe-cleanup", "name: echo");
    for (old, new) in [
        ("maxTurns: 1", "maxTurns: 2"),
        ("tools: []", "tools: [waltr]"),
        ("features: []", "features: [test]"),
        ("permissions: []", "permissions: [test]"),
    ] {
        rejects_without_history(&source.replace(old, new), "dictate", &providers(1, 0.0)).await;
    }
}

#[tokio::test]
async fn unsupported_synthesis_mapping_is_rejected_without_history() {
    let source = include_str!("../../../../docs/examples/routines/audio-synthesize.yaml")
        .replace("name: audio-synthesize", "name: echo");
    rejects_without_history(&source, "speak", &providers(1, 1.0)).await;
}

#[tokio::test]
async fn stereo_synthesis_configuration_cannot_reach_admission() {
    let source = include_str!("../../../../docs/examples/routines/audio-synthesize.yaml")
        .replace("name: audio-synthesize", "name: echo");
    let (_service, engine, _bus, persistence, root, state) = setup_resources(
        &source,
        InteractionLimits::default(),
        session_fixture::adapters(),
        &providers(1, 0.0),
    )
    .await;
    let invalid = providers(2, 0.0)
        .into_iter()
        .find(|(path, _)| *path == "providers/voice.yaml")
        .unwrap()
        .1;
    std::fs::write(root.join(".waltr/resources/providers/voice.yaml"), invalid).unwrap();
    state.reload_from_disk().await;
    assert!(state.reload_error().unwrap().contains("invalid synthesis"));
    assert!(
        persistence
            .routine_invocation_store()
            .list(None, 100, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    engine.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}
