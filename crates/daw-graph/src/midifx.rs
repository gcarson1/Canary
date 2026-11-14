//! MIDI effects: an arpeggiator, a chord maker and a scale lock. Each takes the
//! notes headed for a track's instrument (live and sequenced) and passes on new
//! ones through `midi_out`; the plan feeds that to the instrument
//! (`ExecutionPlan::midi_from`). No audio in or out.

use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext};
use daw_dsp::effects::ParamSpec;

use crate::{AudioNode, ParamBank};

const fn spec(name: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec {
        name,
        min,
        max,
        default,
        log: false,
    }
}

/// Pushes `e` if there is room: `midi_out` is pre-allocated and never grows.
fn emit(out: &mut Vec<MidiEvent>, e: MidiEvent) {
    if out.len() < out.capacity() {
        out.push(e);
    }
}

fn silence(outputs: &mut [&mut [f32]]) {
    outputs.iter_mut().for_each(|o| o.fill(0.0));
}

/// Note on with velocity, note off, or something else.
enum Kind {
    On(u8, u8),
    Off(u8),
    Other,
}

fn kind(data: [u8; 3]) -> Kind {
    match (data[0] & 0xf0, data[2]) {
        (0x90, v) if v > 0 => Kind::On(data[1] & 0x7f, v),
        (0x80 | 0x90, _) => Kind::Off(data[1] & 0x7f),
        _ => Kind::Other,
    }
}

// ---------------------------------------------------------------- Arpeggiator

pub const ARP_PARAMS: [ParamSpec; 4] = [
    // 0: 1/4, 1: 1/8, 2: 1/16, 3: 1/32.
    spec("RATE", 0.0, 3.0, 2.0),
    // 0: up, 1: down, 2: up-down, 3: as played.
    spec("MODE", 0.0, 3.0, 0.0),
    spec("OCTAVES", 1.0, 4.0, 1.0),
    spec("GATE", 10.0, 100.0, 60.0),
];

/// Most keys the arpeggiator follows at once.
const HELD: usize = 16;

/// Plays the held keys one at a time on the beat grid (from the song's
/// position while playing, a free clock while stopped).
pub struct ArpNode {
    params: Arc<ParamBank>,
    /// Held keys in the order they were pressed, and their velocities.
    held: [(u8, u8); HELD],
    count: usize,
    /// Position in the pattern, the next step's beat, and the sounding note.
    index: usize,
    next: f64,
    sounding: Option<(u8, f64)>,
    /// The free clock, in quarter notes, used while the transport is stopped.
    clock: f64,
}

impl ArpNode {
    pub fn new() -> (Self, Arc<ParamBank>) {
        let params = Arc::new(ParamBank::new(ARP_PARAMS.to_vec()));
        (
            Self {
                params: params.clone(),
                held: [(0, 0); HELD],
                count: 0,
                index: 0,
                next: f64::NEG_INFINITY,
                sounding: None,
                clock: 0.0,
            },
            params,
        )
    }

    fn p(&self, id: usize) -> f32 {
        self.params.get(id).unwrap_or(ARP_PARAMS[id].default)
    }

    /// The `index`th note of the pattern over the held keys, or `None`.
    fn note_at(&self, index: usize) -> Option<(u8, u8)> {
        if self.count == 0 {
            return None;
        }
        let mut keys = self.held;
        let keys = &mut keys[..self.count];
        let mode = self.p(1).round() as u32;
        if mode != 3 {
            keys.sort_unstable_by_key(|k| k.0);
        }
        let octaves = self.p(2).round().clamp(1.0, 4.0) as usize;
        let len = keys.len() * octaves;
        // Up-down turns at both ends without repeating them.
        let cycle = if mode == 2 && len > 1 {
            2 * len - 2
        } else {
            len
        };
        let mut i = index % cycle;
        if mode == 2 && i >= len {
            i = cycle - i;
        }
        if mode == 1 {
            i = len - 1 - i;
        }
        let (key, vel) = keys[i % keys.len()];
        let key = u32::from(key) + 12 * (i / keys.len()) as u32;
        (key <= 127).then_some((key as u8, vel))
    }
}

