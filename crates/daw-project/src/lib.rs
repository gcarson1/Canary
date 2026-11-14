//! Project state: undo/redo actions, serialization and `.daw` project files.
//!
//! The project is plain data the UI edits through `Action`s. Turning it into an
//! engine plan (loading audio, building nodes) happens above this crate.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod automation;
mod routing;
pub use automation::{valid_points, Lane, Point, Target};
pub use routing::Send;
pub mod edit;
mod notes;
pub mod smf;
mod tempo;
pub use notes::{sorted, Note, Pattern, PatternClip, PatternNotes, Playback, MAX_TICKS, PPQ, STEP};
pub use tempo::{valid_meters, valid_tempos, BarBeat, MeterChange, TempoChange};

/// Bumped when the file format changes incompatibly.
/// 2: built-in effects' MIX/MATCH/DELTA have fixed parameter ids
/// (`daw_core::COMMON_PARAM_ID`); in 1 they followed the effect's own.
const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub bpm: f64,
    /// Rate that every sample position in the project counts in.
    pub sample_rate: u32,
    pub tracks: Vec<Track>,
    /// A finished track to A/B the mix against, level-matched; never mixed in.
    #[serde(default)]
    pub reference: Option<PathBuf>,
    /// Each ARA plug-in's document (its edits to the project's audio), saved
    /// with the project. Not undoable: the plug-in keeps its own undo.
    #[serde(default)]
    pub ara: Vec<AraArchive>,
    /// FL-style patterns; each track keeps its own notes for each.
    #[serde(default)]
    pub patterns: Vec<Pattern>,
    /// Tempo changes after the start (`bpm` holds from tick 0), by rising tick.
    #[serde(default)]
    pub tempos: Vec<TempoChange>,
    /// Time signatures by rising bar, the first at bar 0; empty means 4/4.
    #[serde(default)]
    pub meters: Vec<MeterChange>,
    /// The song's loop (cycle) region in ticks, `start < end`, and whether the
    /// song plays it over and over.
    #[serde(default)]
    pub loop_region: Option<(u64, u64)>,
    #[serde(default)]
    pub loop_on: bool,
}

/// An ARA plug-in's document state as it wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AraArchive {
    /// The plug-in's ARA factory id: which plug-in reads this.
    pub factory: String,
    /// The archive format id the plug-in wrote it under.
    pub archive_id: String,
    #[serde(with = "base64")]
    pub bytes: Vec<u8>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            sample_rate: 48_000,
            tracks: Vec::new(),
            reference: None,
            ara: Vec::new(),
            patterns: Vec::new(),
            tempos: Vec::new(),
            meters: Vec::new(),
            loop_region: None,
            loop_on: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Unique in the project and stable across edits: routing refers to it.
    /// 0 until the track is inserted, which assigns one.
    #[serde(default)]
    pub id: u64,
    pub name: String,
    /// A bus: other tracks can output or send to it.
    #[serde(default)]
    pub bus: bool,
    /// The bus this track plays into (a track id), or the master when `None`.
    #[serde(default)]
    pub output: Option<u64>,
    /// Post-fader sends to buses.
    #[serde(default)]
    pub sends: Vec<Send>,
    pub gain: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub clips: Vec<ClipRef>,
    /// Plugins on the track, in signal order. An instrument (a plugin without an
    /// audio input) plays before the clips are mixed in; effects follow.
    #[serde(default)]
    pub devices: Vec<Device>,
    /// This track's notes, per pattern; no entry means no notes.
    #[serde(default)]
    pub notes: Vec<PatternNotes>,
    /// Patterns placed on this playlist row. Each plays on every track.
    #[serde(default)]
    pub pattern_clips: Vec<PatternClip>,
    /// Automation lanes, at most one per target.
    #[serde(default)]
    pub automation: Vec<Lane>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginFormat {
    Clap,
    AudioUnit,
    Vst3,
    /// One of Canary's own effects.
    Builtin,
}

/// A plugin on a track: what to load, and its saved state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Unique in the project; lets the app keep a plugin alive across edits.
    pub uid: u64,
    pub format: PluginFormat,
    /// The `.clap`/`.vst3` bundle; Audio Units have none.
    pub bundle: Option<PathBuf>,
    pub id: String,
    pub name: String,
    /// The plugin's opaque state as of the last save.
    #[serde(default, with = "base64")]
    pub state: Vec<u8>,
    /// A track (by id) whose audio feeds the device's key input (sidechain).
    #[serde(default)]
    pub sidechain: Option<u64>,
}

/// Plugin state can be large; base64 keeps it a quarter of a JSON number array.
mod base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    char::from(ABC[(n >> (18 - 6 * i) & 63) as usize])
                } else {
                    '='
                });
            }
        }
        out
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let text = text.trim_end_matches('=');
        let mut out = Vec::with_capacity(text.len() * 3 / 4);
        let (mut acc, mut bits) = (0u32, 0);
        for c in text.bytes() {
            acc = acc << 6 | ABC.iter().position(|&a| a == c)? as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Some(out)
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        decode(&text).ok_or_else(|| serde::de::Error::custom("invalid base64"))
    }
}

impl Track {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: 0,
            name: name.into(),
            bus: false,
            output: None,
            sends: Vec::new(),
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            clips: Vec::new(),
            devices: Vec::new(),
            notes: Vec::new(),
            pattern_clips: Vec::new(),
            automation: Vec::new(),
        }
    }

    /// This track's notes in `pattern`.
    pub fn notes_in(&self, pattern: u64) -> &[Note] {
        self.notes
            .iter()
            .find(|n| n.pattern == pattern)
            .map_or(&[], |n| &n.notes)
    }
}

/// A clip as saved: which file, where it sits, and the window of it that plays.
/// Mirrors `daw_core::Clip`, whose edit methods keep the window valid; the app
/// edits a `Clip` and stores the result here with `Action::SetClip`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipRef {
    pub source: PathBuf,
    /// Timeline position in samples at `Project::sample_rate`.
    pub start: u64,
    pub offset: u64,
    pub length: u64,
    pub fade_in: u64,
    pub fade_out: u64,
    /// Linear clip gain, 0..=4 (+12 dB).
    #[serde(default = "unity")]
    pub gain: f32,
    #[serde(default)]
    pub reverse: bool,
    /// Set when `source` is a time-stretched or pitch-shifted copy: what it was
    /// made from and how, so a new stretch starts from the original again.
    #[serde(default)]
    pub stretch: Option<Stretch>,
}

