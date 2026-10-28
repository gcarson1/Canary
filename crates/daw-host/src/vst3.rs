//! VST3 hosting through the `vst3` crate's bindings (the SDK is MIT licensed).
//!
//! A `Vst3Plugin` (main thread: edit controller, parameters, state) and its
//! `Vst3Node` (audio thread: the audio processor) share one instance. Parameter
//! changes reach the processor as `IParameterChanges` built from a lock-free queue;
//! notes arrive as `IEventList` events. Plugins with more than one audio bus per
//! direction (sidechains) are rejected for now, as with CLAP.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use daw_core::{MidiEvent, ProcessContext};
use daw_graph::{AudioNode, MAX_CHANNELS};
use rtrb::{Consumer, Producer, RingBuffer};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper, Interface};

use crate::{HostError, ParamInfo, PluginInfo};

const PARAM_QUEUE_CAPACITY: usize = 256;
/// Distinct parameters one block can change.
const MAX_CHANGED_PARAMS: usize = 64;
const EVENT_CAPACITY: usize = 1024;
const AUDIO_MODULE: &[u8] = b"Audio Module Class";

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (d, s) in dst.iter_mut().zip(src.encode_utf16()) {
        *d = s as TChar;
        n += 1;
    }
    if let Some(end) = dst.get_mut(n.min(dst.len().saturating_sub(1))) {
        *end = 0;
    }
}

fn wstring(src: &[TChar]) -> String {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf16_lossy(&src[..len])
}

fn cstring(src: &[i8]) -> String {
    let bytes: Vec<u8> = src
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex(cid: &TUID) -> String {
    cid.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn ok(result: tresult) -> Result<(), HostError> {
    if result == kResultOk || result == kResultTrue {
        Ok(())
    } else {
        Err(HostError::Status(result))
    }
}

/// A loaded `.vst3` bundle and its plugin factory.
pub struct Vst3Bundle {
    // Dropped first: the factory must go before the library unloads.
    factory: ComPtr<IPluginFactory>,
    vendor: String,
    _library: Option<Library>,
}

struct Library {
    lib: libloading::Library,
    #[cfg(target_os = "macos")]
    _bundle: objc2_core_foundation::CFRetained<objc2_core_foundation::CFBundle>,
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module was entered in `load`; every object it made is gone.
        unsafe {
            if let Ok(exit) = self
                .lib
                .get::<unsafe extern "system" fn() -> bool>(b"BundleExit")
            {
                exit();
            }
        }
    }
}

// SAFETY: the factory is only used from the main thread; its methods are
// thread-safe per the VST3 module contract.
unsafe impl Send for Vst3Bundle {}
unsafe impl Sync for Vst3Bundle {}

impl Vst3Bundle {
    /// Loads a `.vst3` bundle (macOS) and enters its module. Unsafe because it runs
    /// the plugin's code in this process.
    ///
    /// # Safety
    /// The bundle must be a compliant VST3 plugin.
    #[cfg(target_os = "macos")]
    pub unsafe fn load(path: &Path) -> Result<Arc<Self>, HostError> {
        use objc2_core_foundation::{CFBundle, CFURL};
        let io = |e: &dyn std::fmt::Display| {
            HostError::Load(clack_host::entry::PluginEntryError::Io(
                std::io::Error::other(e.to_string()),
            ))
        };
        let url = CFURL::from_file_path(path).ok_or_else(|| io(&"bad bundle path"))?;
        let bundle = CFBundle::new(None, Some(&url)).ok_or_else(|| io(&"not a bundle"))?;
        // Info.plist names the binary; the folder name may differ.
        let binary = bundle
            .executable_url()
            .and_then(|u| u.to_file_path())
            .ok_or_else(|| io(&"bundle has no executable"))?;
        // SAFETY: upheld by the caller.
        let lib = unsafe { libloading::Library::new(&binary) }.map_err(|e| io(&e))?;
        // SAFETY: symbol types are fixed by the VST3 module ABI.
        unsafe {
            let entry = lib
                .get::<unsafe extern "system" fn(*mut c_void) -> bool>(b"BundleEntry")
                .map_err(|e| io(&e))?;
            let bundle_ref = objc2_core_foundation::CFRetained::as_ptr(&bundle)
                .as_ptr()
                .cast();
            if !entry(bundle_ref) {
                return Err(io(&"BundleEntry failed"));
            }
        }
        // Entered: from here on, dropping the library calls BundleExit.
        let library = Library {
            lib,
            _bundle: bundle,
        };
        // SAFETY: as above.
        let factory = unsafe {
            let get = library
                .lib
                .get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory")
                .map_err(|e| io(&e))?;
            ComPtr::from_raw(get()).ok_or(HostError::NoFactory)?
        };
        Ok(Arc::new(Self::with_factory(factory, Some(library))))
    }

    /// Wraps a factory already in this process (a statically linked plugin).
    pub fn from_factory(factory: ComPtr<IPluginFactory>) -> Arc<Self> {
        Arc::new(Self::with_factory(factory, None))
    }

    fn with_factory(factory: ComPtr<IPluginFactory>, library: Option<Library>) -> Self {
        // SAFETY: PFactoryInfo is plain data the factory fills in.
        let mut info: PFactoryInfo = unsafe { std::mem::zeroed() };
        let vendor = if unsafe { factory.getFactoryInfo(&mut info) } == kResultOk {
            cstring(&info.vendor)
        } else {
            String::new()
        };
        Self {
            factory,
            vendor,
            _library: library,
        }
    }

    /// The audio processors this bundle offers. Ids are class ids in hex.
    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.classes()
            .into_iter()
            .filter(is_audio_module)
            .map(|c| PluginInfo {
                id: hex(&c.cid),
                name: cstring(&c.name),
                vendor: self.vendor.clone(),
            })
            .collect()
    }

    fn classes(&self) -> Vec<PClassInfo> {
        // SAFETY: plain factory queries.
        let count = unsafe { self.factory.countClasses() };
        (0..count)
            .filter_map(|i| {
                let mut info: PClassInfo = unsafe { std::mem::zeroed() };
                (unsafe { self.factory.getClassInfo(i, &mut info) } == kResultOk).then_some(info)
            })
            .collect()
    }

    fn create<I: Interface>(&self, cid: &TUID) -> Option<ComPtr<I>> {
        let mut obj = ptr::null_mut();
        // SAFETY: `cid` and the IID are 16-byte ids; on success `obj` is an
        // `I` with one reference we take over.
        let result = unsafe {
            self.factory
                .createInstance(cid.as_ptr(), I::IID.as_ptr().cast(), &mut obj)
        };
        if result != kResultOk {
            return None;
        }
        unsafe { ComPtr::from_raw(obj.cast()) }
    }
}

