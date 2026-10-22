//! Real-time thread coordinator: audio streams, lock-free queues, plan swapping, disk streaming and garbage collection.

mod load;
mod midi;
mod output;
mod record;
mod transport;

pub use load::{load_audio, resample, LoadError};
pub use midi::{connect_midi, midi_input_ports, MidiError, MidiSender};
pub use output::{start_output, OutputError};
pub use record::{recorder, start_input, InputError, RecordError, RecordInput, Recorder, Take};
pub use transport::Transport;

use std::fmt;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use daw_core::{Command, ProcessContext, Telemetry};
use daw_graph::{ExecutionPlan, LiveNodes, NodeArena, PlanError};
use midi::{midi_queue, MidiReceiver};
use rtrb::{Consumer, Producer, RingBuffer};

/// Plans waiting to be swapped in, and retired plans waiting to be dropped.
const PLAN_QUEUE_CAPACITY: usize = 8;

/// Main-thread side of the engine queues.
pub struct EngineHandle {
    pub commands: Producer<Command>,
    pub telemetry: Consumer<Telemetry>,
    /// Live MIDI input for steps added with `ExecutionPlan::route_midi`. Take it to
    /// hand to `connect_midi` or another MIDI source.
    pub midi: Option<MidiSender>,
    plans: Producer<Box<ExecutionPlan>>,
    garbage: Consumer<Box<ExecutionPlan>>,
    /// Nodes the audio thread will hold once every queued plan is applied.
    live: LiveNodes,
    playhead: Arc<AtomicU64>,
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

