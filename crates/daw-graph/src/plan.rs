use std::any::Any;
use std::fmt;
use std::mem;

use daw_core::{MidiEvent, NodeId, ProcessContext};

use crate::AudioNode;

/// Most input or output channels one node can have. The executor builds its
/// per-node slice arrays on the stack at this size, so it never allocates.
pub const MAX_CHANNELS: usize = 8;
/// Node slots in a `NodeArena`; every `NodeId` must be below this.
pub const MAX_NODES: usize = 4096;
/// MIDI events a node may emit per block without the executor allocating.
const MIDI_CAPACITY: usize = 1024;
/// Most latency one node may report, and the longest compensation delay one edge
/// gets (about 21 s at 48 kHz, 4 MiB). Larger values are clamped, so a plugin
/// reporting garbage cannot exhaust memory.
pub const MAX_LATENCY: u32 = 1 << 20;

/// Index of a step in an `ExecutionPlan`, in the order it was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepId(pub usize);

/// One output channel of an earlier step, feeding an input channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    pub step: StepId,
    pub channel: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    TooManyChannels(usize),
    /// A source names a step that is not earlier in the plan, or a channel it lacks.
    BadSource(Source),
    /// The id is at least `MAX_NODES`.
    IdOutOfRange(NodeId),
    /// Removed, updated or scheduled, but not live once the plan applies.
    UnknownNode(NodeId),
    /// Inserted while live, or listed twice in the same role.
    DuplicateNode(NodeId),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyChannels(n) => write!(f, "{n} channels, the limit is {MAX_CHANNELS}"),
            Self::BadSource(s) => write!(
                f,
                "source step {} channel {} is not an earlier step's output",
                s.step.0, s.channel
            ),
            Self::IdOutOfRange(id) => write!(f, "node {} is past the {MAX_NODES} limit", id.0),
            Self::UnknownNode(id) => write!(f, "node {} is not live", id.0),
            Self::DuplicateNode(id) => write!(f, "node {} is already live or listed twice", id.0),
        }
    }
}

impl std::error::Error for PlanError {}

struct Step {
    node: NodeId,
    /// `sources[ch]` are summed into input channel `ch`.
    sources: Vec<Vec<Source>>,
    inputs: Vec<Box<[f32]>>,
    outputs: Vec<Box<[f32]>>,
    /// Receives the live MIDI input (an instrument); other steps get none.
    takes_midi: bool,
    /// Set by `set_latency`; otherwise read from this plan's inserted node, else 0.
    latency: Option<u32>,
    /// Compensation delays on this step's sources, in (input, source) order.
    delays: Vec<Delay>,
}

/// Delays one source of one input so it lines up with the slowest path into the
/// same step (plugin delay compensation).
struct Delay {
    /// (step node, input, source node, source channel): finds the same edge in the
    /// next plan so the audio in flight carries over.
    key: (NodeId, usize, NodeId, usize),
    input: usize,
    /// Index into `sources[input]`.
    source: usize,
    line: Box<[f32]>,
    pos: usize,
}

impl Delay {
    /// Adds `from`, delayed by the line's length, into `out`.
    fn mix_into(&mut self, out: &mut [f32], from: &[f32]) {
        for (o, &s) in out.iter_mut().zip(from) {
            *o += self.line[self.pos];
            self.line[self.pos] = s;
            self.pos += 1;
            if self.pos == self.line.len() {
                self.pos = 0;
            }
        }
    }
}