/// The host context handed to `initialize`.
struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        // SAFETY: the plugin passes a writable String128.
        copy_wstring("Canary", unsafe { &mut *name });
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // shortcut: no IMessage/IAttributeList yet, so components and controllers
        // that need host-created messages to talk cannot.
        // SAFETY: the plugin passes a writable out pointer.
        unsafe { *obj = ptr::null_mut() };
        kNotImplemented
    }
}

/// An in-memory `IBStream` for plugin state.
struct Stream {
    data: Mutex<(Vec<u8>, usize)>,
}

impl Class for Stream {
    type Interfaces = (IBStream,);
}

impl Stream {
    fn new(bytes: Vec<u8>) -> ComWrapper<Stream> {
        ComWrapper::new(Stream {
            data: Mutex::new((bytes, 0)),
        })
    }
}

impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let mut d = self.data.lock().unwrap();
        let (bytes, pos) = &mut *d;
        // A seek may leave the position past the end; reading there gives nothing.
        let start = (*pos).min(bytes.len());
        let n = (num_bytes.max(0) as usize).min(bytes.len() - start);
        if n > 0 && !buffer.is_null() {
            // SAFETY: the plugin gives a buffer of at least `num_bytes`.
            unsafe { ptr::copy_nonoverlapping(bytes[start..].as_ptr(), buffer.cast::<u8>(), n) };
            *pos = start + n;
        }
        if !num_read.is_null() {
            unsafe { *num_read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_written: *mut int32,
    ) -> tresult {
        let mut d = self.data.lock().unwrap();
        let (bytes, pos) = &mut *d;
        let n = num_bytes.max(0) as usize;
        if n == 0 || buffer.is_null() {
            if !num_written.is_null() {
                unsafe { *num_written = 0 };
            }
            return kResultOk;
        }
        // Plugin state lives in memory; refuse to grow past 2 GiB.
        let Some(end) = pos.checked_add(n).filter(|&e| e <= i32::MAX as usize) else {
            return kResultFalse;
        };
        // SAFETY: the plugin gives `num_bytes` readable bytes.
        let src = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[*pos..end].copy_from_slice(src);
        *pos = end;
        if !num_written.is_null() {
            unsafe { *num_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let mut d = self.data.lock().unwrap();
        let base = match mode as IBStream_::IStreamSeekMode {
            IBStream_::IStreamSeekMode_::kIBSeekSet => 0,
            IBStream_::IStreamSeekMode_::kIBSeekCur => d.1 as int64,
            IBStream_::IStreamSeekMode_::kIBSeekEnd => d.0.len() as int64,
            _ => return kInvalidArgument,
        };
        let Some(to) = base.checked_add(pos).filter(|&to| to >= 0) else {
            return kInvalidArgument;
        };
        d.1 = to as usize;
        if !result.is_null() {
            unsafe { *result = to };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if !pos.is_null() {
            unsafe { *pos = self.data.lock().unwrap().1 as int64 };
        }
        kResultOk
    }
}

/// One parameter's change for a block: a single point at offset 0.
struct ParamQueue {
    id: AtomicU32,
    value: AtomicU64,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.load(Ordering::Relaxed)
    }

    unsafe fn getPointCount(&self) -> int32 {
        1
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        // SAFETY: the plugin passes writable out pointers.
        unsafe {
            *offset = 0;
            *value = f64::from_bits(self.value.load(Ordering::Relaxed));
        }
        kResultOk
    }

    unsafe fn addPoint(&self, _offset: int32, _value: ParamValue, _index: *mut int32) -> tresult {
        kResultFalse
    }
}

/// The parameter changes for one block, allocated once.
struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    count: AtomicUsize,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.count.load(Ordering::Relaxed) as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        let i = index.max(0) as usize;
        if i >= self.count.load(Ordering::Relaxed) {
            return ptr::null_mut();
        }
        self.queues[i]
            .as_com_ref::<IParamValueQueue>()
            .map_or(ptr::null_mut(), |q| q.as_ptr())
    }

    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

/// Input events for one block, allocated once. Touched only by the thread that
/// renders the node, between `process` filling it and the plugin reading it.
struct EventList {
    events: UnsafeCell<Vec<Event>>,
}

// SAFETY: see the type comment; there is never concurrent access.
unsafe impl Sync for EventList {}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        unsafe { (*self.events.get()).len() as int32 }
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        let events = unsafe { &*self.events.get() };
        match events.get(index.max(0) as usize) {
            Some(ev) => {
                // SAFETY: the plugin passes a writable Event.
                unsafe { ptr::copy_nonoverlapping(ev, e, 1) };
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, _e: *mut Event) -> tresult {
        kResultFalse
    }
}

/// The plugin's objects. Torn down in order on the main thread when the last of
/// the plugin handle and node goes.
struct Instance {
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    /// `None` when the component is its own controller.
    separate: Option<ComPtr<IEditController>>,
    controller: ComPtr<IEditController>,
    _host: ComWrapper<HostApp>,
    _bundle: Arc<Vst3Bundle>,
    /// Set once `setActive(1)` succeeded; processing calls are only legal after.
    active: bool,
}

// SAFETY: controller calls stay on the main thread (the plugin handle) and
// processor calls on the rendering thread (the node), as VST3 requires.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: nothing else uses the objects any more.
        unsafe {
            if self.active {
                self.processor.setProcessing(0);
                self.component.setActive(0);
            }
            if let Some(c) = &self.separate {
                c.terminate();
            }
            self.component.terminate();
        }
    }
}