impl AudioNode for ArpNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        silence(outputs);
        let n = context.num_samples;
        let per_sample = context.musical.bpm / (60.0 * f64::from(context.sample_rate.max(1.0)));
        let start = if context.playing {
            context.musical.beats
        } else {
            self.clock
        };
        self.clock = start + per_sample * n as f64;
        let step = 1.0 / f64::from(1u32 << (self.p(0).round().clamp(0.0, 3.0) as u32));
        let gate = f64::from(self.p(3)) / 100.0;
        // A jump either way (seek, loop, play from elsewhere): realign the grid
        // instead of firing every step in between.
        if (self.next - start).abs() > step {
            self.next = (start / step).ceil() * step;
        }
        let mut events = midi_in.iter().peekable();
        for i in 0..n {
            let beat = start + per_sample * i as f64;
            let at = i as u32;
            while let Some(e) = events.next_if(|e| e.sample_offset <= at) {
                match kind(e.data) {
                    Kind::On(key, vel) => {
                        if self.count == 0 {
                            // The first key starts the pattern on the next step.
                            self.index = 0;
                            self.next = (beat / step).ceil() * step;
                        }
                        if self.count < HELD && !self.held[..self.count].iter().any(|h| h.0 == key)
                        {
                            self.held[self.count] = (key, vel);
                            self.count += 1;
                        }
                    }
                    Kind::Off(key) => {
                        if let Some(p) = self.held[..self.count].iter().position(|h| h.0 == key) {
                            self.held.copy_within(p + 1..self.count, p);
                            self.count -= 1;
                        }
                    }
                    Kind::Other => emit(
                        midi_out,
                        MidiEvent {
                            sample_offset: at,
                            ..*e
                        },
                    ),
                }
            }
            if let Some((key, off)) = self.sounding {
                if beat >= off || self.count == 0 {
                    emit(
                        midi_out,
                        MidiEvent {
                            sample_offset: at,
                            data: [0x80, key, 0],
                        },
                    );
                    self.sounding = None;
                }
            }
            if self.count > 0 && beat >= self.next {
                if let Some((key, vel)) = self.note_at(self.index) {
                    if let Some((old, _)) = self.sounding.take() {
                        emit(
                            midi_out,
                            MidiEvent {
                                sample_offset: at,
                                data: [0x80, old, 0],
                            },
                        );
                    }
                    emit(
                        midi_out,
                        MidiEvent {
                            sample_offset: at,
                            data: [0x90, key, vel],
                        },
                    );
                    self.sounding = Some((key, self.next + step * gate));
                }
                self.index += 1;
                self.next += step;
            }
        }
    }

    fn automate(&mut self, id: u32, value: f64) {
        self.params.set(id as usize, value as f32);
    }

    fn reset(&mut self) {
        self.count = 0;
        self.sounding = None;
    }
}

// ---------------------------------------------------------------- Chord

/// Intervals above the root, per CHORD value.
const CHORDS: [&[u8]; 6] = [
    &[0, 4, 7],     // major
    &[0, 3, 7],     // minor
    &[0, 5, 7],     // sus4
    &[0, 4, 7, 11], // major 7
    &[0, 3, 7, 10], // minor 7
    &[0, 7, 12],    // power
];

pub const CHORD_PARAMS: [ParamSpec; 1] = [spec("CHORD", 0.0, 5.0, 0.0)];

/// Plays a chord on every key: the key is the root.
pub struct ChordNode {
    params: Arc<ParamBank>,
    /// The chord each sounding root started (by channel), so its note-off ends
    /// the same one.
    started: [[u8; 128]; 16],
}

impl ChordNode {
    pub fn new() -> (Self, Arc<ParamBank>) {
        let params = Arc::new(ParamBank::new(CHORD_PARAMS.to_vec()));
        (
            Self {
                params: params.clone(),
                started: [[0; 128]; 16],
            },
            params,
        )
    }
}

