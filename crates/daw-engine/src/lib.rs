//! Real-time thread coordinator: audio streams, lock-free queues, plan swapping, disk streaming and garbage collection.

mod load;
mod output;
mod transport;

pub use load::{load_audio, resample, LoadError};
pub use output::{start_output, OutputError};
pub use transport::Transport;

use std::fmt;

use daw_core::{Command, ProcessContext, Telemetry};
use daw_graph::{ExecutionPlan, LiveNodes, NodeArena, PlanError};
use rtrb::{Consumer, Producer, RingBuffer};

/// Plans waiting to be swapped in, and retired plans waiting to be dropped.
const PLAN_QUEUE_CAPACITY: usize = 8;

/// Main-thread side of the engine queues.
pub struct EngineHandle {
    pub commands: Producer<Command>,
    pub telemetry: Consumer<Telemetry>,
    plans: Producer<Box<ExecutionPlan>>,
    garbage: Consumer<Box<ExecutionPlan>>,
    /// Nodes the audio thread will hold once every queued plan is applied.
    live: LiveNodes,
}

#[derive(Debug)]
pub enum SendPlanError {
    Invalid(PlanError),
    /// The plan queue is full; the plan is handed back unsent.
    QueueFull(Box<ExecutionPlan>),
}

impl fmt::Display for SendPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "invalid plan: {e}"),
            Self::QueueFull(_) => write!(f, "plan queue is full"),
        }
    }
}

impl std::error::Error for SendPlanError {}

impl EngineHandle {
    /// Checks `plan` against the nodes the audio thread will have, then queues it to
    /// be swapped in at the start of a block.
    pub fn send_plan(&mut self, plan: Box<ExecutionPlan>) -> Result<(), SendPlanError> {
        let next = self.live.check(&plan).map_err(SendPlanError::Invalid)?;
        self.plans
            .push(plan)
            .map_err(|rtrb::PushError::Full(plan)| SendPlanError::QueueFull(plan))?;
        self.live = next;
        Ok(())
    }

