//! The `AudioNode` trait, the `ExecutionPlan` executor and PDC compiler, and the
//! built-in nodes.

mod effect;
mod monitor;
mod plan;
mod poly;
mod preview;
mod sampler;
mod sequence;
mod synth;
mod track;

pub use effect::{EffectNode, ParamBank};
pub use monitor::{MasterNode, Monitor, MonitorMode, Readings};
pub use plan::{
    ExecutionPlan, LiveNodes, NodeArena, PlanError, Source, StepId, Tap, MAX_CHANNELS, MAX_LATENCY,
    MAX_NODES,
};
pub use poly::{PolySynthNode, SYNTH_PARAMS};
pub use preview::{Preview, PreviewNode};
pub use sampler::{SamplerData, SamplerNode, Zone, SAMPLER_PARAMS};
pub use synth::SynthNode;
pub use track::TrackNode;

use std::any::Any;

use daw_core::{MidiEvent, ProcessContext};

pub trait AudioNode: Send {
    /// Buffer contract:
    /// - The executor slices every input and output to exactly `context.num_samples`
    ///   frames before calling `process`. Nodes may rely on this.
    /// - The compiler matches port layouts: mono is upmixed by passing the same
    ///   buffer twice, or by inserting an adapter node.
    /// - The executor pre-zeroes summing buses and any buffer passed to an untrusted
    ///   third-party plugin.
    /// - The node must `fill(0.0)` every output channel it does not actively write.
    ///   Scratch buffers are reused across the schedule, so anything left unwritten
    ///   is stale audio.
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        midi_out: &mut Vec<MidiEvent>, // Pre-allocated capacity!
        context: &ProcessContext,
    );
    fn latency_samples(&self) -> u32 {
        0
    }
    fn reset(&mut self);
    /// Swaps in data the main thread sent with `ExecutionPlan::update`, such as a
    /// track's clip list, leaving the node's old data in `data` so it is dropped off
    /// the audio thread. Runs on the audio thread: never allocate or drop here.
    /// Data of a type the node does not expect is ignored.
    fn exchange(&mut self, _data: &mut (dyn Any + Send)) {}
    /// Sets parameter `id` from automation, at the start of the block about to be
    /// processed (`ExecutionPlan::automate` calls it every block). Ids are the
    /// node's own (a plugin's parameter ids; `TrackNode::GAIN`/`PAN`). Runs on
    /// the audio thread: never allocate, lock or block. Unknown ids are ignored.
    fn automate(&mut self, _id: u32, _value: f64) {}
}

/// Reports a step's peak to a track's params every block: the track meter,
/// taken after the track's effects.
pub struct PeakTap {
    pub params: std::sync::Arc<daw_core::TrackParams>,
}

impl Tap for PeakTap {
    fn write(&mut self, left: &[f32], right: &[f32], _context: &ProcessContext) {
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        self.params.report_peak([peak(left), peak(right)]);
    }
}

/// A send: stereo through a level the main thread sets (`f32` bits, linear),
/// glided over 20 ms so moving it does not click.
pub struct SendNode {
    level: std::sync::Arc<std::sync::atomic::AtomicU32>,
    smoother: daw_dsp::Smoother,
}

impl SendNode {
    /// Starts settled at the level's current value.
    pub fn new(level: std::sync::Arc<std::sync::atomic::AtomicU32>) -> Self {
        let now = f32::from_bits(level.load(std::sync::atomic::Ordering::Relaxed));
        Self {
            level,
            smoother: daw_dsp::Smoother::new(now),
        }
    }
}

impl AudioNode for SendNode {
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let target = f32::from_bits(self.level.load(std::sync::atomic::Ordering::Relaxed));
        self.smoother.set_time(0.02, context.sample_rate);
        self.smoother
            .set_target(if target.is_finite() { target } else { 0.0 });
        let [l, r, rest @ ..] = outputs else {
            outputs.iter_mut().for_each(|o| o.fill(0.0));
            return;
        };
        rest.iter_mut().for_each(|o| o.fill(0.0));
        let (il, ir) = match inputs {
            [a, b, ..] => (*a, *b),
            [a] => (*a, *a),
            [] => {
                l.fill(0.0);
                r.fill(0.0);
                return;
            }
        };
        for i in 0..l.len() {
            let g = self.smoother.tick();
            l[i] = il[i] * g;
            r[i] = ir[i] * g;
        }
    }

    fn reset(&mut self) {}
}

/// Multiplies each input channel by a fixed linear gain into the matching output
/// channel. Output channels with no matching input are zeroed.
pub struct GainNode {
    pub gain: f32,
}

impl AudioNode for GainNode {
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        _context: &ProcessContext,
    ) {
        for (ch, output) in outputs.iter_mut().enumerate() {
            match inputs.get(ch) {
                Some(input) => {
                    for (o, i) in output.iter_mut().zip(input.iter()) {
                        *o = i * self.gain;
                    }
                }
                None => output.fill(0.0),
            }
        }
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(num_samples: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples,
            playhead: 0,
            playing: false,
            ..Default::default()
        }
    }

    #[test]
    fn gain_node_scales_every_sample() {
        let mut node = GainNode { gain: 0.5 };
        let input = [1.0, -2.0, 0.25, 0.0];
        let mut output = [9.0; 4];

        node.process(&[&input], &mut [&mut output], &[], &mut Vec::new(), &ctx(4));

        assert_eq!(output, [0.5, -1.0, 0.125, 0.0]);
        assert_eq!(node.latency_samples(), 0);
    }

    #[test]
    fn gain_node_zeroes_outputs_without_matching_input() {
        let mut node = GainNode { gain: 2.0 };
        let input = [1.0, 0.5];
        let mut left = [9.0; 2];
        let mut right = [9.0; 2]; // stale scratch data

        node.process(
            &[&input],
            &mut [&mut left, &mut right],
            &[],
            &mut Vec::new(),
            &ctx(2),
        );

        assert_eq!(left, [2.0, 1.0]);
        assert_eq!(right, [0.0, 0.0]);
    }
}
