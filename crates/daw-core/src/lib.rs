//! Pure data types shared by every other crate. No heavy dependencies.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

/// Identifies a node in the audio graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// A raw MIDI message, timestamped within the current block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    /// Offset in samples from the start of the block.
    pub sample_offset: u32,
    /// Status byte followed by up to two data bytes.
    pub data: [u8; 3],
}

/// Per-block information passed to every `AudioNode::process` call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessContext {
    pub sample_rate: f32,
    /// Frames in this tick. Every buffer passed to `process` is exactly this long;
    /// it can vary per callback, up to the plan's max block size.
    pub num_samples: usize,
    /// Transport position in samples at the first frame of this tick.
    pub playhead: u64,
    pub playing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u8,
    pub denominator: u8,
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self {
            numerator: 4,
            denominator: 4,
        }
    }
}

/// Main thread → audio thread. Applied at the start of the next block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Play,
    Stop,
    /// Move the playhead to an absolute sample position.
    Seek(u64),
    SetTempo(f64),
    SetTimeSignature(TimeSignature),
}

/// Audio thread → main thread. Sent once per block; dropped if the queue is full.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Telemetry {
    Playhead { sample: u64, playing: bool },
}

/// Decoded audio, one plane per channel. Built on the main thread and shared with
/// the audio thread as `Arc<SampleData>`, so it is never copied or freed in a callback.
#[derive(Debug, PartialEq)]
pub struct SampleData {
    sample_rate: u32,
    frames: usize,
    channels: Box<[Box<[f32]>]>,
}

impl SampleData {
    /// Returns `None` if there are no channels or the channels differ in length.
    pub fn new(sample_rate: u32, channels: Vec<Vec<f32>>) -> Option<Self> {
        let frames = channels.first()?.len();
        if channels.iter().any(|c| c.len() != frames) {
            return None;
        }
        Some(Self {
            sample_rate,
            frames,
            channels: channels.into_iter().map(Vec::into_boxed_slice).collect(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn channel(&self, index: usize) -> &[f32] {
        &self.channels[index]
    }
}

/// A piece of audio placed on a track's timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// Timeline position of the clip's first frame, in samples.
    pub start_sample: u64,
    pub audio: Arc<SampleData>,
}

impl Clip {
    /// One past the clip's last frame on the timeline.
    pub fn end_sample(&self) -> u64 {
        self.start_sample + self.audio.frames() as u64
    }
}

/// Mixer settings for one track. The main thread writes them and the audio thread
/// reads them through atomics, so neither side ever waits on the other.
#[derive(Debug)]
pub struct TrackParams {
    gain: AtomicU32,
    pan: AtomicU32,
    mute: AtomicBool,
    solo: AtomicBool,
    /// Shared by every track in a mix: how many of them are soloed.
    solo_count: Arc<AtomicUsize>,
}

impl TrackParams {
    /// Unity gain, centred, unmuted. Tracks that should solo against each other
    /// must share one `solo_count`.
    pub fn new(solo_count: Arc<AtomicUsize>) -> Self {
        Self {
            gain: AtomicU32::new(1.0f32.to_bits()),
            pan: AtomicU32::new(0.0f32.to_bits()),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            solo_count,
        }
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    /// Linear gain. Negative or non-finite values are ignored.
    pub fn set_gain(&self, gain: f32) {
        if gain.is_finite() && gain >= 0.0 {
            self.gain.store(gain.to_bits(), Ordering::Relaxed);
        }
    }

    /// -1.0 is hard left, 1.0 hard right.
    pub fn pan(&self) -> f32 {
        f32::from_bits(self.pan.load(Ordering::Relaxed))
    }

    /// Clamped to -1.0..=1.0. Non-finite values are ignored.
    pub fn set_pan(&self, pan: f32) {
        if pan.is_finite() {
            self.pan
                .store(pan.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn is_muted(&self) -> bool {
        self.mute.load(Ordering::Relaxed)
    }

    pub fn set_mute(&self, mute: bool) {
        self.mute.store(mute, Ordering::Relaxed);
    }

    pub fn is_soloed(&self) -> bool {
        self.solo.load(Ordering::Relaxed)
    }

    pub fn set_solo(&self, solo: bool) {
        if self.solo.swap(solo, Ordering::Relaxed) != solo {
            if solo {
                self.solo_count.fetch_add(1, Ordering::Relaxed);
            } else {
                self.solo_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// False when muted, or when another track is soloed and this one is not.
    pub fn is_audible(&self) -> bool {
        !self.is_muted() && (self.is_soloed() || self.solo_count.load(Ordering::Relaxed) == 0)
    }
}

impl Drop for TrackParams {
    /// A deleted soloed track must not keep every other track silenced.
    fn drop(&mut self) {
        self.set_solo(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_data_rejects_ragged_or_empty_channels() {
        assert!(SampleData::new(48_000, vec![]).is_none());
        assert!(SampleData::new(48_000, vec![vec![0.0; 3], vec![0.0; 2]]).is_none());
        let data = SampleData::new(48_000, vec![vec![0.0; 3], vec![1.0; 3]]).unwrap();
        assert_eq!((data.frames(), data.num_channels()), (3, 2));
    }

    #[test]
    fn solo_silences_only_unsoloed_tracks() {
        let count = Arc::new(AtomicUsize::new(0));
        let a = TrackParams::new(count.clone());
        let b = TrackParams::new(count);
        assert!(a.is_audible() && b.is_audible());

        a.set_solo(true);
        a.set_solo(true); // repeated solo must not double count
        assert!(a.is_audible() && !b.is_audible());

        a.set_solo(false);
        assert!(a.is_audible() && b.is_audible());

        b.set_solo(true);
        drop(b);
        assert!(
            a.is_audible(),
            "deleted soloed track still silences the mix"
        );
        let b = TrackParams::new(a.solo_count.clone());

        b.set_mute(true);
        assert!(!b.is_audible());
    }

    #[test]
    fn invalid_gain_and_pan_are_ignored() {
        let p = TrackParams::new(Arc::default());
        p.set_gain(0.5);
        p.set_gain(f32::NAN);
        p.set_gain(-1.0);
        p.set_pan(4.0);
        p.set_pan(f32::INFINITY);
        assert_eq!((p.gain(), p.pan()), (0.5, 1.0));
    }
}
