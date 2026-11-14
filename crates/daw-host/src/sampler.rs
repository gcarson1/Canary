//! The built-in sampler's main-thread handle: its sample layers, slices and
//! parameters. It builds the zone list and sends it to the node over a queue,
//! so editing never needs a new plan.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext, SampleData};
use daw_dsp::slice;
use daw_graph::{AudioNode, ParamBank, SamplerData, SamplerNode, Zone, SAMPLER_PARAMS};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::{HostError, Hosted, Instance, ParamInfo};

pub const ID: &str = "canary.sampler";
/// Zone lists in flight to the node and back; `Sampler` never has more out.
const QUEUE: usize = 4;
/// Params that change the zones; the rest the node reads itself.
const MODE: usize = 0;
const SLICER: usize = 1;
const SENS: usize = 2;
const GRID: usize = 3;
const ROOT: usize = 6;
/// Pads with their own tune and choke group (slice and kit modes).
pub const PADS: usize = 16;
/// Choke groups a pad can join (0 is none).
pub const CHOKE_GROUPS: u8 = 4;

/// One pad's settings: semitones of tune and a choke group (0 is none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pad {
    pub tune: i8,
    pub choke: u8,
}

/// How a dropped file joins the sampler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// Replaces every layer.
    Replace,
    /// Takes turns with the last layer's zone.
    RoundRobin,
    /// A new velocity zone; velocities split evenly between them.
    Velocity,
    /// A new key zone; keys split evenly between them.
    Key,
}

struct Layer {
    /// Key and velocity zone indexes.
    key: u8,
    velocity: u8,
    path: PathBuf,
    audio: Option<Arc<SampleData>>,
}

pub struct Sampler {
    params: Arc<ParamBank>,
    infos: Vec<ParamInfo>,
    layers: Vec<Layer>,
    /// Manual slice points, as fractions of the first layer's length.
    markers: Vec<f64>,
    pads: [Pad; PADS],
    /// Slice starts in frames of the first layer, as last built.
    starts: Vec<usize>,
    /// The slices need recomputing (slicer settings, markers or audio changed);
    /// other edits reuse them, since a transient scan reads the whole file.
    reslice: bool,
    to_node: Producer<Box<SamplerData>>,
    returned: Consumer<Box<SamplerData>>,
    out: usize,
    /// A zone list waiting for queue room.
    pending: Option<Box<SamplerData>>,
}

/// The node with its end of the queue: swaps in each zone list at block start and
/// sends the old one back to be dropped on the main thread.
struct SamplerPlug {
    node: SamplerNode,
    from_main: Consumer<Box<SamplerData>>,
    back: Producer<Box<SamplerData>>,
}

impl AudioNode for SamplerPlug {
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        if let Ok(mut data) = self.from_main.pop() {
            self.node.exchange(&mut *data);
            // Never full: the handle keeps at most QUEUE lists out.
            let _ = self.back.push(data);
        }
        self.node
            .process(inputs, outputs, midi_in, midi_out, context);
    }
    fn reset(&mut self) {
        self.node.reset();
    }
}

pub fn instantiate() -> Instance {
    let (node, params) = SamplerNode::new();
    let (to_node, from_main) = RingBuffer::new(QUEUE);
    let (back, returned) = RingBuffer::new(QUEUE);
    let infos = SAMPLER_PARAMS
        .iter()
        .enumerate()
        .map(|(i, p)| ParamInfo {
            id: i as u32,
            name: p.name.into(),
            min: p.min.into(),
            max: p.max.into(),
            default: p.default.into(),
            log: p.log,
        })
        .collect();
    let handle = Sampler {
        params,
        infos,
        layers: Vec::new(),
        markers: Vec::new(),
        pads: [Pad::default(); PADS],
        starts: Vec::new(),
        reslice: true,
        to_node,
        returned,
        out: 0,
        pending: None,
    };
    let plug = SamplerPlug {
        node,
        from_main,
        back,
    };
    (Box::new(handle), Box::new(plug))
}