/// How a clip's audio was rendered from an original file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stretch {
    pub original: PathBuf,
    /// Length factor, 0.25..=4 (2 = twice as long).
    pub ratio: f64,
    /// -24..=24.
    pub semitones: f64,
}

fn unity() -> f32 {
    1.0
}

/// A loop region runs forward and stays in range.
fn loop_ok(region: Option<(u64, u64)>) -> bool {
    region.is_none_or(|(a, b)| a < b && b <= MAX_TICKS)
}

impl ClipRef {
    /// Clip gain is finite and within 0..=4 (+12 dB).
    pub fn gain_ok(&self) -> bool {
        self.gain.is_finite() && (0.0..=4.0).contains(&self.gain)
    }
}

/// One undoable edit. Each is applied by `History::apply`, which records its exact
/// inverse, so undo and redo never drift from the project.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SetTempo(f64),
    SetReference(Option<PathBuf>),
    InsertTrack {
        index: usize,
        track: Track,
    },
    RemoveTrack {
        index: usize,
    },
    /// Replaces every mixer and name field; the clips are left alone.
    SetTrackProps {
        index: usize,
        props: TrackProps,
    },
    InsertClip {
        track: usize,
        index: usize,
        clip: ClipRef,
    },
    RemoveClip {
        track: usize,
        index: usize,
    },
    SetClip {
        track: usize,
        index: usize,
        clip: ClipRef,
    },
    InsertDevice {
        track: usize,
        index: usize,
        device: Device,
    },
    RemoveDevice {
        track: usize,
        index: usize,
    },
    /// Takes the device at `from` out and puts it back so it ends up at `to`.
    MoveDevice {
        track: usize,
        from: usize,
        to: usize,
    },
    /// Replaces the tempo changes after the start.
    SetTempoMap(Vec<TempoChange>),
    /// Replaces the time signatures.
    SetMeters(Vec<MeterChange>),
    /// Sets the song's loop region (ticks) and whether it loops.
    SetLoop {
        region: Option<(u64, u64)>,
        on: bool,
    },
    InsertPattern {
        index: usize,
        pattern: Pattern,
    },
    /// Removes the pattern only; see `Project::remove_pattern` for the edit that
    /// also removes its notes and clips.
    RemovePattern {
        index: usize,
    },
    SetPattern {
        index: usize,
        pattern: Pattern,
    },
    /// Replaces `track`'s notes in `pattern` (stored sorted).
    SetNotes {
        track: usize,
        pattern: u64,
        notes: Vec<Note>,
    },
    InsertPatternClip {
        track: usize,
        index: usize,
        clip: PatternClip,
    },
    /// Replaces `track`'s automation of `target`; no points removes the lane.
    SetAutomation {
        track: usize,
        target: Target,
        points: Vec<Point>,
    },
    RemovePatternClip {
        track: usize,
        index: usize,
    },
    SetPatternClip {
        track: usize,
        index: usize,
        clip: PatternClip,
    },
    /// Sets where `track` plays (a bus id or the master) and its sends.
    /// Refused if it would route a bus into itself, directly or not.
    SetRouting {
        track: usize,
        output: Option<u64>,
        sends: Vec<Send>,
    },
    /// Feeds track `source`'s audio (by id) into device `device`'s key input.
    SetSidechain {
        track: usize,
        device: usize,
        source: Option<u64>,
    },
    /// Several edits that undo as one step (a split, say). All or nothing.
    Batch(Vec<Action>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackProps {
    pub name: String,
    pub gain: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
}

impl TrackProps {
    pub fn of(track: &Track) -> Self {
        Self {
            name: track.name.clone(),
            gain: track.gain,
            pan: track.pan,
            mute: track.mute,
            solo: track.solo,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// A track or clip index past the end.
    OutOfRange,
    /// A tempo that is not finite and positive.
    BadTempo,
    /// A gain or pan that is not finite, which a project file cannot store, or a
    /// note, pattern or clip with no length or an out-of-range key or velocity.
    BadValue,
    /// A pattern id already in the project.
    DuplicateId,
    /// Routing that would feed a bus back into itself, or a send level that is
    /// not finite.
    BadRoute,
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => write!(f, "no such track or clip"),
            Self::BadTempo => write!(f, "tempo must be a positive number"),
            Self::BadValue => write!(f, "value out of range"),
            Self::DuplicateId => write!(f, "pattern id already in use"),
            Self::BadRoute => write!(f, "that routing would loop back on itself"),
        }
    }
}

impl std::error::Error for ActionError {}

impl Project {
    /// The edit that removes pattern `index` with every track's notes in it and
    /// every clip of it, as one undo step. `None` if there is no such pattern.
    pub fn remove_pattern(&self, index: usize) -> Option<Action> {
        let id = self.patterns.get(index)?.id;
        let mut batch = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            if !t.notes_in(id).is_empty() {
                batch.push(Action::SetNotes {
                    track: ti,
                    pattern: id,
                    notes: Vec::new(),
                });
            }
            // Back to front, so earlier indices stay put.
            for (ci, _) in t.pattern_clips.iter().enumerate().rev() {
                if t.pattern_clips[ci].pattern == id {
                    batch.push(Action::RemovePatternClip {
                        track: ti,
                        index: ci,
                    });
                }
            }
        }
        batch.push(Action::RemovePattern { index });
        Some(Action::Batch(batch))
    }

    /// A pattern id not used in the project.
    /// Also past every id a note list or clip still points at, so a new pattern
    /// never picks up notes it did not make.
    pub fn new_pattern_id(&self) -> u64 {
        let referenced = self.tracks.iter().flat_map(|t| {
            t.notes
                .iter()
                .map(|n| n.pattern)
                .chain(t.pattern_clips.iter().map(|c| c.pattern))
        });
        self.patterns
            .iter()
            .map(|p| p.id)
            .chain(referenced)
            .map(|id| id.saturating_add(1))
            .max()
            .unwrap_or(1)
    }
}