/// A topologically ordered schedule with every buffer allocated up front, plus the
/// node changes to make when it goes live. Built on the main thread, then handed to
/// the audio thread whole.
///
/// Nodes themselves live in the audio thread's `NodeArena` and survive from plan to
/// plan, so delay lines, filters and smoothers keep their state across graph edits.
/// Steps may only read from steps added before them, so the plan is acyclic by
/// construction. The last step added is the plan's output (the master bus).
pub struct ExecutionPlan {
    steps: Vec<Step>,
    max_block: usize,
    midi_out: Vec<MidiEvent>,
    inserts: Vec<(NodeId, Option<Box<dyn AudioNode>>)>,
    removals: Vec<NodeId>,
    /// Receives removed nodes on the audio thread; capacity reserved in `remove`.
    retired: Vec<Box<dyn AudioNode>>,
    updates: Vec<(NodeId, Box<dyn Any + Send>)>,
    /// (step, index into its `delays`) of every delay, sorted by `Delay::key`.
    delay_order: Vec<(usize, usize)>,
    latency: u32,
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
            inserts: Vec::new(),
            removals: Vec::new(),
            retired: Vec::new(),
            updates: Vec::new(),
            delay_order: Vec::new(),
            latency: 0,
        }
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// Adds a new node to the arena when this plan goes live.
    pub fn insert(&mut self, id: NodeId, node: Box<dyn AudioNode>) {
        self.inserts.push((id, Some(node)));
    }

    /// Takes a node out of the arena when this plan goes live. It is dropped later,
    /// off the audio thread, with the plan it was running in.
    pub fn remove(&mut self, id: NodeId) {
        self.removals.push(id);
        self.retired.reserve_exact(self.removals.len());
    }

    /// Hands `data` to a live node's `AudioNode::exchange` when this plan goes live.
    pub fn update(&mut self, id: NodeId, data: Box<dyn Any + Send>) {
        self.updates.push((id, data));
    }

    /// Schedules node `id` to run next. `inputs[ch]` lists the earlier outputs summed
    /// into input channel `ch`; an empty list means that input is silence.
    pub fn add(
        &mut self,
        id: NodeId,
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
            node: id,
            inputs: (0..inputs.len()).map(|_| buffer()).collect(),
            outputs: (0..num_outputs).map(|_| buffer()).collect(),
            sources: inputs,
            takes_midi: false,
            latency: None,
            delays: Vec::new(),
        });
        Ok(StepId(self.steps.len() - 1))
    }

    /// Sends the live MIDI input to `step`, such as an instrument. Any number of steps
    /// may take it. Returns false if there is no such step.
    pub fn route_midi(&mut self, step: StepId) -> bool {
        self.steps
            .get_mut(step.0)
            .map(|s| s.takes_midi = true)
            .is_some()
    }

    /// Sets the latency `step`'s node reports. Needed for a node that went live in an
    /// earlier plan, or whose latency changed; nodes inserted by this plan are read
    /// directly. Returns false if there is no such step.
    pub fn set_latency(&mut self, step: StepId, samples: u32) -> bool {
        self.steps
            .get_mut(step.0)
            .map(|s| s.latency = Some(samples))
            .is_some()
    }

    /// The PDC compiler. Walks the schedule, finds when each step's inputs arrive
    /// (the slowest source path), and gives every faster source a delay line so all
    /// of them line up. Run it once the plan is built; `EngineHandle::send_plan`
    /// does. Running it again starts from scratch.
    pub fn compensate(&mut self) {
        let mut inserted = vec![0u32; MAX_NODES];
        for (id, node) in &self.inserts {
            if let (Some(slot), Some(node)) = (inserted.get_mut(id.0 as usize), node) {
                *slot = node.latency_samples();
            }
        }
        let nodes: Vec<NodeId> = self.steps.iter().map(|s| s.node).collect();
        // When each step's output is ready, in samples behind the dry signal.
        let mut ready: Vec<u32> = Vec::with_capacity(self.steps.len());
        self.delay_order.clear();
        for (i, step) in self.steps.iter_mut().enumerate() {
            let arrive = step
                .sources
                .iter()
                .flatten()
                .map(|s| ready[s.step.0])
                .max()
                .unwrap_or(0);
            step.delays.clear();
            for (input, sources) in step.sources.iter().enumerate() {
                for (k, s) in sources.iter().enumerate() {
                    let delay = (arrive - ready[s.step.0]).min(MAX_LATENCY) as usize;
                    if delay > 0 {
                        self.delay_order.push((i, step.delays.len()));
                        step.delays.push(Delay {
                            key: (step.node, input, nodes[s.step.0], s.channel),
                            input,
                            source: k,
                            line: vec![0.0; delay].into_boxed_slice(),
                            pos: 0,
                        });
                    }
                }
            }
            let own = step
                .latency
                .unwrap_or_else(|| inserted.get(step.node.0 as usize).copied().unwrap_or(0));
            ready.push(arrive.saturating_add(own.min(MAX_LATENCY)));
        }
        let steps = &self.steps;
        self.delay_order
            .sort_by_key(|&(i, j)| steps[i].delays[j].key);
        self.latency = ready.last().copied().unwrap_or(0);
    }

    /// How far the output lags the dry signal after `compensate`, in samples.
    pub fn latency(&self) -> u32 {
        self.latency
    }

    /// Takes over the audio in flight in `old`'s compensation delays for every edge
    /// both plans share with the same delay, so a plan swap does not drop it.
    /// Allocation-free; call it on the audio thread when this plan replaces `old`.
    pub fn adopt_delays(&mut self, old: &mut ExecutionPlan) {
        let (mut a, mut b) = (0, 0);
        while a < self.delay_order.len() && b < old.delay_order.len() {
            let (ni, nj) = self.delay_order[a];
            let (oi, oj) = old.delay_order[b];
            let new = &mut self.steps[ni].delays[nj];
            let prev = &mut old.steps[oi].delays[oj];
            match new.key.cmp(&prev.key) {
                std::cmp::Ordering::Less => a += 1,
                std::cmp::Ordering::Greater => b += 1,
                std::cmp::Ordering::Equal => {
                    if new.line.len() == prev.line.len() {
                        mem::swap(&mut new.line, &mut prev.line);
                        new.pos = prev.pos;
                    }
                    a += 1;
                    b += 1;
                }
            }
        }
    }

    /// The last step's output buffers, valid for `num_samples` frames after `process`.
    pub fn output(&self) -> &[Box<[f32]>] {
        self.steps.last().map_or(&[], |s| &s.outputs)
    }

    /// Moves the nodes this plan removed into `old`, so they are dropped with it.
    /// Allocation-free; call it on the audio thread right after `NodeArena::apply`.
    pub fn hand_retired_to(&mut self, old: &mut ExecutionPlan) {
        mem::swap(&mut self.retired, &mut old.retired);
    }

    /// Runs every step once. Never allocates, locks or blocks.
    /// `context.num_samples` must not exceed `max_block`. `midi` is this block's live
    /// input, offsets below `num_samples` and in order; it goes to `route_midi` steps.
    pub fn process(&mut self, arena: &mut NodeArena, context: &ProcessContext, midi: &[MidiEvent]) {
        let n = context.num_samples;
        assert!(n <= self.max_block, "block of {n} frames exceeds plan max");
        debug_assert!(midi.iter().all(|e| (e.sample_offset as usize) < n.max(1)));

        for i in 0..self.steps.len() {
            let (done, rest) = self.steps.split_at_mut(i);
            let Step {
                node,
                sources,
                inputs,
                outputs,
                takes_midi,
                delays,
                ..
            } = &mut rest[0];

            // Inputs are summing buses, so they start from silence.
            let mut delays = delays.iter_mut().peekable();
            for (ch, (input, sources)) in inputs.iter_mut().zip(sources.iter()).enumerate() {
                let input = &mut input[..n];
                input.fill(0.0);
                for (k, source) in sources.iter().enumerate() {
                    let from = &done[source.step.0].outputs[source.channel][..n];
                    match delays.next_if(|d| d.input == ch && d.source == k) {
                        Some(delay) => delay.mix_into(input, from),
                        None => {
                            for (i, s) in input.iter_mut().zip(from) {
                                *i += s;
                            }
                        }
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

            // `LiveNodes::check` guarantees the node exists; stay silent if not.
            let Some(node) = arena.get_mut(*node) else {
                for out in &mut outs[..num_outputs] {
                    out.fill(0.0);
                }
                continue;
            };
            self.midi_out.clear();
            node.process(
                &ins[..num_inputs],
                &mut outs[..num_outputs],
                if *takes_midi { midi } else { &[] },
                &mut self.midi_out,
                context,
            );
        }
    }
}

impl fmt::Debug for ExecutionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionPlan")
            .field("steps", &self.steps.len())
            .field("max_block", &self.max_block)
            .field("inserts", &self.inserts.len())
            .field("removals", &self.removals)
            .field("updates", &self.updates.len())
            .field("latency", &self.latency)
            .finish()
    }
}