/// Main-thread side of a hosted VST3 plugin. Parameter values are normalized,
/// 0.0 to 1.0, as VST3 defines them.
pub struct Vst3Plugin {
    instance: Arc<Instance>,
    params: Vec<ParamInfo>,
    changes: Producer<(u32, f64)>,
    /// Has an audio input port; without one it is an instrument.
    audio_input: bool,
    /// The editor while it is shown.
    view: Option<ComPtr<IPlugView>>,
    /// The ARA extension, when created bound to a document.
    ara: Option<NonNull<c_void>>,
}

/// A VST3 interface id from ARAVST3.h's four words (non-COM byte order, macOS).
const fn ara_iid(w: [u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 4 {
        let b = w[i].to_be_bytes();
        out[i * 4] = b[0];
        out[i * 4 + 1] = b[1];
        out[i * 4 + 2] = b[2];
        out[i * 4 + 3] = b[3];
        i += 1;
    }
    out
}

const ARA_ENTRY_POINT: [u8; 16] = ara_iid([0x12814E54, 0xA1CE4076, 0x82B96813, 0x16950BD6]);
const ARA_ENTRY_POINT_2: [u8; 16] = ara_iid([0xCD9A5913, 0xC9EB46D7, 0x96CA53AD, 0xD1DB89F5]);

/// FUnknown's three methods, then the interface's own, as the vtable lays them out.
#[repr(C)]
struct AraEntryVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const [u8; 16], *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    /// `getFactory` (IPlugInEntryPoint) or `bindToDocumentControllerWithRoles`
    /// (IPlugInEntryPoint2).
    first: *const c_void,
}

