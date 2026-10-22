//! Real-time thread coordinator: audio streams, lock-free queues, plan swapping, disk streaming and garbage collection.

mod load;
mod output;
mod transport;

pub use load::{load_audio, LoadError};
pub use output::{start_output, OutputError};
pub use transport::Transport;

use daw_core::{Command, ProcessContext, Telemetry};
use daw_graph::ExecutionPlan;
use rtrb::{Consumer, Producer, RingBuffer};

/// Plans waiting to be swapped in, and retired plans waiting to be dropped.
const PLAN_QUEUE_CAPACITY: usize = 8;

/// Main-thread side of the engine queues.
pub struct EngineHandle {
    pub commands: Producer<Command>,
    pub telemetry: Consumer<Telemetry>,
    /// New plans for the audio thread, swapped in at the start of a block.
    pub plans: Producer<Box<ExecutionPlan>>,
    garbage: Consumer<Box<ExecutionPlan>>,
}

impl EngineHandle {
    /// Drops plans the audio thread has retired, freeing their nodes, buffers and
    /// clip audio. Call it regularly from a non-real-time thread.
    pub fn collect_garbage(&mut self) {
        while self.garbage.pop().is_ok() {}
    }
}

/// Audio-thread side. `process` never allocates, locks or blocks.
pub struct AudioEngine {
    commands: Consumer<Command>,
    telemetry: Producer<Telemetry>,
    plans: Consumer<Box<ExecutionPlan>>,
    garbage: Producer<Box<ExecutionPlan>>,
    plan: Option<Box<ExecutionPlan>>,
    transport: Transport,
    sample_rate: f32,
}

/// Creates both ends of the engine queues. The command and telemetry queues each
/// hold `capacity` messages.
pub fn engine(capacity: usize, sample_rate: f32) -> (EngineHandle, AudioEngine) {
    let (command_tx, command_rx) = RingBuffer::new(capacity);
    let (telemetry_tx, telemetry_rx) = RingBuffer::new(capacity);
    let (plan_tx, plan_rx) = RingBuffer::new(PLAN_QUEUE_CAPACITY);
    let (garbage_tx, garbage_rx) = RingBuffer::new(PLAN_QUEUE_CAPACITY);
    (
        EngineHandle {
            commands: command_tx,
            telemetry: telemetry_rx,
            plans: plan_tx,
            garbage: garbage_rx,
        },
        AudioEngine {
            commands: command_rx,
            telemetry: telemetry_tx,
            plans: plan_rx,
            garbage: garbage_tx,
            plan: None,
            transport: Transport::default(),
            sample_rate,
        },
    )
}

