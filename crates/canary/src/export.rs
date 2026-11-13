//! Exporting the mix (and stems): the song plays through, plan taps write each
//! output to a 32-bit float WAV, and other formats are made from it with
//! macOS's `afconvert` on a worker thread.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::thread;

use daw_engine::{recorder, RecordInput, Recorder};
use daw_ui::ExportFormat;

/// Seconds past the song's end, so reverb and delay tails ring out.
pub const TAIL_SECONDS: u64 = 2;

/// One file being written: where, and its recorder.
pub struct Output {
    pub path: PathBuf,
    pub recorder: Recorder,
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
    /// Set once the transport has been seen rolling from the start.
    pub started: bool,
}

fn clean(name: &str) -> String {
    name.replace(['/', ':'], "-")
}

impl Export {
    /// Starts writers for the mix, named `name`, and for each `(index, name)`
    /// stem, into `dir`. Fails if a file cannot be created.
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
        let make = |file: String| -> Result<(Output, RecordInput), String> {
            let path = dir.join(file);
            let (mut rec, input) = recorder(2, sample_rate, playhead.clone());
            rec.start(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((
                Output {
                    path,
                    recorder: rec,
                },
                input,
            ))
        };
        let (master, input) = make(format!("{}.wav", clean(name)))?;
        let mut taps = vec![(None, input)];
        let mut outputs = Vec::new();
        for (i, stem) in stems {
            let (out, input) = make(format!("{} - {}.wav", clean(name), clean(stem)))?;
            taps.push((Some(*i), input));
            outputs.push((*i, out));
        }
        Ok(Self {
            end,
            format,
            master,
            stems: outputs,
            taps,
            started: false,
        })
    }

    /// Stops every writer and converts the files to the chosen format in the
    /// background. Returns the master's path, or the first error.
    pub fn finish(mut self) -> Result<PathBuf, String> {
        let mut paths = Vec::new();
        for out in std::iter::once(&mut self.master).chain(self.stems.iter_mut().map(|s| &mut s.1))
        {
            out.recorder
                .stop()
                .map_err(|e| format!("{}: {e}", out.path.display()))?;
            paths.push(out.path.clone());
        }
        let master = self.master.path.clone();
        if let Some((args, ext)) = afconvert_args(self.format) {
            // Converting can take a while; the WAVs are already safe on disk.
            thread::spawn(move || {
                for wav in paths {
                    let out = wav.with_extension(ext);
                    let status = Process::new("/usr/bin/afconvert")
                        .args(args)
                        .arg(&wav)
                        .arg(&out)
                        .status();
                    if status.is_ok_and(|s| s.success()) {
                        let _ = std::fs::remove_file(&wav);
                    } else {
                        eprintln!("afconvert failed for {}", wav.display());
                    }
                }
            });
            return Ok(master.with_extension(ext));
        }
        Ok(master)
    }

    /// Stops the writers and deletes the partial files.
    pub fn abort(mut self) {
        for out in std::iter::once(&mut self.master).chain(self.stems.iter_mut().map(|s| &mut s.1))
        {
            let _ = out.recorder.stop();
            let _ = std::fs::remove_file(&out.path);
        }
    }
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
}