/// Asks `object` (an FUnknown) for interface `iid`; the result is AddRef'd.
unsafe fn query(object: *mut c_void, iid: &[u8; 16]) -> Option<NonNull<*const AraEntryVtbl>> {
    let vtbl = unsafe { *object.cast::<*const AraEntryVtbl>() };
    let mut out = ptr::null_mut();
    // SAFETY: every VST3 object starts with an FUnknown vtable.
    let status = unsafe { ((*vtbl).query_interface)(object, iid, &mut out) };
    (status == kResultOk).then(|| NonNull::new(out.cast()))?
}

/// The component's ARA factory via `ARA::IPlugInEntryPoint`.
unsafe fn ara_factory_of(component: *mut c_void) -> Option<crate::ara::AraFactory> {
    let entry = unsafe { query(component, &ARA_ENTRY_POINT) }?;
    // SAFETY: an IPlugInEntryPoint; `first` is `getFactory(this)`.
    unsafe {
        let vtbl = &**entry.as_ptr();
        let get: unsafe extern "system" fn(*mut c_void) -> *const c_void =
            std::mem::transmute(vtbl.first);
        let factory = get(entry.as_ptr().cast());
        (vtbl.release)(entry.as_ptr().cast());
        crate::ara::AraFactory::from_raw(factory)
    }
}

/// Binds the component via `ARA::IPlugInEntryPoint2` before processing starts.
unsafe fn ara_bind(component: *mut c_void, controller: *mut c_void) -> Option<NonNull<c_void>> {
    let entry = unsafe { query(component, &ARA_ENTRY_POINT_2) }?;
    // SAFETY: an IPlugInEntryPoint2; `first` is bindToDocumentControllerWithRoles.
    unsafe {
        let vtbl = &**entry.as_ptr();
        let bind: unsafe extern "system" fn(*mut c_void, *mut c_void, i32, i32) -> *const c_void =
            std::mem::transmute(vtbl.first);
        let ext = bind(
            entry.as_ptr().cast(),
            controller,
            crate::ara::KNOWN_ROLES,
            crate::ara::ROLES,
        );
        (vtbl.release)(entry.as_ptr().cast());
        NonNull::new(ext.cast_mut())
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        self.close_editor(); // before the controller goes
    }
}

/// Runs its closure on drop unless disarmed: cleanup for early returns.
struct Undo<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Undo<F> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for Undo<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

fn is_audio_module(c: &PClassInfo) -> bool {
    c.category
        .iter()
        .map(|&b| b as u8)
        .take_while(|&b| b != 0)
        .eq(AUDIO_MODULE.iter().copied())
}

fn bus_channels(
    component: &ComPtr<IComponent>,
    media: MediaType,
    dir: BusDirection,
) -> Result<Option<i32>, HostError> {
    // SAFETY: plain component queries.
    let count = unsafe { component.getBusCount(media, dir) };
    match count {
        0 => Ok(None),
        1 => {
            let mut info: BusInfo = unsafe { std::mem::zeroed() };
            ok(unsafe { component.getBusInfo(media, dir, 0, &mut info) })?;
            // Event buses count MIDI channels, not audio ones.
            let audio = media == MediaTypes_::kAudio as MediaType;
            if audio && info.channelCount as usize > MAX_CHANNELS {
                return Err(HostError::TooManyChannels(info.channelCount as u32));
            }
            Ok(Some(info.channelCount))
        }
        _ if media == MediaTypes_::kAudio as MediaType => Err(HostError::ExtraPorts),
        _ => Ok(Some(0)), // several event buses: use the first
    }
}