/// Splits `0..=127` (or `1..=127` from `first`) into `count` even ranges; the `i`th.
fn split(first: u8, count: u8, i: u8) -> (u8, u8) {
    let span = 128 - u16::from(first);
    let (count, i) = (u16::from(count.max(1)), u16::from(i));
    let lo = u16::from(first) + span * i / count;
    let hi = u16::from(first) + span * (i + 1) / count - 1;
    (lo as u8, hi.max(lo) as u8)
}

impl Sampler {
    fn param(&self, id: usize) -> f32 {
        self.params.get(id).unwrap_or(SAMPLER_PARAMS[id].default)
    }

    /// Adds a sample file; its audio arrives later through `set_audio`.
    pub fn add_layer(&mut self, path: PathBuf, kind: LayerKind) {
        let last = self.layers.last().map_or((0, 0), |l| (l.key, l.velocity));
        let next_key = self.layers.iter().map(|l| l.key).max().map_or(0, |m| m + 1);
        // Velocity zones count within one key zone: the last layer's.
        let next_velocity = self.velocities(last.0);
        let (key, velocity) = match kind {
            LayerKind::Replace => {
                self.layers.clear();
                self.markers.clear();
                self.reslice = true;
                (0, 0)
            }
            LayerKind::RoundRobin => last,
            LayerKind::Velocity => (last.0, next_velocity.min(126)),
            LayerKind::Key => (next_key.min(127), 0),
        };
        self.layers.push(Layer {
            key,
            velocity,
            path,
            audio: None,
        });
        self.rebuild();
    }

    /// Velocity zones in key zone `key`.
    fn velocities(&self, key: u8) -> u8 {
        let in_key = self.layers.iter().filter(|l| l.key == key);
        in_key.map(|l| l.velocity).max().map_or(0, |m| m + 1)
    }

    /// Files whose audio has not arrived yet.
    pub fn missing(&self) -> Vec<PathBuf> {
        self.layers
            .iter()
            .filter(|l| l.audio.is_none())
            .map(|l| l.path.clone())
            .collect()
    }

    /// Gives every layer playing `path` its audio (at the engine's rate).
    pub fn set_audio(&mut self, path: &Path, audio: &Arc<SampleData>) {
        let mut changed = false;
        for l in self.layers.iter_mut().filter(|l| l.path == path) {
            l.audio = Some(audio.clone());
            changed = true;
        }
        if changed {
            self.reslice = true;
            self.rebuild();
        }
    }

