//! Exporting the mix (and stems): plan taps write each output to a 32-bit
//! float WAV, and other formats are made from it with macOS's `afconvert` on a
//! worker thread. Offline (the usual way), a worker borrows the engine from the
//! device callback and renders as fast as it can; if the engine cannot be
//! borrowed, the song plays through in real time into recorders instead.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use daw_engine::{recorder, FileTapDone, RecordInput, Recorder};
use daw_ui::ExportFormat;

/// Seconds past the song's end, so reverb and delay tails ring out.
pub const TAIL_SECONDS: u64 = 2;

/// One file being written: where, what writes it, and how many frames its
/// output lags the dry signal (plugin delay), trimmed off the front when done.
pub struct Output {
    pub path: PathBuf,
    pub writer: Writer,
    pub skip: u64,
}

/// What fills an output's file.
// shortcut: the app always has a lendable engine (start_output sets it), so
// the real-time `Live` path only runs for an engine made without lending; it is
// kept for that case and is tested through the recorder's own tests.
pub enum Writer {
    /// Real time: a recorder's disk-writer thread drains the tap's ring.
    Live(Recorder),
    /// Offline: the tap writes the file itself (set once the plan is built).
    File(Option<FileTapDone>),
}

/// What the next plan taps for an output.
pub enum TapSource {
    Live(RecordInput),
    File,
}

