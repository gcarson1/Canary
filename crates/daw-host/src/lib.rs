//! Third-party plugin hosting. Plugins run in-process for now behind `AudioNode`, so
//! moving them out of process later (ARCHITECTURE.md section 2) changes nothing above.

pub mod ara;
#[cfg(target_os = "macos")]
mod au;
mod builtin;
mod catalog;
mod clap;
mod sampler;
mod vst3;

#[cfg(target_os = "macos")]
pub use au::{audio_units, AuKind, AuNode, AuPlugin};
pub use catalog::{Available, Catalog, Format, Hosted, Instance};
pub use clap::{load_bundle, plugins, ClapPlugin, HostError, ParamInfo, PluginInfo, PluginNode};
pub use sampler::{LayerKind, Sampler};
pub use vst3::{Vst3Bundle, Vst3Node, Vst3Plugin};

#[cfg(test)]
mod test_plugin;

/// Automated parameter values a plugin node collects before its next block:
/// fixed capacity, one entry per parameter (the latest wins), never allocates.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Automated {
    values: [(u32, f64); 64],
    len: usize,
}

impl Default for Automated {
    fn default() -> Self {
        Self {
            values: [(0, 0.0); 64],
            len: 0,
        }
    }
}

impl Automated {
    /// Records a value; parameters past the 64th in one block are dropped.
    pub(crate) fn set(&mut self, id: u32, value: f64) {
        if !value.is_finite() {
            return;
        }
        let len = self.len;
        if let Some(slot) = self.values[..len].iter_mut().find(|v| v.0 == id) {
            slot.1 = value;
        } else if len < self.values.len() {
            self.values[len] = (id, value);
            self.len += 1;
        }
    }

    /// The values recorded since the last take, emptied.
    pub(crate) fn take(&mut self) -> impl Iterator<Item = (u32, f64)> + '_ {
        let len = std::mem::take(&mut self.len);
        self.values[..len].iter().copied()
    }
}

#[cfg(test)]
mod automated_tests {
    use super::Automated;

    #[test]
    fn keeps_the_latest_value_per_parameter_and_empties_on_take() {
        let mut a = Automated::default();
        a.set(3, 0.1);
        a.set(5, 0.2);
        a.set(3, 0.9);
        a.set(7, f64::NAN);
        assert_eq!(a.take().collect::<Vec<_>>(), [(3, 0.9), (5, 0.2)]);
        assert_eq!(a.take().count(), 0);
        for id in 0..100 {
            a.set(id, 1.0);
        }
        assert_eq!(a.take().count(), 64);
    }
}
