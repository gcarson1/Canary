//! A sampler instrument: plays zones of shared sample data by MIDI key and
//! velocity, taking turns (round robin) among zones that share a key/velocity
//! cell. Zones are built on the main thread (whole sample, slices on pads, layers)
//! and swapped in through `AudioNode::exchange` as a `SamplerData`.

use std::any::Any;
use std::mem;
use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext, SampleData};
use daw_dsp::db_to_gain;
use daw_dsp::effects::ParamSpec;

use crate::{AudioNode, ParamBank};

const VOICES: usize = 32;
/// Round-robin counters; cells past this share one.
const CELLS: usize = 256;
/// Frames faded at a zone's end, and the shortest attack, so nothing clicks.
const EDGE_FADE: f32 = 64.0;
/// Release time of a voice cut by a retrigger of its key.
const CHOKE_SECONDS: f32 = 0.005;

const fn spec(name: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec {
        name,
        min,
        max,
        default,
        log: false,
    }
}

/// MODE, SLICER, SENS, GRID and ROOT shape the zones (main thread); the node
/// reads ATTACK, RELEASE and GAIN each block.
pub const SAMPLER_PARAMS: [ParamSpec; 8] = [
    // 0 one-shot, 1 classic (held notes), 2 slice (one slice per pad).
    spec("MODE", 0.0, 2.0, 1.0),
    // 0 transients, 1 grid, 2 manual markers.
    spec("SLICER", 0.0, 2.0, 0.0),
    spec("SENS", 0.0, 1.0, 0.5),
    spec("GRID", 1.0, 64.0, 16.0),
    spec("ATTACK", 0.0, 500.0, 1.0),
    spec("RELEASE", 1.0, 2000.0, 100.0),
    // The key the sample plays at its own pitch; in slice mode, the first pad.
    spec("ROOT", 0.0, 127.0, 60.0),
    spec("GAIN", -24.0, 12.0, 0.0),
];
const ATTACK: usize = 4;
const RELEASE: usize = 5;
const GAIN: usize = 7;

/// A region of a sample mapped to a key and velocity range.
#[derive(Clone)]
pub struct Zone {
    pub sample: Arc<SampleData>,
    /// Frames, end exclusive; clamped to the sample when played.
    pub start: usize,
    pub end: usize,
    pub keys: (u8, u8),
    pub velocities: (u8, u8),
    /// Plays at its own pitch on this key.
    pub root: u8,
    /// Zones in the same cell take turns.
    pub cell: u16,
    /// Plays to its end, ignoring note off.
    pub one_shot: bool,
}

impl Zone {
    fn matches(&self, key: u8, velocity: u8) -> bool {
        (self.keys.0..=self.keys.1).contains(&key)
            && (self.velocities.0..=self.velocities.1).contains(&velocity)
    }
}

/// What the main thread sends: the full zone list, replacing the node's.
#[derive(Default)]
pub struct SamplerData {
    pub zones: Vec<Zone>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Voice {
    active: bool,
    zone: usize,
    key: u8,
    held: bool,
    choked: bool,
    pos: f64,
    step: f64,
    velocity: f32,
    level: f32,
    started: u64,
}

pub struct SamplerNode {
    zones: Vec<Zone>,
    voices: [Voice; VOICES],
    params: Arc<ParamBank>,
    turns: [u32; CELLS],
    notes: u64,
}

impl SamplerNode {
    /// A silent sampler, and the handle the main thread turns its knobs with.
    pub fn new() -> (Self, Arc<ParamBank>) {
        let params = Arc::new(ParamBank::new(SAMPLER_PARAMS.to_vec()));
        let node = Self {
            zones: Vec::new(),
            voices: [Voice::default(); VOICES],
            params: params.clone(),
            turns: [0; CELLS],
            notes: 0,
        };
        (node, params)
    }

    fn note_on(&mut self, key: u8, velocity: u8, sample_rate: f32) {
        // A retriggered key cuts its old voice (pads), quickly but without a click.
        for v in self.voices.iter_mut().filter(|v| v.active && v.key == key) {
            v.choked = true;
        }
        let matching = || {
            self.zones
                .iter()
                .enumerate()
                .filter(|(_, z)| z.matches(key, velocity))
        };
        let Some((_, first)) = matching().next() else {
            return;
        };
        let cell = first.cell;
        let in_cell = || matching().filter(|(_, z)| z.cell == cell);
        let count = in_cell().count() as u32;
        let turn = &mut self.turns[usize::from(cell) % CELLS];
        let Some((index, zone)) = in_cell().nth((*turn % count) as usize) else {
            return;
        };
        *turn = turn.wrapping_add(1);
        let semitones = f64::from(key) - f64::from(zone.root);
        let step = 2f64.powf(semitones / 12.0) * f64::from(zone.sample.sample_rate())
            / f64::from(sample_rate);
        self.notes += 1;
        // A free voice, else the oldest.
        let voice = self
            .voices
            .iter_mut()
            .min_by_key(|v| (v.active, v.started))
            .expect("VOICES > 0");
        *voice = Voice {
            active: true,
            zone: index,
            key,
            held: true,
            choked: false,
            pos: zone.start as f64,
            step,
            velocity: f32::from(velocity) / 127.0,
            level: 0.0,
            started: self.notes,
        };
    }

