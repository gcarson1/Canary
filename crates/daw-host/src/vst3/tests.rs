//! A small VST3 plugin built in-process (after the `vst3` crate's gain example):
//! stereo gain (param 0, normalized), +1.0 while a note is held, 16 samples of
//! latency, gain saved as state, with a separate edit controller.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};

use assert_no_alloc::assert_no_alloc;
use vst3::uid;

use super::*;

static LIVE: AtomicIsize = AtomicIsize::new(0);
/// Tests that count live instances run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

struct Processor {
    gain: AtomicU64,
    held: AtomicBool,
}

impl Class for Processor {
    type Interfaces = (IComponent, IAudioProcessor);
}

const PROCESSOR: TUID = uid(0x6E332252, 0x54224A00, 0xAA69301A, 0xF318797D);
const CONTROLLER: TUID = uid(0x1BA8A477, 0xEE0A4A2D, 0x80F50D14, 0x13D2EAA0);

unsafe fn read_f64(stream: *mut IBStream) -> Option<f64> {
    let s = unsafe { ComRef::from_raw(stream) }?;
    let mut bytes = [0u8; 8];
    let mut n = 0;
    unsafe { s.read(bytes.as_mut_ptr().cast(), 8, &mut n) };
    (n == 8).then(|| f64::from_le_bytes(bytes))
}

use vst3::ComRef;

impl IPluginBaseTrait for Processor {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        LIVE.fetch_add(1, Ordering::SeqCst);
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        LIVE.fetch_sub(1, Ordering::SeqCst);
        kResultOk
    }
}

impl IComponentTrait for Processor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        unsafe { *class_id = CONTROLLER };
        kResultOk
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media: MediaType, dir: BusDirection) -> int32 {
        match (media as MediaTypes, dir as BusDirections) {
            (MediaTypes_::kAudio, _) => 1,
            (MediaTypes_::kEvent, BusDirections_::kInput) => 1,
            _ => 0,
        }
    }
    unsafe fn getBusInfo(
        &self,
        media: MediaType,
        dir: BusDirection,
        index: int32,
        bus: *mut BusInfo,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        let bus = unsafe { &mut *bus };
        bus.mediaType = media;
        bus.direction = dir;
        bus.channelCount = if media == MediaTypes_::kAudio as MediaType {
            2
        } else {
            16
        };
        copy_wstring("Main", &mut bus.name);
        bus.busType = BusTypes_::kMain as BusType;
        bus.flags = BusInfo_::BusFlags_::kDefaultActive;
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _i: *mut RoutingInfo, _o: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(&self, _m: MediaType, _d: BusDirection, _i: int32, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        match unsafe { read_f64(state) } {
            Some(g) => {
                self.gain.store(g.to_bits(), Ordering::Relaxed);
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else {
            return kResultFalse;
        };
        let mut bytes = f64::from_bits(self.gain.load(Ordering::Relaxed)).to_le_bytes();
        unsafe { s.write(bytes.as_mut_ptr().cast(), 8, ptr::null_mut()) };
        kResultOk
    }
}

impl IAudioProcessorTrait for Processor {
    unsafe fn setBusArrangements(
        &self,
        _i: *mut SpeakerArrangement,
        _ni: int32,
        _o: *mut SpeakerArrangement,
        _no: int32,
    ) -> tresult {
        kResultTrue
    }
    unsafe fn getBusArrangement(
        &self,
        _d: BusDirection,
        _i: int32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        unsafe { *arr = SpeakerArr::kStereo };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: int32) -> tresult {
        if size == SymbolicSampleSizes_::kSample32 as int32 {
            kResultOk
        } else {
            kNotImplemented
        }
    }
    unsafe fn getLatencySamples(&self) -> uint32 {
        16
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }
    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let d = unsafe { &*data };
        if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
            for i in 0..unsafe { changes.getParameterCount() } {
                let Some(q) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else {
                    continue;
                };
                let (mut offset, mut value) = (0, 0.0);
                if unsafe { q.getParameterId() } == 0
                    && unsafe { q.getPoint(q.getPointCount() - 1, &mut offset, &mut value) }
                        == kResultOk
                {
                    self.gain.store(value.to_bits(), Ordering::Relaxed);
                }
            }
        }
        let n = d.numSamples as usize;
        // Note on/off times in this block, in order.
        let mut marks = [(usize::MAX, false); 8];
        if let Some(events) = unsafe { ComRef::from_raw(d.inputEvents) } {
            for i in 0..unsafe { events.getEventCount() }.min(8) {
                let mut e: Event = unsafe { std::mem::zeroed() };
                unsafe { events.getEvent(i, &mut e) };
                let on = e.r#type == Event_::EventTypes_::kNoteOnEvent as u16;
                marks[i as usize] = (e.sampleOffset as usize, on);
            }
        }
        if d.numInputs != 1 || d.numOutputs != 1 {
            return kResultOk;
        }
        let gain = f64::from_bits(self.gain.load(Ordering::Relaxed)) as f32;
        let (ib, ob) = unsafe { (&*d.inputs, &*d.outputs) };
        let ins = unsafe { std::slice::from_raw_parts(ib.__field0.channelBuffers32, 2) };
        let outs = unsafe { std::slice::from_raw_parts(ob.__field0.channelBuffers32, 2) };
        let start = self.held.load(Ordering::Relaxed);
        let mut held = start;
        for ch in 0..2 {
            held = start;
            let input = unsafe { std::slice::from_raw_parts(ins[ch], n) };
            let output = unsafe { std::slice::from_raw_parts_mut(outs[ch], n) };
            let mut next = 0;
            for i in 0..n {
                while next < 8 && marks[next].0 <= i {
                    held = marks[next].1;
                    next += 1;
                }
                output[i] = input[i] * gain + if held { 1.0 } else { 0.0 };
            }
        }
        self.held.store(held, Ordering::Relaxed);
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> uint32 {
        0
    }
}

struct Controller {
    gain: AtomicU64,
}

impl Class for Controller {
    type Interfaces = (IEditController,);
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        match unsafe { read_f64(state) } {
            Some(g) => {
                self.gain.store(g.to_bits(), Ordering::Relaxed);
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> int32 {
        1
    }
    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        let info = unsafe { &mut *info };
        info.id = 0;
        copy_wstring("Gain", &mut info.title);
        info.defaultNormalizedValue = 1.0;
        info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate as int32;
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, _id: u32, _v: f64, _s: *mut String128) -> tresult {
        kNotImplemented
    }
    unsafe fn getParamValueByString(&self, _id: u32, _s: *mut TChar, _v: *mut f64) -> tresult {
        kNotImplemented
    }
    unsafe fn normalizedParamToPlain(&self, _id: u32, v: f64) -> f64 {
        v
    }
    unsafe fn plainParamToNormalized(&self, _id: u32, v: f64) -> f64 {
        v
    }
    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        if id == 0 {
            f64::from_bits(self.gain.load(Ordering::Relaxed))
        } else {
            0.0
        }
    }
    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        if id != 0 {
            return kInvalidArgument;
        }
        self.gain.store(value.to_bits(), Ordering::Relaxed);
        kResultOk
    }
    unsafe fn setComponentHandler(&self, _h: *mut IComponentHandler) -> tresult {
        kResultOk
    }
    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        ptr::null_mut()
    }
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory,);
}

