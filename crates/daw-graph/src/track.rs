use std::any::Any;
use std::f32::consts::FRAC_PI_4;
use std::mem;
use std::sync::Arc;

use daw_core::{Clip, MidiEvent, ProcessContext, TrackParams};
use daw_dsp::Smoother;

use crate::AudioNode;

/// How long gain, pan and mute changes take to glide, in seconds.
const SMOOTHING_SECONDS: f32 = 0.005;

/// A stereo audio track: plays its clips at the transport position, then applies
/// smoothed gain, equal-power pan, mute and solo from its shared `TrackParams`.
///
/// Its clip list is replaced with `plan.update(id, TrackNode::clips_update(clips))`.
///
/// Writes output channels 0 and 1. Mono clips play on both; any further output
/// channels are zeroed.
pub struct TrackNode {
    clips: Box<[Clip]>,
    params: Arc<TrackParams>,
    left: Smoother,
    right: Smoother,
}

impl TrackNode {
    /// Built on the main thread. Starts settled at the current parameter values so a
    /// rebuilt plan does not fade the track in.
    pub fn new(clips: Vec<Clip>, params: Arc<TrackParams>) -> Self {
        let (left, right) = target_gains(&params);
        Self {
            clips: clips.into_boxed_slice(),
            params,
            left: Smoother::new(left),
            right: Smoother::new(right),
        }
    }
}

impl TrackNode {
    /// Payload for `ExecutionPlan::update` that replaces a live track's clips.
    pub fn clips_update(clips: Vec<Clip>) -> Box<dyn Any + Send> {
        Box::new(clips.into_boxed_slice())
    }
}

/// Per-channel gains from the params: equal-power pan, zero when inaudible.
fn target_gains(params: &TrackParams) -> (f32, f32) {
    if !params.is_audible() {
        return (0.0, 0.0);
    }
    let angle = (params.pan() + 1.0) * FRAC_PI_4;
    let gain = params.gain();
    (gain * angle.cos(), gain * angle.sin())
}

impl AudioNode for TrackNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        for output in outputs.iter_mut() {
            output.fill(0.0);
        }

        if context.playing {
            let block_start = context.playhead;
            let block_end = block_start + context.num_samples as u64;
            for clip in self.clips.iter() {
                let start = block_start.max(clip.start_sample);
                let end = block_end.min(clip.end_sample());
                if start >= end {
                    continue;
                }
                let out_range = (start - block_start) as usize..(end - block_start) as usize;
                let clip_range =
                    (start - clip.start_sample) as usize..(end - clip.start_sample) as usize;
                let last_channel = clip.audio.num_channels() - 1;
                for (ch, output) in outputs.iter_mut().take(2).enumerate() {
                    let from = &clip.audio.channel(ch.min(last_channel))[clip_range.clone()];
                    for (o, s) in output[out_range.clone()].iter_mut().zip(from) {
                        *o += s;
                    }
                }
            }
        }

        let (left, right) = target_gains(&self.params);
        self.left.set_time(SMOOTHING_SECONDS, context.sample_rate);
        self.right.set_time(SMOOTHING_SECONDS, context.sample_rate);
        self.left.set_target(left);
        self.right.set_target(right);
        match outputs {
            [l, r, ..] => {
                for (l, r) in l.iter_mut().zip(r.iter_mut()) {
                    *l *= self.left.tick();
                    *r *= self.right.tick();
                }
            }
            [mono] => {
                for s in mono.iter_mut() {
                    *s *= self.left.tick();
                    self.right.tick();
                }
            }
            [] => {}
        }
    }

    fn exchange(&mut self, data: &mut (dyn Any + Send)) {
        if let Some(clips) = data.downcast_mut::<Box<[Clip]>>() {
            mem::swap(&mut self.clips, clips);
        }
    }

    fn reset(&mut self) {
        let (left, right) = target_gains(&self.params);
        self.left = Smoother::new(left);
        self.right = Smoother::new(right);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use daw_core::SampleData;

    use super::*;

    fn params(pan: f32) -> Arc<TrackParams> {
        let p = TrackParams::new(Arc::<AtomicUsize>::default());
        p.set_pan(pan);
        Arc::new(p)
    }

    /// Mono clip of `frames` samples, all `value`, starting at `start`.
    fn clip(start: u64, frames: usize, value: f32) -> Clip {
        let audio = SampleData::new(48_000, vec![vec![value; frames]]).unwrap();
        Clip {
            start_sample: start,
            audio: Arc::new(audio),
        }
    }

    fn run(node: &mut TrackNode, playhead: u64, n: usize, playing: bool) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
        node.process(
            &[],
            &mut [&mut l[..], &mut r[..]],
            &[],
            &mut Vec::new(),
            &ProcessContext {
                sample_rate: 48_000.0,
                num_samples: n,
                playhead,
                playing,
            },
        );
        (l, r)
    }

    #[test]
    fn clips_are_placed_by_block_range() {
        // pan -1 gives gain exactly 1.0 left, 0.0 right.
        let mut node = TrackNode::new(vec![clip(104, 4, 1.0)], params(-1.0));
        // block [100, 110): starts mid-block, ends mid-block
        assert_eq!(
            run(&mut node, 100, 10, true).0,
            [0., 0., 0., 0., 1., 1., 1., 1., 0., 0.]
        );
        // block [108, 118): clip ends exactly where the block starts
        assert_eq!(run(&mut node, 108, 10, true).0, [0.0; 10]);
        // block [90, 100): entirely before the clip
        assert_eq!(run(&mut node, 90, 10, true).0, [0.0; 10]);
        // block [106, 112): the clip's tail (frames 2..4) lands at the block start
        assert_eq!(run(&mut node, 106, 6, true).0, [1., 1., 0., 0., 0., 0.]);
    }

    #[test]
    fn overlapping_clips_sum_and_mono_feeds_both_channels() {
        let mut node = TrackNode::new(vec![clip(0, 4, 0.5), clip(2, 4, 0.25)], params(0.0));
        let (l, r) = run(&mut node, 0, 6, true);
        let g = FRAC_PI_4.cos();
        let want = [0.5, 0.5, 0.75, 0.75, 0.25, 0.25].map(|v| v * g);
        for i in 0..6 {
            assert!(
                (l[i] - want[i]).abs() < 1e-6 && (r[i] - want[i]).abs() < 1e-6,
                "frame {i}"
            );
        }
    }

    #[test]
    fn stopped_transport_is_silent() {
        let mut node = TrackNode::new(vec![clip(0, 16, 1.0)], params(0.0));
        let (l, r) = run(&mut node, 0, 8, false);
        assert_eq!((l, r), (vec![0.0; 8], vec![0.0; 8]));
    }

    #[test]
    fn hard_left_pan_silences_right_once_settled() {
        let p = params(0.0);
        let mut node = TrackNode::new(vec![clip(0, 8192, 1.0)], p.clone());
        p.set_pan(-1.0);
        let (l, r) = run(&mut node, 0, 8192, true);
        assert!(r[0] > 0.1, "right should glide, not jump");
        assert_eq!(r[8191], 0.0);
        assert!((l[8191] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn mute_ramps_down_without_a_jump() {
        let p = params(-1.0);
        let mut node = TrackNode::new(vec![clip(0, 8192, 1.0)], p.clone());
        p.set_mute(true);
        let (l, _) = run(&mut node, 0, 8192, true);
        assert!(l[0] < 1.0 && l[0] > 0.9, "first sample {}", l[0]);
        assert!(l.windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(l[8191], 0.0);
    }
}
