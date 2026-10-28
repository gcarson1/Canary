//! ARA 2 hosting (Celemony ARA_API 2.3, headers vendored in `third_party/ARA_API`):
//! an ARA document mirroring the project's clips, which ARA plug-ins (Melodyne
//! and the like) read sample-accurately and render as playback regions.
//!
//! One `Document` per plug-in factory. The host side is the five controller
//! interfaces ARA calls back into: audio access (reads `SampleData`) and
//! archiving (plug-in document state to and from bytes) are implemented;
//! content access, model updates and playback requests are left out, which ARA
//! allows. All calls are main-thread only.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use daw_core::SampleData;

use crate::HostError;

#[cfg_attr(target_arch = "aarch64", path = "sys_aarch64.rs")]
#[cfg_attr(not(target_arch = "aarch64"), path = "sys_x86_64.rs")]
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unnecessary_transmutes,
    clippy::all
)]
pub(crate) mod sys;

/// The plug-in instance roles Canary assigns: it renders playback regions and may
/// show the plug-in's editor. Editor rendering (auditioning) is not used.
pub(crate) const ROLES: sys::ARAPlugInInstanceRoleFlags =
    (sys::kARAPlaybackRendererRole | sys::kARAEditorViewRole) as i32;
pub(crate) const KNOWN_ROLES: sys::ARAPlugInInstanceRoleFlags =
    (sys::kARAPlaybackRendererRole | sys::kARAEditorRendererRole | sys::kARAEditorViewRole) as i32;

/// A plug-in's ARA factory. Valid while the plug-in's library stays loaded,
/// which the catalog guarantees for the life of the app.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AraFactory(NonNull<sys::ARAFactory>);

impl AraFactory {
    /// None for a null pointer, a struct too small, or no API generation in
    /// common (2.0 final up to 2.3).
    ///
    /// # Safety
    /// `raw` must be null or point to a plug-in's `ARAFactory`.
    pub(crate) unsafe fn from_raw(raw: *const c_void) -> Option<Self> {
        let f = NonNull::new(raw.cast_mut().cast::<sys::ARAFactory>())?;
        // SAFETY: a factory from the plug-in, read field by field (packed).
        let (size, low, high) = unsafe {
            let r = f.as_ptr();
            (
                ptr::addr_of!((*r).structSize).read_unaligned(),
                ptr::addr_of!((*r).lowestSupportedApiGeneration).read_unaligned(),
                ptr::addr_of!((*r).highestSupportedApiGeneration).read_unaligned(),
            )
        };
        let usable = size >= sys::kARAFactoryMinSize as usize
            && low <= sys::kARAAPIGeneration_2_3_Final as i32
            && high >= sys::kARAAPIGeneration_2_0_Final as i32;
        usable.then_some(Self(f))
    }

    fn get(&self) -> sys::ARAFactory {
        // SAFETY: checked in `from_raw`; a plain read of a `Copy` struct.
        unsafe { self.0.as_ptr().read_unaligned() }
    }

    pub fn name(&self) -> String {
        text(self.get().plugInName)
    }

    /// The API generation both sides support, newest first.
    fn generation(&self) -> i32 {
        self.get()
            .highestSupportedApiGeneration
            .min(sys::kARAAPIGeneration_2_3_Final as i32)
    }
}

fn text(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: ARA strings are NUL-terminated UTF-8.
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("NULs removed")
}

/// What ARA sees of a clip.
#[derive(Clone)]
pub struct AraClip {
    /// Stable across edits, so the plug-in keeps its edits to the region.
    pub id: u64,
    pub track: usize,
    pub name: String,
    pub audio: Arc<SampleData>,
    /// Timeline position, source offset and length, in samples.
    pub start: u64,
    pub offset: u64,
    pub length: u64,
}

/// Behind each audio source's host ref: the samples the plug-in reads.
struct SourceHost {
    audio: Arc<SampleData>,
}

struct Reader {
    audio: Arc<SampleData>,
    wide: bool,
}