fn track(p: &mut Project, i: usize) -> Result<&mut Track, ActionError> {
    p.tracks.get_mut(i).ok_or(ActionError::OutOfRange)
}

impl Action {
    /// Whether the edit would apply to `project`, without changing it. Clones
    /// the project: for pickers checking a handful of choices on a click.
    pub fn applies_to(&self, project: &Project) -> bool {
        self.clone().apply(&mut project.clone()).is_ok()
    }

    /// Applies the edit and returns the action that undoes it. Changes nothing on error.
    fn apply(self, p: &mut Project) -> Result<Action, ActionError> {
        use Action::*;
        Ok(match self {
            SetTempo(bpm) => {
                if !(bpm.is_finite() && bpm > 0.0) {
                    return Err(ActionError::BadTempo);
                }
                SetTempo(std::mem::replace(&mut p.bpm, bpm))
            }
            SetReference(path) => SetReference(std::mem::replace(&mut p.reference, path)),
            InsertTrack { index, mut track } => {
                if index > p.tracks.len() {
                    return Err(ActionError::OutOfRange);
                }
                // A new track (or one whose id is taken) gets a fresh id; the
                // inverse keeps it, so undo and redo agree.
                if track.id == 0 || p.track_index(track.id).is_some() {
                    track.id = p.new_track_id();
                }
                p.tracks.insert(index, track);
                RemoveTrack { index }
            }
            RemoveTrack { index } => {
                if index >= p.tracks.len() {
                    return Err(ActionError::OutOfRange);
                }
                InsertTrack {
                    index,
                    track: p.tracks.remove(index),
                }
            }
            SetTrackProps { index, props } => {
                if !(props.gain.is_finite() && props.pan.is_finite()) {
                    return Err(ActionError::BadValue);
                }
                let t = track(p, index)?;
                let old = TrackProps::of(t);
                (t.name, t.gain, t.pan, t.mute, t.solo) =
                    (props.name, props.gain, props.pan, props.mute, props.solo);
                SetTrackProps { index, props: old }
            }
            InsertClip {
                track: ti,
                index,
                clip,
            } => {
                if !clip.gain_ok() {
                    return Err(ActionError::BadValue);
                }
                let t = track(p, ti)?;
                if index > t.clips.len() {
                    return Err(ActionError::OutOfRange);
                }
                t.clips.insert(index, clip);
                RemoveClip { track: ti, index }
            }
            RemoveClip { track: ti, index } => {
                let t = track(p, ti)?;
                if index >= t.clips.len() {
                    return Err(ActionError::OutOfRange);
                }
                InsertClip {
                    track: ti,
                    index,
                    clip: t.clips.remove(index),
                }
            }
            SetClip {
                track: ti,
                index,
                clip,
            } => {
                if !clip.gain_ok() {
                    return Err(ActionError::BadValue);
                }
                let slot = track(p, ti)?
                    .clips
                    .get_mut(index)
                    .ok_or(ActionError::OutOfRange)?;
                SetClip {
                    track: ti,
                    index,
                    clip: std::mem::replace(slot, clip),
                }
            }
            InsertDevice {
                track: ti,
                index,
                device,
            } => {
                let t = track(p, ti)?;
                if index > t.devices.len() {
                    return Err(ActionError::OutOfRange);
                }
                t.devices.insert(index, device);
                RemoveDevice { track: ti, index }
            }
            RemoveDevice { track: ti, index } => {
                let t = track(p, ti)?;
                if index >= t.devices.len() {
                    return Err(ActionError::OutOfRange);
                }
                InsertDevice {
                    track: ti,
                    index,
                    device: t.devices.remove(index),
                }
            }
            MoveDevice {
                track: ti,
                from,
                to,
            } => {
                let t = track(p, ti)?;
                if from >= t.devices.len() || to >= t.devices.len() {
                    return Err(ActionError::OutOfRange);
                }
                let device = t.devices.remove(from);
                t.devices.insert(to, device);
                MoveDevice {
                    track: ti,
                    from: to,
                    to: from,
                }
            }
            SetTempoMap(changes) => {
                if !valid_tempos(&changes) {
                    return Err(ActionError::BadTempo);
                }
                SetTempoMap(std::mem::replace(&mut p.tempos, changes))
            }
            SetLoop { region, on } => {
                if !loop_ok(region) {
                    return Err(ActionError::BadValue);
                }
                let old = (p.loop_region, p.loop_on);
                (p.loop_region, p.loop_on) = (region, on && region.is_some());
                SetLoop {
                    region: old.0,
                    on: old.1,
                }
            }
            SetMeters(changes) => {
                if !valid_meters(&changes) {
                    return Err(ActionError::BadValue);
                }
                SetMeters(std::mem::replace(&mut p.meters, changes))
            }
            InsertPattern { index, pattern } => {
                if !pattern.is_valid() {
                    return Err(ActionError::BadValue);
                }
                if index > p.patterns.len() {
                    return Err(ActionError::OutOfRange);
                }
                if p.pattern(pattern.id).is_some() {
                    return Err(ActionError::DuplicateId);
                }
                p.patterns.insert(index, pattern);
                RemovePattern { index }
            }
            RemovePattern { index } => {
                if index >= p.patterns.len() {
                    return Err(ActionError::OutOfRange);
                }
                InsertPattern {
                    index,
                    pattern: p.patterns.remove(index),
                }
            }
            SetPattern { index, pattern } => {
                let slot = p.patterns.get_mut(index).ok_or(ActionError::OutOfRange)?;
                // Notes and clips point at the id, so it never changes.
                if !pattern.is_valid() || pattern.id != slot.id {
                    return Err(ActionError::BadValue);
                }
                SetPattern {
                    index,
                    pattern: std::mem::replace(slot, pattern),
                }
            }
            SetNotes {
                track: ti,
                pattern,
                notes,
            } => {
                if !notes.iter().all(Note::is_valid) {
                    return Err(ActionError::BadValue);
                }
                let t = track(p, ti)?;
                let at = t.notes.iter().position(|n| n.pattern == pattern);
                let old = match at {
                    Some(i) if notes.is_empty() => t.notes.remove(i).notes,
                    Some(i) => std::mem::replace(&mut t.notes[i].notes, sorted(notes)),
                    None if notes.is_empty() => Vec::new(),
                    None => {
                        // Kept in pattern order, so undo restores the same list.
                        let at = t.notes.partition_point(|n| n.pattern < pattern);
                        t.notes.insert(
                            at,
                            PatternNotes {
                                pattern,
                                notes: sorted(notes),
                            },
                        );
                        Vec::new()
                    }
                };
                SetNotes {
                    track: ti,
                    pattern,
                    notes: old,
                }
            }
            SetAutomation {
                track: ti,
                target,
                points,
            } => {
                if !valid_points(target, &points) {
                    return Err(ActionError::BadValue);
                }
                let t = track(p, ti)?;
                let at = t.automation.iter().position(|l| l.target == target);
                let old = match at {
                    Some(i) if points.is_empty() => t.automation.remove(i).points,
                    Some(i) => std::mem::replace(&mut t.automation[i].points, points),
                    None if points.is_empty() => Vec::new(),
                    None => {
                        // Kept in target order, so undo restores the same list.
                        let at = t.automation.partition_point(|l| l.target < target);
                        t.automation.insert(at, Lane { target, points });
                        Vec::new()
                    }
                };
                SetAutomation {
                    track: ti,
                    target,
                    points: old,
                }
            }
            InsertPatternClip {
                track: ti,
                index,
                clip,
            } => {
                if !clip.is_valid() {
                    return Err(ActionError::BadValue);
                }
                let t = track(p, ti)?;
                if index > t.pattern_clips.len() {
                    return Err(ActionError::OutOfRange);
                }
                t.pattern_clips.insert(index, clip);
                RemovePatternClip { track: ti, index }
            }
            RemovePatternClip { track: ti, index } => {
                let t = track(p, ti)?;
                if index >= t.pattern_clips.len() {
                    return Err(ActionError::OutOfRange);
                }
                InsertPatternClip {
                    track: ti,
                    index,
                    clip: t.pattern_clips.remove(index),
                }
            }
            SetPatternClip {
                track: ti,
                index,
                clip,
            } => {
                if !clip.is_valid() {
                    return Err(ActionError::BadValue);
                }
                let slot = track(p, ti)?
                    .pattern_clips
                    .get_mut(index)
                    .ok_or(ActionError::OutOfRange)?;
                SetPatternClip {
                    track: ti,
                    index,
                    clip: std::mem::replace(slot, clip),
                }
            }
            SetRouting {
                track: ti,
                output,
                sends,
            } => {
                if !sends
                    .iter()
                    .all(|s| s.level.is_finite() && (0.0..=2.0).contains(&s.level))
                {
                    return Err(ActionError::BadRoute);
                }
                let t = track(p, ti)?;
                let old_output = std::mem::replace(&mut t.output, output);
                let old_sends = std::mem::replace(&mut t.sends, sends);
                if p.route_order().is_none() {
                    let t = &mut p.tracks[ti];
                    t.output = old_output;
                    t.sends = old_sends;
                    return Err(ActionError::BadRoute);
                }
                SetRouting {
                    track: ti,
                    output: old_output,
                    sends: old_sends,
                }
            }
            SetSidechain {
                track: ti,
                device,
                source,
            } => {
                let slot = track(p, ti)?
                    .devices
                    .get_mut(device)
                    .ok_or(ActionError::OutOfRange)?;
                let old = std::mem::replace(&mut slot.sidechain, source);
                if p.route_order().is_none() {
                    p.tracks[ti].devices[device].sidechain = old;
                    return Err(ActionError::BadRoute);
                }
                SetSidechain {
                    track: ti,
                    device,
                    source: old,
                }
            }
            Batch(actions) => {
                let mut inverses = Vec::with_capacity(actions.len());
                for action in actions {
                    match action.apply(p) {
                        Ok(inverse) => inverses.push(inverse),
                        Err(e) => {
                            // Roll back what applied, so a failed batch changes nothing.
                            for inverse in inverses.into_iter().rev() {
                                inverse.apply(p).expect("fresh inverse applies");
                            }
                            return Err(e);
                        }
                    }
                }
                inverses.reverse();
                Batch(inverses)
            }
        })
    }
}