impl Vst3Plugin {
    /// Instantiates plugin `id` (from [`Vst3Bundle::plugins`]) for blocks of up to
    /// `max_block` frames and returns its handle and node.
    pub fn new(
        bundle: &Arc<Vst3Bundle>,
        id: &str,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<(Vst3Plugin, Vst3Node), HostError> {
        Self::new_ara(bundle, id, sample_rate, max_block, None)
    }

    /// As `new`, bound to ARA document controller `ara` first if given, since ARA
    /// requires binding before processing is set up.
    pub fn new_ara(
        bundle: &Arc<Vst3Bundle>,
        id: &str,
        sample_rate: f32,
        max_block: usize,
        ara: Option<*mut c_void>,
    ) -> Result<(Vst3Plugin, Vst3Node), HostError> {
        let unknown = || HostError::UnknownPlugin(id.to_owned());
        let class = bundle
            .classes()
            .into_iter()
            .find(|c| hex(&c.cid) == id && is_audio_module(c))
            .ok_or_else(unknown)?;
        let host = ComWrapper::new(HostApp);
        let context = host.as_com_ref::<FUnknown>().ok_or_else(unknown)?.as_ptr();
        let component: ComPtr<IComponent> = bundle.create(&class.cid).ok_or_else(unknown)?;
        // SAFETY: standard VST3 setup sequence on the main thread.
        unsafe {
            ok(component.initialize(context))?;
            // Until `Instance` owns the component, a failure must still terminate it.
            let initialized = component.clone();
            let _undo = Undo(Some(move || {
                initialized.terminate();
            }));
            let processor: ComPtr<IAudioProcessor> =
                component.cast().ok_or(HostError::NoFactory)?;
            let (controller, separate) = match component.cast::<IEditController>() {
                Some(c) => (c, None),
                None => {
                    let mut cid: TUID = [0; 16];
                    ok(component.getControllerClassId(&mut cid))?;
                    let c: ComPtr<IEditController> =
                        bundle.create(&cid).ok_or(HostError::NoFactory)?;
                    ok(c.initialize(context))?;
                    (c.clone(), Some(c))
                }
            };
            _undo.disarm();
            let mut instance = Instance {
                processor,
                component,
                separate,
                controller,
                _host: host,
                _bundle: bundle.clone(),
                active: false,
            };
            let inputs = bus_channels(
                &instance.component,
                MediaTypes_::kAudio as MediaType,
                BusDirections_::kInput as BusDirection,
            )?;
            let outputs = bus_channels(
                &instance.component,
                MediaTypes_::kAudio as MediaType,
                BusDirections_::kOutput as BusDirection,
            )?;
            let takes_notes = bus_channels(
                &instance.component,
                MediaTypes_::kEvent as MediaType,
                BusDirections_::kInput as BusDirection,
            )?
            .is_some();
            let activate = |media: MediaTypes, dir: BusDirections, on: bool| {
                if on {
                    instance
                        .component
                        .activateBus(media as MediaType, dir as BusDirection, 0, 1);
                }
            };
            activate(
                MediaTypes_::kAudio,
                BusDirections_::kInput,
                inputs.is_some(),
            );
            activate(
                MediaTypes_::kAudio,
                BusDirections_::kOutput,
                outputs.is_some(),
            );
            activate(MediaTypes_::kEvent, BusDirections_::kInput, takes_notes);

            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as int32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                maxSamplesPerBlock: max_block.min(i32::MAX as usize) as int32,
                sampleRate: f64::from(sample_rate),
            };
            let ara = match ara {
                Some(controller) => Some(
                    ara_bind(instance.component.as_ptr().cast(), controller)
                        .ok_or(HostError::NoAra)?,
                ),
                None => None,
            };
            ok(instance.processor.setupProcessing(&mut setup))?;
            ok(instance.component.setActive(1))?;
            instance.active = true;
            let instance = Arc::new(instance);
            sync_controller(&instance);
            let latency = instance.processor.getLatencySamples();

            let params = (0..instance.controller.getParameterCount())
                .filter_map(|i| {
                    let mut info: ParameterInfo = std::mem::zeroed();
                    (instance.controller.getParameterInfo(i, &mut info) == kResultOk).then(|| {
                        ParamInfo {
                            id: info.id,
                            name: wstring(&info.title),
                            min: 0.0,
                            max: 1.0,
                            default: info.defaultNormalizedValue,
                            log: false,
                        }
                    })
                })
                .collect();

            let (changes, change_rx) = RingBuffer::new(PARAM_QUEUE_CAPACITY);
            let buffers = |n: Option<i32>| -> Box<[Box<[f32]>]> {
                (0..n.unwrap_or(0))
                    .map(|_| vec![0.0; max_block].into_boxed_slice())
                    .collect()
            };
            let mut node = Vst3Node {
                instance: instance.clone(),
                inputs: buffers(inputs),
                outputs: buffers(outputs),
                in_ptrs: [ptr::null_mut(); MAX_CHANNELS],
                out_ptrs: [ptr::null_mut(); MAX_CHANNELS],
                has_input: inputs.is_some(),
                has_output: outputs.is_some(),
                takes_notes,
                changes: change_rx,
                param_changes: ComWrapper::new(ParamChanges {
                    queues: (0..MAX_CHANGED_PARAMS)
                        .map(|_| {
                            ComWrapper::new(ParamQueue {
                                id: AtomicU32::new(0),
                                value: AtomicU64::new(0),
                            })
                        })
                        .collect(),
                    count: AtomicUsize::new(0),
                }),
                events: ComWrapper::new(EventList {
                    events: UnsafeCell::new(Vec::with_capacity(EVENT_CAPACITY)),
                }),
                max_block,
                processing: false,
                latency,
            };
            for (p, b) in node.in_ptrs.iter_mut().zip(node.inputs.iter_mut()) {
                *p = b.as_mut_ptr();
            }
            for (p, b) in node.out_ptrs.iter_mut().zip(node.outputs.iter_mut()) {
                *p = b.as_mut_ptr();
            }
            Ok((
                Vst3Plugin {
                    instance,
                    audio_input: inputs.is_some(),
                    params,
                    changes,
                    view: None,
                    ara,
                },
                node,
            ))
        }
    }