struct Source {
    _host: Box<SourceHost>,
    source: sys::ARAAudioSourceRef,
    modification: sys::ARAAudioModificationRef,
    _ids: [CString; 2],
}

struct Region {
    region: sys::ARAPlaybackRegionRef,
    track: usize,
    /// `Arc::as_ptr` of the clip's audio: the source it plays.
    source: usize,
}

/// A plug-in instance's ARA side, bound to a document.
struct Binding {
    track: usize,
    instance: NonNull<sys::ARAPlugInExtensionInstance>,
    regions: Vec<sys::ARAPlaybackRegionRef>,
}

/// Archive bytes a reader or writer host ref points to.
struct Archive {
    bytes: Vec<u8>,
    id: CString,
}

pub struct Document {
    factory: AraFactory,
    controller: sys::ARADocumentControllerInstance,
    // Boxed so the pointers the plug-in keeps stay put.
    _host: Box<sys::ARADocumentControllerHostInstance>,
    _audio: Box<sys::ARAAudioAccessControllerInterface>,
    _archiving: Box<sys::ARAArchivingControllerInterface>,
    context: sys::ARAMusicalContextRef,
    sequences: HashMap<usize, sys::ARARegionSequenceRef>,
    sources: HashMap<usize, Source>,
    regions: HashMap<u64, Region>,
    bindings: Vec<Binding>,
    sample_rate: f64,
    _names: Vec<CString>,
}

impl Document {
    /// Initializes ARA in the plug-in and creates a document controller with an
    /// empty document at the project's `sample_rate`.
    pub fn new(factory: AraFactory, name: &str, sample_rate: f64) -> Result<Self, HostError> {
        let f = factory.get();
        let config = sys::ARAInterfaceConfiguration {
            structSize: size_of::<sys::ARAInterfaceConfiguration>(),
            desiredApiGeneration: factory.generation(),
            assertFunctionAddress: ptr::null_mut(),
        };
        let init = f.initializeARAWithConfiguration.ok_or(HostError::NoAra)?;
        let create = f
            .createDocumentControllerWithDocument
            .ok_or(HostError::NoAra)?;
        // SAFETY: called once per document, as ARA requires, on the main thread.
        unsafe { init(&config) };

        let audio = Box::new(sys::ARAAudioAccessControllerInterface {
            structSize: size_of::<sys::ARAAudioAccessControllerInterface>(),
            createAudioReaderForSource: Some(create_reader),
            readAudioSamples: Some(read_samples),
            destroyAudioReader: Some(destroy_reader),
        });
        let archiving = Box::new(sys::ARAArchivingControllerInterface {
            structSize: size_of::<sys::ARAArchivingControllerInterface>(),
            getArchiveSize: Some(archive_size),
            readBytesFromArchive: Some(archive_read),
            writeBytesToArchive: Some(archive_write),
            notifyDocumentArchivingProgress: Some(progress),
            notifyDocumentUnarchivingProgress: Some(progress),
            getDocumentArchiveID: Some(archive_id),
        });
        let host = Box::new(sys::ARADocumentControllerHostInstance {
            structSize: size_of::<sys::ARADocumentControllerHostInstance>(),
            audioAccessControllerHostRef: ptr::null_mut(),
            audioAccessControllerInterface: &*audio,
            archivingControllerHostRef: ptr::null_mut(),
            archivingControllerInterface: &*archiving,
            contentAccessControllerHostRef: ptr::null_mut(),
            contentAccessControllerInterface: ptr::null(),
            modelUpdateControllerHostRef: ptr::null_mut(),
            modelUpdateControllerInterface: ptr::null(),
            playbackControllerHostRef: ptr::null_mut(),
            playbackControllerInterface: ptr::null(),
        });
        let doc_name = c(name);
        let props = sys::ARADocumentProperties {
            structSize: size_of::<sys::ARADocumentProperties>(),
            name: doc_name.as_ptr(),
        };
        // SAFETY: host interfaces outlive the controller (they live in `Self`).
        let raw = unsafe { create(&*host, &props) };
        let Some(raw) = NonNull::new(raw.cast_mut()) else {
            if let Some(uninit) = f.uninitializeARA {
                // SAFETY: balances `init` above.
                unsafe { uninit() };
            }
            return Err(HostError::NoAra);
        };
        // SAFETY: the plug-in returned a valid instance.
        let controller = unsafe { raw.as_ptr().read_unaligned() };
        let mut doc = Self {
            factory,
            controller,
            _host: host,
            _audio: audio,
            _archiving: archiving,
            context: ptr::null_mut(),
            sequences: HashMap::new(),
            sources: HashMap::new(),
            regions: HashMap::new(),
            bindings: Vec::new(),
            sample_rate,
            _names: vec![doc_name],
        };
        doc.edit(|d| {
            let name = c("Canary");
            let props = sys::ARAMusicalContextProperties {
                structSize: size_of::<sys::ARAMusicalContextProperties>(),
                name: name.as_ptr(),
                orderIndex: 0,
                color: ptr::null(),
            };
            // SAFETY: inside begin/endEditing.
            d.context =
                unsafe { (d.dc().createMusicalContext.unwrap())(d.r(), ptr::null_mut(), &props) };
            d._names.push(name);
        });
        Ok(doc)
    }

