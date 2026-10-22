//! One front for every format: find installed plugins and instantiate any of
//! them behind the same `Hosted` handle and `AudioNode`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clack_host::prelude::PluginEntry;
use daw_graph::AudioNode;

use crate::{ClapPlugin, HostError, ParamInfo, PluginInfo, Vst3Bundle, Vst3Plugin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Clap,
    AudioUnit,
    Vst3,
}

/// An installed plugin: its format, the bundle it lives in (none for Audio Units,
/// which the system registry finds) and its id within that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub format: Format,
    pub bundle: Option<PathBuf>,
    pub info: PluginInfo,
}

/// A plugin's main-thread handle and its node for the graph.
pub type Instance = (Box<dyn Hosted>, Box<dyn AudioNode>);

/// Main-thread controls every hosted plugin offers.
pub trait Hosted {
    fn params(&self) -> &[ParamInfo];
    fn param_value(&mut self, id: u32) -> Option<f64>;
    /// False for an unknown id, a non-finite value or a full queue.
    fn set_param(&mut self, id: u32, value: f64) -> bool;
    fn save_state(&mut self) -> Result<Vec<u8>, HostError>;
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), HostError>;
    fn latency_samples(&self) -> u32;
    /// False for instruments.
    fn has_audio_input(&self) -> bool;
    /// Main-thread work the plugin asked for; call regularly.
    fn idle(&mut self) {}
}

impl Hosted for ClapPlugin {
    fn params(&self) -> &[ParamInfo] {
        ClapPlugin::params(self)
    }
    fn param_value(&mut self, id: u32) -> Option<f64> {
        ClapPlugin::param_value(self, id)
    }
    fn set_param(&mut self, id: u32, value: f64) -> bool {
        ClapPlugin::set_param(self, id, value)
    }
    fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        ClapPlugin::save_state(self)
    }
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        ClapPlugin::load_state(self, bytes)
    }
    fn latency_samples(&self) -> u32 {
        ClapPlugin::latency_samples(self)
    }
    fn has_audio_input(&self) -> bool {
        ClapPlugin::has_audio_input(self)
    }
    fn idle(&mut self) {
        ClapPlugin::idle(self)
    }
}

impl Hosted for Vst3Plugin {
    fn params(&self) -> &[ParamInfo] {
        Vst3Plugin::params(self)
    }
    fn param_value(&mut self, id: u32) -> Option<f64> {
        Vst3Plugin::param_value(self, id)
    }
    fn set_param(&mut self, id: u32, value: f64) -> bool {
        Vst3Plugin::set_param(self, id, value)
    }
    fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        Vst3Plugin::save_state(self)
    }
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        Vst3Plugin::load_state(self, bytes)
    }
    fn latency_samples(&self) -> u32 {
        Vst3Plugin::latency_samples(self)
    }
    fn has_audio_input(&self) -> bool {
        Vst3Plugin::has_audio_input(self)
    }
}

#[cfg(target_os = "macos")]
impl Hosted for crate::AuPlugin {
    fn params(&self) -> &[ParamInfo] {
        crate::AuPlugin::params(self)
    }
    fn param_value(&mut self, id: u32) -> Option<f64> {
        crate::AuPlugin::param_value(self, id)
    }
    fn set_param(&mut self, id: u32, value: f64) -> bool {
        crate::AuPlugin::set_param(self, id, value)
    }
    fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        crate::AuPlugin::save_state(self)
    }
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        crate::AuPlugin::load_state(self, bytes)
    }
    fn latency_samples(&self) -> u32 {
        crate::AuPlugin::latency_samples(self)
    }
    fn has_audio_input(&self) -> bool {
        self.kind() != crate::AuKind::Instrument
    }
}

/// Installed plugins, and the bundles already loaded to make them.
#[derive(Default)]
pub struct Catalog {
    pub available: Vec<Available>,
    clap: HashMap<PathBuf, PluginEntry>,
    vst3: HashMap<PathBuf, Arc<Vst3Bundle>>,
}

/// Where macOS keeps plugins of a format, system-wide then per user.
fn folders(kind: &str) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Library/Audio/Plug-Ins").join(kind)];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(
            PathBuf::from(home)
                .join("Library/Audio/Plug-Ins")
                .join(kind),
        );
    }
    dirs
}

fn bundles(dirs: &[PathBuf], extension: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .collect();
    found.sort();
    found
}