    pub fn live_nodes(&self) -> &LiveNodes {
        &self.live
    }

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
    nodes: NodeArena,
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
            live: LiveNodes::default(),
        },
        AudioEngine {
            commands: command_rx,
            telemetry: telemetry_tx,
            plans: plan_rx,
            garbage: garbage_tx,
            plan: None,
            nodes: NodeArena::default(),
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
        // the new plan waits, so nothing is ever dropped on this thread. Nodes the
        // new plan removes ride out with the old plan.
        while !self.garbage.is_full() {
            let Ok(mut plan) = self.plans.pop() else {
                break;
            };
            self.nodes.apply(&mut plan);
            if let Some(old) = self.plan.as_deref_mut() {
                plan.hand_retired_to(old);
            }
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
                    plan.process(
                        &mut self.nodes,
                        &ProcessContext {
                            sample_rate: self.sample_rate,
                            num_samples: frames,
                            playhead: self.transport.playhead(),
                            playing: self.transport.is_playing(),
                        },
                    );
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
    use daw_core::{Clip, NodeId, SampleData, TimeSignature, TrackParams};
    use daw_graph::{AudioNode, GainNode, Source, TrackNode};
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        let clip = Clip::new(0, Arc::new(audio));
        let (track, master) = (NodeId(0), NodeId(1));
        let mut plan = ExecutionPlan::new(max_block);
        plan.insert(
            track,
            Box::new(TrackNode::new(vec![clip], Arc::new(params))),
        );
        plan.insert(master, Box::new(GainNode { gain: 1.0 }));
        let track_step = plan.add(track, vec![], 2).unwrap();
        let inputs = (0..2)
            .map(|channel| {
                vec![Source {
                    step: track_step,
                    channel,
                }]
            })
            .collect();
        plan.add(master, inputs, 2).unwrap();
        Box::new(plan)
    }

    #[test]
    fn plan_renders_in_chunks_and_playhead_advances_fully() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        handle.plans.push(plan(64, 300)).ok().unwrap();
        handle.commands.push(Command::Play).unwrap();

        let mut block = vec![9.0; 500 * 2]; // 500 frames: 7 full chunks of 64 plus 52
        audio.process(&mut block, 2);

        for (frame, out) in block.as_chunks::<2>().0.iter().enumerate() {
            let want = if frame < 300 { 1.0 } else { 0.0 };
            assert_eq!(*out, [want, 0.0], "frame {frame}");
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

    #[test]
    fn playing_trimmed_crossfaded_clips_does_not_allocate() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let src = Arc::new(SampleData::new(48_000, vec![vec![1.0; 4096]]).unwrap());
        let mut a = Clip::new(0, src.clone());
        let mut b = Clip::new(1_500, src);
        a.trim_end(2_000);
        b.trim_start(1_600);
        b.trim_end(3_800);
        assert!(Clip::crossfade(&mut a, &mut b));
        let params = TrackParams::new(Arc::default());
        params.set_pan(-1.0);
        let mut p = ExecutionPlan::new(64);
        p.insert(
            NodeId(0),
            Box::new(TrackNode::new(vec![a, b], Arc::new(params))),
        );
        let step = p.add(NodeId(0), vec![], 2).unwrap();
        p.insert(NodeId(1), Box::new(GainNode { gain: 1.0 }));
        let inputs = (0..2)
            .map(|channel| vec![Source { step, channel }])
            .collect();
        p.add(NodeId(1), inputs, 2).unwrap();
        handle.plans.push(Box::new(p)).ok().unwrap();
        handle.commands.push(Command::Play).unwrap();

        let mut block = vec![0.0; 2 * 1024];
        assert_no_alloc(|| {
            for _ in 0..4 {
                audio.process(&mut block, 2); // crosses the fades and both clip edges
            }
        });
        assert_eq!(audio.transport().playhead(), 4096);
        assert!(block.iter().all(|v| v.is_finite()));
    }

    /// Writes its call count to output 0 and counts its own drops.
    struct Probe {
        calls: f32,
        drops: Arc<AtomicUsize>,
    }

    impl AudioNode for Probe {
        fn process(
            &mut self,
            _inputs: &[&[f32]],
            outputs: &mut [&mut [f32]],
            _midi_in: &[daw_core::MidiEvent],
            _midi_out: &mut Vec<daw_core::MidiEvent>,
            _context: &ProcessContext,
        ) {
            self.calls += 1.0;
            outputs[0].fill(self.calls);
        }
        fn reset(&mut self) {}
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn probe(drops: &Arc<AtomicUsize>) -> Box<Probe> {
        Box::new(Probe {
            calls: 0.0,
            drops: drops.clone(),
        })
    }

    /// Schedules the already-live node `id` as the plan's only step.
    fn reschedule(mut plan: ExecutionPlan, id: u32) -> Box<ExecutionPlan> {
        plan.add(NodeId(id), vec![], 1).unwrap();
        Box::new(plan)
    }

    /// Runs one 8-frame mono block and returns its first sample.
    fn first_sample(audio: &mut AudioEngine) -> f32 {
        let mut block = [0.0; 8];
        audio.process(&mut block, 1);
        block[0]
    }

    #[test]
    fn node_state_survives_a_plan_that_only_reschedules_it() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let drops = Arc::default();
        let mut first = ExecutionPlan::new(8);
        first.insert(NodeId(0), probe(&drops));
        handle.send_plan(reschedule(first, 0)).unwrap();
        assert_eq!(first_sample(&mut audio), 1.0);
        assert_eq!(first_sample(&mut audio), 2.0);

        handle
            .send_plan(reschedule(ExecutionPlan::new(8), 0))
            .unwrap();
        assert_eq!(first_sample(&mut audio), 3.0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn removed_node_drops_only_through_garbage() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let drops = Arc::default();
        let mut first = ExecutionPlan::new(8);
        first.insert(NodeId(0), probe(&drops));
        first.insert(NodeId(1), probe(&drops));
        handle.send_plan(reschedule(first, 0)).unwrap();
        first_sample(&mut audio);

        let mut second = ExecutionPlan::new(8);
        second.remove(NodeId(1));
        handle.send_plan(reschedule(second, 0)).unwrap();
        assert!(!handle.live_nodes().is_live(NodeId(1)));
        first_sample(&mut audio); // swap: the first plan carries node 1 to garbage
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(handle.garbage.slots(), 1);

        handle.collect_garbage();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// Clip of ones-times-`value` and a stereo playing engine with it on a track.
    fn clip(value: f32) -> (Clip, Arc<SampleData>) {
        let audio = Arc::new(SampleData::new(48_000, vec![vec![value; 64]]).unwrap());
        let clip = Clip::new(0, audio.clone());
        (clip, audio)
    }

    #[test]
    fn update_swaps_track_clips_and_old_audio_is_freed_by_garbage() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let (old, old_audio) = clip(1.0);
        let params = TrackParams::new(Arc::default());
        params.set_pan(-1.0);
        let mut first = ExecutionPlan::new(8);
        first.insert(
            NodeId(0),
            Box::new(TrackNode::new(vec![old], Arc::new(params))),
        );
        handle.send_plan(reschedule(first, 0)).unwrap();
        handle.commands.push(Command::Play).unwrap();
        let left = |audio: &mut AudioEngine| {
            let mut block = [0.0; 16];
            audio.process(&mut block, 2);
            block[0]
        };
        assert_eq!(left(&mut audio), 1.0);
        assert_eq!(Arc::strong_count(&old_audio), 2);

        let (new, _new_audio) = clip(0.5);
        let mut second = ExecutionPlan::new(8);
        second.update(NodeId(0), TrackNode::clips_update(vec![new]));
        handle.send_plan(reschedule(second, 0)).unwrap();
        handle.commands.push(Command::Seek(0)).unwrap();
        assert_eq!(left(&mut audio), 0.5);
        // The old clips now sit in the live plan's update, not dropped.
        assert_eq!(Arc::strong_count(&old_audio), 2);

        handle
            .send_plan(reschedule(ExecutionPlan::new(8), 0))
            .unwrap();
        left(&mut audio); // retires the second plan, which holds the old clips
        assert_eq!(Arc::strong_count(&old_audio), 2);
        handle.collect_garbage();
        assert_eq!(Arc::strong_count(&old_audio), 1);
    }

    #[test]
    fn send_plan_rejects_invalid_and_hands_back_when_full() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let drops = Arc::default();

        let mut bad = ExecutionPlan::new(8);
        bad.remove(NodeId(0));
        assert!(matches!(
            handle.send_plan(Box::new(bad)),
            Err(SendPlanError::Invalid(PlanError::UnknownNode(NodeId(0))))
        ));
        assert!(handle.plans.slots() == PLAN_QUEUE_CAPACITY);

        let mut first = ExecutionPlan::new(8);
        first.insert(NodeId(0), probe(&drops));
        handle.send_plan(Box::new(first)).unwrap();
        for _ in 1..PLAN_QUEUE_CAPACITY {
            handle.send_plan(Box::new(ExecutionPlan::new(8))).unwrap();
        }
        let mut late = ExecutionPlan::new(8);
        late.insert(NodeId(1), probe(&drops));
        let Err(SendPlanError::QueueFull(late)) = handle.send_plan(Box::new(late)) else {
            panic!("queue should be full");
        };
        assert!(!handle.live_nodes().is_live(NodeId(1)));
        assert!(handle.live_nodes().is_live(NodeId(0)));

        first_sample(&mut audio); // drains the queue
        handle.send_plan(late).unwrap();
        assert!(handle.live_nodes().is_live(NodeId(1)));
    }

    #[test]
    fn swap_that_inserts_removes_and_updates_does_not_allocate() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let drops = Arc::default();
        let (old, _old_audio) = clip(1.0);
        let (new, _new_audio) = clip(0.5);
        let mut first = ExecutionPlan::new(64);
        first.insert(
            NodeId(0),
            Box::new(TrackNode::new(
                vec![old],
                Arc::new(TrackParams::new(Arc::default())),
            )),
        );
        first.insert(NodeId(1), probe(&drops));
        handle.send_plan(reschedule(first, 0)).unwrap();
        handle.commands.push(Command::Play).unwrap();
        let mut block = vec![0.0; 1024];
        audio.process(&mut block, 2);

        let mut second = ExecutionPlan::new(64);
        second.remove(NodeId(1));
        second.insert(NodeId(2), Box::new(GainNode { gain: 1.0 }));
        second.update(NodeId(0), TrackNode::clips_update(vec![new]));
        handle.send_plan(reschedule(second, 0)).unwrap();

        assert_no_alloc(|| {
            audio.process(&mut block, 2); // applies every change and retires the first plan
            audio.process(&mut block, 2);
        });
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        handle.collect_garbage();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