/// The project plus its undo and redo stacks.
#[derive(Debug, Default)]
pub struct History {
    project: Project,
    undo: Vec<Action>,
    redo: Vec<Action>,
    /// Edits since the last save or load.
    dirty: bool,
    /// Bumped by every change, so callers can tell when to rebuild from the project.
    revision: u64,
    /// Device uids below this were handed out, including any only in undo history.
    next_uid: u64,
    /// The gesture whose edits are folding into the top undo step, if any.
    gesture: Option<u64>,
}

impl History {
    pub fn new(project: Project) -> Self {
        Self {
            project,
            ..Self::default()
        }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks unsaved changes that live outside the project data, such as a
    /// plugin parameter, so quitting still saves them. No undo step.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// A device uid not used anywhere in the project or its undo history.
    pub fn new_device_uid(&mut self) -> u64 {
        self.next_uid = self.next_uid.max(
            self.project
                .tracks
                .iter()
                .flat_map(|t| &t.devices)
                .map(|d| d.uid + 1)
                .max()
                .unwrap_or(0),
        );
        self.next_uid += 1;
        self.next_uid - 1
    }

    /// Records a device's current plugin state for the next save. Not an edit:
    /// no undo step, no revision change.
    pub fn store_device_state(&mut self, uid: u64, state: Vec<u8>) {
        let device = self
            .project
            .tracks
            .iter_mut()
            .flat_map(|t| &mut t.devices)
            .find(|d| d.uid == uid);
        if let Some(d) = device {
            d.state = state;
        }
    }

    /// Records ARA document archives for the next save, like `store_device_state`.
    pub fn store_ara(&mut self, archives: Vec<AraArchive>) {
        self.project.ara = archives;
    }

    /// Applies an edit, makes it undoable and clears the redo stack.
    pub fn apply(&mut self, action: Action) -> Result<(), ActionError> {
        self.gesture = None;
        let inverse = action.apply(&mut self.project)?;
        self.undo.push(inverse);
        self.redo.clear();
        self.dirty = true;
        self.revision += 1;
        Ok(())
    }

    /// Applies one edit of a continuous gesture (a fader drag) named `gesture`:
    /// the first makes an undo step, later ones with the same `gesture` fold
    /// into it, so undo returns to where the gesture began. Every edit of one
    /// gesture must change the same thing (one track's props, say).
    pub fn apply_gesture(&mut self, action: Action, gesture: u64) -> Result<(), ActionError> {
        if self.gesture != Some(gesture) || self.undo.is_empty() {
            self.apply(action)?;
            self.gesture = Some(gesture);
            return Ok(());
        }
        action.apply(&mut self.project)?;
        self.redo.clear();
        self.dirty = true;
        self.revision += 1;
        Ok(())
    }

    /// Ends any gesture, so the next `apply_gesture` starts a new undo step.
    pub fn end_gesture(&mut self) {
        self.gesture = None;
    }

    /// Returns false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        self.step(false)
    }