impl AudioNode for ChordNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        midi_out: &mut Vec<MidiEvent>,
        _context: &ProcessContext,
    ) {
        silence(outputs);
        let chord = self.params.get(0).unwrap_or(0.0).round().clamp(0.0, 5.0) as u8;
        for e in midi_in {
            let (key, on, vel) = match kind(e.data) {
                Kind::On(key, vel) => (key, true, vel),
                Kind::Off(key) => (key, false, 0),
                Kind::Other => {
                    emit(midi_out, *e);
                    continue;
                }
            };
            let started = &mut self.started[usize::from(e.data[0] & 0x0f)];
            let which = if on { chord } else { started[usize::from(key)] };
            started[usize::from(key)] = which;
            for &step in CHORDS[usize::from(which)] {
                let note = key.saturating_add(step);
                if note <= 127 {
                    let status = (e.data[0] & 0x0f) | if on { 0x90 } else { 0x80 };
                    emit(
                        midi_out,
                        MidiEvent {
                            sample_offset: e.sample_offset,
                            data: [status, note, vel],
                        },
                    );
                }
            }
        }
    }

    fn automate(&mut self, id: u32, value: f64) {
        self.params.set(id as usize, value as f32);
    }

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------- Scale

/// Semitones in each SCALE value, from its root.
const SCALES: [&[u8]; 6] = [
    &[0, 2, 4, 5, 7, 9, 11], // major
    &[0, 2, 3, 5, 7, 8, 10], // natural minor
    &[0, 2, 3, 5, 7, 9, 10], // dorian
    &[0, 2, 3, 5, 7, 8, 11], // harmonic minor
    &[0, 2, 4, 7, 9],        // major pentatonic
    &[0, 3, 5, 6, 7, 10],    // blues
];

pub const SCALE_PARAMS: [ParamSpec; 2] =
    [spec("ROOT", 0.0, 11.0, 0.0), spec("SCALE", 0.0, 5.0, 0.0)];

/// Moves every key down to the nearest note of the scale.
pub struct ScaleNode {
    params: Arc<ParamBank>,
    /// Where each sounding key was moved to (by channel), so its note-off follows.
    moved: [[u8; 128]; 16],
}

impl ScaleNode {
    pub fn new() -> (Self, Arc<ParamBank>) {
        let params = Arc::new(ParamBank::new(SCALE_PARAMS.to_vec()));
        (
            Self {
                params: params.clone(),
                moved: [std::array::from_fn(|k| k as u8); 16],
            },
            params,
        )
    }
}

/// `key` moved down to the nearest note of `scale` from `root`.
pub fn snap(key: u8, root: u8, scale: &[u8]) -> u8 {
    let rel = (i32::from(key) - i32::from(root)).rem_euclid(12) as u8;
    let down = scale
        .iter()
        .rev()
        .find(|&&s| s <= rel)
        .copied()
        .unwrap_or(0);
    // Below the scale's lowest note on the keyboard: stay put rather than wrap.
    key.saturating_sub(rel - down)
}