    /// The plug-in's ARA 2 factory, if it is an ARA plug-in.
    pub fn ara_factory(&self) -> Option<crate::ara::AraFactory> {
        // SAFETY: a live component.
        unsafe { ara_factory_of(self.instance.component.as_ptr().cast()) }
    }

    pub fn ara_extension(&self) -> Option<NonNull<c_void>> {
        self.ara
    }

    /// Attaches the controller's editor view to `parent`; returns its size.
    ///
    /// # Safety
    /// `parent` must be a valid `NSView` that outlives the editor. Main thread only.
    pub unsafe fn open_editor(&mut self, parent: *mut c_void) -> Result<(f64, f64), HostError> {
        self.close_editor();
        if parent.is_null() {
            return Err(HostError::NoEditor);
        }
        // SAFETY: the controller is live; the view is used as the VST3 API says.
        unsafe {
            let raw = self.instance.controller.createView(ViewType::kEditor);
            let view = ComPtr::from_raw(raw).ok_or(HostError::NoEditor)?;
            if view.isPlatformTypeSupported(kPlatformTypeNSView) != kResultOk
                || view.attached(parent, kPlatformTypeNSView) != kResultOk
            {
                return Err(HostError::NoEditor);
            }
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            view.getSize(&mut rect);
            self.view = Some(view);
            Ok((
                f64::from(rect.right - rect.left),
                f64::from(rect.bottom - rect.top),
            ))
        }
    }

    pub fn close_editor(&mut self) {
        if let Some(view) = self.view.take() {
            // SAFETY: attached in `open_editor`; dropping releases our reference.
            unsafe { view.removed() };
        }
    }

    /// False for instruments, which take notes instead of audio.
    pub fn has_audio_input(&self) -> bool {
        self.audio_input
    }

    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// Asked fresh each time: it can change with the plugin's settings or state.
    pub fn latency_samples(&self) -> u32 {
        // SAFETY: the component is active until the instance drops.
        unsafe { self.instance.processor.getLatencySamples() }
    }

    /// The controller's normalized value for a parameter.
    pub fn param_value(&self, id: u32) -> Option<f64> {
        self.params
            .iter()
            .any(|p| p.id == id)
            // SAFETY: main-thread controller query.
            .then(|| unsafe { self.instance.controller.getParamNormalized(id) })
    }

    /// Sets a normalized parameter on the controller and queues it for the
    /// processor. False for an unknown id, a non-finite value or a full queue.
    pub fn set_param(&mut self, id: u32, value: f64) -> bool {
        if !value.is_finite() || !self.params.iter().any(|p| p.id == id) {
            return false;
        }
        let v = value.clamp(0.0, 1.0);
        if self.changes.push((id, v)).is_err() {
            return false;
        }
        // SAFETY: main-thread controller call.
        unsafe { self.instance.controller.setParamNormalized(id, v) };
        true
    }