    pub fn factory(&self) -> AraFactory {
        self.factory
    }

    /// The controller ref plug-in instances are bound to.
    pub fn controller_ref(&self) -> *mut c_void {
        self.r().cast()
    }

    fn r(&self) -> sys::ARADocumentControllerRef {
        self.controller.documentControllerRef
    }

    fn dc(&self) -> sys::ARADocumentControllerInterface {
        // SAFETY: the plug-in's interface, valid until the controller is destroyed.
        unsafe { self.controller.documentControllerInterface.read_unaligned() }
    }

    fn edit(&mut self, f: impl FnOnce(&mut Self)) {
        let dc = self.dc();
        // SAFETY: paired calls on our live controller.
        unsafe { (dc.beginEditing.unwrap())(self.r()) };
        f(self);
        unsafe { (dc.endEditing.unwrap())(self.r()) };
    }

    /// Adds the plug-in instance a format bound to this document (see
    /// `Hosted::ara_extension`) as the renderer of `track`'s clips.
    ///
    /// # Safety
    /// `instance` must be the `ARAPlugInExtensionInstance` the plug-in returned
    /// when bound to this document, and must stay valid until `unbind`.
    pub unsafe fn bind(&mut self, track: usize, instance: NonNull<c_void>) {
        self.bindings.push(Binding {
            track,
            instance: instance.cast(),
            regions: Vec::new(),
        });
        self.render_regions();
    }

    /// Removes `instance`'s regions before the plug-in instance goes.
    pub fn unbind(&mut self, instance: NonNull<c_void>) {
        if let Some(i) = self
            .bindings
            .iter()
            .position(|b| b.instance.cast() == instance)
        {
            let mut b = self.bindings.remove(i);
            set_renderer_regions(&mut b, &[]);
        }
    }

