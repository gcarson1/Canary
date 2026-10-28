//! Audio Unit (v2 API) hosting on macOS: effects, instruments and MIDI-controlled
//! effects found through the AudioComponent registry.
//!
//! An `AuPlugin` (main thread: parameters, state) and its `AuNode` (audio thread)
//! share one instance. AU parameter calls are thread-safe, so parameter changes go
//! straight to the unit instead of through a queue.

use std::cell::Cell;
use std::ffi::{c_void, CStr};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext};
use daw_graph::{AudioNode, MAX_CHANNELS};
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, sel};
use objc2_audio_toolbox::{
    kAudioUnitProperty_ClassInfo, kAudioUnitProperty_CocoaUI, kAudioUnitProperty_Latency,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_ParameterInfo,
    kAudioUnitProperty_ParameterList, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitType_Effect, kAudioUnitType_MusicDevice,
    kAudioUnitType_MusicEffect, AURenderCallbackStruct, AudioComponent, AudioComponentCopyName,
    AudioComponentDescription, AudioComponentFindNext, AudioComponentGetDescription,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioUnit, AudioUnitGetParameter,
    AudioUnitGetProperty, AudioUnitGetPropertyInfo, AudioUnitInitialize, AudioUnitParameterInfo,
    AudioUnitParameterOptions, AudioUnitRender, AudioUnitRenderActionFlags, AudioUnitReset,
    AudioUnitSetParameter, AudioUnitSetProperty, AudioUnitUninitialize, MusicDeviceMIDIEvent,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::{
    CFData, CFPropertyListCreateData, CFPropertyListCreateWithData, CFPropertyListFormat,
    CFRetained, CFString, CFType, CFURL,
};

use crate::{HostError, ParamInfo, PluginInfo};

/// What kind of Audio Unit, which decides its ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuKind {
    /// Audio in, audio out.
    Effect,
    /// MIDI in, audio out.
    Instrument,
    /// Audio and MIDI in, audio out.
    MidiEffect,
}

const KINDS: [(u32, AuKind); 3] = [
    (kAudioUnitType_Effect, AuKind::Effect),
    (kAudioUnitType_MusicDevice, AuKind::Instrument),
    (kAudioUnitType_MusicEffect, AuKind::MidiEffect),
];

fn fourcc(code: u32) -> String {
    code.to_be_bytes().iter().map(|&b| char::from(b)).collect()
}

fn parse_fourcc(s: &str) -> Option<u32> {
    let b: [u8; 4] = s.as_bytes().try_into().ok()?;
    Some(u32::from_be_bytes(b))
}

/// Ids look like `aufx:dely:appl` (type, subtype, manufacturer).
fn id_of(d: &AudioComponentDescription) -> String {
    format!(
        "{}:{}:{}",
        fourcc(d.componentType),
        fourcc(d.componentSubType),
        fourcc(d.componentManufacturer)
    )
}

