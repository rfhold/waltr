use crate::{Config, Converter, InputFormat, Samples};

#[allow(clippy::cast_precision_loss)] // Test oscillators are at most one second long.
fn tone_rms(rate: u32, frequency: f32) -> f64 {
    let mut converter =
        Converter::new(Config::default(), InputFormat { rate, channels: 1 }).unwrap();
    let tone: Vec<f32> = (0..rate)
        .map(|index| (index as f32 * frequency * std::f32::consts::TAU / rate as f32).sin() * 0.5)
        .collect();
    let mut wire = Vec::new();
    for block in tone.chunks(1024) {
        wire.extend(converter.push(Samples::F32(block)).unwrap());
    }
    wire.extend(converter.finish().unwrap());
    let signal: Vec<f64> = wire
        .chunks_exact(2)
        .map(|v| f64::from(i16::from_le_bytes([v[0], v[1]])) / 32768.0)
        .collect();
    let middle = &signal[1024..signal.len() - 1024];
    (middle.iter().map(|sample| sample * sample).sum::<f64>() / middle.len() as f64).sqrt()
}

#[test]
fn sinc_preserves_voice_band_and_rejects_aliases() {
    for rate in [44_100, 48_000] {
        assert!((tone_rms(rate, 440.0) - 0.35355).abs() < 0.005);
        assert!(tone_rms(rate, 18_000.0) < 0.01);
    }
}

#[test]
fn all_supported_rate_extremes_have_exact_trimmed_tails() {
    for input_rate in [8_000, 24_000, 44_100, 48_000, 192_000] {
        for target_rate in [8_000, 24_000, 48_000] {
            for frames in [1, 255, 256, 257, 1023] {
                let config = Config {
                    target_rate,
                    ..Config::default()
                };
                let mut converter = Converter::new(
                    config,
                    InputFormat {
                        rate: input_rate,
                        channels: 1,
                    },
                )
                .unwrap();
                let mut result = converter.push(Samples::I16(&vec![200; frames])).unwrap();
                result.extend(converter.finish().unwrap());
                assert_eq!(
                    result.len(),
                    frames * target_rate as usize / input_rate as usize * 2,
                    "input {input_rate}, target {target_rate}, frames {frames}"
                );
            }
        }
    }
}

#[test]
fn clip_is_saturating_and_diagnostics_redact_audio() {
    let mut converter = Converter::new(
        Config::default(),
        InputFormat {
            rate: 24_000,
            channels: 1,
        },
    )
    .unwrap();
    let mut wire = converter.push(Samples::F32(&[100.0; 4096])).unwrap();
    wire.extend(converter.finish().unwrap());
    let pcm: Vec<i16> = wire
        .chunks_exact(2)
        .map(|v| i16::from_le_bytes([v[0], v[1]]))
        .collect();
    assert!(pcm[1024..3072].iter().all(|sample| *sample > 32_700));
    assert!(!format!("{:?}", Samples::I16(&[12345])).contains("12345"));
    assert!(!format!("{converter:?}").contains("100.0"));
}