    /// Makes the document match `clips`: one region sequence per track, one audio
    /// source per audio file, one playback region per clip.
    pub fn sync(&mut self, clips: &[AraClip]) {
        let gone: Vec<u64> = self
            .regions
            .keys()
            .filter(|id| !clips.iter().any(|c| c.id == **id))
            .copied()
            .collect();
        // Renderers drop regions before the document destroys them.
        let remaining: Vec<_> = self
            .regions
            .iter()
            .filter(|(id, _)| !gone.contains(id))
            .map(|(_, r)| (r.track, r.region))
            .collect();
        for b in &mut self.bindings {
            let keep: Vec<_> = remaining
                .iter()
                .filter(|(t, _)| *t == b.track)
                .map(|r| r.1)
                .collect();
            set_renderer_regions(b, &keep);
        }
        self.edit(|d| {
            let dc = d.dc();
            for id in gone {
                if let Some(r) = d.regions.remove(&id) {
                    // SAFETY: no renderer holds it any more.
                    unsafe { (dc.destroyPlaybackRegion.unwrap())(d.r(), r.region) };
                }
            }
            for clip in clips {
                d.place(clip);
            }
            let used: Vec<usize> = d.regions.values().map(|r| r.source).collect();
            let unused: Vec<usize> = d
                .sources
                .keys()
                .filter(|k| !used.contains(k))
                .copied()
                .collect();
            for key in unused {
                if let Some(s) = d.sources.remove(&key) {
                    // SAFETY: no region plays it; modification goes first.
                    unsafe {
                        (dc.destroyAudioModification.unwrap())(d.r(), s.modification);
                        (dc.enableAudioSourceSamplesAccess.unwrap())(d.r(), s.source, 0);
                        (dc.destroyAudioSource.unwrap())(d.r(), s.source);
                    }
                }
            }
        });
        self.render_regions();
    }

    /// Creates or updates the region for `clip`, and its source if new.
    fn place(&mut self, clip: &AraClip) {
        let dc = self.dc();
        let key = Arc::as_ptr(&clip.audio) as usize;
        if !self.sources.contains_key(&key) {
            let ids = [
                c(&format!("canary.source.{key:x}")),
                c(&format!("canary.mod.{key:x}")),
            ];
            let name = c(&clip.name);
            let host = Box::new(SourceHost {
                audio: clip.audio.clone(),
            });
            let props = sys::ARAAudioSourceProperties {
                structSize: size_of::<sys::ARAAudioSourceProperties>(),
                name: name.as_ptr(),
                persistentID: ids[0].as_ptr(),
                sampleCount: clip.audio.frames() as i64,
                sampleRate: f64::from(clip.audio.sample_rate()),
                channelCount: clip.audio.num_channels() as i32,
                merits64BitSamples: 0,
                channelArrangementDataType: sys::kARAChannelArrangementUndefined as i32,
                channelArrangement: ptr::null(),
            };
            let mod_props = sys::ARAAudioModificationProperties {
                structSize: size_of::<sys::ARAAudioModificationProperties>(),
                name: name.as_ptr(),
                persistentID: ids[1].as_ptr(),
            };
            let host_ref = (&*host as *const SourceHost).cast_mut().cast();
            // SAFETY: inside editing; `host` lives as long as the source.
            let (source, modification) = unsafe {
                let source = (dc.createAudioSource.unwrap())(self.r(), host_ref, &props);
                (dc.enableAudioSourceSamplesAccess.unwrap())(self.r(), source, 1);
                let m = (dc.createAudioModification.unwrap())(
                    self.r(),
                    source,
                    ptr::null_mut(),
                    &mod_props,
                );
                (source, m)
            };
            self.sources.insert(
                key,
                Source {
                    _host: host,
                    source,
                    modification,
                    _ids: ids,
                },
            );
            self._names.push(name);
        }
        let sequence = self.sequence(clip.track);
        let rate = f64::from(clip.audio.sample_rate());
        let name = c(&clip.name);
        let props = sys::ARAPlaybackRegionProperties {
            structSize: size_of::<sys::ARAPlaybackRegionProperties>(),
            transformationFlags: sys::kARAPlaybackTransformationNoChanges as i32,
            startInModificationTime: clip.offset as f64 / rate,
            durationInModificationTime: clip.length as f64 / rate,
            startInPlaybackTime: clip.start as f64 / self.sample_rate,
            durationInPlaybackTime: clip.length as f64 / rate,
            musicalContextRef: self.context,
            regionSequenceRef: sequence,
            name: name.as_ptr(),
            color: ptr::null(),
        };
        let modification = self.sources[&key].modification;
        match self.regions.get(&clip.id).map(|r| (r.source, r.region)) {
            Some((source, region)) if source == key => {
                // SAFETY: inside editing, a live region.
                unsafe { (dc.updatePlaybackRegionProperties.unwrap())(self.r(), region, &props) };
                if let Some(r) = self.regions.get_mut(&clip.id) {
                    r.track = clip.track;
                }
            }
            existing => {
                if let Some((_, old)) = existing {
                    // A clip now playing another file: ARA regions cannot move.
                    for b in &mut self.bindings {
                        let keep: Vec<_> =
                            b.regions.iter().copied().filter(|r| *r != old).collect();
                        set_renderer_regions(b, &keep);
                    }
                    // SAFETY: inside editing; no renderer holds it now.
                    unsafe { (dc.destroyPlaybackRegion.unwrap())(self.r(), old) };
                }
                // SAFETY: inside editing.
                let region = unsafe {
                    (dc.createPlaybackRegion.unwrap())(
                        self.r(),
                        modification,
                        ptr::null_mut(),
                        &props,
                    )
                };
                self.regions.insert(
                    clip.id,
                    Region {
                        region,
                        track: clip.track,
                        source: key,
                    },
                );
            }
        }
        self._names.push(name);
    }