impl AudioNode for ScaleNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        midi_out: &mut Vec<MidiEvent>,
        _context: &ProcessContext,
    ) {
        silence(outputs);
        let root = self.params.get(0).unwrap_or(0.0).round().clamp(0.0, 11.0) as u8;
        let scale = SCALES[self.params.get(1).unwrap_or(0.0).round().clamp(0.0, 5.0) as usize];
        for e in midi_in {
            let mut out = *e;
            let moved = &mut self.moved[usize::from(e.data[0] & 0x0f)];
            match kind(e.data) {
                Kind::On(key, _) => {
                    let to = snap(key, root, scale);
                    moved[usize::from(key)] = to;
                    out.data[1] = to;
                }
                Kind::Off(key) => out.data[1] = moved[usize::from(key)],
                Kind::Other => {}
            }
            emit(midi_out, out);
        }
    }

    fn automate(&mut self, id: u32, value: f64) {
        self.params.set(id as usize, value as f32);
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_core::Musical;

    fn ev(at: u32, data: [u8; 3]) -> MidiEvent {
        MidiEvent {
            sample_offset: at,
            data,
        }
    }

    fn run(
        node: &mut dyn AudioNode,
        midi: &[MidiEvent],
        ctx: ProcessContext,
    ) -> Vec<(u32, [u8; 3])> {
        let mut out = Vec::with_capacity(256);
        let mut l = vec![0.0; ctx.num_samples];
        node.process(&[], &mut [&mut l], midi, &mut out, &ctx);
        out.iter().map(|e| (e.sample_offset, e.data)).collect()
    }

    /// 120 BPM at 48 kHz: a quarter note is 24 000 samples, a sixteenth 6 000.
    fn ctx(n: usize, beats: f64) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: n,
            playing: true,
            musical: Musical {
                beats,
                ..Musical::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn arpeggiator_steps_through_held_keys_on_the_sixteenth_grid() {
        let (mut arp, _) = ArpNode::new();
        let chord = [
            ev(0, [0x90, 64, 90]),
            ev(0, [0x90, 60, 100]),
            ev(0, [0x90, 67, 80]),
        ];
        let out = run(&mut arp, &chord, ctx(24_000, 0.0));
        let ons: Vec<(u32, u8)> = out
            .iter()
            .filter(|e| e.1[0] == 0x90)
            .map(|e| (e.0, e.1[1]))
            .collect();
        // Up, from the lowest: 60, 64, 67, 60 at each sixteenth.
        assert_eq!(ons, [(0, 60), (6_000, 64), (12_000, 67), (18_000, 60)]);
        // 60% gate: each note ends 3 600 samples after it starts.
        assert!(out.contains(&(3_600, [0x80, 60, 0])));
        // Letting go stops it at once.
        let out = run(
            &mut arp,
            &[
                ev(0, [0x80, 60, 0]),
                ev(0, [0x80, 64, 0]),
                ev(0, [0x80, 67, 0]),
            ],
            ctx(6_000, 1.0),
        );
        assert!(out.iter().all(|e| e.1[0] == 0x80));
    }

    #[test]
    fn arpeggiator_realigns_after_a_jump_instead_of_bursting() {
        let (mut arp, _) = ArpNode::new();
        run(&mut arp, &[ev(0, [0x90, 60, 100])], ctx(6_000, 0.0));
        // Seek forward ten bars with the key held: one step, not hundreds.
        let out = run(&mut arp, &[], ctx(6_000, 40.0));
        assert!(
            out.iter().filter(|e| e.1[0] == 0x90).count() <= 1,
            "{out:?}"
        );
    }

    #[test]
    fn chord_and_scale_rewrite_notes_and_their_offs() {
        let (mut chord, params) = ChordNode::new();
        params.set(0, 1.0); // minor
        let out = run(&mut chord, &[ev(5, [0x90, 57, 100])], ctx(16, 0.0));
        assert_eq!(
            out,
            [
                (5, [0x90, 57, 100]),
                (5, [0x90, 60, 100]),
                (5, [0x90, 64, 100])
            ]
        );
        params.set(0, 0.0); // a change mid-note still ends the minor chord
        let out = run(&mut chord, &[ev(0, [0x80, 57, 0])], ctx(16, 0.0));
        assert_eq!(out.iter().map(|e| e.1[1]).collect::<Vec<_>>(), [57, 60, 64]);

        assert_eq!(snap(61, 0, SCALES[0]), 60, "C# down to C in C major");
        assert_eq!(snap(66, 2, SCALES[0]), 66, "F# is in D major");
        assert_eq!(snap(0, 11, SCALES[0]), 0, "below the lowest B: stays");
        let (mut scale, _) = ScaleNode::new();
        let out = run(
            &mut scale,
            &[ev(0, [0x90, 63, 100]), ev(1, [0x80, 63, 0])],
            ctx(16, 0.0),
        );
        assert_eq!(out, [(0, [0x90, 62, 100]), (1, [0x80, 62, 0])]);
    }
}
