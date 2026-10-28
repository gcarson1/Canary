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

/// What binding a new instance needs: the document controller ref, and the
/// callback that gets the instance's ARA extension before activation.
pub type AraBind<'a> = (*mut c_void, &'a mut dyn FnMut(NonNull<c_void>));

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

    // shortcut: copies the whole 2.3 struct; a plug-in built on an older SDK has a
    // smaller one (static data, so this reads its neighbours, not unmapped
    // memory). Read fields one by one if that ever matters.
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
}

struct Region {
    region: sys::ARAPlaybackRegionRef,
    track: usize,
    /// `Arc::as_ptr` of the clip's audio: the source it plays.
    source: usize,
}

/// A plug-in instance's ARA side, bound to a document, and the regions it was
/// given before activation. ARA only lets a renderer's regions change while it
/// is inactive, so they stay fixed for the instance's life; a track whose clips
/// gain or lose regions gets a new instance instead (see `Document::sync`).
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

static NO_ASSERT: sys::ARAAssertFunction = None;

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
    /// Regions whose clip is gone, with their source key, kept until no
    /// instance renders them.
    orphans: Vec<(sys::ARAPlaybackRegionRef, usize)>,
    sample_rate: f64,
}

impl Document {
    /// Initializes ARA in the plug-in and creates a document controller with an
    /// empty document at the project's `sample_rate`.
    pub fn new(factory: AraFactory, name: &str, sample_rate: f64) -> Result<Self, HostError> {
        let f = factory.get();
        let config = sys::ARAInterfaceConfiguration {
            structSize: size_of::<sys::ARAInterfaceConfiguration>(),
            desiredApiGeneration: factory.generation(),
            // Must point somewhere, even at no function.
            assertFunctionAddress: ptr::addr_of!(NO_ASSERT).cast_mut(),
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
            orphans: Vec::new(),
            sample_rate,
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

    /// Makes the instance a format just bound to this document (the `on_bind`
    /// callback of `Catalog::instantiate_ara`) the renderer of `track`'s regions.
    /// Call it only there: before the instance is activated, as ARA requires.
    ///
    /// # Safety
    /// `instance` must be the `ARAPlugInExtensionInstance` the plug-in returned
    /// when bound to this document, and must stay valid until `release`.
    pub unsafe fn bind(&mut self, track: usize, instance: NonNull<c_void>) {
        let regions: Vec<_> = self
            .regions
            .values()
            .filter(|r| r.track == track)
            .map(|r| r.region)
            .collect();
        let binding = Binding {
            track,
            instance: instance.cast(),
            regions,
        };
        // SAFETY: a live, not yet active extension instance (see above).
        unsafe { add_to_renderer(&binding) };
        self.bindings.push(binding);
    }

    /// Forgets an instance after it was destroyed, then destroys regions only it
    /// still rendered. The plug-in handles instance-before-document teardown.
    pub fn release(&mut self, instance: NonNull<c_void>) {
        self.bindings.retain(|b| b.instance.cast() != instance);
        let held: Vec<_> = self
            .bindings
            .iter()
            .flat_map(|b| b.regions.iter().copied())
            .collect();
        let free: Vec<_> = self
            .orphans
            .iter()
            .map(|o| o.0)
            .filter(|r| !held.contains(r))
            .collect();
        if free.is_empty() {
            return;
        }
        self.orphans.retain(|o| !free.contains(&o.0));
        self.edit(|d| {
            for region in free {
                // SAFETY: inside editing; no renderer holds it.
                unsafe { (d.dc().destroyPlaybackRegion.unwrap())(d.r(), region) };
            }
            d.drop_unused_sources();
        });
    }

    /// Makes the document match `clips` (one region sequence per track, one audio
    /// source per file, one playback region per clip) and returns the tracks whose
    /// bound instances now render the wrong set of regions: their instances must
    /// be replaced, since an active renderer's regions cannot change. Moved or
    /// trimmed clips just update their region, which ARA allows while rendering.
    pub fn sync(&mut self, clips: &[AraClip]) -> Vec<usize> {
        let gone: Vec<u64> = self
            .regions
            .keys()
            .filter(|id| !clips.iter().any(|c| c.id == **id))
            .copied()
            .collect();
        self.edit(|d| {
            for id in gone {
                if let Some(r) = d.regions.remove(&id) {
                    d.retire(r.region, r.source);
                }
            }
            for clip in clips {
                d.place(clip);
            }
            d.drop_unused_sources();
        });
        let mut stale: Vec<usize> = self
            .bindings
            .iter()
            .filter(|b| {
                let mut want: Vec<_> = self
                    .regions
                    .values()
                    .filter(|r| r.track == b.track)
                    .map(|r| r.region as usize)
                    .collect();
                let mut have: Vec<_> = b.regions.iter().map(|r| *r as usize).collect();
                want.sort_unstable();
                have.sort_unstable();
                want != have
            })
            .map(|b| b.track)
            .collect();
        stale.dedup();
        stale
    }

    /// Destroys a region now, or once no instance renders it. Inside editing.
    fn retire(&mut self, region: sys::ARAPlaybackRegionRef, source: usize) {
        if self.bindings.iter().any(|b| b.regions.contains(&region)) {
            self.orphans.push((region, source));
        } else {
            // SAFETY: inside editing; no renderer holds it.
            unsafe { (self.dc().destroyPlaybackRegion.unwrap())(self.r(), region) };
        }
    }

    /// Destroys sources no region (live or orphaned) still plays. Inside editing.
    fn drop_unused_sources(&mut self) {
        let dc = self.dc();
        let used: Vec<usize> = (self.regions.values().map(|r| r.source))
            .chain(self.orphans.iter().map(|o| o.1))
            .collect();
        let unused: Vec<usize> = self
            .sources
            .keys()
            .filter(|k| !used.contains(k))
            .copied()
            .collect();
        for key in unused {
            if let Some(s) = self.sources.remove(&key) {
                // SAFETY: no region plays it; modification goes first.
                unsafe {
                    (dc.destroyAudioModification.unwrap())(self.r(), s.modification);
                    (dc.enableAudioSourceSamplesAccess.unwrap())(self.r(), s.source, 0);
                    (dc.destroyAudioSource.unwrap())(self.r(), s.source);
                }
            }
        }
    }

    /// Creates or updates the region for `clip`, and its source if new.
    fn place(&mut self, clip: &AraClip) {
        let dc = self.dc();
        let key = Arc::as_ptr(&clip.audio) as usize;
        if !self.sources.contains_key(&key) {
            // Only needed for the call: the plug-in copies what it keeps.
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
                },
            );
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
                if let Some((source, old)) = existing {
                    // A clip now playing another file: ARA regions cannot move.
                    self.retire(old, source);
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
        s
    }
}

/// Hands a fresh binding its regions.
///
/// # Safety
/// The instance must be live and not yet active.
unsafe fn add_to_renderer(b: &Binding) {
    // SAFETY: per the caller; packed plug-in structs read unaligned, field by field.
    unsafe {
        let ext = b.instance.as_ptr();
        let iface = ptr::addr_of!((*ext).playbackRendererInterface).read_unaligned();
        let renderer = ptr::addr_of!((*ext).playbackRendererRef).read_unaligned();
        let Some(iface) = NonNull::new(iface.cast_mut()) else {
            return;
        };
        let add = ptr::addr_of!((*iface.as_ptr()).addPlaybackRegion).read_unaligned();
        if let Some(add) = add {
            for region in &b.regions {
                add(renderer, *region);
            }
        }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        // Instances may outlive the document at quit; ARA leaves that order to the
        // plug-in, so regions go without being taken from their renderers.
        self.bindings.clear();
        self.edit(|d| {
            let dc = d.dc();
            // SAFETY: tear down in reverse order of creation, inside editing.
            unsafe {
                let orphans = d.orphans.iter().map(|o| o.0);
                for r in d.regions.values().map(|r| r.region).chain(orphans) {
                    (dc.destroyPlaybackRegion.unwrap())(d.r(), r);
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

    // A fake plug-in side that logs every call, to check the document's call order.
    thread_local!(static LOG: std::cell::RefCell<Vec<String>> = Default::default());
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

    fn log(s: String) {
        LOG.with(|l| l.borrow_mut().push(s));
    }
    fn take_log() -> Vec<String> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }
    fn new_ref<T>(kind: &str) -> *mut T {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        log(format!("create {kind} {n}"));
        n as *mut T
    }
    fn id<T>(p: *mut T) -> usize {
        p as usize
    }

    type R = sys::ARADocumentControllerRef;
    unsafe extern "C" fn begin(_: R) {
        log("begin".into());
    }
    unsafe extern "C" fn end(_: R) {
        log("end".into());
    }
    unsafe extern "C" fn destroy_dc(_: R) {
        log("destroy controller".into());
    }
    unsafe extern "C" fn new_context(
        _: R,
        _: sys::ARAMusicalContextHostRef,
        _: *const sys::ARAMusicalContextProperties,
    ) -> sys::ARAMusicalContextRef {
        new_ref("context")
    }
    unsafe extern "C" fn drop_context(_: R, c: sys::ARAMusicalContextRef) {
        log(format!("destroy context {}", id(c)));
    }
    unsafe extern "C" fn new_sequence(
        _: R,
        _: sys::ARARegionSequenceHostRef,
        _: *const sys::ARARegionSequenceProperties,
    ) -> sys::ARARegionSequenceRef {
        new_ref("sequence")
    }
    unsafe extern "C" fn drop_sequence(_: R, s: sys::ARARegionSequenceRef) {
        log(format!("destroy sequence {}", id(s)));
    }
    unsafe extern "C" fn new_source(
        _: R,
        _: sys::ARAAudioSourceHostRef,
        _: *const sys::ARAAudioSourceProperties,
    ) -> sys::ARAAudioSourceRef {
        new_ref("source")
    }
    unsafe extern "C" fn access(_: R, s: sys::ARAAudioSourceRef, on: sys::ARABool) {
        log(format!("access {} {on}", id(s)));
    }
    unsafe extern "C" fn drop_source(_: R, s: sys::ARAAudioSourceRef) {
        log(format!("destroy source {}", id(s)));
    }
    unsafe extern "C" fn new_mod(
        _: R,
        _: sys::ARAAudioSourceRef,
        _: sys::ARAAudioModificationHostRef,
        _: *const sys::ARAAudioModificationProperties,
    ) -> sys::ARAAudioModificationRef {
        new_ref("modification")
    }
    unsafe extern "C" fn drop_mod(_: R, m: sys::ARAAudioModificationRef) {
        log(format!("destroy modification {}", id(m)));
    }
    unsafe extern "C" fn new_region(
        _: R,
        _: sys::ARAAudioModificationRef,
        _: sys::ARAPlaybackRegionHostRef,
        _: *const sys::ARAPlaybackRegionProperties,
    ) -> sys::ARAPlaybackRegionRef {
        new_ref("region")
    }
    unsafe extern "C" fn update_region(
        _: R,
        r: sys::ARAPlaybackRegionRef,
        _: *const sys::ARAPlaybackRegionProperties,
    ) {
        log(format!("update region {}", id(r)));
    }
    unsafe extern "C" fn drop_region(_: R, r: sys::ARAPlaybackRegionRef) {
        log(format!("destroy region {}", id(r)));
    }
    unsafe extern "C" fn add_region(_: sys::ARAPlaybackRendererRef, r: sys::ARAPlaybackRegionRef) {
        log(format!("render region {}", id(r)));
    }
    unsafe extern "C" fn init(_: *const sys::ARAInterfaceConfiguration) {
        log("init".into());
    }
    unsafe extern "C" fn uninit() {
        log("uninit".into());
    }

    struct Fake {
        dc: sys::ARADocumentControllerInterface,
        instance: sys::ARADocumentControllerInstance,
        renderer: sys::ARAPlaybackRendererInterface,
        ext: sys::ARAPlugInExtensionInstance,
    }

    thread_local!(static FAKE: std::cell::Cell<*const sys::ARADocumentControllerInstance> =
        const { std::cell::Cell::new(ptr::null()) });

    unsafe extern "C" fn create_dc(
        _: *const sys::ARADocumentControllerHostInstance,
        _: *const sys::ARADocumentProperties,
    ) -> *const sys::ARADocumentControllerInstance {
        FAKE.with(|f| f.get())
    }

    #[test]
    fn document_keeps_ara_call_order_through_edits_and_teardown() {
        let mut fake = Box::new(Fake {
            dc: sys::ARADocumentControllerInterface {
                structSize: size_of::<sys::ARADocumentControllerInterface>(),
                destroyDocumentController: Some(destroy_dc),
                beginEditing: Some(begin),
                endEditing: Some(end),
                createMusicalContext: Some(new_context),
                destroyMusicalContext: Some(drop_context),
                createRegionSequence: Some(new_sequence),
                destroyRegionSequence: Some(drop_sequence),
                createAudioSource: Some(new_source),
                enableAudioSourceSamplesAccess: Some(access),
                destroyAudioSource: Some(drop_source),
                createAudioModification: Some(new_mod),
                destroyAudioModification: Some(drop_mod),
                createPlaybackRegion: Some(new_region),
                updatePlaybackRegionProperties: Some(update_region),
                destroyPlaybackRegion: Some(drop_region),
                ..Default::default()
            },
            instance: Default::default(),
            renderer: sys::ARAPlaybackRendererInterface {
                structSize: size_of::<sys::ARAPlaybackRendererInterface>(),
                addPlaybackRegion: Some(add_region),
                removePlaybackRegion: None,
            },
            ext: Default::default(),
        });
        fake.instance.documentControllerInterface = &fake.dc;
        fake.renderer.removePlaybackRegion = None; // must never be needed
        fake.ext.playbackRendererInterface = &fake.renderer;
        FAKE.with(|f| f.set(&fake.instance));
        let factory = sys::ARAFactory {
            structSize: size_of::<sys::ARAFactory>(),
            lowestSupportedApiGeneration: sys::kARAAPIGeneration_2_0_Final as i32,
            highestSupportedApiGeneration: sys::kARAAPIGeneration_2_3_Final as i32,
            initializeARAWithConfiguration: Some(init),
            uninitializeARA: Some(uninit),
            createDocumentControllerWithDocument: Some(create_dc),
            ..Default::default()
        };
        // SAFETY: a complete fake factory that outlives the document.
        let factory = unsafe { AraFactory::from_raw((&factory as *const sys::ARAFactory).cast()) };
        let mut doc = Document::new(factory.unwrap(), "test", 48_000.0).unwrap();
        take_log();

        let audio = |n| Arc::new(SampleData::new(48_000, vec![vec![0.5; n]]).unwrap());
        let (one, two) = (audio(100), audio(200));
        let clip = |id, audio: &Arc<SampleData>, start| AraClip {
            id,
            track: 0,
            name: "c".into(),
            audio: audio.clone(),
            start,
            offset: 0,
            length: 50,
        };
        assert!(
            doc.sync(&[clip(1, &one, 0), clip(2, &two, 100)]).is_empty(),
            "no renderers yet"
        );
        let regions: Vec<String> = take_log()
            .into_iter()
            .filter_map(|l| l.strip_prefix("create region ").map(String::from))
            .collect();
        assert_eq!(regions.len(), 2);

        let ext = NonNull::from(&fake.ext).cast();
        // SAFETY: the fake extension, not active.
        unsafe { doc.bind(0, ext) };
        let mut rendered = take_log();
        rendered.sort();
        assert_eq!(
            rendered,
            regions
                .iter()
                .map(|r| format!("render region {r}"))
                .collect::<Vec<_>>()
        );

        // Moving a clip updates it in place; removing one keeps its region while
        // the renderer has it, and asks for the track's instance to be remade.
        assert_eq!(doc.sync(&[clip(2, &two, 300)]), [0]);
        let log = take_log();
        assert!(log.iter().any(|l| l.starts_with("update region")));
        assert!(!log.iter().any(|l| l.starts_with("destroy")), "{log:?}");

        doc.release(ext);
        let log = take_log();
        let first = &regions[0];
        let pos = |s: &str| {
            log.iter()
                .position(|l| l.starts_with(s))
                .unwrap_or(usize::MAX)
        };
        assert_eq!(log.first().map(String::as_str), Some("begin"));
        assert!(log.contains(&format!("destroy region {first}")), "{log:?}");
        assert!(pos("destroy region") < pos("destroy modification"));
        assert!(pos("destroy modification") < pos("access"));
        assert!(pos("access") < pos("destroy source"));
        assert_eq!(log.last().map(String::as_str), Some("end"));

        drop(doc);
        let log = take_log();
        let pos = |s: &str| log.iter().position(|l| l.starts_with(s)).unwrap();
        assert!(pos("destroy region") < pos("destroy source"));
        assert!(pos("destroy source") < pos("destroy sequence"));
        assert!(pos("destroy sequence") < pos("destroy context"));
        assert!(pos("end") < pos("destroy controller"));
        assert_eq!(log.last().map(String::as_str), Some("uninit"));
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