    pub fn redo(&mut self) -> bool {
        self.step(true)
    }

    fn step(&mut self, forward: bool) -> bool {
        self.gesture = None;
        let (from, to) = if forward {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some(action) = from.pop() else {
            return false;
        };
        // Recorded inverses always apply to the state they were recorded against.
        let inverse = action
            .apply(&mut self.project)
            .expect("recorded inverse applies");
        to.push(inverse);
        self.dirty = true;
        self.revision += 1;
        true
    }

    /// Writes the project as JSON and marks it clean.
    pub fn save(&mut self, path: &Path) -> Result<(), ProjectError> {
        self.save_copy(path)?;
        self.dirty = false;
        Ok(())
    }

    /// Writes the project to `path` without marking it saved: an autosave or a
    /// template.
    pub fn save_copy(&self, path: &Path) -> Result<(), ProjectError> {
        let file = ProjectFile {
            version: FORMAT_VERSION,
            project: self.project.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(ProjectError::Format)?;
        // Write, flush to disk, then rename, so a crash, power cut or full disk
        // never leaves a broken or empty project.
        let tmp = path.with_extension("daw.tmp");
        let written = fs::File::create(&tmp).and_then(|mut f| {
            f.write_all(&json)?;
            f.sync_all()
        });
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(ProjectError::Io(e));
        }
        fs::rename(&tmp, path).map_err(ProjectError::Io)
    }

    /// Reads a project written by `save`, with empty undo history.
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let bytes = fs::read(path).map_err(ProjectError::Io)?;
        let mut file: ProjectFile = serde_json::from_slice(&bytes).map_err(ProjectError::Format)?;
        match file.version {
            1 => migrate_common_params(&mut file.project),
            FORMAT_VERSION => {}
            v => return Err(ProjectError::Version(v)),
        }
        // Projects from before routing have no track ids: number them.
        let first = file.project.new_track_id();
        if first == u64::MAX {
            return Err(ProjectError::Invalid);
        }
        let unnumbered = file.project.tracks.iter_mut().filter(|t| t.id == 0);
        for (id, t) in (first..).zip(unnumbered) {
            t.id = id;
        }
        let p = &file.project;
        let mut track_ids: Vec<u64> = p.tracks.iter().map(|t| t.id).collect();
        track_ids.sort_unstable();
        let routable = track_ids.windows(2).all(|w| w[0] != w[1])
            && track_ids.last().is_none_or(|&id| id < u64::MAX)
            && p.route_order().is_some()
            && p.tracks
                .iter()
                .flat_map(|t| &t.sends)
                .all(|s| s.level.is_finite() && (0.0..=2.0).contains(&s.level));
        let finite = p
            .tracks
            .iter()
            .all(|t| t.gain.is_finite() && t.pan.is_finite());
        // Two devices sharing a uid would put one plugin node in the graph twice.
        let mut uids: Vec<u64> = p
            .tracks
            .iter()
            .flat_map(|t| &t.devices)
            .map(|d| d.uid)
            .collect();
        uids.sort_unstable();
        let mut unique = uids.windows(2).all(|w| w[0] != w[1]);
        let mut ids: Vec<u64> = p.patterns.iter().map(|q| q.id).collect();
        ids.sort_unstable();
        unique &= ids.windows(2).all(|w| w[0] != w[1]);
        let musical = loop_ok(p.loop_region)
            && p.tracks.iter().flat_map(|t| &t.clips).all(ClipRef::gain_ok)
            && valid_tempos(&p.tempos)
            && valid_meters(&p.meters)
            && p.patterns.iter().all(Pattern::is_valid)
            && p.tracks.iter().all(|t| {
                // One note list per pattern, or removing a pattern leaves notes behind.
                let mut lists: Vec<u64> = t.notes.iter().map(|n| n.pattern).collect();
                lists.sort_unstable();
                lists.windows(2).all(|w| w[0] != w[1])
                    && t.notes.iter().flat_map(|n| &n.notes).all(Note::is_valid)
                    && t.pattern_clips.iter().all(PatternClip::is_valid)
                    && t.automation.iter().all(|l| valid_points(l.target, &l.points))
                    // One lane per target.
                    && t.automation.iter().enumerate().all(|(i, a)| {
                        t.automation[i + 1..].iter().all(|b| a.target != b.target)
                    })
            });
        if !(p.bpm.is_finite() && p.bpm > 0.0)
            || p.sample_rate == 0
            || !finite
            || !unique
            || !musical
            || !routable
        {
            return Err(ProjectError::Invalid);
        }
        Ok(Self::new(file.project))
    }
}