    /// Component state then controller state, length-prefixed.
    pub fn save_state(&self) -> Result<Vec<u8>, HostError> {
        let component = Stream::new(Vec::new());
        let controller = Stream::new(Vec::new());
        // SAFETY: main-thread state calls with valid streams.
        unsafe {
            let c = component
                .as_com_ref::<IBStream>()
                .ok_or(HostError::State)?
                .as_ptr();
            ok(self.instance.component.getState(c)).map_err(|_| HostError::State)?;
            let e = controller
                .as_com_ref::<IBStream>()
                .ok_or(HostError::State)?
                .as_ptr();
            // A controller without state of its own is fine.
            self.instance.controller.getState(e);
        }
        let a = std::mem::take(&mut component.data.lock().unwrap().0);
        let b = std::mem::take(&mut controller.data.lock().unwrap().0);
        let mut out = (a.len() as u64).to_le_bytes().to_vec();
        out.extend(a);
        out.extend(b);
        Ok(out)
    }

    /// Restores state from [`Vst3Plugin::save_state`].
    pub fn load_state(&self, bytes: &[u8]) -> Result<(), HostError> {
        let len = bytes.get(..8).ok_or(HostError::State)?;
        let len = u64::from_le_bytes(len.try_into().unwrap()) as usize;
        let rest = &bytes[8..];
        if len > rest.len() {
            return Err(HostError::State);
        }
        let (a, b) = rest.split_at(len);
        let component = Stream::new(a.to_vec());
        let controller = Stream::new(b.to_vec());
        // SAFETY: main-thread state calls with valid streams.
        unsafe {
            let c = component
                .as_com_ref::<IBStream>()
                .ok_or(HostError::State)?
                .as_ptr();
            ok(self.instance.component.setState(c)).map_err(|_| HostError::State)?;
            sync_controller(&self.instance);
            if !b.is_empty() {
                let e = controller
                    .as_com_ref::<IBStream>()
                    .ok_or(HostError::State)?
                    .as_ptr();
                self.instance.controller.setState(e);
            }
        }
        Ok(())
    }
}

/// Hands the component's state to a separate controller so its parameter view
/// matches the processor.
unsafe fn sync_controller(instance: &Instance) {
    if instance.separate.is_none() {
        return;
    }
    let stream = Stream::new(Vec::new());
    let Some(s) = stream.as_com_ref::<IBStream>().map(|s| s.as_ptr()) else {
        return;
    };
    unsafe {
        if instance.component.getState(s) == kResultOk {
            stream.seek(
                0,
                IBStream_::IStreamSeekMode_::kIBSeekSet as int32,
                ptr::null_mut(),
            );
            instance.controller.setComponentState(s);
        }
    }
}

/// Audio-thread side of a hosted VST3 plugin. Copies its inputs into the plugin's
/// main bus, and the main output bus to its outputs (a mono plugin feeds every
/// output; other unmatched outputs are zeroed).
pub struct Vst3Node {
    instance: Arc<Instance>,
    inputs: Box<[Box<[f32]>]>,
    outputs: Box<[Box<[f32]>]>,
    in_ptrs: [*mut f32; MAX_CHANNELS],
    out_ptrs: [*mut f32; MAX_CHANNELS],
    has_input: bool,
    has_output: bool,
    takes_notes: bool,
    changes: Consumer<(u32, f64)>,
    param_changes: ComWrapper<ParamChanges>,
    events: ComWrapper<EventList>,
    max_block: usize,
    processing: bool,
    latency: u32,
}

// SAFETY: the raw pointers point into this node's own boxed buffers.
unsafe impl Send for Vst3Node {}

impl Vst3Node {
    fn fill_changes(&mut self) {
        let changes = &self.param_changes;
        let mut count = 0;
        while count < MAX_CHANGED_PARAMS {
            let Ok((id, value)) = self.changes.pop() else {
                break;
            };
            // A later change to the same parameter in one block replaces the earlier.
            let slot = changes.queues[..count]
                .iter()
                .position(|q| q.id.load(Ordering::Relaxed) == id)
                .unwrap_or_else(|| {
                    count += 1;
                    count - 1
                });
            changes.queues[slot].id.store(id, Ordering::Relaxed);
            changes.queues[slot]
                .value
                .store(value.to_bits(), Ordering::Relaxed);
        }
        changes.count.store(count, Ordering::Relaxed);
    }

