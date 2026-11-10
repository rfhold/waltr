//! Layout evidence for pre-decode admission; this is not a bounded-decoder test.
use prost::Message;
use waltr_audio::stt::WordInfo;

#[test]
fn encoded_bytes_do_not_bound_generated_word_vector_retention() {
    let layout = std::mem::size_of::<WordInfo>();
    let word = WordInfo {
        word: "a".into(),
        speaker_label: "S".into(),
        ..Default::default()
    };
    // "S:" plus 524,287 occurrences of " a" is exactly one MiB.
    let words = (1024 * 1024 - 2) / 2;
    let vector = words * layout;
    let encoded_words = words * (2 + word.encoded_len());
    eprintln!(
        "WordInfo={layout} bytes; 131072 layouts={} bytes; one-MiB note words={words}; vector={vector} bytes; encoded words={encoded_words} bytes; three response layouts={} bytes",
        131_072 * layout,
        3 * vector,
    );
    assert!(layout >= 64);
    assert!(131_072 * layout >= 8 * 1024 * 1024);
    assert!(vector > encoded_words);
}
