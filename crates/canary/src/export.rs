//! Exporting the mix (and stems): the song plays through, plan taps write each
//! output to a 32-bit float WAV, and other formats are made from it with
//! macOS's `afconvert` on a worker thread.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use daw_engine::{recorder, RecordInput, Recorder};
use daw_ui::ExportFormat;

/// Seconds past the song's end, so reverb and delay tails ring out.
pub const TAIL_SECONDS: u64 = 2;

/// One file being written: where, its recorder, and how many frames its output
/// lags the dry signal (plugin delay), trimmed off the front when it is done.
pub struct Output {
    pub path: PathBuf,
    pub recorder: Recorder,
    pub skip: u64,
}

pub struct Export {
    /// Timeline sample where the export ends.
    pub end: u64,
    pub format: ExportFormat,
    pub master: Output,
    /// (track index, file) per stem.
    pub stems: Vec<(usize, Output)>,
    /// Recorder inputs for the next plan to tap: `None` is the master.
    pub taps: Vec<(Option<usize>, RecordInput)>,
    /// Play has been sent, once the plan carrying the taps was delivered.
    pub play_sent: bool,
    /// The transport has been seen rolling since Play was sent.
    pub started: bool,
}

/// A background format conversion; it returns the files it could not convert.
pub type Conversion = JoinHandle<Vec<String>>;

fn clean(name: &str) -> String {
    name.replace(['/', ':'], "-")
}

impl Export {
    /// Starts writers for the mix, named `name`, and for each `(index, name)`
    /// stem, into `dir`. Fails, leaving no files, if one cannot be created.
    pub fn start(
        dir: &Path,
        name: &str,
        stems: &[(usize, String)],
        sample_rate: u32,
        end: u64,
        format: ExportFormat,
        playhead: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut made: Vec<(Output, RecordInput)> = Vec::new();
        let files = std::iter::once(format!("{}.wav", clean(name))).chain(
            stems
                .iter()
                // The index keeps two tracks with one name from sharing a file.
                .map(|(i, stem)| format!("{} - {:02} {}.wav", clean(name), i + 1, clean(stem))),
        );
        for file in files {
            let path = dir.join(file);
            let (mut rec, input) = recorder(2, sample_rate, playhead.clone());
            if let Err(e) = rec.start(&path) {
                for (mut out, _) in made {
                    let _ = out.recorder.stop();
                    let _ = std::fs::remove_file(&out.path);
                }
                return Err(format!("{}: {e}", path.display()));
            }
            made.push((
                Output {
                    path,
                    recorder: rec,
                    skip: 0,
                },
                input,
            ));
        }
        let mut made = made.into_iter();
        let (master, input) = made.next().expect("the master is always made");
        let mut taps = vec![(None, input)];
        let mut outputs = Vec::new();
        for ((out, input), (i, _)) in made.zip(stems) {
            taps.push((Some(*i), input));
            outputs.push((*i, out));
        }
        Ok(Self {
            end,
            format,
            master,
            stems: outputs,
            taps,
            play_sent: false,
            started: false,
        })
    }

    /// The latest sample any output must record to, plugin delay included.
    pub fn record_end(&self) -> u64 {
        let skip = self.outputs().map(|o| o.skip).max().unwrap_or(0);
        self.end + skip
    }

    fn outputs(&self) -> impl Iterator<Item = &Output> {
        std::iter::once(&self.master).chain(self.stems.iter().map(|s| &s.1))
    }

    fn outputs_mut(&mut self) -> impl Iterator<Item = &mut Output> {
        std::iter::once(&mut self.master).chain(self.stems.iter_mut().map(|s| &mut s.1))
    }