    fn handle(&mut self, [status, key, value]: [u8; 3], sample_rate: f32) {
        let (key, value) = (key & 0x7F, value & 0x7F);
        match (status & 0xF0, value) {
            (0x90, v) if v > 0 => self.note_on(key, v, sample_rate),
            (0x80, _) | (0x90, _) => {
                for v in self.voices.iter_mut().filter(|v| v.key == key) {
                    v.held = false;
                }
            }
            (0xB0, _) if key == 120 || key == 123 => {
                for v in &mut self.voices {
                    v.held = false;
                    v.choked |= key == 120; // all sound off: now, not after release
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, left: &mut [f32], right: &mut [f32], sample_rate: f32) {
        let ms = |i| self.params.get(i).unwrap_or(SAMPLER_PARAMS[i].default) / 1000.0;
        let attack = 1.0 / (ms(ATTACK) * sample_rate).max(EDGE_FADE);
        let decay = |seconds: f32| (1e-4f32.ln() / (seconds * sample_rate).max(1.0)).exp();
        let (release, choke) = (decay(ms(RELEASE)), decay(CHOKE_SECONDS));
        let gain = db_to_gain(self.params.get(GAIN).unwrap_or(0.0));
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let Some(zone) = self.zones.get(v.zone) else {
                v.active = false;
                continue;
            };
            let data = &zone.sample;
            let end = zone.end.min(data.frames());
            let (l, r) = (
                data.channel(0),
                data.channel(data.num_channels().min(2) - 1),
            );
            for (ol, or) in left.iter_mut().zip(right.iter_mut()) {
                let remaining = end as f64 - v.pos;
                let sustaining = (v.held || zone.one_shot) && !v.choked;
                v.level = if v.choked {
                    v.level * choke
                } else if sustaining {
                    (v.level + attack).min(1.0)
                } else {
                    v.level * release
                };
                if remaining <= 1.0 || (!sustaining && v.level < 1e-4) {
                    v.active = false;
                    break;
                }
                let fade = (remaining as f32 / EDGE_FADE).min(1.0);
                let g = v.level * v.velocity * gain * fade;
                *ol += cubic(l, v.pos, zone.start, end) * g;
                *or += cubic(r, v.pos, zone.start, end) * g;
                v.pos += v.step;
            }
        }
    }
}

/// Cubic Hermite interpolation of `x` at `pos`, reading only inside start..end.
fn cubic(x: &[f32], pos: f64, start: usize, end: usize) -> f32 {
    let i = pos as usize;
    let t = (pos - i as f64) as f32;
    let at = |k: isize| x[(i as isize + k).clamp(start as isize, end as isize - 1) as usize];
    let (y0, y1, y2, y3) = (at(-1), at(0), at(1), at(2));
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

impl AudioNode for SamplerNode {
    /// Stereo out; a mono sample plays on both channels. Outputs past the second
    /// are zeroed.
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        outputs.iter_mut().for_each(|o| o.fill(0.0));
        let (left, right) = match outputs {
            [l, r, ..] => (&mut **l, &mut **r),
            _ => return,
        };
        let sr = context.sample_rate;
        let mut at = 0;
        for event in midi_in {
            let offset = (event.sample_offset as usize).clamp(at, left.len());
            self.render(&mut left[at..offset], &mut right[at..offset], sr);
            self.handle(event.data, sr);
            at = offset;
        }
        self.render(&mut left[at..], &mut right[at..], sr);
    }

    fn reset(&mut self) {
        self.voices = [Voice::default(); VOICES];
    }

    /// Takes a `SamplerData`. Voices stop: their zone indexes mean nothing in the
    /// new list.
    // shortcut: a zone edit cuts sounding notes; fade them if it is audible in use.
    fn exchange(&mut self, data: &mut (dyn Any + Send)) {
        if let Some(data) = data.downcast_mut::<SamplerData>() {
            mem::swap(&mut self.zones, &mut data.zones);
            self.voices = [Voice::default(); VOICES];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            num_samples: n,
            playhead: 0,
            playing: true,
        }
    }

    fn on(key: u8, velocity: u8, at: u32) -> MidiEvent {
        MidiEvent {
            sample_offset: at,
            data: [0x90, key, velocity],
        }
    }

    /// A zone over a constant-`value` sample of `frames`, all keys and velocities.
    fn zone(value: f32, frames: usize) -> Zone {
        Zone {
            sample: Arc::new(SampleData::new(48_000, vec![vec![value; frames]]).unwrap()),
            start: 0,
            end: frames,
            keys: (0, 127),
            velocities: (1, 127),
            root: 60,
            cell: 0,
            one_shot: true,
        }
    }

    fn sampler(zones: Vec<Zone>) -> SamplerNode {
        let (mut node, _) = SamplerNode::new();
        node.exchange(&mut SamplerData { zones });
        node
    }

    /// Runs one block with `midi` and returns the left channel.
    fn block(node: &mut SamplerNode, n: usize, midi: &[MidiEvent]) -> Vec<f32> {
        let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
        node.process(&[], &mut [&mut l, &mut r], midi, &mut Vec::new(), &ctx(n));
        assert_eq!(l, r, "mono sample on both channels");
        l
    }

    #[test]
    fn note_plays_from_its_offset_and_one_shot_ends_with_the_zone() {
        let mut s = sampler(vec![zone(0.5, 1_000)]);
        let out = block(&mut s, 2_000, &[on(60, 127, 100)]);
        assert!(
            out[..100].iter().all(|&x| x == 0.0),
            "silent before the note"
        );
        let peak = out.iter().fold(0.0f32, |m, x| m.max(*x));
        assert!((peak - 0.5).abs() < 1e-3, "full velocity at root: {peak}");
        assert!(
            out[1_100..].iter().all(|&x| x == 0.0),
            "stops at the zone end"
        );
        assert!(!s.voices.iter().any(|v| v.active));
    }

    #[test]
    fn round_robin_takes_turns_within_a_cell_and_velocity_picks_the_layer() {
        let mut soft = zone(0.1, 48_000);
        soft.velocities = (1, 63);
        soft.cell = 1;
        let mut loud_a = zone(0.4, 48_000);
        loud_a.velocities = (64, 127);
        let mut loud_b = zone(0.8, 48_000);
        loud_b.velocities = (64, 127);
        let mut s = sampler(vec![soft, loud_a, loud_b]);
        let level = |s: &mut SamplerNode, v| {
            let out = block(s, 512, &[on(60, v, 0)]);
            out[511] / (f32::from(v) / 127.0)
        };
        assert!((level(&mut s, 30) - 0.1).abs() < 1e-3, "soft layer");
        assert!((level(&mut s, 127) - 0.4).abs() < 1e-3, "first turn");
        assert!((level(&mut s, 127) - 0.8).abs() < 1e-3, "second turn");
        assert!((level(&mut s, 127) - 0.4).abs() < 1e-3, "back to the first");
    }

    #[test]
    fn classic_releases_on_note_off_and_pitch_follows_the_key() {
        let mut z = zone(0.5, 96_000);
        z.one_shot = false;
        // A ramp: playback position is readable from the output.
        z.sample = Arc::new(
            SampleData::new(
                48_000,
                vec![(0..96_000).map(|i| i as f32 / 96_000.0).collect()],
            )
            .unwrap(),
        );
        let mut s = sampler(vec![z]);
        // An octave up plays twice as fast.
        let out = block(&mut s, 1_000, &[on(72, 127, 0)]);
        let rate = (out[999] - out[899]) * 96_000.0 / 100.0;
        assert!((rate - 2.0).abs() < 0.01, "rate {rate}");
        let off = MidiEvent {
            sample_offset: 0,
            data: [0x80, 72, 0],
        };
        block(&mut s, 48_000, &[off]);
        assert!(!s.voices.iter().any(|v| v.active), "released within 100 ms");
    }

    #[test]
    fn keys_outside_every_zone_are_silent_and_exchange_stops_voices() {
        let mut z = zone(0.5, 48_000);
        z.keys = (36, 36);
        let mut s = sampler(vec![z]);
        assert!(block(&mut s, 64, &[on(37, 100, 0)])
            .iter()
            .all(|&x| x == 0.0));
        block(&mut s, 64, &[on(36, 100, 0)]);
        assert!(s.voices.iter().any(|v| v.active));
        let mut old = SamplerData::default();
        s.exchange(&mut old);
        assert_eq!(old.zones.len(), 1, "old zones go back to be dropped");
        assert!(block(&mut s, 64, &[]).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn interpolation_reads_only_inside_the_zone() {
        let x = [9.0, 1.0, 2.0, 3.0, 9.0];
        assert_eq!(cubic(&x, 1.0, 1, 4), 1.0);
        assert_eq!(cubic(&x, 3.0, 1, 4), 3.0);
        let mid = cubic(&x, 2.5, 1, 4);
        assert!((2.0..=3.0).contains(&mid), "{mid}");
    }
}
