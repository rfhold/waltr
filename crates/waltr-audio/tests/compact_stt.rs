//! Wire-backed speech admission, ownership, and loopback transport contracts.
#[path = "support/compact_wire.rs"]
mod wire;
use futures_util::{StreamExt, stream};
use std::time::Duration;
use waltr_audio::{CancellationToken, audio::PcmS16Le, stt};

#[tokio::test]
#[allow(
    clippy::significant_drop_tightening,
    reason = "Responses remains owned through its awaited worker joins"
)]
async fn actual_tonic_large_wire_borrowed_words_and_checked_out_gate() {
    let (client, _server) = wire::start(
        vec![
            wire::cumulative(524_287, "S", true),
            wire::cumulative(10000, "T", false),
        ],
        true,
        false,
    )
    .await;
    let mut responses = client
        .streaming_recognize_compact(
            "r".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, 1).unwrap(),
            stream::iter([Ok(vec![0, 0])]),
            CancellationToken::new(),
        )
        .unwrap();
    let response = responses.next().await.unwrap().unwrap();
    assert!(response.wire_bytes() > 4 * 1024 * 1024 - 32);
    let result = response.results().next().unwrap();
    assert!(result.is_final());
    assert_eq!(result.channel(), 2);
    let alternative = result.alternatives().next().unwrap();
    assert_eq!(alternative.words().count(), 524_287);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), responses.next())
            .await
            .is_err()
    );
    drop(response);
    let second = responses.next().await.unwrap().unwrap();
    assert_eq!(
        second
            .results()
            .next()
            .unwrap()
            .alternatives()
            .next()
            .unwrap()
            .words()
            .next()
            .unwrap()
            .speaker(),
        "T"
    );
    drop(second);
    assert!(responses.next().await.is_none());
    responses.finish().await.unwrap();
}

#[tokio::test]
#[allow(
    clippy::significant_drop_tightening,
    reason = "Responses remains owned through its awaited worker joins"
)]
async fn malformed_bombs_utf8_offsets_oversize_and_cancel_are_joined() {
    let mut missing_word = Vec::new();
    wire::field(3, &[], &mut missing_word);
    let mut result = Vec::new();
    wire::field(1, &missing_word, &mut result);
    let mut bomb = Vec::new();
    wire::field(6, &result, &mut bomb);
    let mut negative = Vec::new();
    wire::field(
        7,
        &[8, 255, 255, 255, 255, 255, 255, 255, 255, 255, 1],
        &mut negative,
    );
    let mut overflow = vec![0x32];
    overflow.extend_from_slice(&[255; 11]);
    for bytes in [
        bomb,
        negative,
        overflow,
        wire::cumulative(524_289, "S", true),
        vec![0; 8 * 1024 * 1024 + 1],
        vec![0x32, 5, 0x0a, 3, 0x0a, 1, 255],
    ] {
        let (client, _server) = wire::start(vec![bytes], true, false).await;
        let mut responses = client
            .streaming_recognize_compact(
                "r".into(),
                stt::StreamingRecognitionConfig::default(),
                PcmS16Le::new(24000, 1).unwrap(),
                stream::empty(),
                CancellationToken::new(),
            )
            .unwrap();
        assert!(responses.next().await.unwrap().is_err());
        responses.cancel_and_join().await.unwrap();
    }
    let (client, _server) =
        wire::start(vec![wire::cumulative(10000, "S", false)], false, false).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let responses = client
        .streaming_recognize_compact(
            "r".into(),
            stt::StreamingRecognitionConfig::default(),
            PcmS16Le::new(24000, 1).unwrap(),
            stream::pending(),
            cancel,
        )
        .unwrap();
    responses.cancel_and_join().await.unwrap();
}

#[tokio::test]
#[allow(
    clippy::significant_drop_tightening,
    reason = "Responses remains owned through its awaited worker joins"
)]
async fn compact_cancel_joins_with_checked_out_or_queued_response() {
    for checkout in [false, true] {
        let (client, _server) = wire::start(
            vec![
                wire::cumulative(10000, "S", true),
                wire::cumulative(10000, "T", true),
            ],
            true,
            false,
        )
        .await;
        let mut responses = client
            .streaming_recognize_compact(
                "r".into(),
                stt::StreamingRecognitionConfig::default(),
                PcmS16Le::new(24000, 1).unwrap(),
                stream::empty(),
                CancellationToken::new(),
            )
            .unwrap();
        let held = if checkout {
            Some(responses.next().await.unwrap().unwrap())
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
            None
        };
        tokio::time::timeout(Duration::from_secs(2), responses.cancel_and_join())
            .await
            .unwrap()
            .unwrap();
        if let Some(held) = held {
            assert_eq!(
                held.results()
                    .next()
                    .unwrap()
                    .alternatives()
                    .next()
                    .unwrap()
                    .words()
                    .count(),
                10000
            );
        }
    }
}
