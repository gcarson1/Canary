//! A tiny CLAP plugin for the host tests: stereo gain (param 1, 0..=4) plus 1.0
//! added to the output while a note is held. Reports 32 samples of latency and
//! saves its gain as state. Counts live instances so tests can see it destroyed.

use std::ffi::CStr;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
    PluginAudioPortsImpl,
};
use clack_extensions::latency::{PluginLatency, PluginLatencyImpl};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use clack_plugin::utils::Cookie;

pub const ID: &str = "dev.canary.test-gain";
pub const GAIN: u32 = 1;
pub const LATENCY: u32 = 32;
pub static LIVE: AtomicUsize = AtomicUsize::new(0);

pub struct TestGain;

pub struct Shared {
    gain: AtomicU64,
}

impl Shared {
    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn set_gain(&self, gain: f64) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn apply(&self, events: &InputEvents) {
        for e in events {
            if let Some(p) = e.as_event::<ParamValueEvent>() {
                if p.param_id() == Some(ClapId::new(GAIN)) {
                    self.set_gain(p.value());
                }
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PluginShared<'_> for Shared {}

pub struct Main<'a> {
    shared: &'a Shared,
}

impl<'a> PluginMainThread<'a, Shared> for Main<'a> {}

pub struct Processor<'a> {
    shared: &'a Shared,
    held: bool,
}

impl Plugin for TestGain {
    type AudioProcessor<'a> = Processor<'a>;
    type Shared<'a> = Shared;
    type MainThread<'a> = Main<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&Shared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginLatency>();
    }
}

impl DefaultPluginFactory for TestGain {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new(ID, "Test Gain")
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Shared, PluginError> {
        LIVE.fetch_add(1, Ordering::SeqCst);
        Ok(Shared {
            gain: AtomicU64::new(1.0f64.to_bits()),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Shared,
    ) -> Result<Main<'a>, PluginError> {
        Ok(Main { shared })
    }
}

impl<'a> PluginAudioProcessor<'a, Shared, Main<'a>> for Processor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main: &Main<'a>,
        shared: &'a Shared,
        _config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            shared,
            held: false,
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        self.shared.apply(events.input);
        let gain = self.shared.gain() as f32;
        let start = self.held;
        for mut port in &mut audio {
            let Some(pairs) = port.channels()?.into_f32() else {
                continue;
            };
            for pair in pairs {
                let ChannelPair::InputOutput(input, output) = pair else {
                    continue;
                };
                let mut held = start;
                let mut notes = events.input.into_iter().peekable();
                for (i, (o, s)) in output.iter_mut().zip(input.iter()).enumerate() {
                    while let Some(e) = notes.next_if(|e| e.header().time() as usize <= i) {
                        if e.as_event::<NoteOnEvent>().is_some() {
                            held = true;
                        } else if e.as_event::<NoteOffEvent>().is_some() {
                            held = false;
                        }
                    }
                    *o = s * gain + if held { 1.0 } else { 0.0 };
                }
                self.held = held;
            }
        }
        Ok(ProcessStatus::Continue)
    }
}

impl PluginAudioPortsImpl for Main<'_> {
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    fn get(&self, index: u32, _is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"Main",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginMainThreadParams for Main<'_> {
    fn count(&self) -> u32 {
        1
    }

    fn get_info(&self, index: u32, info: &mut ParamInfoWriter) {
        if index == 0 {
            info.set(&ParamInfo {
                id: ClapId::new(GAIN),
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Cookie::empty(),
                name: b"Gain",
                module: b"",
                min_value: 0.0,
                max_value: 4.0,
                default_value: 1.0,
            });
        }
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        (id == ClapId::new(GAIN)).then(|| self.shared.gain())
    }

    fn value_to_text(
        &self,
        _id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        use std::fmt::Write as _;
        write!(writer, "{value:.2}")
    }

    fn text_to_value(&self, _id: ClapId, text: &CStr) -> Option<f64> {
        text.to_str().ok()?.parse().ok()
    }

    fn flush(&self, input: &InputEvents, _output: &mut OutputEvents) {
        self.shared.apply(input);
    }
}

impl PluginAudioProcessorParams for Processor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        self.shared.apply(input);
    }
}

impl PluginStateImpl for Main<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        output.write_all(&self.shared.gain().to_le_bytes())?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut bytes = [0; 8];
        input.read_exact(&mut bytes)?;
        self.shared.set_gain(f64::from_le_bytes(bytes));
        Ok(())
    }
}

impl PluginLatencyImpl for Main<'_> {
    fn get(&self) -> u32 {
        LATENCY
    }
}
