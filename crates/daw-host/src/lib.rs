//! Third-party plugin hosting. Plugins run in-process for now behind `AudioNode`, so
//! moving them out of process later (ARCHITECTURE.md section 2) changes nothing above.

mod clap;

pub use clap::{load_bundle, plugins, ClapPlugin, HostError, ParamInfo, PluginInfo, PluginNode};

#[cfg(test)]
mod test_plugin;