    /// Stops every writer, then on a worker thread trims each file's plugin
    /// delay and converts it to the chosen format. Returns the master's final
    /// path and that job (which reports failures). If a writer failed, every
    /// file is deleted and the error returned.
    pub fn finish(mut self) -> Result<(PathBuf, Conversion), String> {
        let mut files = Vec::new();
        let mut error = None;
        for out in self.outputs_mut() {
            match out.recorder.stop() {
                Ok(take) if take.dropped_frames > 0 => {
                    error.get_or_insert(format!(
                        "{}: the disk fell behind and {} frames were lost",
                        out.path.display(),
                        take.dropped_frames
                    ));
                }
                Ok(_) => {}
                Err(e) => {
                    error.get_or_insert(format!("{}: {e}", out.path.display()));
                }
            }
            files.push((out.path.clone(), out.skip));
        }
        if let Some(e) = error {
            for (path, _) in &files {
                let _ = std::fs::remove_file(path);
            }
            return Err(e);
        }
        let convert = afconvert_args(self.format);
        let master = match convert {
            Some((_, ext)) => self.master.path.with_extension(ext),
            None => self.master.path.clone(),
        };
        let job = thread::spawn(move || {
            let mut failures = Vec::new();
            for (wav, skip) in files {
                if skip > 0 {
                    if let Err(e) = trim_front(&wav, skip) {
                        failures.push(format!("could not trim {}: {e}", wav.display()));
                        continue;
                    }
                }
                let Some((args, ext)) = convert else {
                    continue;
                };
                let out = wav.with_extension(ext);
                let status = Process::new("/usr/bin/afconvert")
                    .args(args)
                    .arg(&wav)
                    .arg(&out)
                    .status();
                if status.is_ok_and(|s| s.success()) {
                    let _ = std::fs::remove_file(&wav);
                } else {
                    // The WAV stays, so nothing is lost.
                    failures.push(format!("could not convert {}", wav.display()));
                }
            }
            failures
        });
        Ok((master, job))
    }

    /// Stops the writers and deletes the partial files.
    pub fn abort(mut self) {
        for out in self.outputs_mut() {
            let _ = out.recorder.stop();
            let _ = std::fs::remove_file(&out.path);
        }
    }
}

/// Rewrites a stereo float WAV without its first `frames` frames.
fn trim_front(path: &Path, frames: u64) -> Result<(), String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let samples: Vec<f32> = reader
        .samples::<f32>()
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let skip = (frames as usize * channels).min(samples.len());
    let split: Vec<Vec<f32>> = (0..channels)
        .map(|c| {
            samples[skip..]
                .iter()
                .skip(c)
                .step_by(channels)
                .copied()
                .collect()
        })
        .collect();
    daw_engine::write_wav(path, &split, spec.sample_rate)
}

/// `afconvert` options and the file extension for a format other than the
/// float WAV the export writes.
fn afconvert_args(format: ExportFormat) -> Option<(&'static [&'static str], &'static str)> {
    match format {
        ExportFormat::Wav32 => None,
        ExportFormat::Wav24 => Some((&["-f", "WAVE", "-d", "LEI24"], "24.wav")),
        ExportFormat::Aiff24 => Some((&["-f", "AIFF", "-d", "BEI24"], "aif")),
        ExportFormat::Flac => Some((&["-f", "flac", "-d", "flac"], "flac")),
        ExportFormat::Aac => Some((&["-f", "m4af", "-d", "aac", "-b", "256000"], "m4a")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_converted_format_gets_its_own_extension() {
        let all = [
            ExportFormat::Wav24,
            ExportFormat::Aiff24,
            ExportFormat::Flac,
            ExportFormat::Aac,
        ];
        let mut exts: Vec<_> = all.iter().map(|&f| afconvert_args(f).unwrap().1).collect();
        exts.dedup();
        assert_eq!(exts.len(), 4);
        assert!(afconvert_args(ExportFormat::Wav32).is_none());
        assert_eq!(clean("a/b:c"), "a-b-c");
    }

    #[test]
    fn trimming_drops_the_delay_from_the_front() {
        let dir = std::env::temp_dir().join(format!("canary-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let left: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let right: Vec<f32> = left.iter().map(|x| -x).collect();
        daw_engine::write_wav(&path, &[left, right], 48_000).unwrap();
        trim_front(&path, 3).unwrap();
        let s: Vec<f32> = hound::WavReader::open(&path)
            .unwrap()
            .into_samples::<f32>()
            .map(Result::unwrap)
            .collect();
        assert_eq!(&s[..4], &[3.0, -3.0, 4.0, -4.0]);
        assert_eq!(s.len(), 14);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
