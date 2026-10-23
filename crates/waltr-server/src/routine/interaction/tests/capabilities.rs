use super::*;

#[tokio::test]
async fn static_preflight_checks_helper_limits_and_empty_capability_lists() {
    let valid = request(CLEANUP).await;
    validate_capabilities(&valid.routine, &valid.configuration).unwrap();
    for (old, new) in [
        ("maxTurns: 1", "maxTurns: 2"),
        ("tools: []", "tools: [waltr]"),
        ("features: []", "features: [test]"),
        ("permissions: []", "permissions: [test]"),
    ] {
        let source = CLEANUP.replace(old, new);
        assert_ne!(source, CLEANUP);
        let request = request(&source).await;
        assert_eq!(
            validate_capabilities(&request.routine, &request.configuration)
                .unwrap_err()
                .code,
            "invalid_helper"
        );
    }
}

#[tokio::test]
async fn static_preflight_checks_streaming_models_and_synthesis_mapping() {
    let mut request = request(SYNTHESIZE).await;
    validate_capabilities(&request.routine, &request.configuration).unwrap();
    let configuration = Arc::make_mut(&mut request.configuration);
    let model = configuration
        .providers
        .get_mut("voice")
        .unwrap()
        .models
        .get_mut("reader")
        .unwrap();
    model.speech_capabilities.as_mut().unwrap().streaming = false;
    assert_eq!(
        validate_capabilities(&request.routine, &request.configuration)
            .unwrap_err()
            .code,
        "invalid_model"
    );
    let model = Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("voice")
        .unwrap()
        .models
        .get_mut("reader")
        .unwrap();
    model.speech_capabilities.as_mut().unwrap().streaming = true;
    model.synthesis.as_mut().unwrap().channels = 2;
    assert_eq!(
        validate_capabilities(&request.routine, &request.configuration)
            .unwrap_err()
            .code,
        "invalid_model"
    );
    let model = Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("voice")
        .unwrap()
        .models
        .get_mut("reader")
        .unwrap();
    model.synthesis.as_mut().unwrap().channels = 1;
    model.synthesis.as_mut().unwrap().pitch = Some(1.0);
    assert_eq!(
        validate_capabilities(&request.routine, &request.configuration)
            .unwrap_err()
            .code,
        "unsupported_format"
    );
    let mut recognition = super::request(TRANSCRIBE).await;
    Arc::make_mut(&mut recognition.configuration)
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
    assert_eq!(
        validate_capabilities(&recognition.routine, &recognition.configuration)
            .unwrap_err()
            .code,
        "invalid_model"
    );
}

#[tokio::test]
async fn static_preflight_rejects_non_inference_helper_model_and_start_sessions_uses_it() {
    let mut request = request(CLEANUP).await;
    Arc::make_mut(&mut request.configuration)
        .providers
        .get_mut("inference")
        .unwrap()
        .models
        .get_mut("cleanup")
        .unwrap()
        .r#type = waltr::grpc::waltr::config::v1::ModelType::Stt as i32;
    assert_eq!(
        validate_capabilities(&request.routine, &request.configuration)
            .unwrap_err()
            .code,
        "invalid_model"
    );
    let (host, _sandbox) = fixtures::Host::new(Arc::new(tokio::sync::Notify::new())).await;
    assert!(matches!(
        start_sessions(request, host.clone()),
        Err(Error {
            code: "invalid_model",
            ..
        })
    ));
    assert_eq!(host.created.load(Ordering::SeqCst), 0);
    use crate::session::SessionHost;
    host.shutdown().await;
}

#[tokio::test]
async fn complete_request_feed_preserves_validation_without_double_charging() {
    let limits = InteractionLimits {
        chunk_bytes: 4,
        edge_bytes: 16,
        ..Default::default()
    };
    let (sender, _receiver) = stream::edge(&limits);
    let (failures, _errors) = watch::channel(None);
    let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut input = Input {
        sender,
        cancel: CancellationToken::new(),
        closed: false,
        state: State::new(
            RoutineStreamFormat::Audio {
                encoding: crate::resource_catalog::routine::RoutineAudioEncoding::PcmS16Le,
                sample_rate_hz: 24000,
                channels: 1,
            },
            64,
        ),
        bytes: bytes.clone(),
        max_bytes: 4,
        failures,
    };
    input.send_complete(Chunk::Audio(vec![0; 4])).await.unwrap();
    input.send_complete(Chunk::Audio(vec![0; 4])).await.unwrap();
    assert_eq!(bytes.load(Ordering::SeqCst), 0);
    assert_eq!(
        input
            .send_complete(Chunk::Audio(vec![0; 3]))
            .await
            .unwrap_err()
            .code,
        "invalid_format"
    );
}

#[test]
fn queue_wait_configuration_defaults_and_validation() {
    let defaults: InteractionLimits = serde_yaml_ng::from_str("{}").unwrap();
    assert_eq!(defaults.queue_wait_seconds, 30);
    let custom: InteractionLimits = serde_yaml_ng::from_str("queueWaitSeconds: 2").unwrap();
    assert_eq!(custom.queue_wait_seconds, 2);
    custom.validate().unwrap();
    let mut invalid = custom;
    invalid.queue_wait_seconds = 0;
    assert!(invalid.validate().is_err());
    invalid.queue_wait_seconds = u64::MAX;
    assert!(invalid.validate().is_err());
}
