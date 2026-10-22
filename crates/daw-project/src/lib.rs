//! Project state: undo/redo actions, serialization and `.daw` project files.
//!
//! The project is plain data the UI edits through `Action`s. Turning it into an
//! engine plan (loading audio, building nodes) happens above this crate.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped when the file format changes incompatibly.
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub bpm: f64,
    /// Rate that every sample position in the project counts in.
    pub sample_rate: u32,
    pub tracks: Vec<Track>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            sample_rate: 48_000,
            tracks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub name: String,
    pub gain: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub clips: Vec<ClipRef>,
}

impl Track {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            clips: Vec::new(),
        }
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
}

/// One undoable edit. Each is applied by `History::apply`, which records its exact
/// inverse, so undo and redo never drift from the project.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SetTempo(f64),
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
    /// A gain or pan that is not finite, which a project file cannot store.
    BadValue,
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => write!(f, "no such track or clip"),
            Self::BadTempo => write!(f, "tempo must be a positive number"),
            Self::BadValue => write!(f, "gain and pan must be finite numbers"),
        }
    }
}

impl std::error::Error for ActionError {}

fn track(p: &mut Project, i: usize) -> Result<&mut Track, ActionError> {
    p.tracks.get_mut(i).ok_or(ActionError::OutOfRange)
}

impl Action {
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
            InsertTrack { index, track } => {
                if index > p.tracks.len() {
                    return Err(ActionError::OutOfRange);
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

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Applies an edit, makes it undoable and clears the redo stack.
    pub fn apply(&mut self, action: Action) -> Result<(), ActionError> {
        let inverse = action.apply(&mut self.project)?;
        self.undo.push(inverse);
        self.redo.clear();
        self.dirty = true;
        self.revision += 1;
        Ok(())
    }

    /// Returns false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        self.step(false)
    }

    pub fn redo(&mut self) -> bool {
        self.step(true)
    }

    fn step(&mut self, forward: bool) -> bool {
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
        let file = ProjectFile {
            version: FORMAT_VERSION,
            project: self.project.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(ProjectError::Format)?;
        // Write then rename, so a crash or full disk never leaves a broken project.
        let tmp = path.with_extension("daw.tmp");
        fs::write(&tmp, json)
            .and_then(|()| fs::rename(&tmp, path))
            .map_err(ProjectError::Io)?;
        self.dirty = false;
        Ok(())
    }

    /// Reads a project written by `save`, with empty undo history.
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let bytes = fs::read(path).map_err(ProjectError::Io)?;
        let file: ProjectFile = serde_json::from_slice(&bytes).map_err(ProjectError::Format)?;
        if file.version != FORMAT_VERSION {
            return Err(ProjectError::Version(file.version));
        }
        let p = &file.project;
        let finite = p
            .tracks
            .iter()
            .all(|t| t.gain.is_finite() && t.pan.is_finite());
        if !(p.bpm.is_finite() && p.bpm > 0.0) || p.sample_rate == 0 || !finite {
            return Err(ProjectError::Invalid);
        }
        Ok(Self::new(file.project))
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
            Action::InsertTrack {
                index: 0,
                track: Track::new("Drums"),
            },
            Action::InsertTrack {
                index: 0,
                track: Track::new("Keys"),
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
            .replace("\"version\": 1", "\"version\": 99");
        fs::write(&path, text).unwrap();
        assert!(matches!(
            History::load(&path),
            Err(ProjectError::Version(99))
        ));
        let text = fs::read_to_string(&path).unwrap();
        let zero_tempo = text
            .replace("\"version\": 99", "\"version\": 1")
            .replace("\"bpm\": 120.0", "\"bpm\": 0.0");
        fs::write(&path, zero_tempo).unwrap();
        assert!(matches!(History::load(&path), Err(ProjectError::Invalid)));
        fs::write(&path, "{").unwrap();
        assert!(matches!(History::load(&path), Err(ProjectError::Format(_))));
        fs::remove_dir_all(&dir).unwrap();
    }
}
