//! Canary's own effects behind the same `Hosted` handle as third-party plugins, so
//! the rack, picker and project treat them alike.

use std::sync::Arc;

use daw_dsp::effects::{effect, EFFECTS};
use daw_graph::{EffectNode, ParamBank};

use crate::{HostError, Hosted, Instance, ParamInfo, PluginInfo};

/// The built-in effects, for the catalog.
pub fn builtins() -> Vec<PluginInfo> {
    [(crate::sampler::ID, "Sampler")]
        .iter()
        .chain(&EFFECTS)
        .map(|(id, name)| PluginInfo {
            id: (*id).into(),
            name: (*name).into(),
            vendor: "Canary".into(),
        })
        .collect()
}

pub fn instantiate(id: &str, sample_rate: f32, max_block: usize) -> Result<Instance, HostError> {
    if id == crate::sampler::ID {
        return Ok(crate::sampler::instantiate());
    }
    let fx = effect(id, sample_rate).ok_or_else(|| HostError::UnknownPlugin(id.into()))?;
    let (node, params) = EffectNode::new(fx, sample_rate, max_block);
    let infos = params
        .specs()
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
    Ok((Box::new(Builtin { params, infos }), Box::new(node)))
}

struct Builtin {
    params: Arc<ParamBank>,
    infos: Vec<ParamInfo>,
}

impl Hosted for Builtin {
    fn params(&self) -> &[ParamInfo] {
        &self.infos
    }
    fn param_value(&mut self, id: u32) -> Option<f64> {
        self.params.get(id as usize).map(f64::from)
    }
    fn set_param(&mut self, id: u32, value: f64) -> bool {
        self.params.set(id as usize, value as f32)
    }
    /// `NAME=value` lines, so state survives parameters being added or reordered.
    fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        let lines: Vec<String> = self
            .infos
            .iter()
            .filter_map(|p| Some(format!("{}={}", p.name, self.params.get(p.id as usize)?)))
            .collect();
        Ok(lines.join("\n").into_bytes())
    }
    /// Unknown names and bad values are skipped; parameters not named keep theirs.
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        let text = std::str::from_utf8(bytes).map_err(|_| HostError::State)?;
        for (name, value) in text.lines().filter_map(|l| l.split_once('=')) {
            let id = self.infos.iter().find(|p| p.name == name).map(|p| p.id);
            if let (Some(id), Ok(v)) = (id, value.trim().parse::<f32>()) {
                self.params.set(id as usize, v);
            }
        }
        Ok(())
    }
    fn latency_samples(&self) -> u32 {
        0
    }
    fn has_audio_input(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_instantiates_and_state_round_trips() {
        for info in builtins() {
            let (mut a, _) = instantiate(&info.id, 48_000.0, 64).unwrap();
            let p = a.params()[0].clone();
            assert!(a.set_param(p.id, p.max));
            let state = a.save_state().unwrap();
            let (mut b, _) = instantiate(&info.id, 48_000.0, 64).unwrap();
            b.load_state(&state).unwrap();
            assert_eq!(b.param_value(p.id), Some(p.max), "{}", info.id);
        }
        assert!(instantiate("canary.nope", 48_000.0, 64).is_err());
    }

    #[test]
    fn every_builtin_processes_without_allocating() {
        use daw_core::{MidiEvent, ProcessContext};
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            num_samples: 48,
            playhead: 0,
            playing: true,
        };
        let input = [0.5f32; 64];
        let note = [MidiEvent {
            sample_offset: 3,
            data: [0x90, 60, 100],
        }];
        for info in builtins() {
            let (mut handle, mut node) = instantiate(&info.id, 48_000.0, 64).unwrap();
            for p in handle.params().to_vec() {
                handle.set_param(p.id, (p.min + p.max) / 2.0);
            }
            if let Some(s) = handle.sampler() {
                // Something to play, so voices run too.
                let audio = daw_core::SampleData::new(48_000, vec![vec![0.3; 4_800]]).unwrap();
                s.add_layer("x.wav".into(), crate::LayerKind::Replace);
                s.set_audio(std::path::Path::new("x.wav"), &Arc::new(audio));
            }
            let (mut l, mut r) = ([0.0f32; 48], [0.0f32; 48]);
            let mut midi_out = Vec::with_capacity(16);
            for _ in 0..8 {
                assert_no_alloc::assert_no_alloc(|| {
                    node.process(
                        &[&input[..48], &input[..48]],
                        &mut [&mut l, &mut r],
                        &note,
                        &mut midi_out,
                        &ctx,
                    );
                });
                handle.idle(); // the sampler's zone lists go back and forth
            }
            assert!(l.iter().chain(&r).all(|x| x.is_finite()), "{}", info.id);
            if info.id == crate::sampler::ID {
                assert!(l.iter().any(|&x| x != 0.0), "the sampler played");
            }
        }
    }

    #[test]
    fn load_state_skips_junk_and_rejects_non_text() {
        let (mut fx, _) = instantiate("canary.compressor", 48_000.0, 64).unwrap();
        fx.load_state(b"THRESH=-30\nNOPE=1\nRATIO=abc\nno equals")
            .unwrap();
        assert_eq!(fx.param_value(0), Some(-30.0));
        assert_eq!(fx.param_value(1), Some(4.0), "bad value left the default");
        assert!(fx.load_state(&[0xff, 0xfe]).is_err());
    }
}