    /// The playhead at the start of the block now rendering, updated every callback.
    /// Other real-time threads (the input callback) read it without a queue.
    pub fn playhead_clock(&self) -> Arc<AtomicU64> {
        self.playhead.clone()
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
    midi: MidiReceiver,
    plan: Option<Box<ExecutionPlan>>,
    nodes: NodeArena,
    transport: Transport,
    playhead: Arc<AtomicU64>,
    sample_rate: f32,
}

/// Creates both ends of the engine queues. The command and telemetry queues each
/// hold `capacity` messages.
pub fn engine(capacity: usize, sample_rate: f32) -> (EngineHandle, AudioEngine) {
    let (command_tx, command_rx) = RingBuffer::new(capacity);
    let (telemetry_tx, telemetry_rx) = RingBuffer::new(capacity);
    let (plan_tx, plan_rx) = RingBuffer::new(PLAN_QUEUE_CAPACITY);
    let (garbage_tx, garbage_rx) = RingBuffer::new(PLAN_QUEUE_CAPACITY);
    let (midi_tx, midi_rx) = midi_queue();
    let playhead = Arc::new(AtomicU64::new(0));
    (
        EngineHandle {
            commands: command_tx,
            telemetry: telemetry_rx,
            midi: Some(midi_tx),
            plans: plan_tx,
            garbage: garbage_rx,
            live: LiveNodes::default(),
            playhead: playhead.clone(),
        },
        AudioEngine {
            commands: command_rx,
            telemetry: telemetry_tx,
            plans: plan_rx,
            garbage: garbage_tx,
            midi: midi_rx,
            plan: None,
            nodes: NodeArena::default(),
            transport: Transport::default(),
            playhead,
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
        self.playhead
            .store(self.transport.playhead(), Ordering::Relaxed);
        let mut midi = self.midi.drain(output.len() / channels, self.sample_rate);
        match self.plan.as_deref_mut() {
            None => {
                output.fill(0.0);
                self.transport.advance((output.len() / channels) as u64);
            }
            Some(plan) => {
                // The device may hand us more frames than the plan's buffers hold.
                for (i, chunk) in output.chunks_mut(plan.max_block() * channels).enumerate() {
                    let frames = chunk.len() / channels;
                    // Events are in offset order: take this chunk's, made chunk-relative.
                    let first = (i * plan.max_block()) as u32;
                    let end = first + frames as u32;
                    let count = midi.iter().take_while(|e| e.sample_offset < end).count();
                    let (now, later) = mem::take(&mut midi).split_at_mut(count);
                    for e in now.iter_mut() {
                        e.sample_offset -= first;
                    }
                    midi = later;
                    plan.process(
                        &mut self.nodes,
                        &ProcessContext {
                            sample_rate: self.sample_rate,
                            num_samples: frames,
                            playhead: self.transport.playhead(),
                            playing: self.transport.is_playing(),
                        },
                        now,
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
    use daw_graph::{AudioNode, GainNode, Source, SynthNode, TrackNode};
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

    /// Instrument track: a synth taking live MIDI, feeding a track, as the master.
    fn instrument_plan(max_block: usize) -> Box<ExecutionPlan> {
        let mut plan = ExecutionPlan::new(max_block);
        plan.insert(NodeId(0), Box::new(SynthNode::new()));
        plan.insert(
            NodeId(1),
            Box::new(TrackNode::new(
                vec![],
                Arc::new(TrackParams::new(Arc::default())),
            )),
        );
        let synth = plan.add(NodeId(0), vec![], 1).unwrap();
        plan.route_midi(synth);
        let mono = vec![Source {
            step: synth,
            channel: 0,
        }];
        plan.add(NodeId(1), vec![mono], 2).unwrap();
        Box::new(plan)
    }

    #[test]
    fn live_midi_plays_an_instrument_track_without_allocating() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        handle.send_plan(instrument_plan(64)).unwrap();
        let mut block = vec![0.0; 2 * 1024]; // 16 chunks of 64 frames
        audio.process(&mut block, 2); // sets the MIDI clock reference
        assert!(block.iter().all(|&v| v == 0.0));

        let midi = handle.midi.as_mut().unwrap();
        assert!(midi.send(&[0x90, 69, 127]));
        assert_no_alloc(|| audio.process(&mut block, 2));
        // Stopped transport: live instruments still play.
        assert!(block.iter().any(|v| v.abs() > 0.01));
        assert!(block.iter().all(|v| v.is_finite()));

        assert!(handle.midi.as_mut().unwrap().send(&[0x80, 69, 0]));
        assert_no_alloc(|| {
            for _ in 0..8 {
                audio.process(&mut block, 2); // past the release
            }
        });
        assert!(block.iter().all(|&v| v == 0.0));
    }

    /// `(call index, chunk-relative offset)` of each MIDI event, packed as
    /// `call << 32 | offset` in lock-free slots (a Mutex would allocate on macOS).
    #[derive(Default)]
    struct Seen {
        len: AtomicUsize,
        slots: [AtomicU64; 16],
    }

    impl Seen {
        fn list(&self) -> Vec<(u64, u64)> {
            let n = self.len.load(Ordering::SeqCst);
            let slot = |s: &AtomicU64| {
                (
                    s.load(Ordering::SeqCst) >> 32,
                    s.load(Ordering::SeqCst) & 0xFFFF_FFFF,
                )
            };
            self.slots[..n].iter().map(slot).collect()
        }
    }

    /// Records every MIDI event it receives into `Seen`.
    struct MidiProbe {
        calls: u64,
        seen: Arc<Seen>,
    }

    impl AudioNode for MidiProbe {
        fn process(
            &mut self,
            _inputs: &[&[f32]],
            _outputs: &mut [&mut [f32]],
            midi_in: &[daw_core::MidiEvent],
            _midi_out: &mut Vec<daw_core::MidiEvent>,
            _context: &ProcessContext,
        ) {
            for e in midi_in {
                let i = self.seen.len.fetch_add(1, Ordering::SeqCst);
                let packed = self.calls << 32 | u64::from(e.sample_offset);
                self.seen.slots[i].store(packed, Ordering::SeqCst);
            }
            self.calls += 1;
        }
        fn reset(&mut self) {}
    }

    fn midi_probe_plan(seen: &Arc<Seen>) -> Box<ExecutionPlan> {
        let mut plan = ExecutionPlan::new(64);
        let node = MidiProbe {
            calls: 0,
            seen: seen.clone(),
        };
        plan.insert(NodeId(0), Box::new(node));
        let step = plan.add(NodeId(0), vec![], 1).unwrap();
        plan.route_midi(step);
        Box::new(plan)
    }

    #[test]
    fn midi_events_land_in_their_chunk_at_a_chunk_relative_offset() {
        let (mut handle, mut audio) = engine(16, 1_000.0); // 1 frame = 1 ms
        let seen = Arc::<Seen>::default();
        handle.send_plan(midi_probe_plan(&seen)).unwrap();
        audio.midi.set_reference(0);
        let tx = handle.midi.as_mut().unwrap();
        // 200 frames in chunks of 64: [0,64) [64,128) [128,192) [192,200).
        for frame in [5, 63, 64, 130, 199] {
            tx.send_at(frame * 1_000_000, [0x90, 60, 100]);
        }

        let mut block = vec![0.0; 200];
        assert_no_alloc(|| audio.process(&mut block, 1));

        assert_eq!(seen.list(), [(0, 5), (0, 63), (1, 0), (2, 2), (3, 7)]);
    }

    #[test]
    fn midi_without_a_plan_is_discarded() {
        let (mut handle, mut audio) = engine(16, 1_000.0);
        let seen = Arc::<Seen>::default();
        audio.midi.set_reference(0);
        let tx = handle.midi.as_mut().unwrap();
        tx.send_at(5_000_000, [0x90, 60, 100]);
        let mut block = vec![1.0; 64];
        assert_no_alloc(|| audio.process(&mut block, 1));
        assert!(block.iter().all(|&v| v == 0.0));

        // The stale event is gone, not replayed into the plan that arrives later.
        handle.send_plan(midi_probe_plan(&seen)).unwrap();
        audio.process(&mut block, 1);
        assert!(seen.list().is_empty());
    }

    #[test]
    fn engine_publishes_the_playhead_for_other_threads() {
        let (mut handle, mut audio) = engine(16, 48_000.0);
        let clock = handle.playhead_clock();
        handle.commands.push(Command::Seek(500)).unwrap();
        handle.commands.push(Command::Play).unwrap();
        let mut block = [0.0; 128];
        audio.process(&mut block, 2);
        assert_eq!(clock.load(Ordering::Relaxed), 500, "start of the block");
        audio.process(&mut block, 2);
        assert_eq!(clock.load(Ordering::Relaxed), 564);
    }
}