    fn sequence(&mut self, track: usize) -> sys::ARARegionSequenceRef {
        if let Some(s) = self.sequences.get(&track) {
            return *s;
        }
        let name = c(&format!("Track {}", track + 1));
        let props = sys::ARARegionSequenceProperties {
            structSize: size_of::<sys::ARARegionSequenceProperties>(),
            name: name.as_ptr(),
            orderIndex: track as i32,
            musicalContextRef: self.context,
            color: ptr::null(),
        };
        // SAFETY: called inside editing.
        let s =
            unsafe { (self.dc().createRegionSequence.unwrap())(self.r(), ptr::null_mut(), &props) };
        self.sequences.insert(track, s);
        self._names.push(name);
        s
    }

    /// Gives each bound renderer exactly its track's regions.
    fn render_regions(&mut self) {
        for b in &mut self.bindings {
            let want: Vec<_> = self
                .regions
                .values()
                .filter(|r| r.track == b.track)
                .map(|r| r.region)
                .collect();
            set_renderer_regions(b, &want);
        }
    }

    /// The plug-in's document data, for saving with the project.
    pub fn store(&self) -> Result<Vec<u8>, HostError> {
        let mut archive = Archive {
            bytes: Vec::new(),
            id: CString::default(),
        };
        let writer = (&mut archive as *mut Archive).cast();
        // SAFETY: `archive` outlives the call.
        let ok = unsafe { (self.dc().storeDocumentToArchive.unwrap())(self.r(), writer) };
        if ok == 0 {
            return Err(HostError::State);
        }
        Ok(archive.bytes)
    }

    /// Restores document data saved by `store`. Call before `sync`.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        let f = self.factory.get();
        let mut archive = Archive {
            bytes: bytes.to_vec(),
            id: CString::from(
                // SAFETY: the factory's archive id is a NUL-terminated string.
                unsafe { CStr::from_ptr(f.documentArchiveID) },
            ),
        };
        let reader = (&mut archive as *mut Archive).cast();
        let dc = self.dc();
        // SAFETY: paired restore calls; `archive` outlives them.
        let ok = unsafe {
            (dc.beginRestoringDocumentFromArchive.unwrap())(self.r(), reader) != 0
                && (dc.endRestoringDocumentFromArchive.unwrap())(self.r(), reader) != 0
        };
        ok.then_some(()).ok_or(HostError::State)
    }
}

