use crate::{DeviceSelection, Error};

/// Android enumeration is unsupported: CPAL's discovery helpers access the host's Java context.
///
/// # Errors
/// Always returns `UnsupportedDeviceSelection` without querying a host or device.
pub const fn input_devices() -> Result<Vec<String>, Error> {
    Err(Error::UnsupportedDeviceSelection)
}

// The only supplied backend capability is obtaining the default device, not discovery.
pub fn select_input<D>(
    selection: &DeviceSelection,
    default_device: impl FnOnce() -> Option<D>,
) -> Result<(D, cpal::StreamConfig, cpal::SampleFormat), Error> {
    if !matches!(selection, DeviceSelection::Default) {
        return Err(Error::UnsupportedDeviceSelection);
    }
    let device = default_device().ok_or(Error::DeviceNotFound)?;
    Ok((
        device,
        cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        },
        cpal::SampleFormat::I16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn fixed_default_plan_needs_only_default_device() {
        let calls = Cell::new(0);
        let (device, config, samples) = select_input(&DeviceSelection::Default, || {
            calls.set(calls.get() + 1);
            Some(123)
        })
        .unwrap();
        assert_eq!(device, 123);
        assert_eq!(calls.get(), 1);
        assert_eq!(config.channels, 1);
        assert_eq!(config.sample_rate.0, 48_000);
        assert_eq!(config.buffer_size, cpal::BufferSize::Default);
        assert_eq!(samples, cpal::SampleFormat::I16);
    }

    #[test]
    fn selection_and_enumeration_reject_without_backend_access() {
        let result = select_input::<()>(&DeviceSelection::Name("microphone".into()), || {
            panic!("unsupported selection must not access the backend")
        });
        assert!(matches!(result, Err(Error::UnsupportedDeviceSelection)));
        assert_eq!(input_devices(), Err(Error::UnsupportedDeviceSelection));
        let missing = select_input::<()>(&DeviceSelection::Default, || None);
        assert!(matches!(missing, Err(Error::DeviceNotFound)));
    }
}