/// The audio thread's nodes, indexed by `NodeId`. Allocated once on the main thread.
pub struct NodeArena {
    slots: Box<[Option<Box<dyn AudioNode>>]>,
}

impl Default for NodeArena {
    fn default() -> Self {
        Self {
            slots: (0..MAX_NODES).map(|_| None).collect(),
        }
    }
}

impl NodeArena {
    fn get_mut(&mut self, id: NodeId) -> Option<&mut (dyn AudioNode + 'static)> {
        self.slots.get_mut(id.0 as usize)?.as_deref_mut()
    }

    /// Makes `plan`'s node changes: removals, then inserts, then updates. Never
    /// allocates or drops a node; anything it cannot place stays in the plan.
    pub fn apply(&mut self, plan: &mut ExecutionPlan) {
        for &id in &plan.removals {
            // Capacity was reserved in `remove`, so this push never allocates.
            if plan.retired.len() < plan.retired.capacity() {
                if let Some(node) = self.slots.get_mut(id.0 as usize).and_then(Option::take) {
                    plan.retired.push(node);
                }
            }
        }
        for (id, node) in &mut plan.inserts {
            if let Some(slot @ None) = self.slots.get_mut(id.0 as usize) {
                *slot = node.take();
            }
        }
        for (id, data) in &mut plan.updates {
            if let Some(node) = self.get_mut(*id) {
                node.exchange(data.as_mut());
            }
        }
    }
}