    fn fill_events(&mut self, midi: &[MidiEvent], frames: usize) {
        // SAFETY: only this thread touches the list, and the plugin is not running.
        let events = unsafe { &mut *self.events.events.get() };
        events.clear();
        if !self.takes_notes {
            return;
        }
        for e in midi.iter().take(EVENT_CAPACITY) {
            let [status, key, velocity] = e.data;
            let channel = i16::from(status & 0x0F);
            // SAFETY: Event is plain data.
            let mut ev: Event = unsafe { std::mem::zeroed() };
            ev.sampleOffset = e.sample_offset.min(frames.saturating_sub(1) as u32) as int32;
            match status & 0xF0 {
                0x90 if velocity > 0 => {
                    ev.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
                    ev.__field0.noteOn = NoteOnEvent {
                        channel,
                        pitch: i16::from(key),
                        tuning: 0.0,
                        velocity: f32::from(velocity) / 127.0,
                        length: 0,
                        noteId: -1,
                    };
                }
                0x80 | 0x90 => {
                    ev.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
                    ev.__field0.noteOff = NoteOffEvent {
                        channel,
                        pitch: i16::from(key),
                        velocity: f32::from(velocity) / 127.0,
                        noteId: -1,
                        tuning: 0.0,
                    };
                }
                _ => continue, // shortcut: CC and pitch bend need IMidiMapping
            }
            events.push(ev);
        }
    }
}

impl AudioNode for Vst3Node {
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let n = context.num_samples;
        if n > self.max_block || n == 0 {
            outputs.iter_mut().for_each(|o| o.fill(0.0));
            return;
        }
        for (ch, buffer) in self.inputs.iter_mut().enumerate() {
            match inputs.get(ch) {
                Some(input) => buffer[..n].copy_from_slice(input),
                None => buffer[..n].fill(0.0),
            }
        }
        for buffer in self.outputs.iter_mut() {
            buffer[..n].fill(0.0);
        }
        self.fill_changes();
        self.fill_events(midi_in, n);
        let processor = &self.instance.processor;
        if !self.processing {
            // SAFETY: first block on the rendering thread.
            unsafe { processor.setProcessing(1) };
            self.processing = true;
        }
        let bus = |count: usize, ptrs: &mut [*mut f32; MAX_CHANNELS]| AudioBusBuffers {
            numChannels: count as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: ptrs.as_mut_ptr(),
            },
        };
        let mut in_bus = bus(self.inputs.len(), &mut self.in_ptrs);
        let mut out_bus = bus(self.outputs.len(), &mut self.out_ptrs);
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            numSamples: n as int32,
            numInputs: i32::from(self.has_input),
            numOutputs: i32::from(self.has_output),
            inputs: if self.has_input {
                &mut in_bus
            } else {
                ptr::null_mut()
            },
            outputs: if self.has_output {
                &mut out_bus
            } else {
                ptr::null_mut()
            },
            inputParameterChanges: self
                .param_changes
                .as_com_ref::<IParameterChanges>()
                .map_or(ptr::null_mut(), |c| c.as_ptr()),
            outputParameterChanges: ptr::null_mut(),
            inputEvents: self
                .events
                .as_com_ref::<IEventList>()
                .map_or(ptr::null_mut(), |e| e.as_ptr()),
            outputEvents: ptr::null_mut(),
            processContext: ptr::null_mut(),
        };
        // SAFETY: every pointer in `data` lives until this call returns.
        let ok = unsafe { processor.process(&mut data) } == kResultOk;
        let last = self.outputs.len().checked_sub(1);
        for (ch, out) in outputs.iter_mut().enumerate() {
            match last {
                Some(last) if ok && (ch <= last || last == 0) => {
                    out.copy_from_slice(&self.outputs[ch.min(last)][..n])
                }
                _ => out.fill(0.0),
            }
        }
    }

    fn latency_samples(&self) -> u32 {
        self.latency
    }

    fn reset(&mut self) {
        if self.processing {
            // SAFETY: rendering thread; toggling processing flushes plugin state.
            unsafe {
                self.instance.processor.setProcessing(0);
                self.instance.processor.setProcessing(1);
            }
        }
    }
}

#[cfg(test)]
mod tests;