    /// The first layer's file: the one slices and the waveform come from.
    pub fn main_path(&self) -> Option<&Path> {
        self.layers.first().map(|l| l.path.as_path())
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Kit mode's pads: one per key zone (a file dropped with Cmd).
    pub fn kit_pads(&self) -> usize {
        self.layers
            .iter()
            .map(|l| usize::from(l.key) + 1)
            .max()
            .unwrap_or(0)
    }

    pub fn pads(&self) -> [Pad; PADS] {
        self.pads
    }

    /// Sets pad `index`'s tune (held to two octaves) and choke group.
    pub fn set_pad(&mut self, index: usize, pad: Pad) {
        if let Some(p) = self.pads.get_mut(index) {
            *p = Pad {
                tune: pad.tune.clamp(-24, 24),
                choke: pad.choke.min(CHOKE_GROUPS),
            };
            self.rebuild();
        }
    }

    /// Pad `i`'s tune and choke group, as a zone takes them.
    fn pad(&self, i: usize) -> (f32, u8) {
        let p = self.pads.get(i).copied().unwrap_or_default();
        (f32::from(p.tune), p.choke)
    }

    /// Slice starts as fractions of the first layer's length.
    pub fn slice_points(&self) -> Vec<f64> {
        let frames = self
            .layers
            .first()
            .and_then(|l| l.audio.as_ref())
            .map_or(0, |a| a.frames());
        if frames == 0 {
            return Vec::new();
        }
        self.starts
            .iter()
            .map(|&s| s as f64 / frames as f64)
            .collect()
    }

    /// Adds a manual slice point at `at` (a fraction of the length), or removes
    /// the nearest one. Switches the slicer to manual, starting from the slices
    /// shown, so what the user sees is what they edit.
    pub fn edit_marker(&mut self, at: f64, add: bool) {
        if self.param(SLICER).round() != 2.0 {
            self.markers = self.slice_points().into_iter().skip(1).collect();
            self.params.set(SLICER, 2.0);
        }
        if add {
            if at.is_finite() && at > 0.0 && at < 1.0 {
                self.markers.push(at);
            }
        } else if let Some(i) = self
            .markers
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1 - at).abs().total_cmp(&(b.1 - at).abs()))
            .map(|(i, _)| i)
        {
            self.markers.remove(i);
        }
        self.reslice = true;
        self.rebuild();
    }

    /// Recomputes slices and zones and queues them for the node.
    fn rebuild(&mut self) {
        let zones = self.zones();
        // A newer list replaces one still waiting.
        self.pending = Some(Box::new(SamplerData { zones }));
        self.flush();
    }

    /// Recomputes the slices, then the zones from the layers or slices.
    fn zones(&mut self) -> Vec<Zone> {
        let root = self.param(ROOT).round().clamp(0.0, 127.0) as u8;
        let mode = self.param(MODE).round() as u8;
        let main = self.layers.first().and_then(|l| l.audio.clone());
        if std::mem::take(&mut self.reslice) {
            self.starts = main.as_ref().map_or_else(Vec::new, |a| {
                let ch = a.channel(0);
                let mut starts = match self.param(SLICER).round() as u8 {
                    0 => slice::transients(ch, a.sample_rate() as f32, self.param(SENS)),
                    1 => slice::grid(ch.len(), self.param(GRID).round() as usize),
                    _ => slice::markers(ch.len(), &self.markers),
                };
                slice::snap_to_zero(ch, &mut starts, a.sample_rate() as usize / 500);
                // Close starts can snap to the same crossing: no empty slices.
                starts.dedup();
                starts
            });
        }
        let mut zones = Vec::new();
        if mode == 2 {
            // One slice per key from ROOT up: the pads.
            if let Some(audio) = main {
                let ends = self.starts.iter().skip(1).copied().chain([audio.frames()]);
                for (i, (start, end)) in self.starts.iter().copied().zip(ends).enumerate() {
                    let Some(key) = u8::try_from(usize::from(root) + i)
                        .ok()
                        .filter(|k| *k <= 127)
                    else {
                        break;
                    };
                    let (tune, choke) = self.pad(i);
                    zones.push(Zone {
                        sample: audio.clone(),
                        start,
                        end,
                        keys: (key, key),
                        velocities: (1, 127),
                        root: key,
                        cell: i as u16,
                        one_shot: true,
                        tune,
                        choke,
                    });
                }
            }
        } else if mode == 3 {
            // A kit: key zone n is pad n, on ROOT + n at its own pitch.
            for l in &self.layers {
                let (Some(audio), Some(key)) = (l.audio.clone(), root.checked_add(l.key)) else {
                    continue;
                };
                if key > 127 {
                    continue;
                }
                let (tune, choke) = self.pad(usize::from(l.key));
                zones.push(Zone {
                    end: audio.frames(),
                    sample: audio,
                    start: 0,
                    keys: (key, key),
                    velocities: split(1, self.velocities(l.key), l.velocity),
                    root: key,
                    cell: u16::from(l.key) * 128 + u16::from(l.velocity),
                    one_shot: true,
                    tune,
                    choke,
                });
            }
        } else {
            let keys = self.layers.iter().map(|l| l.key).max().map_or(1, |m| m + 1);
            for l in &self.layers {
                let Some(audio) = l.audio.clone() else {
                    continue;
                };
                let key_range = split(0, keys, l.key);
                zones.push(Zone {
                    end: audio.frames(),
                    sample: audio,
                    start: 0,
                    keys: key_range,
                    velocities: split(1, self.velocities(l.key), l.velocity),
                    // One key zone plays at ROOT; split zones play true at their middle.
                    root: if keys == 1 {
                        root
                    } else {
                        ((u16::from(key_range.0) + u16::from(key_range.1)) / 2) as u8
                    },
                    cell: u16::from(l.key) * 128 + u16::from(l.velocity),
                    one_shot: mode == 0,
                    tune: 0.0,
                    choke: 0,
                });
            }
        }
        zones
    }

    /// Drops lists the node sent back and sends the pending one if there is room.
    fn flush(&mut self) {
        while let Ok(old) = self.returned.pop() {
            drop(old);
            self.out -= 1;
        }
        if self.out < QUEUE {
            if let Some(data) = self.pending.take() {
                match self.to_node.push(data) {
                    Ok(()) => self.out += 1,
                    Err(rtrb::PushError::Full(data)) => self.pending = Some(data),
                }
            }
        }
    }
}