/// The main thread's record of which nodes the audio thread will have once every
/// plan sent so far is applied. Checks a plan against it before sending.
#[derive(Clone)]
pub struct LiveNodes {
    live: Vec<bool>,
}

impl Default for LiveNodes {
    fn default() -> Self {
        Self {
            live: vec![false; MAX_NODES],
        }
    }
}

impl LiveNodes {
    pub fn is_live(&self, id: NodeId) -> bool {
        self.live.get(id.0 as usize).copied().unwrap_or(false)
    }

    /// Validates `plan` and returns the live set after it applies. Keep the old set
    /// until the plan is actually queued.
    pub fn check(&self, plan: &ExecutionPlan) -> Result<LiveNodes, PlanError> {
        let in_range = |id: NodeId| {
            if (id.0 as usize) < MAX_NODES {
                Ok(id.0 as usize)
            } else {
                Err(PlanError::IdOutOfRange(id))
            }
        };
        let mut next = self.clone();
        for &id in &plan.removals {
            let i = in_range(id)?;
            if !next.live[i] {
                return Err(PlanError::UnknownNode(id));
            }
            next.live[i] = false;
        }
        for (id, _) in &plan.inserts {
            let i = in_range(*id)?;
            if next.live[i] {
                return Err(PlanError::DuplicateNode(*id));
            }
            next.live[i] = true;
        }
        for (id, _) in &plan.updates {
            if !next.is_live(*id) {
                return Err(PlanError::UnknownNode(*id));
            }
        }
        let mut scheduled = vec![false; MAX_NODES];
        for step in &plan.steps {
            let i = in_range(step.node)?;
            if !next.live[i] {
                return Err(PlanError::UnknownNode(step.node));
            }
            if mem::replace(&mut scheduled[i], true) {
                return Err(PlanError::DuplicateNode(step.node));
            }
        }
        Ok(next)
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

    /// Inserts `node` under the next free id and schedules it.
    fn add(
        plan: &mut ExecutionPlan,
        node: Box<dyn AudioNode>,
        inputs: Vec<Vec<Source>>,
        num_outputs: usize,
    ) -> Result<StepId, PlanError> {
        let id = NodeId(plan.inserts.len() as u32);
        plan.insert(id, node);
        plan.add(id, inputs, num_outputs)
    }

    /// Applies pending node changes (a no-op after the first call) and runs a block.
    fn run(plan: &mut ExecutionPlan, arena: &mut NodeArena, n: usize) {
        arena.apply(plan);
        plan.process(arena, &ctx(n), &[]);
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
        let mut arena = NodeArena::default();
        let mut plan = ExecutionPlan::new(4);
        let a = add(&mut plan, script(&[1.0, 0.0]), vec![], 1).unwrap();
        let b = add(&mut plan, script(&[2.0, 0.0]), vec![], 1).unwrap();
        let gain = Box::new(GainNode { gain: 1.0 });
        add(&mut plan, gain, vec![vec![src(a), src(b)]], 1).unwrap();

        run(&mut plan, &mut arena, 4);
        assert_eq!(&*plan.output()[0], &[3.0; 4]);
        run(&mut plan, &mut arena, 4);
        assert_eq!(&*plan.output()[0], &[0.0; 4]);
    }

    #[test]
    fn add_rejects_bad_sources_and_too_many_channels() {
        let mut plan = ExecutionPlan::new(4);
        let a = add(&mut plan, script(&[]), vec![], 1).unwrap();
        let node = || Box::new(GainNode { gain: 1.0 });

        let later = Source {
            step: StepId(1),
            channel: 0,
        }; // would be this node itself
        assert_eq!(
            add(&mut plan, node(), vec![vec![later]], 1),
            Err(PlanError::BadSource(later))
        );
        let channel = Source {
            step: a,
            channel: 1,
        };
        assert_eq!(
            add(&mut plan, node(), vec![vec![channel]], 1),
            Err(PlanError::BadSource(channel))
        );
        let too_many = MAX_CHANNELS + 1;
        assert_eq!(
            add(&mut plan, node(), vec![], too_many),
            Err(PlanError::TooManyChannels(too_many))
        );
        assert_eq!(
            add(&mut plan, node(), vec![vec![]; too_many], 1),
            Err(PlanError::TooManyChannels(too_many))
        );
        assert!(add(&mut plan, node(), vec![vec![src(a)]], MAX_CHANNELS).is_ok());
    }

    #[test]
    fn short_block_only_touches_first_frames() {
        let mut arena = NodeArena::default();
        let mut plan = ExecutionPlan::new(8);
        add(&mut plan, script(&[1.0, 2.0]), vec![], 1).unwrap();
        run(&mut plan, &mut arena, 8);
        run(&mut plan, &mut arena, 3);
        assert_eq!(
            &*plan.output()[0],
            &[2.0, 2.0, 2.0, 1.0, 1.0, 1.0, 1.0, 1.0]
        );
    }

    fn gain() -> Box<dyn AudioNode> {
        Box::new(GainNode { gain: 1.0 })
    }

    /// A plan scheduling `id` once.
    fn scheduled(id: u32) -> ExecutionPlan {
        let mut plan = ExecutionPlan::new(4);
        plan.add(NodeId(id), vec![], 1).unwrap();
        plan
    }

    fn live_with(ids: &[u32]) -> LiveNodes {
        let mut plan = ExecutionPlan::new(4);
        for &id in ids {
            plan.insert(NodeId(id), gain());
        }
        LiveNodes::default().check(&plan).unwrap()
    }

    #[test]
    fn check_rejects_bad_node_changes_and_accepts_replace() {
        let live = live_with(&[1]);
        let (one, two) = (NodeId(1), NodeId(2));
        let bad = NodeId(MAX_NODES as u32);

        let mut plan = ExecutionPlan::new(4);
        plan.remove(two);
        assert_eq!(live.check(&plan).err(), Some(PlanError::UnknownNode(two)));

        let mut plan = ExecutionPlan::new(4);
        plan.insert(one, gain());
        assert_eq!(live.check(&plan).err(), Some(PlanError::DuplicateNode(one)));

        let mut plan = ExecutionPlan::new(4);
        plan.update(two, Box::new(()));
        assert_eq!(live.check(&plan).err(), Some(PlanError::UnknownNode(two)));

        assert_eq!(
            live.check(&scheduled(2)).err(),
            Some(PlanError::UnknownNode(two))
        );

        let mut plan = scheduled(1);
        plan.add(one, vec![], 1).unwrap();
        assert_eq!(live.check(&plan).err(), Some(PlanError::DuplicateNode(one)));

        let mut plan = ExecutionPlan::new(4);
        plan.remove(bad);
        assert_eq!(live.check(&plan).err(), Some(PlanError::IdOutOfRange(bad)));
        let mut plan = ExecutionPlan::new(4);
        plan.insert(bad, gain());
        assert_eq!(live.check(&plan).err(), Some(PlanError::IdOutOfRange(bad)));
        assert_eq!(
            live.check(&scheduled(bad.0)).err(),
            Some(PlanError::IdOutOfRange(bad))
        );

        // Remove then insert the same id in one plan, and use the new node.
        let mut plan = scheduled(1);
        plan.remove(one);
        plan.insert(one, gain());
        plan.update(one, Box::new(()));
        assert!(live.check(&plan).unwrap().is_live(one));

        // A node removed in a plan cannot be scheduled by it.
        let mut plan = scheduled(1);
        plan.remove(one);
        assert_eq!(live.check(&plan).err(), Some(PlanError::UnknownNode(one)));
    }

    /// Writes the number of MIDI events it received to every output frame.
    struct MidiCount;

    impl AudioNode for MidiCount {
        fn process(
            &mut self,
            _inputs: &[&[f32]],
            outputs: &mut [&mut [f32]],
            midi_in: &[MidiEvent],
            _midi_out: &mut Vec<MidiEvent>,
            _context: &ProcessContext,
        ) {
            outputs[0].fill(midi_in.len() as f32);
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn live_midi_reaches_only_routed_steps() {
        let mut arena = NodeArena::default();
        let mut plan = ExecutionPlan::new(4);
        let routed = add(&mut plan, Box::new(MidiCount), vec![], 1).unwrap();
        let other = add(&mut plan, Box::new(MidiCount), vec![], 1).unwrap();
        assert!(plan.route_midi(routed));
        assert!(!plan.route_midi(StepId(9)));
        let gain = Box::new(GainNode { gain: 1.0 });
        // 10 x routed count + other count, so both show up in the output.
        add(
            &mut plan,
            gain,
            vec![vec![src(routed); 10], vec![src(other)]],
            2,
        )
        .unwrap();
        arena.apply(&mut plan);
        let note = MidiEvent {
            sample_offset: 1,
            data: [0x90, 60, 100],
        };
        plan.process(&mut arena, &ctx(4), &[note, note]);
        assert_eq!(plan.output()[0][0], 20.0);
        assert_eq!(plan.output()[1][0], 0.0);
    }

    /// Delays its one input by `line.len()` samples and reports that as latency.
    struct Lag {
        line: Vec<f32>,
        pos: usize,
    }

    fn lag(samples: usize) -> Box<Lag> {
        Box::new(Lag {
            line: vec![0.0; samples],
            pos: 0,
        })
    }

    impl AudioNode for Lag {
        fn process(
            &mut self,
            inputs: &[&[f32]],
            outputs: &mut [&mut [f32]],
            _midi_in: &[MidiEvent],
            _midi_out: &mut Vec<MidiEvent>,
            _context: &ProcessContext,
        ) {
            for (o, &i) in outputs[0].iter_mut().zip(inputs[0]) {
                *o = self.line[self.pos];
                self.line[self.pos] = i;
                self.pos = (self.pos + 1) % self.line.len();
            }
        }
        fn latency_samples(&self) -> u32 {
            self.line.len() as u32
        }
        fn reset(&mut self) {}
    }

    /// An impulse into a dry path and a path lagging 3 samples, summed at a bus.
    fn parallel_paths(plan: &mut ExecutionPlan) {
        let src_id = NodeId(0);
        plan.insert(src_id, script(&[1.0]));
        let source = plan.add(src_id, vec![], 1).unwrap();
        plan.insert(NodeId(1), lag(3));
        let wet = plan.add(NodeId(1), vec![vec![src(source)]], 1).unwrap();
        plan.insert(NodeId(2), gain());
        plan.add(NodeId(2), vec![vec![src(source), src(wet)]], 1)
            .unwrap();
    }

    #[test]
    fn compensation_lines_up_parallel_paths() {
        let mut arena = NodeArena::default();
        let mut plan = ExecutionPlan::new(8);
        parallel_paths(&mut plan);
        plan.compensate();
        plan.compensate(); // idempotent
        assert_eq!(plan.latency(), 3);
        run(&mut plan, &mut arena, 8);
        // Script writes 1.0 for the whole first block, so both paths step up at 3.
        assert_eq!(
            &*plan.output()[0],
            &[0.0, 0.0, 0.0, 2.0, 2.0, 2.0, 2.0, 2.0]
        );
    }

    #[test]
    fn without_compensation_paths_are_misaligned() {
        let mut arena = NodeArena::default();
        let mut plan = ExecutionPlan::new(8);
        parallel_paths(&mut plan);
        run(&mut plan, &mut arena, 8);
        assert_eq!(plan.output()[0][0], 1.0);
        assert_eq!(plan.latency(), 0);
    }

    #[test]
    fn set_latency_covers_nodes_from_earlier_plans_and_delays_survive_swaps() {
        let mut arena = NodeArena::default();
        let mut first = ExecutionPlan::new(4);
        parallel_paths(&mut first);
        first.compensate();
        arena.apply(&mut first);
        // Last frame of a block: only one sample in, three still in the delay line.
        first.process(&mut arena, &ctx(4), &[]);
        assert_eq!(&*first.output()[0], &[0.0, 0.0, 0.0, 2.0]);

        // Same graph from live nodes; the lag node's latency must be stated.
        let mut second = ExecutionPlan::new(4);
        let source = second.add(NodeId(0), vec![], 1).unwrap();
        let wet = second.add(NodeId(1), vec![vec![src(source)]], 1).unwrap();
        second
            .add(NodeId(2), vec![vec![src(source), src(wet)]], 1)
            .unwrap();
        second.compensate();
        assert_eq!(second.latency(), 0, "unknown latency counts as 0");
        assert!(second.set_latency(wet, 3));
        assert!(!second.set_latency(StepId(9), 3));
        second.compensate();
        assert_eq!(second.latency(), 3);
        arena.apply(&mut second);
        second.adopt_delays(&mut first);
        // Script now writes 0.0: the dry path's 1.0s still in flight come out
        // alongside the lag node's own tail.
        second.process(&mut arena, &ctx(4), &[]);
        assert_eq!(&*second.output()[0], &[2.0, 2.0, 2.0, 0.0]);
    }

    /// Old plan delays sources {1, 3, 4} into node 9; the new one {2, 3, 4}, with
    /// node 4's delay a different length. Only node 3's edge carries its audio.
    #[test]
    fn adopt_delays_matches_only_same_edge_and_length() {
        let mut arena = NodeArena::default();
        let master = NodeId(9);
        let build = |plan: &mut ExecutionPlan, first: u32, d_latency: u32| {
            let steps: Vec<StepId> = [first, 3, 4, 5]
                .iter()
                .map(|&id| plan.add(NodeId(id), vec![], 1).unwrap())
                .collect();
            plan.set_latency(steps[2], d_latency);
            plan.set_latency(steps[3], 4);
            plan.add(master, vec![steps.iter().map(|&s| src(s)).collect()], 1)
                .unwrap();
        };
        let mut old = ExecutionPlan::new(2);
        for (id, v) in [(1, 1.0), (3, 2.0), (4, 3.0), (5, 0.0)] {
            old.insert(NodeId(id), script(&[v]));
        }
        old.insert(master, gain());
        build(&mut old, 1, 0);
        old.compensate();
        arena.apply(&mut old);
        old.process(&mut arena, &ctx(2), &[]);

        let mut new = ExecutionPlan::new(2);
        new.insert(NodeId(2), script(&[]));
        build(&mut new, 2, 1);
        new.compensate();
        new.adopt_delays(&mut old);

        let delays = &new.steps[4].delays;
        let line = |source: usize| {
            let d = delays.iter().find(|d| d.source == source).unwrap();
            (d.line.to_vec(), d.pos)
        };
        assert_eq!(line(0), (vec![0.0; 4], 0), "new edge starts silent");
        assert_eq!(
            line(1),
            (vec![2.0, 2.0, 0.0, 0.0], 2),
            "same edge carries over"
        );
        assert_eq!(line(2), (vec![0.0; 3], 0), "new length starts silent");
    }

    #[test]
    fn chained_latencies_add_up() {
        let mut plan = ExecutionPlan::new(4);
        let source = plan.add(NodeId(0), vec![], 1).unwrap();
        let p1 = plan.add(NodeId(1), vec![vec![src(source)]], 1).unwrap();
        let p2 = plan.add(NodeId(2), vec![vec![src(p1)]], 1).unwrap();
        plan.set_latency(p1, 2);
        plan.set_latency(p2, 3);
        plan.add(NodeId(3), vec![vec![src(source), src(p2)]], 1)
            .unwrap();
        plan.compensate();
        assert_eq!(plan.latency(), 5);
        let delays = &plan.steps[3].delays;
        assert_eq!(delays.len(), 1);
        assert_eq!((delays[0].source, delays[0].line.len()), (0, 5));
    }

    #[test]
    fn huge_reported_latency_is_clamped() {
        struct Liar;
        impl AudioNode for Liar {
            fn process(
                &mut self,
                _inputs: &[&[f32]],
                outputs: &mut [&mut [f32]],
                _midi_in: &[MidiEvent],
                _midi_out: &mut Vec<MidiEvent>,
                _context: &ProcessContext,
            ) {
                outputs[0].fill(0.0);
            }
            fn latency_samples(&self) -> u32 {
                u32::MAX
            }
            fn reset(&mut self) {}
        }
        let mut plan = ExecutionPlan::new(4);
        let a = add(&mut plan, Box::new(Liar), vec![], 1).unwrap();
        let b = add(&mut plan, script(&[]), vec![], 1).unwrap();
        add(&mut plan, gain(), vec![vec![src(a), src(b)]], 1).unwrap();
        plan.compensate();
        assert_eq!(plan.latency(), MAX_LATENCY);
    }

    #[test]
    #[should_panic(expected = "exceeds plan max")]
    fn oversized_block_panics() {
        let mut arena = NodeArena::default();
        let mut plan = ExecutionPlan::new(4);
        add(&mut plan, script(&[]), vec![], 1).unwrap();
        run(&mut plan, &mut arena, 5);
    }
}
