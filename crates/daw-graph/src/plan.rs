use std::fmt;

use daw_core::{MidiEvent, ProcessContext};

use crate::AudioNode;

/// Most input or output channels one node can have. The executor builds its
/// per-node slice arrays on the stack at this size, so it never allocates.
pub const MAX_CHANNELS: usize = 8;
/// MIDI events a node may emit per block without the executor allocating.
const MIDI_CAPACITY: usize = 1024;

/// Index of a node in an `ExecutionPlan`, in the order it was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepId(pub usize);

/// One output channel of an earlier node, feeding an input channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    pub step: StepId,
    pub channel: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    TooManyChannels(usize),
    /// A source names a node that is not earlier in the plan, or a channel it lacks.
    BadSource(Source),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyChannels(n) => write!(f, "{n} channels, the limit is {MAX_CHANNELS}"),
            Self::BadSource(s) => write!(
                f,
                "source step {} channel {} is not an earlier node's output",
                s.step.0, s.channel
            ),
        }
    }
}

impl std::error::Error for PlanError {}

struct Step {
    node: Box<dyn AudioNode>,
    /// `sources[ch]` are summed into input channel `ch`.
    sources: Vec<Vec<Source>>,
    inputs: Vec<Box<[f32]>>,
    outputs: Vec<Box<[f32]>>,
}

/// A compiled, topologically ordered schedule of nodes with every buffer allocated
/// up front. Built on the main thread, then handed to the audio thread whole.
///
/// Nodes may only read from nodes added before them, so the plan is acyclic by
/// construction. The last node added is the plan's output (the master bus).
pub struct ExecutionPlan {
    steps: Vec<Step>,
    max_block: usize,
    midi_out: Vec<MidiEvent>,
}