impl Catalog {
    /// Lists installed plugins. Loading a bundle runs its code, so a broken plugin
    /// can crash the scan; bundles that fail to load are skipped.
    pub fn scan() -> Self {
        let mut c = Catalog::default();
        #[cfg(target_os = "macos")]
        c.available
            .extend(crate::audio_units().into_iter().map(|(info, _)| Available {
                format: Format::AudioUnit,
                bundle: None,
                info,
            }));
        for path in bundles(&folders("CLAP"), "clap") {
            // SAFETY: plugins in the system plugin folders were installed by the user.
            if let Ok(entry) = unsafe { crate::load_bundle(&path) } {
                c.list(Format::Clap, &path, crate::plugins(&entry));
                c.clap.insert(path, entry);
            }
        }
        for path in bundles(&folders("VST3"), "vst3") {
            // SAFETY: as above.
            #[cfg(target_os = "macos")]
            if let Ok(bundle) = unsafe { Vst3Bundle::load(&path) } {
                c.list(Format::Vst3, &path, bundle.plugins());
                c.vst3.insert(path, bundle);
            }
        }
        c
    }

    fn list(&mut self, format: Format, bundle: &Path, plugins: Vec<PluginInfo>) {
        self.available
            .extend(plugins.into_iter().map(|info| Available {
                format,
                bundle: Some(bundle.to_path_buf()),
                info,
            }));
    }

    /// Makes an instance of a plugin found by [`Catalog::scan`] (or saved in a
    /// project) for blocks of up to `max_block` frames.
    pub fn instantiate(
        &mut self,
        format: Format,
        bundle: Option<&Path>,
        id: &str,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<Instance, HostError> {
        let missing = || HostError::UnknownPlugin(id.to_owned());
        match format {
            Format::Clap => {
                let path = bundle.ok_or_else(missing)?;
                if !self.clap.contains_key(path) {
                    // SAFETY: the user chose this plugin.
                    let entry = unsafe { crate::load_bundle(path) }?;
                    self.clap.insert(path.to_path_buf(), entry);
                }
                let (p, n) = ClapPlugin::new(&self.clap[path], id, sample_rate, max_block)?;
                Ok((Box::new(p), Box::new(n)))
            }
            Format::Vst3 => {
                let path = bundle.ok_or_else(missing)?;
                if !self.vst3.contains_key(path) {
                    #[cfg(target_os = "macos")]
                    // SAFETY: the user chose this plugin.
                    self.vst3
                        .insert(path.to_path_buf(), unsafe { Vst3Bundle::load(path) }?);
                }
                let bundle = self.vst3.get(path).ok_or_else(missing)?;
                let (p, n) = Vst3Plugin::new(bundle, id, sample_rate, max_block)?;
                Ok((Box::new(p), Box::new(n)))
            }
            #[cfg(target_os = "macos")]
            Format::AudioUnit => {
                let (p, n) = crate::AuPlugin::new(id, sample_rate, max_block)?;
                Ok((Box::new(p), Box::new(n)))
            }
            #[cfg(not(target_os = "macos"))]
            Format::AudioUnit => Err(missing()),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_apple_units_and_instantiates_through_one_interface() {
        let mut c = Catalog::scan();
        let delay = c
            .available
            .iter()
            .find(|a| a.info.id == "aufx:dely:appl")
            .expect("AUDelay")
            .clone();
        assert_eq!(
            (delay.format, delay.bundle.as_deref()),
            (Format::AudioUnit, None)
        );
        let (mut plugin, node) = c
            .instantiate(delay.format, None, &delay.info.id, 48_000.0, 256)
            .unwrap();
        assert!(plugin.has_audio_input() && !plugin.params().is_empty());
        let p = plugin.params()[0].clone();
        assert!(plugin.set_param(p.id, p.min));
        assert_eq!(plugin.param_value(p.id), Some(p.min));
        let state = plugin.save_state().unwrap();
        plugin.load_state(&state).unwrap();
        assert_eq!(node.latency_samples(), plugin.latency_samples());

        let (dls, _) = c
            .instantiate(Format::AudioUnit, None, "aumu:dls :appl", 48_000.0, 256)
            .unwrap();
        assert!(!dls.has_audio_input(), "instrument");
        assert!(matches!(
            c.instantiate(Format::Clap, None, "x", 48_000.0, 256),
            Err(HostError::UnknownPlugin(_))
        ));
    }
}