/// Swaps a renderer's regions for `want`, removing before adding.
fn set_renderer_regions(b: &mut Binding, want: &[sys::ARAPlaybackRegionRef]) {
    // SAFETY: a live extension instance (see `Document::bind`).
    let ext = unsafe { b.instance.as_ptr().read_unaligned() };
    let Some(renderer) = NonNull::new(ext.playbackRendererInterface.cast_mut()) else {
        return;
    };
    // SAFETY: the plug-in's interface for this instance.
    let r = unsafe { renderer.as_ptr().read_unaligned() };
    for old in b.regions.iter().filter(|o| !want.contains(o)) {
        // SAFETY: added earlier, still alive.
        unsafe { (r.removePlaybackRegion.unwrap())(ext.playbackRendererRef, *old) };
    }
    for new in want.iter().filter(|n| !b.regions.contains(n)) {
        // SAFETY: a live region of the same document.
        unsafe { (r.addPlaybackRegion.unwrap())(ext.playbackRendererRef, *new) };
    }
    b.regions = want.to_vec();
}

impl Drop for Document {
    fn drop(&mut self) {
        for mut b in std::mem::take(&mut self.bindings) {
            set_renderer_regions(&mut b, &[]);
        }
        self.edit(|d| {
            let dc = d.dc();
            // SAFETY: tear down in reverse order of creation, inside editing.
            unsafe {
                for r in d.regions.values() {
                    (dc.destroyPlaybackRegion.unwrap())(d.r(), r.region);
                }
                for s in d.sources.values() {
                    (dc.destroyAudioModification.unwrap())(d.r(), s.modification);
                    (dc.enableAudioSourceSamplesAccess.unwrap())(d.r(), s.source, 0);
                    (dc.destroyAudioSource.unwrap())(d.r(), s.source);
                }
                for s in d.sequences.values() {
                    (dc.destroyRegionSequence.unwrap())(d.r(), *s);
                }
                if !d.context.is_null() {
                    (dc.destroyMusicalContext.unwrap())(d.r(), d.context);
                }
            }
        });
        let dc = self.dc();
        let f = self.factory.get();
        // SAFETY: the document is empty; balances `initializeARAWithConfiguration`.
        unsafe {
            (dc.destroyDocumentController.unwrap())(self.r());
            if let Some(uninit) = f.uninitializeARA {
                uninit();
            }
        }
        self.regions.clear();
        self.sources.clear();
    }
}

// Host callbacks. The plug-in calls these on its own threads too (audio reads
// happen on its analysis threads), so they touch only what their refs point to.

unsafe extern "C" fn create_reader(
    _controller: sys::ARAAudioAccessControllerHostRef,
    source: sys::ARAAudioSourceHostRef,
    wide: sys::ARABool,
) -> sys::ARAAudioReaderHostRef {
    let Some(source) = NonNull::new(source.cast::<SourceHost>()) else {
        return ptr::null_mut();
    };
    // SAFETY: a `SourceHost` we gave as this source's host ref; it outlives readers.
    let audio = unsafe { source.as_ref() }.audio.clone();
    Box::into_raw(Box::new(Reader {
        audio,
        wide: wide != 0,
    }))
    .cast()
}

unsafe extern "C" fn read_samples(
    _controller: sys::ARAAudioAccessControllerHostRef,
    reader: sys::ARAAudioReaderHostRef,
    position: sys::ARASamplePosition,
    count: sys::ARASampleCount,
    buffers: *const *mut c_void,
) -> sys::ARABool {
    let (Some(reader), false) = (NonNull::new(reader.cast::<Reader>()), buffers.is_null()) else {
        return 0;
    };
    // SAFETY: a reader from `create_reader`, not yet destroyed.
    let r = unsafe { reader.as_ref() };
    let Ok(count) = usize::try_from(count) else {
        return 0;
    };
    for ch in 0..r.audio.num_channels() {
        // SAFETY: ARA passes one buffer per source channel, `count` samples long.
        let out = unsafe { *buffers.add(ch) };
        if out.is_null() {
            return 0;
        }
        let data = r.audio.channel(ch);
        let sample = |i: usize| {
            let at = position
                .checked_add(i as i64)
                .and_then(|p| usize::try_from(p).ok());
            at.and_then(|p| data.get(p)).copied().unwrap_or(0.0)
        };
        // SAFETY: as above; the plug-in asked for `wide` (f64) or f32 samples.
        unsafe {
            if r.wide {
                let out = std::slice::from_raw_parts_mut(out.cast::<f64>(), count);
                out.iter_mut()
                    .enumerate()
                    .for_each(|(i, o)| *o = f64::from(sample(i)));
            } else {
                let out = std::slice::from_raw_parts_mut(out.cast::<f32>(), count);
                out.iter_mut().enumerate().for_each(|(i, o)| *o = sample(i));
            }
        }
    }
    1
}

