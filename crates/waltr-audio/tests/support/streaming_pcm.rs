//! Loopback Speech v2 message sizing and byte-preservation regressions.
use super::support;
use futures_util::{StreamExt, stream};
use waltr_audio::{CancellationToken, audio::PcmS16Le, error::Error, stt};
use std::time::Duration;

async fn sized_recording(channels: i32, sizes: &[usize], expected_sizes: &[usize]) {
    let (client, _server, capture) = support::start_speech_capture(|config| {
        config.queue_bytes = 1024 * 1024;
    })
    .await;
    let chunks: Vec<Vec<u8>> = sizes
        .iter()
        .map(|size| (0..*size).map(|n| n.to_le_bytes()[0]).collect())
        .collect();
    let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, channels).unwrap(),
            stream::iter(chunks.into_iter().map(Ok)),
            CancellationToken::new(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(response) = output.next().await {
            response.unwrap();
        }
    })
    .await
    .unwrap();
    let requests = capture.lock().unwrap().clone();
    assert_eq!(requests[0].recognizer, "recognizer");
    let Some(stt::streaming_recognize_request::StreamingRequest::StreamingConfig(config)) =
        &requests[0].streaming_request
    else {
        panic!("initial config absent");
    };
    assert!(matches!(
        config.config.as_ref().unwrap().decoding_config,
        Some(stt::recognition_config::DecodingConfig::ExplicitDecodingConfig(decoding))
            if decoding.sample_rate_hertz == 24000 && decoding.audio_channel_count == channels
    ));
    let audio: Vec<&[u8]> = requests[1..]
        .iter()
        .map(|request| {
            assert!(request.recognizer.is_empty());
            let Some(stt::streaming_recognize_request::StreamingRequest::Audio(bytes)) =
                &request.streaming_request
            else {
                panic!("repeated config");
            };
            assert!(bytes.len() <= 15_360);
            assert_eq!(bytes.len() % (usize::try_from(channels).unwrap() * 2), 0);
            bytes.as_slice()
        })
        .collect();
    assert_eq!(
        audio.iter().map(|bytes| bytes.len()).collect::<Vec<_>>(),
        expected_sizes
    );
    assert_eq!(audio.concat(), expected);
}

#[tokio::test]
async fn browser_recording_is_split_before_upstream_message_limit() {
    sized_recording(1, &[48_000], &[15_360, 15_360, 15_360, 1920]).await;
}

#[tokio::test]
async fn exact_cap_empty_and_caller_boundaries_preserve_audio() {
    sized_recording(1, &[0, 15_360, 15_362, 2], &[0, 15_360, 15_360, 2, 2]).await;
    sized_recording(1, &[], &[]).await;
    sized_recording(7, &[30_716], &[15_358, 15_358]).await;
}

#[tokio::test]
async fn malformed_large_chunk_is_rejected_before_any_of_its_audio() {
    let (client, _server, capture) = support::start_speech_capture(|config| {
        config.queue_bytes = 1024 * 1024;
    })
    .await;
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, 1).unwrap(),
            stream::iter([Ok(vec![0; 48_001])]),
            CancellationToken::new(),
        )
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), output.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Invalid("incomplete PCM frame")));
    assert!(output.next().await.is_none());
    assert!(capture.lock().unwrap().iter().all(|request| matches!(
        request.streaming_request,
        Some(stt::streaming_recognize_request::StreamingRequest::StreamingConfig(_))
    )));
}

#[tokio::test]
async fn original_caller_admission_limits_include_protobuf_overhead() {
    for encoding_limit in [false, true] {
        for (limit, accepted) in [(48_003, false), (48_004, true)] {
            let (client, _server, capture) = support::start_speech_capture(|config| {
                config.queue_bytes = 1024 * 1024;
                if encoding_limit {
                    config.max_encoding_bytes = limit;
                } else {
                    config.queue_bytes = limit;
                }
            })
            .await;
            let mut output = client
                .streaming_recognize(
                    "recognizer".into(),
                    stt::StreamingRecognitionConfig::default(),
                    PcmS16Le::new(24000, 1).unwrap(),
                    stream::iter([Ok(vec![0; 48_000])]),
                    CancellationToken::new(),
                )
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), output.next())
                .await
                .unwrap()
                .unwrap();
            if accepted {
                result.unwrap();
                assert_eq!(capture.lock().unwrap().len(), 5);
            } else {
                assert!(matches!(
                    result,
                    Err(Error::Invalid(
                        "input chunk exceeds queue or encoding limit"
                    ))
                ));
                assert!(capture.lock().unwrap().len() <= 1);
            }
            assert!(output.next().await.is_none());
        }
    }
}

#[tokio::test]
async fn frame_wider_than_cap_and_tiny_limits_fail_without_looping() {
    let (client, _server) = support::start(support::Mode::Ok, Duration::from_secs(5)).await;
    assert!(matches!(
        client.streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, 7681).unwrap(),
            stream::pending(),
            CancellationToken::new(),
        ),
        Err(Error::Invalid("PCM frame exceeds streaming audio limit"))
    ));
    let (client, _server, _) = support::start_speech_capture(|config| config.queue_bytes = 1).await;
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, 1).unwrap(),
            stream::iter([Ok(vec![0; 2])]),
            CancellationToken::new(),
        )
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), output.next())
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Invalid(
            "input chunk exceeds queue or encoding limit"
        ))
    ));
}

#[tokio::test]
async fn split_input_is_dropped_on_cancel_deadline_status_and_early_eof() {
    for mode in [
        support::Mode::Stall,
        support::Mode::Fail,
        support::Mode::EarlyEof,
    ] {
        let (client, _server) = support::start_with(mode, Duration::from_millis(150), |config| {
            config.queue_bytes = 1024 * 1024;
        })
        .await;
        let (input, dropped) = super::tracked(stream::repeat_with(|| Ok(vec![0; 48_000])));
        let mut output = client
            .streaming_recognize(
                "recognizer".into(),
                stt::StreamingRecognitionConfig::default(),
                PcmS16Le::new(24000, 1).unwrap(),
                input,
                CancellationToken::new(),
            )
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), output.next())
            .await
            .unwrap();
        match mode {
            support::Mode::Stall => assert!(matches!(
                response,
                Some(Err(Error::Deadline | Error::Status(_)))
            )),
            support::Mode::Fail => super::check_status(response.unwrap().unwrap_err()),
            support::Mode::EarlyEof => assert!(response.is_none()),
            support::Mode::Ok | support::Mode::TerminalFail => unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(3), dropped)
            .await
            .unwrap()
            .unwrap();
    }
    let (client, _server) =
        support::start_with(support::Mode::Stall, Duration::from_secs(5), |config| {
            config.queue_bytes = 1024 * 1024;
        })
        .await;
    let (polled, first_poll) = tokio::sync::oneshot::channel();
    let (input, dropped) = super::tracked(
        stream::once(async move {
            polled.send(()).unwrap();
            Ok(vec![0; 48_000])
        })
        .chain(stream::pending()),
    );
    let parent = CancellationToken::new();
    let cancel = parent.child_token();
    let mut output = client
        .streaming_recognize(
            "recognizer".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, 1).unwrap(),
            input,
            cancel.clone(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), first_poll)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), output.next())
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Cancelled)
    ));
    tokio::time::timeout(Duration::from_secs(3), dropped)
        .await
        .unwrap()
        .unwrap();
    assert!(!parent.is_cancelled());
}