fn description(id: &str) -> Option<AudioComponentDescription> {
    let mut parts = id.split(':').map(parse_fourcc);
    let d = AudioComponentDescription {
        componentType: parts.next()??,
        componentSubType: parts.next()??,
        componentManufacturer: parts.next()??,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    parts.next().is_none().then_some(d)
}

fn find(d: &AudioComponentDescription, after: AudioComponent) -> Option<AudioComponent> {
    let mut d = *d;
    // SAFETY: `d` is a valid description; `after` is null or a component from the registry.
    let c = unsafe { AudioComponentFindNext(after, NonNull::from(&mut d)) };
    (!c.is_null()).then_some(c)
}

/// Every installed effect, instrument and MIDI effect Audio Unit.
pub fn audio_units() -> Vec<(PluginInfo, AuKind)> {
    let mut out = Vec::new();
    for (ty, kind) in KINDS {
        let any = AudioComponentDescription {
            componentType: ty,
            componentSubType: 0,
            componentManufacturer: 0,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let mut c = ptr::null_mut();
        while let Some(next) = find(&any, c) {
            c = next;
            let mut d = any;
            // SAFETY: `c` came from the registry; `d` is writable.
            if unsafe { AudioComponentGetDescription(c, NonNull::from(&mut d)) } != 0 {
                continue;
            }
            let mut name: *const CFString = ptr::null();
            // SAFETY: on success `name` is a +1 CFString we take ownership of.
            let full = if unsafe { AudioComponentCopyName(c, NonNull::from(&mut name)) } == 0 {
                NonNull::new(name.cast_mut())
                    .map(|n| unsafe { CFRetained::from_raw(n) }.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            // Names read "Vendor: Plugin".
            let (vendor, name) = full.split_once(": ").unwrap_or(("", &full));
            out.push((
                PluginInfo {
                    id: id_of(&d),
                    name: name.to_owned(),
                    vendor: vendor.to_owned(),
                },
                kind,
            ));
        }
    }
    out
}

fn check(status: i32) -> Result<(), HostError> {
    if status == 0 {
        Ok(())
    } else {
        Err(HostError::Status(status))
    }
}

/// One live Audio Unit. Disposed when the last of its plugin handle and node goes,
/// which is always on the main thread (nodes leave through garbage collection).
struct Instance(AudioUnit);

// SAFETY: the unit is used from the main thread for properties and parameters and
// from one audio thread for rendering, which the AU API allows.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the unit is valid and nothing else holds it any more.
        unsafe {
            AudioUnitUninitialize(self.0);
            AudioComponentInstanceDispose(self.0);
        }
    }
}

impl Instance {
    fn set<T>(&self, id: u32, scope: u32, element: u32, value: &T) -> Result<(), HostError> {
        // SAFETY: `value` points at a `T` of the size passed.
        check(unsafe {
            AudioUnitSetProperty(
                self.0,
                id,
                scope,
                element,
                ptr::from_ref(value).cast(),
                size_of::<T>() as u32,
            )
        })
    }

    fn get<T: Default>(&self, id: u32, scope: u32, element: u32) -> Result<T, HostError> {
        let mut value = T::default();
        let mut size = size_of::<T>() as u32;
        // SAFETY: `value` is writable for `size` bytes.
        check(unsafe {
            AudioUnitGetProperty(
                self.0,
                id,
                scope,
                element,
                NonNull::from(&mut value).cast(),
                NonNull::from(&mut size),
            )
        })?;
        Ok(value)
    }

    /// A property of a type with no sensible default; None unless the unit fills
    /// exactly one `T`.
    fn get_raw<T>(&self, id: u32, scope: u32, element: u32) -> Option<T> {
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        // SAFETY: `value` is writable for `size` bytes.
        let status = unsafe {
            AudioUnitGetProperty(
                self.0,
                id,
                scope,
                element,
                NonNull::from(&mut value).cast(),
                NonNull::from(&mut size),
            )
        };
        // SAFETY: the unit wrote a whole `T`.
        (status == 0 && size as usize == size_of::<T>()).then(|| unsafe { value.assume_init() })
    }
}

/// Where the render callback reads the node's input from, set before each render.
/// Shared (`Arc`) with interior mutability, so the pointer the unit holds stays
/// valid while the node updates it.
struct InputFeed {
    channels: [Cell<*const f32>; MAX_CHANNELS],
    count: Cell<usize>,
    frames: Cell<usize>,
}

// SAFETY: the pointers are only set and read within one `AuNode::process` call on
// the thread running it, and cleared before it returns.
unsafe impl Send for InputFeed {}
unsafe impl Sync for InputFeed {}

/// Feeds the effect's input bus from the node's inputs. Runs on the audio thread,
/// inside `AudioUnitRender`; copies only.
unsafe extern "C-unwind" fn feed_input(
    ref_con: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _time: NonNull<AudioTimeStamp>,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: the ref con is the node's boxed `InputFeed`, alive while it renders.
    let feed = unsafe { ref_con.cast::<InputFeed>().as_ref() };
    let Some(list) = (unsafe { data.as_mut() }) else {
        return 0;
    };
    // SAFETY: the unit hands us `mNumberBuffers` buffers.
    let buffers = unsafe {
        std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), list.mNumberBuffers as usize)
    };
    let n = (frames as usize).min(feed.frames.get());
    for (ch, b) in buffers.iter_mut().enumerate() {
        if b.mData.is_null() {
            continue;
        }
        // SAFETY: each buffer holds at least `frames` floats.
        let out = unsafe { std::slice::from_raw_parts_mut(b.mData.cast::<f32>(), frames as usize) };
        out.fill(0.0);
        if ch < feed.count.get() {
            // SAFETY: `channels[ch]` points at `feed.frames` samples for this render.
            let input =
                unsafe { std::slice::from_raw_parts(feed.channels[ch].get(), feed.frames.get()) };
            out[..n].copy_from_slice(&input[..n]);
        }
    }
    0
}