unsafe extern "C" fn destroy_reader(
    _controller: sys::ARAAudioAccessControllerHostRef,
    reader: sys::ARAAudioReaderHostRef,
) {
    if !reader.is_null() {
        // SAFETY: made by `create_reader`, destroyed once.
        drop(unsafe { Box::from_raw(reader.cast::<Reader>()) });
    }
}

unsafe extern "C" fn archive_size(
    _controller: sys::ARAArchivingControllerHostRef,
    reader: sys::ARAArchiveReaderHostRef,
) -> sys::ARASize {
    // SAFETY: an `Archive` of `restore`, alive for the call.
    NonNull::new(reader.cast::<Archive>()).map_or(0, |a| unsafe { a.as_ref() }.bytes.len())
}

unsafe extern "C" fn archive_read(
    _controller: sys::ARAArchivingControllerHostRef,
    reader: sys::ARAArchiveReaderHostRef,
    position: sys::ARASize,
    length: sys::ARASize,
    buffer: *mut sys::ARAByte,
) -> sys::ARABool {
    let Some(a) = NonNull::new(reader.cast::<Archive>()) else {
        return 0;
    };
    // SAFETY: as in `archive_size`.
    let bytes = &unsafe { a.as_ref() }.bytes;
    match position
        .checked_add(length)
        .and_then(|end| bytes.get(position..end))
    {
        Some(src) if !buffer.is_null() => {
            // SAFETY: the plug-in's buffer holds `length` bytes.
            unsafe { ptr::copy_nonoverlapping(src.as_ptr(), buffer, length) };
            1
        }
        _ => 0,
    }
}

unsafe extern "C" fn archive_write(
    _controller: sys::ARAArchivingControllerHostRef,
    writer: sys::ARAArchiveWriterHostRef,
    position: sys::ARASize,
    length: sys::ARASize,
    buffer: *const sys::ARAByte,
) -> sys::ARABool {
    let (Some(mut a), false) = (NonNull::new(writer.cast::<Archive>()), buffer.is_null()) else {
        return 0;
    };
    // SAFETY: an `Archive` of `store`, alive and exclusively ours for the call.
    let bytes = &mut unsafe { a.as_mut() }.bytes;
    let Some(end) = position.checked_add(length) else {
        return 0;
    };
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    // SAFETY: the plug-in's buffer holds `length` bytes.
    let src = unsafe { std::slice::from_raw_parts(buffer, length) };
    bytes[position..end].copy_from_slice(src);
    1
}

unsafe extern "C" fn progress(_controller: sys::ARAArchivingControllerHostRef, _value: f32) {}

