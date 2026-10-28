use std::ffi::{c_void, CStr, CString};
use std::fmt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
use clack_extensions::latency::PluginLatency;
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_extensions::state::PluginState;
use clack_host::entry::PluginEntryError;
use clack_host::events::event_types::{
    MidiEvent as ClapMidi, NoteOffEvent, NoteOnEvent, ParamValueEvent,
};
use clack_host::events::Match;
use clack_host::prelude::*;
use daw_core::{MidiEvent, ProcessContext};
use daw_graph::{AudioNode, MAX_CHANNELS};
use rtrb::{Consumer, Producer, RingBuffer};

/// Parameter changes waiting for the audio thread.
const PARAM_QUEUE_CAPACITY: usize = 256;
/// Events one block can carry to the plugin: a full MIDI block plus a full param queue.
const EVENT_CAPACITY: usize = 1024 + PARAM_QUEUE_CAPACITY;

#[derive(Debug)]
pub enum HostError {
    Load(PluginEntryError),
    NoFactory,
    UnknownPlugin(String),
    Instance(PluginInstanceError),
    /// More audio channels on one port than a node can carry.
    TooManyChannels(u32),
    /// More than one input or output port (a sidechain, say); not routed yet.
    ExtraPorts,
    State,
    /// An Audio Unit call failed with this OSStatus.
    Status(i32),
    /// The plugin has no editor this host can embed (an `NSView`).
    NoEditor,
    /// The plugin has no usable ARA 2 factory, or its document controller failed.
    NoAra,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(e) => write!(f, "could not load the plugin bundle: {e}"),
            Self::NoFactory => write!(f, "the bundle has no plugin factory"),
            Self::UnknownPlugin(id) => write!(f, "no plugin with id {id} in the bundle"),
            Self::Instance(e) => write!(f, "plugin failed: {e}"),
            Self::TooManyChannels(n) => {
                write!(
                    f,
                    "plugin port has {n} channels, the limit is {MAX_CHANNELS}"
                )
            }
            Self::ExtraPorts => write!(
                f,
                "plugins with more than one input or output port are not supported yet"
            ),
            Self::State => write!(f, "plugin could not save or load its state"),
            Self::Status(s) => write!(f, "Audio Unit call failed (OSStatus {s})"),
            Self::NoEditor => write!(f, "the plugin has no editor to show"),
            Self::NoAra => write!(f, "the plugin does not support ARA 2"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<PluginInstanceError> for HostError {
    fn from(e: PluginInstanceError) -> Self {
        Self::Instance(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// Turns logarithmically (a frequency, say).
    pub log: bool,
}

impl ParamInfo {
    /// A parameter with a usable range: `None` if a bound is not finite, bounds
    /// given backwards are swapped, and the default is kept inside them. Plugins
    /// are untrusted; a bad range must not reach code that clamps to it.
    pub fn new(id: u32, name: String, min: f64, max: f64, default: f64) -> Option<Self> {
        if !(min.is_finite() && max.is_finite()) {
            return None;
        }
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        let default = if default.is_finite() {
            default.clamp(min, max)
        } else {
            min
        };
        Some(Self {
            id,
            name,
            min,
            max,
            default,
            log: false,
        })
    }
}

/// Loads a `.clap` bundle. Unsafe because it runs the bundle's code in this process;
/// only load plugins the user chose.
///
/// # Safety
/// The bundle must be a compliant CLAP plugin.
pub unsafe fn load_bundle(path: &Path) -> Result<PluginEntry, HostError> {
    // SAFETY: upheld by the caller.
    unsafe { PluginEntry::load(path) }.map_err(HostError::Load)
}

/// The plugins a bundle offers.
pub fn plugins(entry: &PluginEntry) -> Vec<PluginInfo> {
    let text = |s: Option<&CStr>| {
        s.map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    entry
        .get_plugin_factory()
        .map(|f| {
            f.plugin_descriptors()
                .map(|d| PluginInfo {
                    id: text(d.id()),
                    name: text(d.name()),
                    vendor: text(d.vendor()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Host callbacks. Requests are flags the main thread acts on in `ClapPlugin::idle`.
struct CanaryHost;

#[derive(Default)]
struct Requests {
    callback: AtomicBool,
    /// A size the editor asked for, `width << 32 | height`; 0 when none.
    resize: AtomicU64,
}

/// Editor requests may come from any thread; the size waits for the main thread.
impl HostGuiImpl for Requests {
    fn resize_hints_changed(&self) {}
    fn request_resize(&self, size: GuiSize) -> Result<(), clack_host::prelude::HostError> {
        let packed = u64::from(size.width) << 32 | u64::from(size.height);
        self.resize.store(packed.max(1), Ordering::Release);
        Ok(())
    }
    fn request_show(&self) -> Result<(), clack_host::prelude::HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), clack_host::prelude::HostError> {
        Ok(())
    }
    // shortcut: embedded editors only; a lost connection shows as a blank window
    // until the user closes it.
    fn closed(&self, _was_destroyed: bool) {}
}

impl SharedHandler<'_> for Requests {
    fn request_restart(&self) {} // shortcut: restarting (re-activation) is not wired yet
    fn request_process(&self) {} // nodes always process, so nothing to wake
    fn request_callback(&self) {
        self.callback.store(true, Ordering::Release);
    }
}

impl HostHandlers for CanaryHost {
    type Shared<'a> = Requests;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostGui>();
    }
}

/// Main-thread side of one hosted CLAP plugin: parameters, state and callbacks.
/// Not `Send`; it stays on the main thread while its `PluginNode` runs in the graph.
///
/// Remove the node from the graph and collect garbage before dropping this, so the
/// plugin is deactivated and destroyed; dropped first, the plugin is leaked.
pub struct ClapPlugin {
    instance: PluginInstance<CanaryHost>,
    params: Vec<ParamInfo>,
    param_changes: Producer<(u32, f64)>,
    latency: u32,
    /// Has an audio input port; without one it is an instrument.
    audio_input: bool,
    /// The editor is created and embedded.
    editor: bool,
    /// The ARA extension, when created bound to a document.
    ara: Option<NonNull<c_void>>,
}

/// `clap_ara_plugin_extension` (ARACLAP.h).
#[repr(C)]
struct ClapAraExtension {
    get_factory: unsafe extern "C" fn(plugin: *const c_void) -> *const c_void,
    bind_to_document_controller:
        unsafe extern "C" fn(*const c_void, *mut c_void, i32, i32) -> *const c_void,
}

/// The plugin's ARA extension, if it has one.
fn ara_extension(instance: &PluginInstance<CanaryHost>) -> Option<&ClapAraExtension> {
    let raw = instance.raw_instance();
    let get = raw.get_extension?;
    // SAFETY: the CLAP plugin's own lookup with ARACLAP.h's id.
    let ext = unsafe { get(raw, c"org.ara-audio.ara.pluginextension/2".as_ptr()) };
    // SAFETY: a non-null result has the ARACLAP layout and lives with the plugin.
    unsafe { ext.cast::<ClapAraExtension>().as_ref() }
}

const EMBEDDED_COCOA: GuiConfiguration = GuiConfiguration {
    api_type: GuiApiType::COCOA,
    is_floating: false,
};

impl ClapPlugin {
    /// Instantiates plugin `id` from `entry` and activates it for blocks of up to
    /// `max_block` frames, returning the main-thread handle and the node to put in a
    /// plan. Everything is allocated here, on the main thread.
    pub fn new(
        entry: &PluginEntry,
        id: &str,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<(ClapPlugin, PluginNode), HostError> {
        Self::new_ara(entry, id, sample_rate, max_block, None)
    }

    /// As `new`, bound to ARA document controller `ara.0` first if given, since
    /// ARA requires binding before the plugin is activated; `ara.1` gets the
    /// extension while the plugin is still inactive.
    pub fn new_ara(
        entry: &PluginEntry,
        id: &str,
        sample_rate: f32,
        max_block: usize,
        ara: Option<crate::ara::AraBind<'_>>,
    ) -> Result<(ClapPlugin, PluginNode), HostError> {
        let factory = entry.get_plugin_factory().ok_or(HostError::NoFactory)?;
        let unknown = || HostError::UnknownPlugin(id.to_owned());
        let cid = CString::new(id).map_err(|_| unknown())?;
        if !factory
            .plugin_descriptors()
            .any(|d| d.id() == Some(cid.as_c_str()))
        {
            return Err(unknown());
        }
        let host_info = HostInfo::new(
            "Canary",
            "Canary",
            "https://github.com/gcarson1/Canary",
            env!("CARGO_PKG_VERSION"),
        )
        .expect("host info has no NUL bytes");
        let mut instance = PluginInstance::<CanaryHost>::new(
            |_| Requests::default(),
            |_| (),
            entry,
            &cid,
            &host_info,
        )?;

        let (inputs, outputs) = {
            let handle = instance.plugin_handle();
            // Channels of the single port of each kind, or None for no port. The
            // plugin must be handed exactly the ports it declared.
            let channels = |is_input| -> Result<Option<u32>, HostError> {
                let Some(ports) = handle.get_extension::<PluginAudioPorts>() else {
                    return Ok(None);
                };
                match ports.count(&handle, is_input) {
                    0 => return Ok(None),
                    1 => {}
                    _ => return Err(HostError::ExtraPorts),
                }
                let mut buffer = AudioPortInfoBuffer::new();
                let n = ports
                    .get(&handle, 0, is_input, &mut buffer)
                    .map_or(0, |p| p.channel_count);
                if n as usize > MAX_CHANNELS {
                    return Err(HostError::TooManyChannels(n));
                }
                Ok(Some(n))
            };
            (channels(true)?, channels(false)?)
        };

        let max_frames = u32::try_from(max_block).unwrap_or(u32::MAX);
        let config = PluginAudioConfiguration {
            sample_rate: f64::from(sample_rate),
            min_frames_count: 1,
            max_frames_count: max_frames,
        };
        let ara = match ara {
            Some((controller, on_bind)) => {
                let ext = ara_extension(&instance).ok_or(HostError::NoAra)?;
                // SAFETY: the plugin's own extension, before activation as ARA asks.
                let bound = unsafe {
                    (ext.bind_to_document_controller)(
                        instance.raw_instance() as *const _ as *const c_void,
                        controller,
                        crate::ara::KNOWN_ROLES,
                        crate::ara::ROLES,
                    )
                };
                let bound = NonNull::new(bound.cast_mut()).ok_or(HostError::NoAra)?;
                on_bind(bound); // regions go to the renderer before activation
                Some(bound)
            }
            None => None,
        };
        let processor = instance.activate(|_, _| (), config)?;

        let (params, latency) = {
            let handle = instance.plugin_handle();
            let params = handle
                .get_extension::<PluginParams>()
                .map_or_else(Vec::new, |ext| {
                    let mut buffer = ParamInfoBuffer::new();
                    (0..ext.count(&handle))
                        .filter_map(|i| {
                            let p = ext.get_info(&handle, i, &mut buffer)?;
                            ParamInfo::new(
                                p.id.get(),
                                String::from_utf8_lossy(p.name).into_owned(),
                                p.min_value,
                                p.max_value,
                                p.default_value,
                            )
                        })
                        .collect()
                });
            let latency = handle
                .get_extension::<PluginLatency>()
                .map_or(0, |ext| ext.get(&handle));
            (params, latency)
        };

        let (param_changes, param_rx) = RingBuffer::new(PARAM_QUEUE_CAPACITY);
        let buffers = |n: Option<u32>| {
            (0..n.unwrap_or(0))
                .map(|_| vec![0.0; max_block].into_boxed_slice())
                .collect()
        };
        let ports = |n: Option<u32>| AudioPorts::with_capacity(n.unwrap_or(0) as usize, 1);
        let node = PluginNode {
            processor: processor.into(),
            input_ports: ports(inputs),
            output_ports: ports(outputs),
            has_input: inputs.is_some(),
            has_output: outputs.is_some(),
            inputs: buffers(inputs),
            outputs: buffers(outputs),
            max_block,
            events: EventBuffer::with_capacity(EVENT_CAPACITY),
            param_changes: param_rx,
            steady_time: 0,
            latency,
        };
        Ok((
            ClapPlugin {
                instance,
                audio_input: inputs.is_some(),
                params,
                param_changes,
                latency,
                editor: false,
                ara,
            },
            node,
        ))
    }

    /// False for instruments, which take notes instead of audio.
    pub fn has_audio_input(&self) -> bool {
        self.audio_input
    }

    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// Delay the plugin adds, in samples, as reported at activation.
    pub fn latency_samples(&self) -> u32 {
        self.latency
    }

    /// The plugin's current value for a parameter.
    pub fn param_value(&mut self, id: u32) -> Option<f64> {
        let handle = self.instance.plugin_handle();
        let ext = handle.get_extension::<PluginParams>()?;
        ext.get_value(&handle, ClapId::new(id))
    }

    /// Sends a parameter change, clamped to its range, to the audio thread. Returns
    /// false for an unknown id, a non-finite value or a full queue.
    pub fn set_param(&mut self, id: u32, value: f64) -> bool {
        let Some(p) = self.params.iter().find(|p| p.id == id) else {
            return false;
        };
        value.is_finite()
            && self
                .param_changes
                .push((id, value.clamp(p.min, p.max)))
                .is_ok()
    }

    /// The plugin's opaque state, to store with the project.
    pub fn save_state(&mut self) -> Result<Vec<u8>, HostError> {
        let handle = self.instance.plugin_handle();
        let ext = handle
            .get_extension::<PluginState>()
            .ok_or(HostError::State)?;
        let mut bytes = Vec::new();
        ext.save(&handle, &mut bytes)
            .map_err(|_| HostError::State)?;
        Ok(bytes)
    }

    /// Restores state from `save_state`. Plugins accept this while processing.
    pub fn load_state(&mut self, mut bytes: &[u8]) -> Result<(), HostError> {
        let handle = self.instance.plugin_handle();
        let ext = handle
            .get_extension::<PluginState>()
            .ok_or(HostError::State)?;
        ext.load(&handle, &mut bytes).map_err(|_| HostError::State)
    }

    /// Creates the plugin's editor embedded in `parent`; returns its size.
    ///
    /// # Safety
    /// `parent` must be a valid `NSView` that outlives the editor. Main thread only.
    pub unsafe fn open_editor(&mut self, parent: *mut c_void) -> Result<(f64, f64), HostError> {
        self.close_editor();
        if parent.is_null() {
            return Err(HostError::NoEditor);
        }
        // A request left over from a previous editor must not resize this one.
        self.instance
            .access_shared_handler(|r| r.resize.store(0, Ordering::Relaxed));
        let handle = self.instance.plugin_handle();
        let gui = handle
            .get_extension::<PluginGui>()
            .filter(|g| g.is_api_supported(&handle, EMBEDDED_COCOA))
            .ok_or(HostError::NoEditor)?;
        gui.create(&handle, EMBEDDED_COCOA)
            .map_err(|_| HostError::NoEditor)?;
        // SAFETY: upheld by the caller.
        let shown = unsafe { gui.set_parent(&handle, Window::from_cocoa_nsview(parent)) }
            .and_then(|()| gui.show(&handle));
        if shown.is_err() {
            gui.destroy(&handle);
            return Err(HostError::NoEditor);
        }
        let size = gui.get_size(&handle).unwrap_or(GuiSize {
            width: 400,
            height: 300,
        });
        self.editor = true;
        Ok((f64::from(size.width), f64::from(size.height)))
    }

    pub fn close_editor(&mut self) {
        if std::mem::take(&mut self.editor) {
            let handle = self.instance.plugin_handle();
            if let Some(gui) = handle.get_extension::<PluginGui>() {
                gui.destroy(&handle);
            }
        }
    }

    /// The plug-in's ARA 2 factory, if it is an ARA plug-in.
    pub fn ara_factory(&self) -> Option<crate::ara::AraFactory> {
        let ext = ara_extension(&self.instance)?;
        let plugin = self.instance.raw_instance() as *const _ as *const c_void;
        // SAFETY: the plugin's own extension and pointer.
        unsafe { crate::ara::AraFactory::from_raw((ext.get_factory)(plugin)) }
    }

    pub fn ara_extension(&self) -> Option<NonNull<c_void>> {
        self.ara
    }

    /// A size the editor asked for since the last call, in points.
    pub fn editor_resized(&mut self) -> Option<(f64, f64)> {
        let packed = self
            .instance
            .access_shared_handler(|r| r.resize.swap(0, Ordering::Acquire));
        (packed != 0).then(|| (f64::from((packed >> 32) as u32), f64::from(packed as u32)))
    }

    /// Runs main-thread work the plugin asked for. Call it regularly, like
    /// `EngineHandle::collect_garbage`.
    pub fn idle(&mut self) {
        let asked = self
            .instance
            .access_shared_handler(|r| r.callback.swap(false, Ordering::Acquire));
        if asked {
            self.instance.call_on_main_thread_callback();
        }
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        self.close_editor(); // CLAP: destroy the GUI before the plugin
    }
}

/// Audio-thread side of a hosted CLAP plugin. Copies its inputs into the plugin's
/// main input port, sends live MIDI (notes as CLAP note events, everything else as
/// raw MIDI) and parameter changes, and copies the main output port to its outputs.
/// A mono plugin output feeds every node output; other unmatched outputs are zeroed.
pub struct PluginNode {
    processor: PluginAudioProcessor<CanaryHost>,
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    has_input: bool,
    has_output: bool,
    inputs: Box<[Box<[f32]>]>,
    outputs: Box<[Box<[f32]>]>,
    /// The plugin was activated for blocks up to this long.
    max_block: usize,
    events: EventBuffer,
    param_changes: Consumer<(u32, f64)>,
    steady_time: u64,
    latency: u32,
}

impl PluginNode {
    fn fill_events(&mut self, midi: &[MidiEvent], frames: u32) {
        let any = Pckn::match_all(); // global, not per-voice
        self.events.clear();
        // Parameter changes apply from the block start, before any note.
        while self.events.len() < PARAM_QUEUE_CAPACITY as u32 {
            let Ok((id, value)) = self.param_changes.pop() else {
                break;
            };
            self.events
                .push(&ParamValueEvent::new(0, ClapId::new(id), any, value));
        }
        for e in midi {
            if self.events.len() as usize >= EVENT_CAPACITY {
                break; // never grow the buffer on the audio thread
            }
            let time = e.sample_offset.min(frames.saturating_sub(1));
            let [status, key, velocity] = e.data;
            let channel = u16::from(status & 0x0F);
            let note = Pckn::new(0u16, channel, u16::from(key), Match::All);
            match status & 0xF0 {
                0x90 if velocity > 0 => {
                    self.events
                        .push(&NoteOnEvent::new(time, note, f64::from(velocity) / 127.0))
                }
                0x80 | 0x90 => {
                    self.events
                        .push(&NoteOffEvent::new(time, note, f64::from(velocity) / 127.0))
                }
                _ => self.events.push(&ClapMidi::new(time, 0, e.data)),
            }
        }
    }
}

impl AudioNode for PluginNode {
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let n = context.num_samples;
        if n > self.max_block {
            // Never hand the plugin more than it was activated for.
            for output in outputs.iter_mut() {
                output.fill(0.0);
            }
            return;
        }
        // Untrusted plugins get pre-zeroed inputs where nothing feeds them.
        for (ch, buffer) in self.inputs.iter_mut().enumerate() {
            match inputs.get(ch) {
                Some(input) => buffer[..n].copy_from_slice(input),
                None => buffer[..n].fill(0.0),
            }
        }
        for buffer in self.outputs.iter_mut() {
            buffer[..n].fill(0.0);
        }
        self.fill_events(midi_in, n as u32);

        let ok = match self.processor.ensure_processing_started() {
            Ok(processor) => {
                let audio_in = self.input_ports.with_input_buffers(
                    self.has_input.then_some(AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            self.inputs
                                .iter_mut()
                                .map(|b| InputChannel::variable(&mut b[..n])),
                        ),
                    }),
                );
                let mut audio_out =
                    self.output_ports
                        .with_output_buffers(self.has_output.then_some(AudioPortBuffer {
                            latency: 0,
                            channels: AudioPortBufferType::f32_output_only(
                                self.outputs.iter_mut().map(|b| &mut b[..n]),
                            ),
                        }));
                processor
                    .process(
                        &audio_in,
                        &mut audio_out,
                        &self.events.as_input(),
                        // Plugin output events are not used yet; void never allocates.
                        &mut OutputEvents::void(),
                        Some(self.steady_time),
                        None,
                    )
                    .is_ok()
            }
            Err(_) => false,
        };
        self.steady_time += n as u64;

        let last = self.outputs.len().checked_sub(1);
        for (ch, output) in outputs.iter_mut().enumerate() {
            match last {
                Some(last) if ok && (ch <= last || last == 0) => {
                    output.copy_from_slice(&self.outputs[ch.min(last)][..n]);
                }
                _ => output.fill(0.0),
            }
        }
    }

    fn latency_samples(&self) -> u32 {
        self.latency
    }

    fn reset(&mut self) {
        self.processor.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_plugin::{self, TestGain, GAIN, LATENCY};
    use assert_no_alloc::{assert_no_alloc, AllocDisabler};
    use clack_plugin::prelude::SinglePluginEntry;

    #[global_allocator]
    static ALLOCATOR: AllocDisabler = AllocDisabler;

    /// Held by every test that makes plugins, so the live-instance count one test
    /// checks is not moved by another running in parallel.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entry() -> PluginEntry {
        PluginEntry::load_from_clack::<SinglePluginEntry<TestGain>>(c"/test/gain.clap").unwrap()
    }

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: n,
            playhead: 0,
            playing: true,
        }
    }

    /// One 8-frame block of ones through the node; returns the left output.
    fn run(node: &mut PluginNode, midi: &[MidiEvent]) -> [f32; 8] {
        let input = [1.0; 8];
        let (mut l, mut r) = ([9.0; 8], [9.0; 8]);
        node.process(
            &[&input, &input],
            &mut [&mut l, &mut r],
            midi,
            &mut Vec::new(),
            &ctx(8),
        );
        assert_eq!(l, r);
        l
    }

    #[test]
    fn lists_binds_and_reports_latency() {
        let _serial = serial();
        let entry = entry();
        assert_eq!(plugins(&entry)[0].id, test_plugin::ID);
        assert!(matches!(
            ClapPlugin::new(&entry, "nope", 48_000.0, 64),
            Err(HostError::UnknownPlugin(_))
        ));
        let (mut plugin, node) = ClapPlugin::new(&entry, test_plugin::ID, 48_000.0, 64).unwrap();
        assert_eq!(
            plugin.params(),
            [ParamInfo {
                id: GAIN,
                name: "Gain".into(),
                min: 0.0,
                max: 4.0,
                default: 1.0,
                log: false,
            }]
        );
        assert_eq!(
            (plugin.latency_samples(), node.latency_samples()),
            (LATENCY, LATENCY)
        );
        assert_eq!(plugin.param_value(GAIN), Some(1.0));
        assert!(!plugin.set_param(99, 1.0), "unknown id");
        assert!(!plugin.set_param(GAIN, f64::NAN));
        drop(node);
    }

    #[test]
    fn params_and_notes_reach_the_plugin_without_allocating() {
        let _serial = serial();
        let entry = entry();
        let (mut plugin, mut node) =
            ClapPlugin::new(&entry, test_plugin::ID, 48_000.0, 64).unwrap();
        let note_on = MidiEvent {
            sample_offset: 5,
            data: [0x90, 60, 100],
        };
        let note_off = MidiEvent {
            sample_offset: 2,
            data: [0x80, 60, 0],
        };

        assert_eq!(run(&mut node, &[]), [1.0; 8]); // starts processing
        assert!(plugin.set_param(GAIN, 10.0)); // clamped to 4.0
        let (with_note, after_off) = assert_no_alloc(|| {
            let a = run(&mut node, &[note_on]);
            (a, run(&mut node, &[note_off]))
        });
        assert_eq!(with_note, [4., 4., 4., 4., 4., 5., 5., 5.]);
        assert_eq!(after_off, [5., 5., 4., 4., 4., 4., 4., 4.]);
        assert_eq!(plugin.param_value(GAIN), Some(4.0));
        drop(node);
    }

    #[test]
    fn block_longer_than_activation_is_silence_not_a_panic() {
        let _serial = serial();
        let entry = entry();
        let (_plugin, mut node) = ClapPlugin::new(&entry, test_plugin::ID, 48_000.0, 4).unwrap();
        assert_eq!(run(&mut node, &[]), [0.0; 8]); // 8 frames, activated for 4
    }

    #[test]
    fn state_round_trips_and_plugin_is_destroyed_after_its_node() {
        let _serial = serial();
        let entry = entry();
        let before = test_plugin::LIVE.load(Ordering::SeqCst);
        let (mut plugin, mut node) =
            ClapPlugin::new(&entry, test_plugin::ID, 48_000.0, 64).unwrap();
        let saved = plugin.save_state().unwrap();
        plugin.set_param(GAIN, 3.0);
        run(&mut node, &[]);
        assert_eq!(plugin.param_value(GAIN), Some(3.0));
        plugin.load_state(&saved).unwrap();
        assert_eq!(plugin.param_value(GAIN), Some(1.0));
        assert_eq!(run(&mut node, &[]), [1.0; 8]);
        assert!(matches!(plugin.load_state(&[1, 2]), Err(HostError::State)));
        plugin.idle();

        drop(node); // as garbage collection would, on the main thread
        drop(plugin);
        assert_eq!(test_plugin::LIVE.load(Ordering::SeqCst), before);
    }
}