/// `AudioBufferList` with room for `MAX_CHANNELS` buffers (same layout as the C
/// struct with its trailing array extended).
#[repr(C)]
struct BufferList {
    count: u32,
    buffers: [AudioBuffer; MAX_CHANNELS],
}

/// Main-thread side of a hosted Audio Unit: parameters and state.
pub struct AuPlugin {
    unit: Arc<Instance>,
    params: Vec<ParamInfo>,
    sample_rate: f32,
    kind: AuKind,
    /// The editor's `NSView` while it is shown.
    editor: Option<Retained<AnyObject>>,
    /// The ARA extension, when created bound to a document.
    ara: Option<NonNull<c_void>>,
}

const ARA_MAGIC: u32 = u32::from_be_bytes(*b"Ara!");
const ARA_FACTORY_PROPERTY: u32 = u32::from_be_bytes(*b"AraF");
const ARA_BINDING_PROPERTY: u32 = u32::from_be_bytes(*b"AraE");

/// `ARAAudioUnitFactory`.
#[repr(C)]
struct AuAraFactory {
    magic: u32,
    factory: *const c_void,
}

/// `ARAAudioUnitPlugInExtensionBinding`.
#[repr(C)]
struct AuAraBinding {
    magic: u32,
    controller: *mut c_void,
    extension: *const c_void,
    known_roles: i32,
    assigned_roles: i32,
}

/// Reads a property whose struct the caller pre-fills (ARA's magic number).
fn get_in_out<T>(unit: &Instance, id: u32, mut value: T) -> Option<T> {
    let mut size = size_of::<T>() as u32;
    // SAFETY: `value` is readable and writable for `size` bytes.
    let status = unsafe {
        AudioUnitGetProperty(
            unit.0,
            id,
            kAudioUnitScope_Global,
            0,
            NonNull::from(&mut value).cast(),
            NonNull::from(&mut size),
        )
    };
    (status == 0 && size as usize == size_of::<T>()).then_some(value)
}

/// Binds an uninitialized unit to an ARA document controller.
fn bind_ara(unit: &Instance, controller: *mut c_void) -> Result<NonNull<c_void>, HostError> {
    let binding = AuAraBinding {
        magic: ARA_MAGIC,
        controller,
        extension: ptr::null(),
        known_roles: crate::ara::KNOWN_ROLES,
        assigned_roles: crate::ara::ROLES,
    };
    get_in_out(unit, ARA_BINDING_PROPERTY, binding)
        .filter(|b| b.magic == ARA_MAGIC)
        .and_then(|b| NonNull::new(b.extension.cast_mut()))
        .ok_or(HostError::NoAra)
}

impl Drop for AuPlugin {
    fn drop(&mut self) {
        self.close_editor(); // before the unit goes
    }
}

// AUGenericView, the editor for units without their own Cocoa view.
#[link(name = "CoreAudioKit", kind = "framework")]
extern "C" {}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// SAFETY: these match CoreGraphics' layouts and names.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