impl ExecutionPlan {
    /// `max_block` is the most frames `process` will ever be asked for.
    /// Panics on zero, here on the main thread, so the audio thread never sees it.
    pub fn new(max_block: usize) -> Self {
        assert!(max_block > 0, "plan max_block must be at least 1");
        Self {
            steps: Vec::new(),
            max_block,
            midi_out: Vec::with_capacity(MIDI_CAPACITY),
        }
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// Appends `node`. `inputs[ch]` lists the earlier outputs summed into input
    /// channel `ch`; an empty list means that input is silence.
    pub fn add(
        &mut self,
        node: Box<dyn AudioNode>,
        inputs: Vec<Vec<Source>>,
        num_outputs: usize,
    ) -> Result<StepId, PlanError> {
        for n in [inputs.len(), num_outputs] {
            if n > MAX_CHANNELS {
                return Err(PlanError::TooManyChannels(n));
            }
        }
        for &source in inputs.iter().flatten() {
            let valid = self
                .steps
                .get(source.step.0)
                .is_some_and(|s| source.channel < s.outputs.len());
            if !valid {
                return Err(PlanError::BadSource(source));
            }
        }
        let buffer = || vec![0.0; self.max_block].into_boxed_slice();
        self.steps.push(Step {
            node,
            inputs: (0..inputs.len()).map(|_| buffer()).collect(),
            outputs: (0..num_outputs).map(|_| buffer()).collect(),
            sources: inputs,
        });
        Ok(StepId(self.steps.len() - 1))
    }

    /// The last node's output buffers, valid for `num_samples` frames after `process`.
    pub fn output(&self) -> &[Box<[f32]>] {
        self.steps.last().map_or(&[], |s| &s.outputs)
    }

    /// Runs every node once. Never allocates, locks or blocks.
    /// `context.num_samples` must not exceed `max_block`.
    pub fn process(&mut self, context: &ProcessContext) {
        let n = context.num_samples;
        assert!(n <= self.max_block, "block of {n} frames exceeds plan max");

        for i in 0..self.steps.len() {
            let (done, rest) = self.steps.split_at_mut(i);
            let Step {
                node,
                sources,
                inputs,
                outputs,
            } = &mut rest[0];

            // Inputs are summing buses, so they start from silence.
            for (input, sources) in inputs.iter_mut().zip(sources.iter()) {
                let input = &mut input[..n];
                input.fill(0.0);
                for source in sources {
                    let from = &done[source.step.0].outputs[source.channel][..n];
                    for (i, s) in input.iter_mut().zip(from) {
                        *i += s;
                    }
                }
            }

            let (num_inputs, num_outputs) = (inputs.len(), outputs.len());
            let mut ins: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            for (slot, buffer) in ins.iter_mut().zip(inputs.iter()) {
                *slot = &buffer[..n];
            }
            let mut outs: [&mut [f32]; MAX_CHANNELS] = Default::default();
            for (slot, buffer) in outs.iter_mut().zip(outputs.iter_mut()) {
                *slot = &mut buffer[..n];
            }

            self.midi_out.clear();
            node.process(
                &ins[..num_inputs],
                &mut outs[..num_outputs],
                &[],
                &mut self.midi_out,
                context,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GainNode;

    /// Writes `values[call]` (then 0.0) to every output frame, one value per block.
    struct Script {
        values: Vec<f32>,
        call: usize,
    }

    impl AudioNode for Script {
        fn process(
            &mut self,
            _inputs: &[&[f32]],
            outputs: &mut [&mut [f32]],
            _midi_in: &[MidiEvent],
            _midi_out: &mut Vec<MidiEvent>,
            _context: &ProcessContext,
        ) {
            let v = self.values.get(self.call).copied().unwrap_or(0.0);
            self.call += 1;
            for o in outputs.iter_mut() {
                o.fill(v);
            }
        }
        fn reset(&mut self) {}
    }

    fn script(values: &[f32]) -> Box<Script> {
        Box::new(Script {
            values: values.to_vec(),
            call: 0,
        })
    }

    fn src(step: StepId) -> Source {
        Source { step, channel: 0 }
    }

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: n,
            playhead: 0,
            playing: true,
        }
    }

    #[test]
    fn sources_are_summed_and_input_does_not_keep_stale_data() {
        let mut plan = ExecutionPlan::new(4);
        let a = plan.add(script(&[1.0, 0.0]), vec![], 1).unwrap();
        let b = plan.add(script(&[2.0, 0.0]), vec![], 1).unwrap();
        let gain = Box::new(GainNode { gain: 1.0 });
        plan.add(gain, vec![vec![src(a), src(b)]], 1).unwrap();

        plan.process(&ctx(4));
        assert_eq!(&*plan.output()[0], &[3.0; 4]);
        plan.process(&ctx(4));
        assert_eq!(&*plan.output()[0], &[0.0; 4]);
    }

    #[test]
    fn add_rejects_bad_sources_and_too_many_channels() {
        let mut plan = ExecutionPlan::new(4);
        let a = plan.add(script(&[]), vec![], 1).unwrap();
        let node = || Box::new(GainNode { gain: 1.0 });

        let later = Source {
            step: StepId(1),
            channel: 0,
        }; // would be this node itself
        assert_eq!(
            plan.add(node(), vec![vec![later]], 1),
            Err(PlanError::BadSource(later))
        );
        let channel = Source {
            step: a,
            channel: 1,
        };
        assert_eq!(
            plan.add(node(), vec![vec![channel]], 1),
            Err(PlanError::BadSource(channel))
        );
        let too_many = MAX_CHANNELS + 1;
        assert_eq!(
            plan.add(node(), vec![], too_many),
            Err(PlanError::TooManyChannels(too_many))
        );
        assert_eq!(
            plan.add(node(), vec![vec![]; too_many], 1),
            Err(PlanError::TooManyChannels(too_many))
        );
        assert!(plan.add(node(), vec![vec![src(a)]], MAX_CHANNELS).is_ok());
    }

    #[test]
    fn short_block_only_touches_first_frames() {
        let mut plan = ExecutionPlan::new(8);
        plan.add(script(&[1.0, 2.0]), vec![], 1).unwrap();
        plan.process(&ctx(8));
        plan.process(&ctx(3));
        assert_eq!(
            &*plan.output()[0],
            &[2.0, 2.0, 2.0, 1.0, 1.0, 1.0, 1.0, 1.0]
        );
    }

    #[test]
    #[should_panic(expected = "exceeds plan max")]
    fn oversized_block_panics() {
        let mut plan = ExecutionPlan::new(4);
        plan.add(script(&[]), vec![], 1).unwrap();
        plan.process(&ctx(5));
    }
}