fn copy_cstring(src: &str, dst: &mut [i8]) {
    for (d, s) in dst.iter_mut().zip(src.bytes().chain(std::iter::once(0))) {
        *d = s as i8;
    }
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        copy_cstring("Canary Tests", unsafe { &mut (*info).vendor });
        kResultOk
    }
    unsafe fn countClasses(&self) -> int32 {
        2
    }
    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        let info = unsafe { &mut *info };
        let (cid, category) = match index {
            0 => (PROCESSOR, "Audio Module Class"),
            1 => (CONTROLLER, "Component Controller Class"),
            _ => return kInvalidArgument,
        };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring("Test Gain", &mut info.name);
        kResultOk
    }
    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        let instance = match unsafe { *(cid as *const TUID) } {
            PROCESSOR => ComWrapper::new(Processor {
                gain: AtomicU64::new(1.0f64.to_bits()),
                held: AtomicBool::new(false),
            })
            .to_com_ptr::<FUnknown>(),
            CONTROLLER => ComWrapper::new(Controller {
                gain: AtomicU64::new(1.0f64.to_bits()),
            })
            .to_com_ptr::<FUnknown>(),
            _ => None,
        };
        match instance {
            Some(i) => {
                let p = i.as_ptr();
                unsafe { ((*(*p).vtbl).queryInterface)(p, iid as *mut TUID, obj) }
            }
            None => kInvalidArgument,
        }
    }
}

fn bundle() -> Arc<Vst3Bundle> {
    Vst3Bundle::from_factory(
        ComWrapper::new(Factory)
            .to_com_ptr::<IPluginFactory>()
            .unwrap(),
    )
}

fn ctx(n: usize) -> ProcessContext {
    ProcessContext {
        sample_rate: 48_000.0,
        num_samples: n,
        playhead: 0,
        playing: true,
    }
}

