//! Third-party plugin hosting. Plugins run in-process for now behind `AudioNode`, so
//! moving them out of process later (ARCHITECTURE.md section 2) changes nothing above.

#[cfg(target_os = "macos")]
mod au;
mod clap;
mod vst3;

#[cfg(target_os = "macos")]
pub use au::{audio_units, AuKind, AuNode, AuPlugin};
pub use clap::{load_bundle, plugins, ClapPlugin, HostError, ParamInfo, PluginInfo, PluginNode};
pub use vst3::{Vst3Bundle, Vst3Node, Vst3Plugin};

#[cfg(test)]
mod test_plugin;