impl AudioEngine {
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Renders one block of interleaved audio with `channels` channels.
    /// Commands and plans queued before the call take effect at the start of the block.
    pub fn process(&mut self, output: &mut [f32], channels: usize) {
        while let Ok(command) = self.commands.pop() {
            self.transport.apply(command);
        }

        // Retire the old plan before taking a new one; if the garbage queue is full
        // the new plan waits, so nothing is ever dropped on this thread.
        while !self.garbage.is_full() {
            let Ok(plan) = self.plans.pop() else { break };
            if let Some(old) = self.plan.replace(plan) {
                let _ = self.garbage.push(old); // cannot fail: checked not full
            }
        }

        let channels = channels.max(1);
        match self.plan.as_deref_mut() {
            None => {
                output.fill(0.0);
                self.transport.advance((output.len() / channels) as u64);
            }
            Some(plan) => {
                // The device may hand us more frames than the plan's buffers hold.
                for chunk in output.chunks_mut(plan.max_block() * channels) {
                    let frames = chunk.len() / channels;
                    plan.process(&ProcessContext {
                        sample_rate: self.sample_rate,
                        num_samples: frames,
                        playhead: self.transport.playhead(),
                        playing: self.transport.is_playing(),
                    });
                    let master = plan.output();
                    for (frame, out) in chunk.chunks_exact_mut(channels).enumerate() {
                        for (ch, sample) in out.iter_mut().enumerate() {
                            *sample = master.get(ch).map_or(0.0, |m| m[frame]);
                        }
                    }
                    self.transport.advance(frames as u64);
                }
            }
        }

        // A full queue means the main thread is behind; dropping one update is fine.
        let _ = self.telemetry.push(Telemetry::Playhead {
            sample: self.transport.playhead(),
            playing: self.transport.is_playing(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::{assert_no_alloc, AllocDisabler};
    use daw_core::{Clip, SampleData, TimeSignature, TrackParams};
    use daw_graph::{GainNode, Source, TrackNode};
    use std::sync::Arc;

    #[global_allocator]
    static ALLOCATOR: AllocDisabler = AllocDisabler;

    #[test]
    fn commands_reach_transport_and_telemetry_comes_back() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        handle.commands.push(Command::SetTempo(90.0)).unwrap();
        handle.commands.push(Command::Play).unwrap();

        let mut block = [1.0; 256]; // 128 stereo frames of stale data
        audio.process(&mut block, 2);

        assert_eq!(block, [0.0; 256]);
        assert_eq!(audio.transport().bpm(), 90.0);
        assert_eq!(
            handle.telemetry.pop(),
            Ok(Telemetry::Playhead {
                sample: 128,
                playing: true
            })
        );
    }

    #[test]
    fn full_telemetry_queue_does_not_block() {
        let (mut handle, mut audio) = engine(1, 48_000.0);
        handle.commands.push(Command::Play).unwrap();
        let mut block = [0.0; 64];
        audio.process(&mut block, 1);
        audio.process(&mut block, 1); // queue already full

        assert_eq!(audio.transport().playhead(), 128);
        assert_eq!(
            handle.telemetry.pop(),
            Ok(Telemetry::Playhead {
                sample: 64,
                playing: true
            })
        );
        assert!(handle.telemetry.pop().is_err());
    }

    #[test]
    fn process_does_not_allocate() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let mut block = vec![0.0; 1024];
        for command in [
            Command::Play,
            Command::Seek(1_000),
            Command::SetTempo(128.0),
            Command::SetTimeSignature(TimeSignature {
                numerator: 7,
                denominator: 8,
            }),
        ] {
            handle.commands.push(command).unwrap();
        }

        assert_no_alloc(|| {
            audio.process(&mut block, 2);
            audio.process(&mut block, 2);
        });

        assert_eq!(audio.transport().playhead(), 1_000 + 1024);
    }

    /// Mono clip of ones at frame 0, panned hard left, through a unity master:
    /// left output is 1.0 while the clip plays, right is 0.0.
    fn plan(max_block: usize, clip_frames: usize) -> Box<ExecutionPlan> {
        let params = TrackParams::new(Arc::default());
        params.set_pan(-1.0);
        let audio = SampleData::new(48_000, vec![vec![1.0; clip_frames]]).unwrap();
        let clip = Clip {
            start_sample: 0,
            audio: Arc::new(audio),
        };
        let mut plan = ExecutionPlan::new(max_block);
        let track = plan
            .add(
                Box::new(TrackNode::new(vec![clip], Arc::new(params))),
                vec![],
                2,
            )
            .unwrap();
        let inputs = (0..2)
            .map(|channel| {
                vec![Source {
                    step: track,
                    channel,
                }]
            })
            .collect();
        plan.add(Box::new(GainNode { gain: 1.0 }), inputs, 2)
            .unwrap();
        Box::new(plan)
    }

    #[test]
    fn plan_renders_in_chunks_and_playhead_advances_fully() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        handle.plans.push(plan(64, 300)).ok().unwrap();
        handle.commands.push(Command::Play).unwrap();

        let mut block = vec![9.0; 500 * 2]; // 500 frames: 7 full chunks of 64 plus 52
        audio.process(&mut block, 2);

        for (frame, out) in block.chunks_exact(2).enumerate() {
            let want = if frame < 300 { 1.0 } else { 0.0 };
            assert_eq!(out, [want, 0.0], "frame {frame}");
        }
        assert_eq!(audio.transport().playhead(), 500);
    }

    #[test]
    fn retired_plan_reaches_garbage_only_after_swap() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let mut block = [0.0; 16];
        handle.plans.push(plan(8, 4)).ok().unwrap();
        audio.process(&mut block, 2);
        handle.plans.push(plan(8, 4)).ok().unwrap();
        assert!(handle.garbage.is_empty());

        audio.process(&mut block, 2);
        assert_eq!(handle.garbage.slots(), 1);
        handle.collect_garbage();
        assert!(handle.garbage.is_empty());
    }

    #[test]
    fn process_with_swapped_in_playing_plan_does_not_allocate() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let mut block = vec![0.0; 1024];
        handle.plans.push(plan(64, 4096)).ok().unwrap();
        handle.commands.push(Command::Play).unwrap();
        handle.plans.push(plan(64, 4096)).ok().unwrap();

        assert_no_alloc(|| {
            audio.process(&mut block, 2); // swaps both plans in, retiring the first
            audio.process(&mut block, 2);
        });

        assert_eq!(block[0], 1.0);
        assert_eq!(audio.transport().playhead(), 1024);
    }
}