/// CoreAudioKit's generic parameter view for `unit`.
unsafe fn generic_view(unit: AudioUnit) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"AUGenericView")?;
    // SAFETY: AUGenericView takes the unit it shows.
    unsafe {
        let view: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![view, initWithAudioUnit: unit]
    }
}

/// The unit's latency now, in samples: it can change with its settings or state.
fn latency(unit: &Instance, sample_rate: f32) -> u32 {
    let seconds: f64 = unit
        .get(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0)
        .unwrap_or(0.0);
    (seconds * f64::from(sample_rate))
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

impl AuPlugin {
    /// Instantiates `id` (from [`audio_units`]) as stereo at `sample_rate` for blocks
    /// of up to `max_block` frames, and returns its handle and node.
    pub fn new(
        id: &str,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<(AuPlugin, AuNode), HostError> {
        Self::new_ara(id, sample_rate, max_block, None)
    }

    /// As `new`, bound to ARA document controller `ara` first if given, since ARA
    /// requires binding before the unit is initialized.
    pub fn new_ara(
        id: &str,
        sample_rate: f32,
        max_block: usize,
        ara: Option<*mut c_void>,
    ) -> Result<(AuPlugin, AuNode), HostError> {
        let unknown = || HostError::UnknownPlugin(id.to_owned());
        let desc = description(id).ok_or_else(unknown)?;
        let kind = KINDS
            .iter()
            .find(|(t, _)| *t == desc.componentType)
            .map(|&(_, k)| k)
            .ok_or_else(unknown)?;
        let component = find(&desc, ptr::null_mut()).ok_or_else(unknown)?;
        let mut raw = ptr::null_mut();
        // SAFETY: `component` came from the registry.
        check(unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut raw)) })?;
        let unit = Arc::new(Instance(raw));

        let format = AudioStreamBasicDescription {
            mSampleRate: f64::from(sample_rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        unit.set(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            0,
            &format,
        )?;
        let feed = Arc::new(InputFeed {
            channels: std::array::from_fn(|_| Cell::new(ptr::null())),
            count: Cell::new(0),
            frames: Cell::new(0),
        });
        if kind != AuKind::Instrument {
            unit.set(
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                &format,
            )?;
            let callback = AURenderCallbackStruct {
                inputProc: Some(feed_input),
                inputProcRefCon: Arc::as_ptr(&feed).cast_mut().cast(),
            };
            unit.set(
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                &callback,
            )?;
        }
        let max = u32::try_from(max_block).unwrap_or(u32::MAX);
        unit.set(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &max,
        )?;
        let ara = match ara {
            Some(controller) => Some(bind_ara(&unit, controller)?),
            None => None,
        };
        // SAFETY: the unit is configured and not yet initialized.
        check(unsafe { AudioUnitInitialize(unit.0) })?;

        let latency = latency(&unit, sample_rate);
        let plugin = AuPlugin {
            params: params(&unit),
            unit: unit.clone(),
            sample_rate,
            kind,
            editor: None,
            ara,
        };
        let channel = || vec![0.0f32; max_block].into_boxed_slice();
        let node = AuNode {
            unit,
            feed,
            outputs: [channel(), channel()],
            max_block,
            sample_time: 0.0,
            latency,
            kind,
        };
        Ok((plugin, node))
    }

    /// The unit's own Cocoa view if it has one, else Apple's generic view of its
    /// parameters, added to `parent`. Returns its size in points.
    ///
    /// # Safety
    /// `parent` must be a valid `NSView` that outlives the editor. Main thread only.
    pub unsafe fn open_editor(&mut self, parent: *mut c_void) -> Result<(f64, f64), HostError> {
        self.close_editor();
        let parent = NonNull::new(parent.cast::<AnyObject>()).ok_or(HostError::NoEditor)?;
        // SAFETY: the unit is live; the view classes are called as documented.
        let view = unsafe { self.cocoa_view() }
            .or_else(|| unsafe { generic_view(self.unit.0) })
            .ok_or(HostError::NoEditor)?;
        // SAFETY: both are NSViews on the main thread.
        let frame: CGRect = unsafe {
            let _: () = msg_send![parent.as_ref(), addSubview: &*view];
            msg_send![&*view, frame]
        };
        self.editor = Some(view);
        Ok((frame.size.width, frame.size.height))
    }

    pub fn close_editor(&mut self) {
        if let Some(view) = self.editor.take() {
            // SAFETY: an NSView we added; dropping `view` releases our reference.
            let _: () = unsafe { msg_send![&*view, removeFromSuperview] };
        }
    }

    /// The view from the unit's own Cocoa UI bundle, if it names one.
    unsafe fn cocoa_view(&self) -> Option<Retained<AnyObject>> {
        // Raw pointers, not `AudioUnitCocoaViewInfo`: a buggy unit may leave one null.
        #[repr(C)]
        struct ViewInfo {
            url: *mut CFURL,
            class: *mut CFString,
        }
        let info: ViewInfo =
            self.unit
                .get_raw(kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0)?;
        let (url, class) = (NonNull::new(info.url), NonNull::new(info.class));
        // The unit hands over a reference to each; take them so they are released.
        // SAFETY: CF objects from the property, owned by us now.
        let (url, class) = unsafe {
            (
                url.map(|p| CFRetained::from_raw(p)),
                class.map(|p| CFRetained::from_raw(p)),
            )
        };
        let (url, class) = (url?, class?);
        let bundle_class = AnyClass::get(c"NSBundle")?;
        // SAFETY: CFURL and CFString are toll-free bridged to NSURL and NSString.
        unsafe {
            let url: *const AnyObject = (&*url as *const CFURL).cast();
            let name: *const AnyObject = (&*class as *const CFString).cast();
            let bundle: Option<Retained<AnyObject>> = msg_send![bundle_class, bundleWithURL: url];
            let factory_class: *const AnyClass = msg_send![&*bundle?, classNamed: name];
            let factory: Option<Retained<AnyObject>> = msg_send![factory_class.as_ref()?, new];
            let size = CGSize {
                width: 400.0,
                height: 300.0,
            };
            let factory = factory?;
            // A stale bundle's class may lack the method; calling it would throw.
            let ok: bool =
                msg_send![&*factory, respondsToSelector: sel!(uiViewForAudioUnit:withSize:)];
            if !ok {
                return None;
            }
            msg_send![&*factory, uiViewForAudioUnit: self.unit.0, withSize: size]
        }
    }

    /// The unit's ARA 2 factory, if it is an ARA plug-in.
    pub fn ara_factory(&self) -> Option<crate::ara::AraFactory> {
        let query = AuAraFactory {
            magic: ARA_MAGIC,
            factory: ptr::null(),
        };
        let found = get_in_out(&self.unit, ARA_FACTORY_PROPERTY, query)?;
        // SAFETY: the unit's factory pointer, or null.
        (found.magic == ARA_MAGIC)
            .then(|| unsafe { crate::ara::AraFactory::from_raw(found.factory) })?
    }

    pub fn ara_extension(&self) -> Option<NonNull<c_void>> {
        self.ara
    }

    pub fn kind(&self) -> AuKind {
        self.kind
    }

    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    pub fn latency_samples(&self) -> u32 {
        latency(&self.unit, self.sample_rate)
    }

    pub fn param_value(&self, id: u32) -> Option<f64> {
        let mut v = 0.0f32;
        // SAFETY: `v` is writable.
        let ok = unsafe {
            AudioUnitGetParameter(
                self.unit.0,
                id,
                kAudioUnitScope_Global,
                0,
                NonNull::from(&mut v),
            )
        };
        (ok == 0).then_some(f64::from(v))
    }

    /// Sets a parameter, clamped to its range. False for an unknown id or a
    /// non-finite value.
    pub fn set_param(&self, id: u32, value: f64) -> bool {
        let Some(p) = self.params.iter().find(|p| p.id == id) else {
            return false;
        };
        if !value.is_finite() {
            return false;
        }
        let v = value.clamp(p.min, p.max) as f32;
        // SAFETY: the unit is valid; AU parameter calls are thread-safe.
        unsafe { AudioUnitSetParameter(self.unit.0, id, kAudioUnitScope_Global, 0, v, 0) == 0 }
    }

    /// The unit's state (its class info) as a binary property list.
    pub fn save_state(&self) -> Result<Vec<u8>, HostError> {
        let raw: usize = self
            .unit
            .get(kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0)?;
        let plist = NonNull::new(raw as *mut CFType).ok_or(HostError::State)?;
        // SAFETY: the unit returns a +1 property list; we take ownership.
        let plist = unsafe { CFRetained::from_raw(plist) };
        // SAFETY: `plist` is a valid property list.
        let data = unsafe {
            CFPropertyListCreateData(
                None,
                Some(&plist),
                CFPropertyListFormat::BinaryFormat_v1_0,
                0,
                ptr::null_mut(),
            )
        }
        .ok_or(HostError::State)?;
        Ok(data.to_vec())
    }

    /// Restores state from [`AuPlugin::save_state`].
    pub fn load_state(&self, bytes: &[u8]) -> Result<(), HostError> {
        let data = CFData::from_bytes(bytes);
        // SAFETY: `data` is valid; a malformed list returns None.
        let plist = unsafe {
            CFPropertyListCreateWithData(None, Some(&data), 0, ptr::null_mut(), ptr::null_mut())
        }
        .ok_or(HostError::State)?;
        let raw = CFRetained::as_ptr(&plist).as_ptr() as usize;
        self.unit
            .set(
                kAudioUnitProperty_ClassInfo,
                kAudioUnitScope_Global,
                0,
                &raw,
            )
            .map_err(|_| HostError::State)
    }
}

fn params(unit: &Instance) -> Vec<ParamInfo> {
    let mut size = 0u32;
    // SAFETY: querying the size only.
    let ok = unsafe {
        AudioUnitGetPropertyInfo(
            unit.0,
            kAudioUnitProperty_ParameterList,
            kAudioUnitScope_Global,
            0,
            &mut size,
            ptr::null_mut(),
        )
    };
    if ok != 0 || size == 0 {
        return Vec::new();
    }
    let mut ids = vec![0u32; size as usize / 4];
    // SAFETY: `ids` holds `size` bytes.
    let ok = unsafe {
        AudioUnitGetProperty(
            unit.0,
            kAudioUnitProperty_ParameterList,
            kAudioUnitScope_Global,
            0,
            NonNull::new(ids.as_mut_ptr()).unwrap().cast(),
            NonNull::from(&mut size),
        )
    };
    if ok != 0 {
        return Vec::new();
    }
    ids.truncate(size as usize / 4);
    ids.into_iter()
        .filter_map(|id| {
            // SAFETY: the struct is plain data; zeroed is a valid empty info.
            let mut info: AudioUnitParameterInfo = unsafe { std::mem::zeroed() };
            let mut size = size_of::<AudioUnitParameterInfo>() as u32;
            // SAFETY: `info` is writable for `size` bytes.
            let ok = unsafe {
                AudioUnitGetProperty(
                    unit.0,
                    kAudioUnitProperty_ParameterInfo,
                    kAudioUnitScope_Global,
                    id,
                    NonNull::from(&mut info).cast(),
                    NonNull::from(&mut size),
                )
            };
            if ok != 0 {
                return None;
            }
            let has_cf = info
                .flags
                .contains(AudioUnitParameterOptions::Flag_HasCFNameString);
            let name = match NonNull::new(info.cfNameString.cast_mut()) {
                Some(n) if has_cf => {
                    // SAFETY: the unit provides a valid CFString; borrow it without
                    // taking ownership.
                    let s = unsafe { n.as_ref() }.to_string();
                    if info
                        .flags
                        .contains(AudioUnitParameterOptions::Flag_CFNameRelease)
                    {
                        // SAFETY: the flag says we own this reference.
                        drop(unsafe { CFRetained::from_raw(n) });
                    }
                    s
                }
                _ => {
                    // SAFETY: `name` is a NUL-terminated C array per the AU API.
                    unsafe { CStr::from_ptr(info.name.as_ptr()) }
                        .to_string_lossy()
                        .into_owned()
                }
            };
            ParamInfo::new(
                id,
                name,
                f64::from(info.minValue),
                f64::from(info.maxValue),
                f64::from(info.defaultValue),
            )
        })
        .collect()
}

/// Audio-thread side of a hosted Audio Unit. Effects take their first two inputs;
/// instruments and MIDI effects take live MIDI. Writes two outputs; any further
/// node outputs are zeroed, and a single node output takes the left channel.
pub struct AuNode {
    unit: Arc<Instance>,
    feed: Arc<InputFeed>,
    outputs: [Box<[f32]>; 2],
    max_block: usize,
    sample_time: f64,
    latency: u32,
    kind: AuKind,
}

impl AudioNode for AuNode {
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
        // Pre-zeroed silence stands in for missing inputs (untrusted plugin).
        self.feed.count.set(inputs.len().min(2));
        self.feed.frames.set(n);
        for (slot, input) in self.feed.channels.iter().zip(inputs) {
            slot.set(input.as_ptr());
        }
        if self.kind != AuKind::Effect {
            for e in midi_in {
                let [status, d1, d2] = e.data;
                // SAFETY: real-time safe MIDI scheduling call on a valid unit.
                unsafe {
                    MusicDeviceMIDIEvent(
                        self.unit.0,
                        u32::from(status),
                        u32::from(d1),
                        u32::from(d2),
                        e.sample_offset.min(n as u32 - 1),
                    );
                }
            }
        }
        let [left, right] = &mut self.outputs;
        // Pre-zeroed, so a unit that skips writing (silence) leaves silence.
        left[..n].fill(0.0);
        right[..n].fill(0.0);
        let mut list = BufferList {
            count: 2,
            // SAFETY: AudioBuffer is plain data; filled in below.
            buffers: unsafe { std::mem::zeroed() },
        };
        for (b, out) in list.buffers.iter_mut().zip([left, right]) {
            *b = AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: (n * 4) as u32,
                mData: out.as_mut_ptr().cast(),
            };
        }
        // SAFETY: plain data; only the sample time is meaningful here.
        let mut time: AudioTimeStamp = unsafe { std::mem::zeroed() };
        time.mSampleTime = self.sample_time;
        time.mFlags = AudioTimeStampFlags::SampleTimeValid;
        let mut flags = AudioUnitRenderActionFlags(0);
        // SAFETY: `list` points at two buffers of `n` floats; the input feed points
        // at slices that live for this call.
        let status = unsafe {
            AudioUnitRender(
                self.unit.0,
                &mut flags,
                NonNull::from(&mut time),
                0,
                n as u32,
                NonNull::from(&mut list).cast(),
            )
        };
        self.sample_time += n as f64;
        self.feed.count.set(0); // the input slices die with this call
        for (ch, out) in outputs.iter_mut().enumerate() {
            match self.outputs.get(ch) {
                Some(src) if status == 0 => out.copy_from_slice(&src[..n]),
                _ => out.fill(0.0),
            }
        }
    }

    fn latency_samples(&self) -> u32 {
        self.latency
    }

    fn reset(&mut self) {
        // SAFETY: valid unit; reset is allowed from the render thread.
        unsafe { AudioUnitReset(self.unit.0, kAudioUnitScope_Global, 0) };
        self.sample_time = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: &str = "aufx:dely:appl";
    const DLS: &str = "aumu:dls :appl";

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: n,
            playhead: 0,
            playing: true,
        }
    }

    /// A 512-frame stereo block into fixed arrays, so it can run under
    /// `assert_no_alloc`; returns the left channel.
    fn render_into(node: &mut AuNode, input: &[f32; 512]) -> [f32; 512] {
        let (mut l, mut r) = ([9.0f32; 512], [9.0f32; 512]);
        let ins: [&[f32]; 2] = [input, input];
        node.process(&ins, &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx(512));
        l
    }

    fn render(node: &mut AuNode, input: &[f32], midi: &[MidiEvent]) -> (Vec<f32>, Vec<f32>) {
        let n = input.len();
        let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
        node.process(
            &[input, input],
            &mut [&mut l, &mut r],
            midi,
            &mut Vec::new(),
            &ctx(n),
        );
        (l, r)
    }

    #[test]
    fn lists_apple_units_and_rejects_bad_ids() {
        let units = audio_units();
        let delay = units
            .iter()
            .find(|(p, _)| p.id == DELAY)
            .expect("AUDelay ships with macOS");
        assert_eq!(
            (delay.0.vendor.as_str(), delay.1),
            ("Apple", AuKind::Effect)
        );
        assert!(units
            .iter()
            .any(|(p, k)| p.id == DLS && *k == AuKind::Instrument));
        for bad in [
            "",
            "aufx:dely",
            "aufx:dely:appl:x",
            "aufx:nope:none",
            "zzzz:dely:appl",
        ] {
            assert!(
                matches!(
                    AuPlugin::new(bad, 48_000.0, 256),
                    Err(HostError::UnknownPlugin(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn delay_effect_processes_audio_and_binds_params_and_state() {
        let (plugin, mut node) = AuPlugin::new(DELAY, 48_000.0, 512).unwrap();
        assert!(!plugin.params().is_empty());
        let p = plugin.params()[0].clone();
        assert!(p.min <= p.default && p.default <= p.max, "{p:?}");

        // An impulse comes out delayed; something reaches the output within a second.
        let mut impulse = [0.0f32; 512];
        impulse[0] = 1.0;
        let first = assert_no_alloc::assert_no_alloc(|| render_into(&mut node, &impulse));
        let mut heard = first.iter().any(|v| v.abs() > 1e-4);
        for _ in 0..100 {
            heard |= render(&mut node, &[0.0; 512], &[])
                .0
                .iter()
                .any(|v| v.abs() > 1e-4);
        }
        assert!(heard, "delay produced no output");

        let saved = plugin.save_state().unwrap();
        assert!(plugin.set_param(p.id, p.max * 10.0)); // clamped
        assert_eq!(plugin.param_value(p.id), Some(p.max));
        plugin.load_state(&saved).unwrap();
        assert_eq!(plugin.param_value(p.id), Some(p.default));
        assert!(!plugin.set_param(u32::MAX, 0.0) && !plugin.set_param(p.id, f64::NAN));
        assert!(matches!(
            plugin.load_state(b"not a plist"),
            Err(HostError::State)
        ));

        // Oversized blocks are silence, not a crash.
        let (l, _) = render(&mut node, &[1.0; 600], &[]);
        assert!(l.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn instrument_plays_a_note() {
        let (_plugin, mut node) = AuPlugin::new(DLS, 48_000.0, 512).unwrap();
        let on = MidiEvent {
            sample_offset: 0,
            data: [0x90, 60, 110],
        };
        let mut out = [0.0f32; 512];
        assert_no_alloc::assert_no_alloc(|| {
            node.process(&[], &mut [&mut out], &[on], &mut Vec::new(), &ctx(512));
        });
        let mut heard = out.iter().any(|v| v.abs() > 1e-4);
        for _ in 0..20 {
            heard |= render(&mut node, &[0.0; 512], &[])
                .0
                .iter()
                .any(|v| v.abs() > 1e-4);
        }
        assert!(heard, "DLS synth stayed silent");
    }
}
