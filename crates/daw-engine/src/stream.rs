//! What output and input streams share: picking a device config at a wanted
//! rate, and reporting stream errors to the main thread without printing.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use cpal::{ErrorKind, SampleFormat, SupportedStreamConfig, SupportedStreamConfigRange};

/// The device's own config, unless `rate` is wanted and the device offers it in
/// f32 (same channel count preferred): then that, so the device switches rate
/// instead of Canary refusing the project or the input.
pub fn pick_config(
    default: SupportedStreamConfig,
    ranges: impl IntoIterator<Item = SupportedStreamConfigRange>,
    rate: Option<u32>,
) -> SupportedStreamConfig {
    let Some(rate) = rate.filter(|&r| r != default.sample_rate()) else {
        return default;
    };
    let fits: Vec<SupportedStreamConfig> = ranges
        .into_iter()
        .filter(|r| r.sample_format() == SampleFormat::F32)
        .filter_map(|r| r.try_with_sample_rate(rate))
        .collect();
    let same = fits.iter().position(|c| c.channels() == default.channels());
    match same.or((!fits.is_empty()).then_some(0)) {
        Some(i) => fits[i],
        None => default,
    }
}

/// A stream problem the app should show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The device went away or its config changed: the stream no longer runs.
    Lost,
    /// Audio dropped out (an xrun): the CPU or a plug-in missed the deadline.
    Dropout,
    /// Anything else the device reported.
    Other,
}

const LOST: u32 = 1;
const DROPOUT: u32 = 2;
const OTHER: u32 = 4;

/// Faults seen since the main thread last looked. The error callback may run
/// on the real-time thread (cpal reports xruns there), so it only sets a bit.
#[derive(Clone, Default)]
pub struct StreamFaults(Arc<AtomicU32>);

impl StreamFaults {
    /// For a stream's error callback: lock-free, no allocation, no printing.
    pub fn record(&self, e: &cpal::Error) {
        let bit = match e.kind() {
            ErrorKind::DeviceNotAvailable
            | ErrorKind::StreamInvalidated
            | ErrorKind::HostUnavailable => LOST,
            ErrorKind::Xrun => DROPOUT,
            // The stream followed the new default device by itself.
            ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied => 0,
            _ => OTHER,
        };
        self.0.fetch_or(bit, Ordering::Relaxed);
    }

    /// The worst fault since the last call, and clears them.
    pub fn take(&self) -> Option<Fault> {
        let bits = self.0.swap(0, Ordering::Relaxed);
        [
            (LOST, Fault::Lost),
            (OTHER, Fault::Other),
            (DROPOUT, Fault::Dropout),
        ]
        .into_iter()
        .find(|&(b, _)| bits & b != 0)
        .map(|(_, f)| f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::SupportedBufferSize;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, min, max, SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn picks_the_wanted_rate_when_the_device_has_it() {
        let default = range(2, 44_100, 44_100, SampleFormat::F32).with_sample_rate(44_100);
        let ranges = [
            range(2, 44_100, 96_000, SampleFormat::I16),
            range(4, 44_100, 96_000, SampleFormat::F32),
            range(2, 44_100, 96_000, SampleFormat::F32),
        ];
        let c = pick_config(default, ranges, Some(48_000));
        assert_eq!(
            (c.sample_rate(), c.channels()),
            (48_000, 2),
            "f32, same channels"
        );
        let c = pick_config(default, ranges, Some(192_000));
        assert_eq!(c.sample_rate(), 44_100, "not offered: the default");
        assert_eq!(pick_config(default, ranges, None), default);
    }

    #[test]
    fn faults_report_the_worst_once() {
        let f = StreamFaults::default();
        assert_eq!(f.take(), None);
        f.record(&cpal::Error::new(ErrorKind::Xrun));
        f.record(&cpal::Error::new(ErrorKind::DeviceNotAvailable));
        f.record(&cpal::Error::new(ErrorKind::DeviceChanged));
        assert_eq!(f.take(), Some(Fault::Lost));
        assert_eq!(f.take(), None);
    }
}