fn run(node: &mut Vst3Node, midi: &[MidiEvent]) -> [f32; 8] {
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
fn lists_binds_params_and_reports_latency() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let b = bundle();
    let plugins = b.plugins();
    assert_eq!(plugins.len(), 1, "controllers are not listed");
    assert_eq!(
        (plugins[0].name.as_str(), plugins[0].vendor.as_str()),
        ("Test Gain", "Canary Tests")
    );
    assert!(matches!(
        Vst3Plugin::new(&b, "00", 48_000.0, 64),
        Err(HostError::UnknownPlugin(_))
    ));
    let controller_id = hex(&CONTROLLER);
    assert!(matches!(
        Vst3Plugin::new(&b, &controller_id, 48_000.0, 64),
        Err(HostError::UnknownPlugin(_))
    ));

    let (mut plugin, node) = Vst3Plugin::new(&b, &plugins[0].id, 48_000.0, 64).unwrap();
    assert_eq!(
        plugin.params(),
        [ParamInfo {
            id: 0,
            name: "Gain".into(),
            min: 0.0,
            max: 1.0,
            default: 1.0,
            log: false,
        }]
    );
    assert_eq!((plugin.latency_samples(), node.latency_samples()), (16, 16));
    assert!(!plugin.set_param(9, 0.5) && !plugin.set_param(0, f64::NAN));
}

#[test]
fn params_and_notes_reach_the_processor_without_allocating() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let b = bundle();
    let id = b.plugins()[0].id.clone();
    let (mut plugin, mut node) = Vst3Plugin::new(&b, &id, 48_000.0, 64).unwrap();
    assert_eq!(run(&mut node, &[]), [1.0; 8]);
    assert!(plugin.set_param(0, 0.5));
    assert_eq!(plugin.param_value(0), Some(0.5));
    let on = MidiEvent {
        sample_offset: 3,
        data: [0x90, 60, 100],
    };
    let off = MidiEvent {
        sample_offset: 6,
        data: [0x80, 60, 0],
    };
    let out = assert_no_alloc(|| run(&mut node, &[on, off]));
    assert_eq!(out, [0.5, 0.5, 0.5, 1.5, 1.5, 1.5, 0.5, 0.5]);
    // Oversized blocks are silence.
    let input = [1.0; 80];
    let mut l = [9.0; 80];
    node.process(&[&input], &mut [&mut l], &[], &mut Vec::new(), &ctx(80));
    assert!(l.iter().all(|&v| v == 0.0));
}

#[test]
fn state_round_trips_and_the_plugin_terminates_when_both_halves_go() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let before = LIVE.load(Ordering::SeqCst);
    let b = bundle();
    let id = b.plugins()[0].id.clone();
    let (mut plugin, mut node) = Vst3Plugin::new(&b, &id, 48_000.0, 64).unwrap();
    let saved = plugin.save_state().unwrap();
    plugin.set_param(0, 0.25);
    assert_eq!(run(&mut node, &[]), [0.25; 8]);
    plugin.load_state(&saved).unwrap();
    assert_eq!(
        plugin.param_value(0),
        Some(1.0),
        "controller synced from component state"
    );
    assert_eq!(run(&mut node, &[]), [1.0; 8]);
    assert!(matches!(
        plugin.load_state(&[1, 2, 3]),
        Err(HostError::State)
    ));
    assert!(matches!(
        plugin.load_state(&[255; 9]),
        Err(HostError::State)
    ));

    assert_eq!(LIVE.load(Ordering::SeqCst), before + 1);
    drop(plugin);
    assert_eq!(
        LIVE.load(Ordering::SeqCst),
        before + 1,
        "node still holds it"
    );
    drop(node);
    assert_eq!(LIVE.load(Ordering::SeqCst), before);
}

#[test]
fn stream_reads_nothing_past_the_end_and_rejects_bad_seeks() {
    let stream = Stream::new(vec![1, 2, 3]);
    let s = stream.as_com_ref::<IBStream>().unwrap();
    let (mut buf, mut n, mut at) = ([0u8; 4], -1, 0i64);
    unsafe {
        assert_eq!(
            s.seek(4, IBStream_::IStreamSeekMode_::kIBSeekEnd as int32, &mut at),
            kResultOk
        );
        assert_eq!(at, 7);
        assert_eq!(s.read(buf.as_mut_ptr().cast(), 4, &mut n), kResultOk);
        assert_eq!(n, 0);
        assert_eq!(
            s.seek(
                i64::MAX,
                IBStream_::IStreamSeekMode_::kIBSeekCur as int32,
                ptr::null_mut()
            ),
            kInvalidArgument
        );
        assert_eq!(
            s.seek(
                -1,
                IBStream_::IStreamSeekMode_::kIBSeekSet as int32,
                ptr::null_mut()
            ),
            kInvalidArgument
        );
        assert_eq!(s.read(ptr::null_mut(), 0, &mut n), kResultOk);
        assert_eq!(s.write(ptr::null_mut(), 0, &mut n), kResultOk);
        s.seek(
            1,
            IBStream_::IStreamSeekMode_::kIBSeekSet as int32,
            ptr::null_mut(),
        );
        assert_eq!(s.read(buf.as_mut_ptr().cast(), 4, &mut n), kResultOk);
        assert_eq!((n, &buf[..2]), (2, &[2u8, 3][..]));
    }
}