impl Hosted for Sampler {
    fn params(&self) -> &[ParamInfo] {
        &self.infos
    }
    fn param_value(&mut self, id: u32) -> Option<f64> {
        self.params.get(id as usize).map(f64::from)
    }
    fn set_param(&mut self, id: u32, value: f64) -> bool {
        let id = id as usize;
        let before = self.params.get(id);
        if !self.params.set(id, value as f32) {
            return false;
        }
        let shapes_zones = [MODE, SLICER, SENS, GRID, ROOT].contains(&id);
        if shapes_zones && self.params.get(id) != before {
            self.reslice |= [SLICER, SENS, GRID].contains(&id);
            self.rebuild();
        }
        true
    }
    /// Param lines, then `LAYER=key,velocity,path`, `MARKER=fraction` and
    /// `PAD=index,tune,choke` lines (pads left at their defaults are omitted).
    fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        let mut lines: Vec<String> = self
            .infos
            .iter()
            .map(|p| format!("{}={}", p.name, self.param(p.id as usize)))
            .collect();
        for l in &self.layers {
            lines.push(format!(
                "LAYER={},{},{}",
                l.key,
                l.velocity,
                l.path.display()
            ));
        }
        lines.extend(self.markers.iter().map(|m| format!("MARKER={m}")));
        for (i, p) in self.pads.iter().enumerate() {
            if *p != Pad::default() {
                lines.push(format!("PAD={i},{},{}", p.tune, p.choke));
            }
        }
        Ok(lines.join("\n").into_bytes())
    }
    /// Bad lines are skipped. Layer audio must be given again with `set_audio`.
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        let text = std::str::from_utf8(bytes).map_err(|_| HostError::State)?;
        self.layers.clear();
        self.markers.clear();
        self.pads = [Pad::default(); PADS];
        self.reslice = true;
        for (name, value) in text.lines().filter_map(|l| l.split_once('=')) {
            match name {
                "LAYER" => {
                    let mut parts = value.splitn(3, ',');
                    let (Some(k), Some(v), Some(path)) = (parts.next(), parts.next(), parts.next())
                    else {
                        continue;
                    };
                    if let (Ok(key), Ok(velocity)) = (k.parse::<u8>(), v.parse::<u8>()) {
                        self.layers.push(Layer {
                            key: key.min(127),
                            velocity: velocity.min(126),
                            path: path.into(),
                            audio: None,
                        });
                    }
                }
                "MARKER" => self.markers.extend(value.parse::<f64>().ok()),
                "PAD" => {
                    let v: Vec<&str> = value.split(',').collect();
                    if let [i, tune, choke] = v[..] {
                        let pad = (i.parse::<usize>(), tune.parse::<i8>(), choke.parse::<u8>());
                        if let (Ok(i), Ok(tune), Ok(choke)) = pad {
                            if let Some(p) = self.pads.get_mut(i) {
                                *p = Pad {
                                    tune: tune.clamp(-24, 24),
                                    choke: choke.min(CHOKE_GROUPS),
                                };
                            }
                        }
                    }
                }
                _ => {
                    let id = self.infos.iter().find(|p| p.name == name).map(|p| p.id);
                    if let (Some(id), Ok(v)) = (id, value.trim().parse::<f32>()) {
                        self.params.set(id as usize, v);
                    }
                }
            }
        }
        self.rebuild();
        Ok(())
    }
    fn latency_samples(&self) -> u32 {
        0
    }
    fn has_audio_input(&self) -> bool {
        false
    }
    fn idle(&mut self) {
        self.flush();
    }
    fn sampler(&mut self) -> Option<&mut Sampler> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(frames: usize) -> Arc<SampleData> {
        let ramp = (0..frames).map(|i| (i as f32 * 0.37).sin()).collect();
        Arc::new(SampleData::new(48_000, vec![ramp]).unwrap())
    }

    fn handle() -> (Box<dyn Hosted>, Box<dyn AudioNode>) {
        instantiate()
    }

    /// Runs the node for a block so it takes the latest zone list.
    fn tick(node: &mut dyn AudioNode) {
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            num_samples: 8,
            playhead: 0,
            playing: false,
            ..Default::default()
        };
        let (mut l, mut r) = ([0.0; 8], [0.0; 8]);
        node.process(&[], &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx);
    }

    #[test]
    fn zones_split_keys_and_velocities_and_round_robin_shares_a_cell() {
        let (mut h, _) = handle();
        let s = h.sampler().unwrap();
        s.add_layer("a.wav".into(), LayerKind::Replace);
        s.add_layer("b.wav".into(), LayerKind::RoundRobin);
        s.add_layer("c.wav".into(), LayerKind::Velocity);
        s.add_layer("d.wav".into(), LayerKind::Key);
        assert_eq!(s.missing().len(), 4);
        for p in ["a.wav", "b.wav", "c.wav", "d.wav"] {
            s.set_audio(Path::new(p), &audio(100));
        }
        assert!(s.missing().is_empty());
        let zones: Vec<_> = s
            .zones()
            .iter()
            .map(|z| (z.keys, z.velocities, z.cell, z.root))
            .collect();
        assert_eq!(
            zones,
            [
                // Kick and its alternate share soft hits; the hard kick has the rest.
                ((0, 63), (1, 63), 0, 31),
                ((0, 63), (1, 63), 0, 31),
                ((0, 63), (64, 127), 1, 31),
                // A new key zone covers every velocity.
                ((64, 127), (1, 127), 128, 95),
            ]
        );
        // Kick, then a snare key zone with a hard layer: hard kicks still play.
        s.add_layer("kick.wav".into(), LayerKind::Replace);
        s.add_layer("snare.wav".into(), LayerKind::Key);
        s.add_layer("snare-hard.wav".into(), LayerKind::Velocity);
        for p in ["kick.wav", "snare.wav", "snare-hard.wav"] {
            s.set_audio(Path::new(p), &audio(100));
        }
        let velocities: Vec<_> = s.zones().iter().map(|z| z.velocities).collect();
        assert_eq!(velocities, [(1, 127), (1, 63), (64, 127)]);
        assert_eq!(split(0, 2, 0), (0, 63));
        assert_eq!(split(0, 2, 1), (64, 127));
        assert_eq!(split(1, 2, 0), (1, 63));
        assert_eq!(split(1, 2, 1), (64, 127));
        assert_eq!(split(1, 1, 0), (1, 127));
    }

    #[test]
    fn slice_mode_maps_slices_to_keys_and_markers_switch_to_manual() {
        let (mut h, mut node) = handle();
        assert!(h.set_param(MODE as u32, 2.0));
        assert!(h.set_param(SLICER as u32, 1.0));
        assert!(h.set_param(GRID as u32, 4.0));
        let s = h.sampler().unwrap();
        s.add_layer("loop.wav".into(), LayerKind::Replace);
        s.set_audio(Path::new("loop.wav"), &audio(4_000));
        assert_eq!(s.slice_points().len(), 4);
        s.edit_marker(0.1, true);
        assert_eq!(s.param(SLICER), 2.0, "editing switched to manual");
        assert_eq!(s.slice_points().len(), 5, "kept the grid and added one");
        s.edit_marker(0.12, false);
        assert_eq!(s.slice_points().len(), 4, "removed the nearest");
        for _ in 0..10 {
            tick(&mut *node);
            h.idle();
        }
        let s = h.sampler().unwrap();
        assert_eq!(s.out, 0, "every list came back");
        assert!(s.pending.is_none());
    }

    #[test]
    fn state_round_trips_layers_markers_and_params() {
        let (mut a, _) = handle();
        a.set_param(ROOT as u32, 48.0);
        let s = a.sampler().unwrap();
        s.add_layer("/x/kick=1.wav".into(), LayerKind::Replace);
        s.add_layer("/x/kick2.wav".into(), LayerKind::RoundRobin);
        s.markers = vec![0.5];
        let state = a.save_state().unwrap();
        let (mut b, _) = handle();
        b.load_state(&state).unwrap();
        assert_eq!(b.param_value(ROOT as u32), Some(48.0));
        let s = b.sampler().unwrap();
        assert_eq!(
            s.missing(),
            [PathBuf::from("/x/kick=1.wav"), "/x/kick2.wav".into()]
        );
        assert_eq!(s.markers, [0.5]);
        assert_eq!(s.pads, [Pad::default(); PADS]);
        b.load_state(b"LAYER=x\nLAYER=1,2\nMARKER=nope\nGRID=abc")
            .unwrap();
        assert!(
            b.sampler().unwrap().missing().is_empty(),
            "bad lines skipped"
        );
        assert!(b.load_state(&[0xff]).is_err());
    }

    #[test]
    fn a_kit_puts_each_key_zone_on_its_own_pad_with_its_tune_and_choke() {
        let (mut a, _) = handle();
        a.set_param(MODE as u32, 3.0);
        a.set_param(ROOT as u32, 36.0);
        let s = a.sampler().unwrap();
        s.add_layer("kick.wav".into(), LayerKind::Replace);
        s.add_layer("closed.wav".into(), LayerKind::Key);
        s.add_layer("open.wav".into(), LayerKind::Key);
        for p in ["kick.wav", "closed.wav", "open.wav"] {
            s.set_audio(Path::new(p), &audio(100));
        }
        s.set_pad(1, Pad { tune: 3, choke: 1 });
        s.set_pad(
            2,
            Pad {
                tune: -99,
                choke: 9,
            },
        );
        assert_eq!(s.kit_pads(), 3);
        let zones: Vec<_> = s
            .zones()
            .iter()
            .map(|z| (z.keys, z.root, z.tune, z.choke, z.one_shot))
            .collect();
        assert_eq!(
            zones,
            [
                ((36, 36), 36, 0.0, 0, true),
                ((37, 37), 37, 3.0, 1, true),
                ((38, 38), 38, -24.0, CHOKE_GROUPS, true),
            ]
        );
        // Pads survive a save.
        let state = a.save_state().unwrap();
        let (mut b, _) = handle();
        b.load_state(&state).unwrap();
        assert_eq!(b.sampler().unwrap().pads()[1], Pad { tune: 3, choke: 1 });
        b.load_state(b"PAD=99,1,1\nPAD=0,x,1").unwrap();
        assert_eq!(b.sampler().unwrap().pads(), [Pad::default(); PADS]);
    }

    #[test]
    fn queue_never_overfills_when_the_node_is_not_running() {
        let (mut h, mut node) = handle();
        let s = h.sampler().unwrap();
        for i in 0..20 {
            s.add_layer(format!("{i}.wav").into(), LayerKind::RoundRobin);
        }
        assert_eq!(s.out, QUEUE, "stopped at capacity");
        assert!(s.pending.is_some(), "the newest list waits");
        for _ in 0..QUEUE + 1 {
            tick(&mut *node);
            h.idle();
        }
        let s = h.sampler().unwrap();
        assert!(s.pending.is_none());
        assert_eq!(s.out, 0, "every list came back; the node plays the newest");
    }
}
