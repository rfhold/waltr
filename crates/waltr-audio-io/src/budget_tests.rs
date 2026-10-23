use crate::{Config, Converter, Error, InputFormat, Samples};

fn desktop(target: u32) -> Config {
    let mut config = Config {
        target_rate: target,
        ..Config::default()
    };
    config.limits.raw_queue_items = 1;
    config.limits.preaccept_external_chunks = Some(6);
    config.limits.output_queue_bytes = target as usize / 2;
    config.limits.output_queue_items = 4;
    config.limits.max_chunk_bytes = target as usize / 16 / 2 * 2;
    config
}
#[test]
fn aggregate_accounts_pool_inflight_history_flush_and_external_credits() {
    let config = desktop(24_000);
    let input = InputFormat {
        rate: 8_000,
        channels: 1,
    };
    let budget = config.audio_budget(input, 6).unwrap();
    // Two raw blocks include BOTH possible in-flight owners, not merely the one queue slot.
    assert_eq!(budget.raw, 24_576);
    assert_eq!(budget.converter_input, 2_688);
    assert_eq!(budget.converter_output, 17_116);
    assert_eq!(budget.native_output, 3_750);
    assert_eq!(budget.external, 4_500);
    assert_eq!(budget.total, 52_630);
    assert_eq!(config.preflight(input), Err(Error::PreacceptBudgetExceeded));
    assert_eq!(
        Converter::new(config, input).unwrap_err(),
        Error::PreacceptBudgetExceeded
    );
}
#[test]
fn accepted_actual_subset_and_lowest_target_have_explicit_admission() {
    for target in [8_000, 24_000, 48_000] {
        for input in [
            InputFormat {
                rate: 8_000,
                channels: 1,
            },
            InputFormat {
                rate: 16_000,
                channels: 1,
            },
            InputFormat {
                rate: 44_100,
                channels: 1,
            },
            InputFormat {
                rate: 48_000,
                channels: 1,
            }, // Android context-free native default.
            InputFormat {
                rate: 192_000,
                channels: 32,
            },
        ] {
            let config = desktop(target);
            let budget = config.audio_budget(input, 6).unwrap();
            let result = Converter::new(config, input);
            if budget.total <= u64::from(target) {
                assert!(result.is_ok(), "{input:?}, {target}");
            } else {
                assert_eq!(result.unwrap_err(), Error::PreacceptBudgetExceeded);
            }
        }
    }
    let config = desktop(24_000);
    let input = InputFormat {
        rate: 48_000,
        channels: 1,
    };
    assert_eq!(config.audio_budget(input, 6).unwrap().total, 15_830);
    assert!(config.preflight(input).is_ok());
}
#[test]
fn actual_format_reserve_uses_largest_fitting_layout_with_all_credits() {
    for target in [8_000, 24_000, 48_000] {
        for rate in [8_000, 16_000, 44_100, 48_000, 96_000, 192_000] {
            for channels in [1, 2, 6, 32] {
                let mut config = desktop(target);
                config.limits.raw_queue_items = 8;
                let input = InputFormat { rate, channels };
                let slots = config.callback_queue_items(input).unwrap();
                assert!((1..=8).contains(&slots));
                let budget = config.audio_budget(input, 6).unwrap();
                if budget.total > u64::from(target) {
                    assert_eq!(slots, 1);
                    assert_eq!(config.preflight(input), Err(Error::PreacceptBudgetExceeded));
                } else {
                    assert!(config.preflight(input).is_ok());
                    if slots < 8 {
                        let mut larger = config.clone();
                        larger.limits.preaccept_external_chunks = None;
                        larger.limits.raw_queue_items = slots + 1;
                        assert!(larger.audio_budget(input, 6).unwrap().total > u64::from(target));
                    }
                    let raw_frames = 4096 / u64::from(channels) * (slots as u64 + 1);
                    assert_eq!(
                        budget.raw,
                        (raw_frames * u64::from(target)).div_ceil(u64::from(rate))
                    );
                }
            }
        }
    }
    let mut config = desktop(24_000);
    config.limits.raw_queue_items = 8;
    assert_eq!(
        config
            .callback_queue_items(InputFormat {
                rate: 48_000,
                channels: 1
            })
            .unwrap(),
        4
    );
    assert_eq!(
        config
            .callback_queue_items(InputFormat {
                rate: 48_000,
                channels: 2
            })
            .unwrap(),
        8
    );
}

#[test]
fn non_desktop_defaults_preserve_conversion_and_checked_limits() {
    let input = InputFormat {
        rate: 8_000,
        channels: 1,
    };
    assert!(Converter::new(Config::default(), input).is_ok());
    let config = desktop(24_000);
    assert_eq!(
        config.audio_budget(input, usize::MAX),
        Err(Error::InvalidConfig)
    );
    assert_eq!(
        config.audio_budget(
            InputFormat {
                rate: 0,
                channels: 0
            },
            6
        ),
        Err(Error::UnsupportedFormat)
    );
}
#[test]
fn converter_transient_bounds_cover_all_rates_channels_and_flush_phases() {
    for rate in [8_000, 16_000, 44_100, 48_000, 192_000] {
        for target in [8_000, 24_000, 48_000] {
            for channels in [1, 2, 32] {
                for tail in [1, 255, 256, 257, 4095] {
                    let config = desktop(target);
                    let input = InputFormat { rate, channels };
                    let budget = config.audio_budget(input, 6).unwrap();
                    // Inspect the converter's pure capacities even for layouts that admission rejects.
                    let mut unrestricted = config;
                    unrestricted.limits.preaccept_external_chunks = None;
                    let mut converter = Converter::new(unrestricted, input).unwrap();
                    let samples = vec![0_i16; 4096 / usize::from(channels) * usize::from(channels)];
                    let first = converter.push(Samples::I16(&samples)).unwrap();
                    let tail = vec![0_i16; tail / usize::from(channels) * usize::from(channels)];
                    let second = converter.push(Samples::I16(&tail)).unwrap();
                    let flush = converter.finish().unwrap();
                    let scratch = converter.output_capacity_frames();
                    assert!(
                        (scratch + first.len() / 2 + flush.len() / 2) as u64
                            <= budget.converter_output
                    );
                    assert!(
                        (scratch + second.len() / 2 + flush.len() / 2) as u64
                            <= budget.converter_output
                    );
                }
            }
        }
    }
}