/// Each built-in effect's own parameter count in format 1, where MIX, MATCH
/// and DELTA took the ids right after them. Frozen: it describes old files.
const V1_OWN_PARAMS: [(&str, u32); 15] = [
    ("canary.eq", 5),
    ("canary.eq8", 33),
    ("canary.compressor", 5),
    ("canary.limiter", 3),
    ("canary.gate", 4),
    ("canary.saturator", 2),
    ("canary.dynamic_eq", 5),
    ("canary.deesser", 3),
    ("canary.filter", 4),
    ("canary.reverb", 4),
    ("canary.delay", 4),
    ("canary.chorus", 4),
    ("canary.phaser", 5),
    ("canary.crusher", 2),
    ("canary.utility", 4),
];

/// Moves format-1 automation of a built-in effect's MIX/MATCH/DELTA to their
/// fixed ids.
fn migrate_common_params(project: &mut Project) {
    let own: std::collections::HashMap<u64, u32> = project
        .tracks
        .iter()
        .flat_map(|t| &t.devices)
        .filter(|d| d.format == PluginFormat::Builtin)
        .filter_map(|d| {
            let (_, n) = V1_OWN_PARAMS.iter().find(|(id, _)| *id == d.id)?;
            Some((d.uid, *n))
        })
        .collect();
    for lane in project.tracks.iter_mut().flat_map(|t| &mut t.automation) {
        if let Target::Param { device, id } = &mut lane.target {
            if let Some(&n) = own.get(&*device) {
                if (n..n + 3).contains(id) {
                    *id = daw_core::COMMON_PARAM_ID + (*id - n);
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    version: u32,
    project: Project,
}

#[derive(Debug)]
pub enum ProjectError {
    Io(std::io::Error),
    Format(serde_json::Error),
    /// Written by a newer or older, incompatible Canary.
    Version(u32),
    /// Parses, but holds a tempo, sample rate, gain or pan no project can have.
    Invalid,
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read or write the project: {e}"),
            Self::Format(e) => write!(f, "not a valid project file: {e}"),
            Self::Version(v) => write!(f, "project format {v} is not supported"),
            Self::Invalid => write!(f, "project file holds invalid values"),
        }
    }
}

impl std::error::Error for ProjectError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(start: u64) -> ClipRef {
        ClipRef {
            source: "kick.wav".into(),
            start,
            offset: 0,
            length: 100,
            fade_in: 0,
            fade_out: 0,
            gain: 1.0,
            reverse: false,
            stretch: None,
        }
    }

    #[test]
    fn every_action_undoes_and_redoes_exactly() {
        let mut h = History::default();
        let mut snapshots = vec![h.project().clone()];
        let props = TrackProps {
            name: "Bass".into(),
            gain: 0.5,
            pan: -0.25,
            mute: true,
            solo: true,
        };
        for action in [
            Action::SetTempo(140.0),
            Action::SetReference(Some("/refs/master.wav".into())),
            Action::SetTempoMap(vec![TempoChange {
                tick: 4 * PPQ,
                bpm: 90.0,
            }]),
            Action::SetMeters(vec![MeterChange {
                bar: 0,
                numerator: 3,
                denominator: 4,
            }]),
            Action::InsertTrack {
                index: 0,
                track: Track::new("Drums"),
            },
            Action::InsertTrack {
                index: 0,
                track: Track::new("Keys"),
            },
            Action::SetAutomation {
                track: 0,
                target: Target::Gain,
                points: vec![Point {
                    tick: 0,
                    value: 0.5,
                    curve: 0.0,
                }],
            },
            Action::SetAutomation {
                track: 0,
                target: Target::Param { device: 3, id: 7 },
                points: vec![Point {
                    tick: 10,
                    value: 440.0,
                    curve: -0.5,
                }],
            },
            Action::SetAutomation {
                track: 0,
                target: Target::Gain,
                points: Vec::new(),
            },
            Action::SetTrackProps { index: 1, props },
            Action::InsertClip {
                track: 1,
                index: 0,
                clip: clip(0),
            },
            Action::SetClip {
                track: 1,
                index: 0,
                clip: clip(480),
            },
            Action::InsertClip {
                track: 1,
                index: 1,
                clip: clip(960),
            },
            Action::RemoveClip { track: 1, index: 0 },
            Action::Batch(vec![
                Action::SetClip {
                    track: 1,
                    index: 0,
                    clip: clip(900),
                },
                Action::InsertClip {
                    track: 1,
                    index: 1,
                    clip: clip(1_000),
                },
            ]),
            Action::RemoveTrack { index: 0 },
        ] {
            h.apply(action).unwrap();
            snapshots.push(h.project().clone());
        }
        assert_eq!(h.project().tracks[0].name, "Bass");
        assert_eq!(h.project().tracks[0].clips, [clip(900), clip(1_000)]);

        for want in snapshots.iter().rev().skip(1) {
            assert!(h.undo());
            assert_eq!(h.project(), want);
        }
        assert!(!h.undo());
        for want in snapshots.iter().skip(1) {
            assert!(h.redo());
            assert_eq!(h.project(), want);
        }
        assert!(!h.redo());
    }

    #[test]
    fn bad_actions_change_nothing_and_new_edits_clear_redo() {
        let mut h = History::default();
        for bad in [
            Action::SetTempo(0.0),
            Action::SetTempo(f64::NAN),
            Action::RemoveTrack { index: 0 },
            Action::InsertTrack {
                index: 1,
                track: Track::new("x"),
            },
            Action::InsertClip {
                track: 0,
                index: 0,
                clip: clip(0),
            },
            Action::SetClip {
                track: 0,
                index: 0,
                clip: clip(0),
            },
        ] {
            assert!(h.apply(bad).is_err());
        }
        assert!(h.project() == &Project::default() && !h.is_dirty());
        h.apply(Action::InsertTrack {
            index: 0,
            track: Track::new("x"),
        })
        .unwrap();
        let before = h.project().clone();
        let mut props = TrackProps::of(&before.tracks[0]);
        props.gain = f32::NAN;
        assert_eq!(
            h.apply(Action::SetTrackProps { index: 0, props }),
            Err(ActionError::BadValue)
        );
        let half_bad = Action::Batch(vec![
            Action::SetTempo(77.0),
            Action::RemoveTrack { index: 5 },
        ]);
        assert_eq!(h.apply(half_bad), Err(ActionError::OutOfRange));
        assert_eq!(h.project(), &before);
        assert!(h.undo() && !h.undo());

        h.apply(Action::SetTempo(90.0)).unwrap();
        h.undo();
        h.apply(Action::SetTempo(100.0)).unwrap();
        assert!(!h.redo(), "a new edit drops the redo branch");
    }

    #[test]
    fn save_and_load_round_trip_and_reject_other_versions() {
        let dir = std::env::temp_dir().join(format!("canary-project-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.daw");
        let mut h = History::default();
        h.apply(Action::InsertTrack {
            index: 0,
            track: Track::new("Drums"),
        })
        .unwrap();
        h.apply(Action::InsertClip {
            track: 0,
            index: 0,
            clip: clip(48_000),
        })
        .unwrap();
        h.save(&path).unwrap();
        assert!(!h.is_dirty());

        let loaded = History::load(&path).unwrap();
        assert_eq!(loaded.project(), h.project());

        let text = fs::read_to_string(&path)
            .unwrap()
            .replace("\"version\": 2", "\"version\": 99");
        fs::write(&path, text).unwrap();
        assert!(matches!(
            History::load(&path),
            Err(ProjectError::Version(99))
        ));
        let text = fs::read_to_string(&path).unwrap();
        let zero_tempo = text
            .replace("\"version\": 99", "\"version\": 2")
            .replace("\"bpm\": 120.0", "\"bpm\": 0.0");
        fs::write(&path, zero_tempo).unwrap();
        assert!(matches!(History::load(&path), Err(ProjectError::Invalid)));
        fs::write(&path, "{").unwrap();
        assert!(matches!(History::load(&path), Err(ProjectError::Format(_))));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_1_mix_automation_moves_to_the_fixed_id() {
        let dir = std::env::temp_dir().join(format!("canary-v1-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.daw");
        let mut project = Project::default();
        let mut track = Track::new("Vox");
        track.devices.push(Device {
            uid: 7,
            format: PluginFormat::Builtin,
            bundle: None,
            id: "canary.delay".into(),
            name: "Delay".into(),
            state: Vec::new(),
            sidechain: None,
        });
        // Format 1: the delay had 4 params, so 4 was MIX; 1 is FEEDBACK.
        for id in [4, 1] {
            track.automation.push(Lane {
                target: Target::Param { device: 7, id },
                points: Vec::new(),
            });
        }
        project.tracks.push(track);
        History::new(project).save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("\"version\": 2", "\"version\": 1")).unwrap();
        let loaded = History::load(&path).unwrap();
        let ids: Vec<_> = loaded.project().tracks[0]
            .automation
            .iter()
            .map(|l| l.target)
            .collect();
        assert_eq!(
            ids,
            [
                Target::Param {
                    device: 7,
                    id: daw_core::COMMON_PARAM_ID
                },
                Target::Param { device: 7, id: 1 },
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn devices_undo_save_and_get_fresh_uids() {
        let dir = std::env::temp_dir().join(format!("canary-devices-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut h = History::default();
        h.apply(Action::InsertTrack {
            index: 0,
            track: Track::new("Synth"),
        })
        .unwrap();
        let uid = h.new_device_uid();
        let device = Device {
            uid,
            format: PluginFormat::AudioUnit,
            bundle: None,
            id: "aumu:dls :appl".into(),
            name: "DLSMusicDevice".into(),
            state: Vec::new(),
            sidechain: None,
        };
        h.apply(Action::InsertDevice {
            track: 0,
            index: 0,
            device,
        })
        .unwrap();
        let revision = h.revision();
        h.store_device_state(uid, (0..=255).collect());
        assert_eq!(h.revision(), revision, "state is not an edit");
        let path = dir.join("d.daw");
        h.save(&path).unwrap();
        let loaded = History::load(&path).unwrap();
        assert_eq!(loaded.project(), h.project());

        h.apply(Action::RemoveDevice { track: 0, index: 0 })
            .unwrap();
        assert!(h.project().tracks[0].devices.is_empty());
        assert_ne!(
            h.new_device_uid(),
            uid,
            "a removed device's uid is not reused"
        );

        // A file with two devices sharing a uid does not load.
        let text = fs::read_to_string(&path).unwrap();
        let mut file: serde_json::Value = serde_json::from_str(&text).unwrap();
        let devices = file["project"]["tracks"][0]["devices"]
            .as_array_mut()
            .unwrap();
        devices.push(devices[0].clone());
        fs::write(&path, file.to_string()).unwrap();
        assert!(matches!(History::load(&path), Err(ProjectError::Invalid)));
        assert!(h.undo());
        assert_eq!(h.project().tracks[0].devices[0].state.len(), 256);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn move_device_reorders_undoes_and_rejects_bad_slots() {
        let mut h = History::default();
        h.apply(Action::InsertTrack {
            index: 0,
            track: Track::new("Bus"),
        })
        .unwrap();
        for (i, id) in ["canary.compressor", "canary.saturator", "canary.deesser"]
            .iter()
            .enumerate()
        {
            let device = Device {
                uid: h.new_device_uid(),
                format: PluginFormat::Builtin,
                bundle: None,
                id: (*id).into(),
                name: (*id).into(),
                state: Vec::new(),
                sidechain: None,
            };
            h.apply(Action::InsertDevice {
                track: 0,
                index: i,
                device,
            })
            .unwrap();
        }
        let order = |h: &History| -> Vec<String> {
            h.project().tracks[0]
                .devices
                .iter()
                .map(|d| d.id.clone())
                .collect()
        };
        let before = order(&h);
        h.apply(Action::MoveDevice {
            track: 0,
            from: 0,
            to: 2,
        })
        .unwrap();
        assert_eq!(
            order(&h),
            ["canary.saturator", "canary.deesser", "canary.compressor"]
        );
        assert!(h.undo());
        assert_eq!(order(&h), before);
        let bad = Action::MoveDevice {
            track: 0,
            from: 1,
            to: 3,
        };
        assert_eq!(h.apply(bad), Err(ActionError::OutOfRange));
        assert_eq!(order(&h), before);
    }

    #[test]
    fn pattern_edits_undo_validate_and_remove_cleanly() {
        let mut h = History::default();
        let mut snapshots = vec![h.project().clone()];
        let pattern = |id, length| Pattern {
            id,
            name: format!("P{id}"),
            length,
        };
        let note = |start, key| Note {
            start,
            length: STEP,
            key,
            velocity: 100,
        };
        let clip = |start| PatternClip {
            pattern: 1,
            start,
            length: 4 * PPQ,
            offset: 0,
        };
        for action in [
            Action::InsertTrack {
                index: 0,
                track: Track::new("Kick"),
            },
            Action::InsertPattern {
                index: 0,
                pattern: pattern(1, 4 * PPQ),
            },
            Action::InsertPattern {
                index: 1,
                pattern: pattern(2, 8 * PPQ),
            },
            Action::SetPattern {
                index: 0,
                pattern: pattern(1, 2 * PPQ),
            },
            Action::SetNotes {
                track: 0,
                pattern: 1,
                notes: vec![note(STEP * 4, 36), note(0, 36)],
            },
            Action::InsertPatternClip {
                track: 0,
                index: 0,
                clip: clip(0),
            },
            Action::SetPatternClip {
                track: 0,
                index: 0,
                clip: clip(PPQ),
            },
            Action::InsertPatternClip {
                track: 0,
                index: 1,
                clip: clip(8 * PPQ),
            },
            Action::RemovePatternClip { track: 0, index: 1 },
        ] {
            h.apply(action).unwrap();
            snapshots.push(h.project().clone());
        }
        let t = &h.project().tracks[0];
        assert_eq!(t.notes_in(1), [note(0, 36), note(STEP * 4, 36)], "sorted");
        assert!(t.notes_in(2).is_empty());

        for bad in [
            Action::InsertPattern {
                index: 0,
                pattern: pattern(2, PPQ),
            },
            Action::InsertPattern {
                index: 0,
                pattern: pattern(3, 0),
            },
            Action::SetPattern {
                index: 1,
                pattern: pattern(1, PPQ),
            },
            Action::SetNotes {
                track: 0,
                pattern: 1,
                notes: vec![Note {
                    velocity: 0,
                    ..note(0, 1)
                }],
            },
            Action::SetNotes {
                track: 0,
                pattern: 1,
                notes: vec![note(0, 128)],
            },
            Action::InsertPatternClip {
                track: 0,
                index: 0,
                clip: PatternClip {
                    length: 0,
                    ..clip(0)
                },
            },
            Action::InsertPatternClip {
                track: 0,
                index: 0,
                clip: clip(MAX_TICKS),
            },
            Action::InsertPattern {
                index: 0,
                pattern: pattern(3, STEP - 1),
            },
            // The id is what notes and clips point at; it cannot change.
            Action::SetPattern {
                index: 0,
                pattern: pattern(9, PPQ),
            },
        ] {
            assert!(h.apply(bad).is_err());
        }
        assert_eq!(h.project(), snapshots.last().unwrap());

        // Note lists stay in pattern order, so a removal undoes to the same list.
        h.apply(Action::SetNotes {
            track: 0,
            pattern: 2,
            notes: vec![note(0, 40)],
        })
        .unwrap();
        let two_lists = h.project().clone();
        h.apply(h.project().remove_pattern(0).unwrap()).unwrap();
        assert!(h.undo());
        assert_eq!(h.project(), &two_lists);
        assert!(h.undo());

        // Removing a pattern takes its notes and clips with it, in one undo step.
        let remove = h.project().remove_pattern(0).unwrap();
        h.apply(remove).unwrap();
        let p = h.project();
        assert_eq!(p.patterns, [pattern(2, 8 * PPQ)]);
        assert!(p.tracks[0].notes.is_empty() && p.tracks[0].pattern_clips.is_empty());
        assert_eq!(p.new_pattern_id(), 3);
        // An id a note list still points at (a pattern removed on its own) is skipped.
        let mut dangling = p.clone();
        dangling.tracks[0].notes.push(PatternNotes {
            pattern: 6,
            notes: vec![note(0, 1)],
        });
        assert_eq!(dangling.new_pattern_id(), 7);
        assert!(h.undo());
        assert_eq!(h.project(), snapshots.last().unwrap());

        for want in snapshots.iter().rev().skip(1) {
            assert!(h.undo());
            assert_eq!(h.project(), want);
        }
        for want in snapshots.iter().skip(1) {
            assert!(h.redo());
            assert_eq!(h.project(), want);
        }
    }

    #[test]
    fn a_gesture_folds_into_one_undo_step() {
        let mut h = History::default();
        h.apply(Action::InsertTrack {
            index: 0,
            track: Track::new("A"),
        })
        .unwrap();
        let start = h.project().clone();
        let props = |gain| TrackProps {
            gain,
            ..TrackProps::of(&start.tracks[0])
        };
        for g in [0.9, 0.7, 0.5] {
            h.apply_gesture(
                Action::SetTrackProps {
                    index: 0,
                    props: props(g),
                },
                7,
            )
            .unwrap();
        }
        assert_eq!(h.project().tracks[0].gain, 0.5);
        h.end_gesture();
        h.apply_gesture(
            Action::SetTrackProps {
                index: 0,
                props: props(0.2),
            },
            7,
        )
        .unwrap();
        assert!(h.undo());
        assert_eq!(
            h.project().tracks[0].gain,
            0.5,
            "a new gesture, its own step"
        );
        assert!(h.undo());
        assert_eq!(h.project(), &start, "the first gesture undoes as one");
    }

    #[test]
    fn base64_round_trips_every_length() {
        for n in 0..10 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 200) as u8).collect();
            let text = base64::encode(&bytes);
            assert_eq!(text.len() % 4, 0);
            assert_eq!(base64::decode(&text).unwrap(), bytes);
        }
        assert_eq!(base64::encode(b"Man"), "TWFu");
        assert!(base64::decode("$$$$").is_none());
    }
}
