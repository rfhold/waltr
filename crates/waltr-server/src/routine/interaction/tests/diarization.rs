use super::*;
use waltr_audio::stt;

fn response(text: &str, words: &[(&str, &str)], is_final: bool) -> stt::StreamingRecognizeResponse {
    stt::StreamingRecognizeResponse {
        results: vec![stt::StreamingRecognitionResult {
            alternatives: vec![stt::SpeechRecognitionAlternative {
                transcript: text.into(),
                words: words
                    .iter()
                    .map(|(word, speaker)| stt::WordInfo {
                        word: (*word).into(),
                        speaker_label: (*speaker).into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            is_final,
            channel_tag: 7,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn cumulative_diarization_replaces_revisions_and_collects_speaker_turns_once() {
    let mut mapper = adapters::Transcripts::new(true);
    let mut state = State::new(RoutineStreamFormat::Text, 4096);
    for response in [
        response("hi", &[], false),
        response("Hello, world! Yes. Unclear. Again.", &[], false),
        response(
            "Hello, world! Yes. Unclear. Again.",
            &[
                ("Hello,", "S1"),
                ("world!", "S1"),
                ("Yes.", "S2"),
                ("Unclear.", ""),
                ("Again.", "S1"),
            ],
            true,
        ),
    ] {
        for text in mapper.update(response).unwrap() {
            state.accept(&Chunk::Text(text)).unwrap();
        }
    }
    state.close().unwrap();
    assert_eq!(
        state.collect(" ", 4096).unwrap(),
        "S1: Hello, world!\nS2: Yes.\nunknown: Unclear.\nS1: Again."
    );
    assert!(
        mapper
            .update(response("duplicate", &[("duplicate", "S1")], true))
            .is_err()
    );
}

#[test]
fn plain_transcription_ignores_word_labels_and_keeps_segment_behavior() {
    let mut mapper = adapters::Transcripts::new(false);
    let first = mapper
        .update(response("Hello, world!", &[("ignored", "S1")], true))
        .unwrap();
    let second = mapper.update(response("Next.", &[], true)).unwrap();
    assert_eq!(first[0].text, "Hello, world!");
    assert_eq!(first[0].segment, "channel-7-0");
    assert_eq!(second[0].segment, "channel-7-1");
}

#[test]
fn diarization_rejects_missing_mismatched_or_unsafe_attribution() {
    for response in [
        response("missing", &[], true),
        response("different", &[("word", "S1")], true),
        response("two words", &[("two words", "S1")], true),
        response("word", &[("word", "S1\nS2")], true),
        response("word", &[("word", &"x".repeat(257))], true),
        stt::StreamingRecognizeResponse {
            results: vec![stt::StreamingRecognitionResult {
                is_final: true,
                ..Default::default()
            }],
            ..Default::default()
        },
    ] {
        assert!(adapters::Transcripts::new(true).update(response).is_err());
    }
    let output = adapters::Transcripts::new(true)
        .update(response("", &[], true))
        .unwrap();
    assert!(output[0].text.is_empty() && output[0].committed);
    let output = adapters::Transcripts::new(true)
        .update(response(
            "hello\tthere!",
            &[("hello", " "), ("there!", "")],
            true,
        ))
        .unwrap();
    assert_eq!(output[0].text, "unknown: hello there!");
}

#[tokio::test]
async fn recognition_diarization_defaults_off_and_sends_empty_opt_in_config() {
    let request = request(TRANSCRIBE).await;
    let options = request.configuration.providers["speech"].models["dictation"]
        .recognition
        .as_ref()
        .unwrap();
    assert!(!options.diarization);
    assert!(
        adapters::recognition_features(options)
            .diarization_config
            .is_none()
    );
    let mut enabled = options.clone();
    enabled.diarization = true;
    let features = adapters::recognition_features(&enabled);
    assert_eq!(
        features.diarization_config,
        Some(stt::SpeakerDiarizationConfig::default())
    );
    assert_eq!(
        features.enable_automatic_punctuation,
        options.automatic_punctuation
    );
    let mut value = serde_json::to_value(&enabled).unwrap();
    value.as_object_mut().unwrap().remove("diarization");
    let old: waltr::grpc::waltr::config::v1::SpeechRecognitionOptions =
        serde_json::from_value(value).unwrap();
    assert!(!old.diarization);
}
