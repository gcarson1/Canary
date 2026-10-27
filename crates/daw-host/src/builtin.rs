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
    fn load_state_skips_junk_and_rejects_non_text() {
        let (mut fx, _) = instantiate("canary.compressor", 48_000.0, 64).unwrap();
        fx.load_state(b"THRESH=-30\nNOPE=1\nRATIO=abc\nno equals")
            .unwrap();
        assert_eq!(fx.param_value(0), Some(-30.0));
        assert_eq!(fx.param_value(1), Some(4.0), "bad value left the default");
        assert!(fx.load_state(&[0xff, 0xfe]).is_err());
    }
}