unsafe extern "C" fn archive_id(
    _controller: sys::ARAArchivingControllerHostRef,
    reader: sys::ARAArchiveReaderHostRef,
) -> sys::ARAPersistentID {
    // SAFETY: as in `archive_size`.
    NonNull::new(reader.cast::<Archive>())
        .map_or(ptr::null(), |a| unsafe { a.as_ref() }.id.as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(channels: Vec<Vec<f32>>) -> Box<SourceHost> {
        Box::new(SourceHost {
            audio: Arc::new(SampleData::new(48_000, channels).unwrap()),
        })
    }

    #[test]
    fn reader_reads_each_channel_and_zeroes_outside_the_file() {
        let s = source(vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]]);
        let host = (&*s as *const SourceHost).cast_mut().cast();
        // SAFETY: the callbacks as the plug-in would call them.
        unsafe {
            let reader = create_reader(ptr::null_mut(), host, 0);
            let (mut l, mut r) = ([9.0f32; 4], [9.0f32; 4]);
            let bufs = [l.as_mut_ptr().cast::<c_void>(), r.as_mut_ptr().cast()];
            assert_eq!(
                read_samples(ptr::null_mut(), reader, -1, 4, bufs.as_ptr()),
                1
            );
            assert_eq!((l, r), ([0.0, 1.0, 2.0, 3.0], [0.0, -1.0, -2.0, -3.0]));
            destroy_reader(ptr::null_mut(), reader);

            let wide = create_reader(ptr::null_mut(), host, 1);
            let (mut l, mut r) = ([9.0f64; 2], [9.0f64; 2]);
            let bufs = [l.as_mut_ptr().cast::<c_void>(), r.as_mut_ptr().cast()];
            assert_eq!(read_samples(ptr::null_mut(), wide, 2, 2, bufs.as_ptr()), 1);
            assert_eq!((l, r), ([3.0, 0.0], [-3.0, 0.0]));
            assert_eq!(read_samples(ptr::null_mut(), wide, 0, -1, bufs.as_ptr()), 0);
            assert_eq!(read_samples(ptr::null_mut(), wide, 0, 1, ptr::null()), 0);
            destroy_reader(ptr::null_mut(), wide);
            assert!(create_reader(ptr::null_mut(), ptr::null_mut(), 0).is_null());
        }
    }

    #[test]
    fn archive_round_trips_and_rejects_reads_past_the_end() {
        let mut a = Archive {
            bytes: Vec::new(),
            id: c("com.example.doc"),
        };
        let w: sys::ARAArchiveWriterHostRef = (&mut a as *mut Archive).cast();
        let at: sys::ARAArchiveReaderHostRef = w.cast();
        // SAFETY: the callbacks as the plug-in would call them.
        unsafe {
            assert_eq!(archive_write(ptr::null_mut(), w, 2, 3, b"abc".as_ptr()), 1);
            assert_eq!(archive_write(ptr::null_mut(), w, 0, 2, b"xy".as_ptr()), 1);
            assert_eq!(archive_size(ptr::null_mut(), at), 5);
            let mut out = [0u8; 5];
            assert_eq!(archive_read(ptr::null_mut(), at, 0, 5, out.as_mut_ptr()), 1);
            assert_eq!(&out, b"xyabc");
            assert_eq!(archive_read(ptr::null_mut(), at, 3, 5, out.as_mut_ptr()), 0);
            assert_eq!(
                archive_read(ptr::null_mut(), at, usize::MAX, 2, out.as_mut_ptr()),
                0
            );
            assert_eq!(
                CStr::from_ptr(archive_id(ptr::null_mut(), at)),
                c"com.example.doc"
            );
        }
    }

    #[test]
    fn factories_must_share_an_api_generation() {
        let mut f = sys::ARAFactory {
            structSize: size_of::<sys::ARAFactory>(),
            lowestSupportedApiGeneration: sys::kARAAPIGeneration_2_0_Final as i32,
            highestSupportedApiGeneration: 99,
            ..Default::default()
        };
        // SAFETY: a valid factory struct on the stack.
        unsafe {
            let ok = AraFactory::from_raw((&f as *const sys::ARAFactory).cast()).unwrap();
            assert_eq!(ok.generation(), sys::kARAAPIGeneration_2_3_Final as i32);
            f.highestSupportedApiGeneration = 1; // ARA 1 only
            assert!(AraFactory::from_raw((&f as *const sys::ARAFactory).cast()).is_none());
            f.highestSupportedApiGeneration = 5;
            f.structSize = 8;
            assert!(AraFactory::from_raw((&f as *const sys::ARAFactory).cast()).is_none());
            assert!(AraFactory::from_raw(ptr::null()).is_none());
        }
    }
}
