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
    Playhead {
        sample: u64,
        playing: bool,
    },
    /// Master output levels for the block, left and right, as linear amplitude.
    Levels {
        peak: [f32; 2],
        rms: [f32; 2],
    },
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
    /// Returns `None` if there are no channels or frames, or the channels differ in length.
    pub fn new(sample_rate: u32, channels: Vec<Vec<f32>>) -> Option<Self> {
        let frames = channels.first()?.len();
        if frames == 0 || channels.iter().any(|c| c.len() != frames) {
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

/// A window onto shared audio, placed on a track's timeline. Edits (trim, split,
/// fades) change only this metadata, never the samples, so every edit can be undone
/// by widening the window again.
///
/// The window always lies inside the audio and holds at least one frame, and the
/// fades always fit inside it, so the audio thread can index without checks failing.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// Timeline position of the clip's first frame, in samples.
    pub start_sample: u64,
    audio: Arc<SampleData>,
    offset: u64,
    length: u64,
    fade_in: u64,
    fade_out: u64,
}

impl Clip {
    /// The whole of `audio` with no fades.
    pub fn new(start_sample: u64, audio: Arc<SampleData>) -> Self {
        Self {
            start_sample,
            length: audio.frames() as u64,
            audio,
            offset: 0,
            fade_in: 0,
            fade_out: 0,
        }
    }

    pub fn audio(&self) -> &Arc<SampleData> {
        &self.audio
    }

    /// First frame of the audio that plays, at `start_sample`.
    pub fn offset_samples(&self) -> u64 {
        self.offset
    }

    pub fn length_samples(&self) -> u64 {
        self.length
    }

    pub fn fade_in_samples(&self) -> u64 {
        self.fade_in
    }

    pub fn fade_out_samples(&self) -> u64 {
        self.fade_out
    }

    /// One past the clip's last frame on the timeline.
    pub fn end_sample(&self) -> u64 {
        self.start_sample.saturating_add(self.length)
    }

    /// Moves the left edge to timeline position `at`, keeping the audio where it is
    /// on the timeline. Clamped to the start of the audio and to one frame before the
    /// right edge.
    pub fn trim_start(&mut self, at: u64) {
        let earliest = self.start_sample - self.offset.min(self.start_sample);
        let at = at.clamp(earliest, self.end_sample() - 1);
        if at >= self.start_sample {
            let d = at - self.start_sample;
            self.offset += d;
            self.length -= d;
        } else {
            let d = self.start_sample - at;
            self.offset -= d;
            self.length += d;
        }
        self.start_sample = at;
        self.set_fades(self.fade_in, self.fade_out);
    }

    /// Moves the right edge to timeline position `at`. Clamped to one frame after the
    /// left edge and to the end of the audio.
    pub fn trim_end(&mut self, at: u64) {
        let latest = self.start_sample + (self.audio.frames() as u64 - self.offset);
        self.length = at.clamp(self.start_sample + 1, latest) - self.start_sample;
        self.set_fades(self.fade_in, self.fade_out);
    }

    /// Two clips that meet at timeline position `at`: the left keeps the fade in, the
    /// right keeps the fade out. `None` unless `at` is strictly inside the clip.
    pub fn split(&self, at: u64) -> Option<(Clip, Clip)> {
        if at <= self.start_sample || at >= self.end_sample() {
            return None;
        }
        let mut left = self.clone();
        left.fade_out = 0;
        left.trim_end(at);
        let mut right = self.clone();
        right.fade_in = 0;
        right.trim_start(at);
        Some((left, right))
    }

    /// Fade lengths in samples, shortened so that together they fit in the clip.
    pub fn set_fades(&mut self, fade_in: u64, fade_out: u64) {
        self.fade_in = fade_in.min(self.length);
        self.fade_out = fade_out.min(self.length - self.fade_in);
    }

    /// Fades `left` out and `right` in across the stretch where they overlap, so they
    /// can sit on one track and blend with constant power. Returns false (and changes
    /// nothing) unless `right` starts inside `left` and ends after it.
    pub fn crossfade(left: &mut Clip, right: &mut Clip) -> bool {
        let overlaps = left.start_sample < right.start_sample
            && right.start_sample < left.end_sample()
            && left.end_sample() <= right.end_sample();
        if !overlaps {
            return false;
        }
        let overlap = left.end_sample() - right.start_sample;
        left.set_fades(left.fade_in.min(left.length - overlap), overlap);
        right.set_fades(overlap, right.fade_out.min(right.length - overlap));
        true
    }

    /// Fade gain at `frame` frames into the clip: equal-power (sine) curves, so a fade
    /// out and a fade in of the same length sum to constant power. 1.0 between the
    /// fades, 0.0 past the end.
    #[inline]
    pub fn gain_at(&self, frame: u64) -> f32 {
        use std::f64::consts::FRAC_PI_2;
        if frame >= self.length {
            0.0
        } else if frame < self.fade_in {
            ((frame as f64 + 0.5) / self.fade_in as f64 * FRAC_PI_2).sin() as f32
        } else if self.fade_out > 0 && frame >= self.length - self.fade_out {
            let left = (self.length - frame) as f64;
            ((left - 0.5) / self.fade_out as f64 * FRAC_PI_2).sin() as f32
        } else {
            1.0
        }
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
        assert!(SampleData::new(48_000, vec![vec![]]).is_none());
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

    /// Clip over 100 frames of audio where frame i holds i.
    fn ramp_clip(start: u64) -> Clip {
        let audio = SampleData::new(48_000, vec![(0..100).map(|i| i as f32).collect()]).unwrap();
        Clip::new(start, Arc::new(audio))
    }

    #[test]
    fn trims_move_the_window_and_clamp_to_the_audio() {
        let mut c = ramp_clip(1_000);
        c.trim_start(1_010);
        assert_eq!(
            (c.start_sample, c.offset_samples(), c.length_samples()),
            (1_010, 10, 90)
        );
        c.trim_start(0); // can only widen back to the audio's first frame
        assert_eq!(
            (c.start_sample, c.offset_samples(), c.length_samples()),
            (1_000, 0, 100)
        );
        c.trim_start(5_000); // keeps one frame
        assert_eq!((c.start_sample, c.length_samples()), (1_099, 1));
        c.trim_end(9_999); // only as far as the audio goes
        assert_eq!((c.offset_samples(), c.end_sample()), (99, 1_100));
        c.trim_end(0);
        assert_eq!(c.length_samples(), 1);

        // a clip near timeline 0 cannot widen past it
        let mut c = ramp_clip(0);
        c.trim_start(50);
        c.start_sample = 20;
        c.trim_start(0);
        assert_eq!(
            (c.start_sample, c.offset_samples(), c.length_samples()),
            (0, 30, 70)
        );
    }

    #[test]
    fn split_halves_meet_and_keep_their_outer_fades() {
        let mut c = ramp_clip(1_000);
        c.set_fades(10, 20);
        assert!(c.split(1_000).is_none() && c.split(1_100).is_none());
        let (l, r) = c.split(1_040).unwrap();
        assert_eq!(
            (l.start_sample, l.end_sample(), l.offset_samples()),
            (1_000, 1_040, 0)
        );
        assert_eq!(
            (r.start_sample, r.end_sample(), r.offset_samples()),
            (1_040, 1_100, 40)
        );
        assert_eq!((l.fade_in_samples(), l.fade_out_samples()), (10, 0));
        assert_eq!((r.fade_in_samples(), r.fade_out_samples()), (0, 20));
        assert!(
            Arc::ptr_eq(l.audio(), r.audio()),
            "split must not copy audio"
        );
    }

    #[test]
    fn fades_fit_and_crossfade_is_equal_power() {
        let mut c = ramp_clip(0);
        c.set_fades(80, 80);
        assert_eq!((c.fade_in_samples(), c.fade_out_samples()), (80, 20));
        c.trim_end(50);
        assert_eq!((c.fade_in_samples(), c.fade_out_samples()), (50, 0));

        let mut a = ramp_clip(0);
        let mut b = ramp_clip(60);
        assert!(!Clip::crossfade(&mut b, &mut a), "order matters");
        assert!(Clip::crossfade(&mut a, &mut b));
        assert_eq!((a.fade_out_samples(), b.fade_in_samples()), (40, 40));
        for t in 60..100 {
            let (ga, gb) = (a.gain_at(t), b.gain_at(t - 60));
            assert!((ga * ga + gb * gb - 1.0).abs() < 1e-5, "power at {t}");
        }
        assert_eq!((a.gain_at(59), b.gain_at(40)), (1.0, 1.0));
        assert!(a.gain_at(99) < 0.05 && b.gain_at(0) < 0.05);
        assert_eq!((a.gain_at(100), a.gain_at(u64::MAX)), (0.0, 0.0));
    }

    /// Tiny xorshift so the property test is deterministic without a dependency.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn check_invariants(c: &Clip, what: &str) {
        let frames = c.audio().frames() as u64;
        assert!(c.length_samples() >= 1, "{what}: empty clip");
        assert!(
            c.offset_samples() + c.length_samples() <= frames,
            "{what}: window past audio"
        );
        assert!(
            c.fade_in_samples() + c.fade_out_samples() <= c.length_samples(),
            "{what}: fades"
        );
        for f in 0..c.length_samples() {
            let g = c.gain_at(f);
            assert!(
                g.is_finite() && (0.0..=1.0).contains(&g),
                "{what}: gain {g} at {f}"
            );
        }
    }

    #[test]
    fn random_edit_sequences_keep_clip_invariants() {
        let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..300 {
            let mut clips = [
                ramp_clip(next(&mut rng) % 150),
                ramp_clip(next(&mut rng) % 150),
            ];
            for step in 0..40 {
                let i = (next(&mut rng) % 2) as usize;
                let at = next(&mut rng) % 400; // often outside the clip
                let (a, b) = (next(&mut rng) % 120, next(&mut rng) % 120);
                let what = format!("step {step} op on clip {i} at {at}");
                match next(&mut rng) % 6 {
                    0 => clips[i].trim_start(at),
                    1 => clips[i].trim_end(at),
                    2 => {
                        if let Some((l, r)) = clips[i].split(at) {
                            check_invariants(&r, &what);
                            clips[i] = l;
                        }
                    }
                    3 => clips[i].set_fades(a, b),
                    4 => clips[i].start_sample = at,
                    _ => {
                        let [x, y] = &mut clips;
                        let before = (x.clone(), y.clone());
                        if !Clip::crossfade(x, y) {
                            assert_eq!((x.clone(), y.clone()), before, "failed crossfade mutated");
                        }
                    }
                }
                check_invariants(&clips[0], &what);
                check_invariants(&clips[1], &what);
            }
        }
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