pub struct Export {
    /// Timeline samples where the export starts and ends.
    pub start: u64,
    pub end: u64,
    pub format: ExportFormat,
    /// The sample rate to convert the files to; `None` keeps the project's.
    pub rate: Option<u32>,
    /// Each file and what it records: `None` is the mix, else a track.
    pub outputs: Vec<(Option<usize>, Output)>,
    /// What the next plan taps, per output: `None` is the master.
    pub taps: Vec<(Option<usize>, TapSource)>,
    /// A freeze: the frozen track's id, whether to bounce it in place, and the
    /// project revision it rendered.
    pub freeze: Option<(u64, bool, u64)>,
    /// Rendering offline on a worker (when the engine could be borrowed).
    pub offline: bool,
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
    /// Starts writers into `dir` for each output: `(None, _)` is the mix, named
    /// `name`, and `(Some(index), track)` a track's stem. Fails, leaving no
    /// files, if one cannot be created.
    pub fn start(
        dir: &Path,
        name: &str,
        outputs: &[(Option<usize>, String)],
        sample_rate: u32,
        end: u64,
        format: ExportFormat,
        // The engine's playhead clock for a real-time export; `None` renders
        // offline into file taps.
        playhead: Option<Arc<AtomicU64>>,
    ) -> Result<Self, String> {
        let offline = playhead.is_none();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut made: Vec<(Output, TapSource)> = Vec::new();
        let files = outputs.iter().map(|(who, stem)| match who {
            None => format!("{}.wav", clean(name)),
            // The index keeps two tracks with one name from sharing a file.
            Some(i) => format!("{} - {:02} {}.wav", clean(name), i + 1, clean(stem)),
        });
        for file in files {
            let path = dir.join(file);
            if offline {
                // The file is made with the tap, when the plan is built.
                let writer = Writer::File(None);
                made.push((
                    Output {
                        path,
                        writer,
                        skip: 0,
                    },
                    TapSource::File,
                ));
                continue;
            }
            let clock = playhead.clone().expect("real time has a clock");
            let (mut rec, input) = recorder(2, sample_rate, clock);
            if let Err(e) = rec.start(&path) {
                for (out, _) in made {
                    out.discard();
                }
                return Err(format!("{}: {e}", path.display()));
            }
            let writer = Writer::Live(rec);
            made.push((
                Output {
                    path,
                    writer,
                    skip: 0,
                },
                TapSource::Live(input),
            ));
        }
        let mut taps = Vec::new();
        let mut files = Vec::new();
        for ((out, input), (who, _)) in made.into_iter().zip(outputs) {
            taps.push((*who, input));
            files.push((*who, out));
        }
        Ok(Self {
            start: 0,
            end,
            format,
            rate: None,
            outputs: files,
            freeze: None,
            taps,
            offline,
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
        self.outputs.iter().map(|o| &o.1)
    }

    fn outputs_mut(&mut self) -> impl Iterator<Item = &mut Output> {
        self.outputs.iter_mut().map(|o| &mut o.1)
    }

    /// Stops every writer, then on a worker thread trims each file's plugin
    /// delay and converts it to the chosen format. Returns the first output's
    /// final path and that job (which reports failures). If a writer failed,
    /// every file is deleted and the error returned.
    pub fn finish(mut self) -> Result<(PathBuf, Conversion), String> {
        let mut files = Vec::new();
        let mut error = None;
        for out in self.outputs_mut() {
            let stopped = match &mut out.writer {
                Writer::Live(rec) => rec.stop().map_err(|e| e.to_string()),
                Writer::File(done) => match done.take() {
                    Some(done) => done.finish().map(|()| daw_engine::Take {
                        path: out.path.clone(),
                        start_sample: 0,
                        frames: 0,
                        dropped_frames: 0,
                    }),
                    None => Err("never written".into()),
                },
            };
            match stopped {
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
        let convert = afconvert_args(self.format, self.rate);
        let first = self.outputs.first().map(|o| o.1.path.clone());
        let first = first.ok_or_else(|| "nothing to export".to_string())?;
        let master = match &convert {
            Some((_, ext)) => first.with_extension(ext),
            None => first,
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
                let Some((args, ext)) = &convert else {
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
    pub fn abort(self) {
        self.outputs.into_iter().for_each(|o| o.1.discard());
    }
}

impl Output {
    /// Stops writing and deletes the file.
    fn discard(mut self) {
        match &mut self.writer {
            Writer::Live(rec) => drop(rec.stop()),
            Writer::File(done) => {
                if let Some(done) = done.take() {
                    let _ = done.finish();
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
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

/// `afconvert` options and the file extension for the chosen format and
/// sample rate, unless that is the float WAV the export writes. Integer
/// formats are dithered (TPDF).
fn afconvert_args(format: ExportFormat, rate: Option<u32>) -> Option<(Vec<String>, String)> {
    // Apple's AAC encoder goes no higher than 48 kHz.
    let rate = match format {
        ExportFormat::Aac => rate.map(|r| r.min(48_000)),
        _ => rate,
    };
    let (file, data, dither, ext) = match format {
        ExportFormat::Wav32 if rate.is_none() => return None,
        // Named by rate so it cannot overwrite the float file it converts.
        ExportFormat::Wav32 => ("WAVE", "LEF32", false, "f32.wav"),
        ExportFormat::Wav24 => ("WAVE", "LEI24", true, "24.wav"),
        ExportFormat::Wav16 => ("WAVE", "LEI16", true, "16.wav"),
        ExportFormat::Aiff24 => ("AIFF", "BEI24", true, "aif"),
        ExportFormat::Flac => ("flac", "flac", false, "flac"),
        ExportFormat::Aac => ("m4af", "aac", false, "m4a"),
    };
    let data = match rate {
        Some(r) => format!("{data}@{r}"),
        None => data.to_string(),
    };
    let mut args: Vec<String> = ["-f", file, "-d", &data].map(String::from).into();
    if dither {
        args.extend(["--dither", "1"].map(String::from));
    }
    if format == ExportFormat::Aac {
        args.extend(["-b", "256000"].map(String::from));
    }
    let ext = match rate {
        Some(r) => format!("{}k.{ext}", r / 1000),
        None => ext.to_string(),
    };
    Some((args, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_converted_format_gets_its_own_extension() {
        let all = [
            ExportFormat::Wav24,
            ExportFormat::Wav16,
            ExportFormat::Aiff24,
            ExportFormat::Flac,
            ExportFormat::Aac,
        ];
        let mut exts: Vec<_> = all
            .iter()
            .map(|&f| afconvert_args(f, None).unwrap().1)
            .collect();
        exts.sort();
        exts.dedup();
        assert_eq!(exts.len(), 5);
        assert!(afconvert_args(ExportFormat::Wav32, None).is_none());
        // A new rate resamples (even the float WAV) and names the file by it.
        let (args, ext) = afconvert_args(ExportFormat::Wav32, Some(44_100)).unwrap();
        assert_eq!(
            (args[3].as_str(), ext.as_str()),
            ("LEF32@44100", "44k.f32.wav")
        );
        let (args, _) = afconvert_args(ExportFormat::Wav16, Some(48_000)).unwrap();
        assert!(args.ends_with(&["--dither".into(), "1".into()]));
        let (args, ext) = afconvert_args(ExportFormat::Aac, Some(96_000)).unwrap();
        assert_eq!((args[3].as_str(), ext.as_str()), ("aac@48000", "48k.m4a"));
        assert_eq!(clean("a/b:c"), "a-b-c");
    }

    #[test]
    fn an_offline_export_finishes_its_file_taps_and_trims_their_delay() {
        let dir = std::env::temp_dir().join(format!("canary-offline-{}", std::process::id()));
        let mut e = Export::start(
            &dir,
            "Song",
            &[(None, String::new())],
            48_000,
            8,
            ExportFormat::Wav32,
            None,
        )
        .unwrap();
        assert!(
            e.offline && !dir.join("Song.wav").exists(),
            "files come with the taps"
        );
        // What rebuild does: a file tap for the master, two frames of delay.
        let master = &mut e.outputs[0].1;
        let (mut tap, done) = daw_engine::file_tap(&master.path, 48_000, 10).unwrap();
        master.skip = 2;
        master.writer = Writer::File(Some(done));
        let ctx = daw_core::ProcessContext {
            playing: true,
            num_samples: 10,
            ..Default::default()
        };
        let ramp: Vec<f32> = (0..10).map(|i| i as f32).collect();
        tap.write(&ramp, &ramp, &ctx);
        drop(tap);
        let (path, job) = e.finish().unwrap();
        assert!(job.join().unwrap().is_empty());
        let s: Vec<f32> = hound::WavReader::open(&path)
            .unwrap()
            .into_samples::<f32>()
            .map(Result::unwrap)
            .collect();
        assert_eq!(s.len(), 16, "eight frames after the delay");
        assert_eq!(&s[..2], &[2.0, 2.0]);
        std::fs::remove_dir_all(&dir).unwrap();
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
