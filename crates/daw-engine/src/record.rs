use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use rtrb::{Consumer, Producer, RingBuffer};

/// Seconds of input the ring holds while the disk writer catches up.
const BUFFER_SECONDS: usize = 2;
/// How long the disk writer sleeps when the ring is empty.
const WRITER_POLL: Duration = Duration::from_millis(5);

/// A finished recording on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Take {
    pub path: PathBuf,
    /// Output playhead when the first frame was captured: where the take goes on the
    /// timeline. Not yet corrected for device input/output latency.
    pub start_sample: u64,
    pub frames: u64,
    /// Frames lost because the disk writer fell behind. Non-zero means a gap.
    pub dropped_frames: u64,
}

#[derive(Debug)]
pub enum RecordError {
    AlreadyRecording,
    NotRecording,
    /// The WAV could not be created or written. If writing failed partway, `take`
    /// describes what reached the file, so the audio before the failure can be kept.
    Wav {
        error: hound::Error,
        take: Option<Take>,
    },
    WriterPanicked,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRecording => write!(f, "already recording"),
            Self::NotRecording => write!(f, "not recording"),
            Self::Wav { error, .. } => write!(f, "could not write the take: {error}"),
            Self::WriterPanicked => write!(f, "the disk writer thread panicked"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<hound::Error> for RecordError {
    fn from(error: hound::Error) -> Self {
        Self::Wav { error, take: None }
    }
}

struct Shared {
    armed: AtomicBool,
    /// Bumped by every `Recorder::start`, so the input side notices a new take even
    /// if the last one stopped between two of its callbacks.
    take: AtomicU32,
    start_sample: AtomicU64,
    dropped_frames: AtomicU64,
}

/// Input-callback side. `push` never allocates, locks or blocks.
pub struct RecordInput {
    queue: Producer<f32>,
    channels: usize,
    shared: Arc<Shared>,
    playhead: Arc<AtomicU64>,
    seen_take: u32,
}

impl RecordInput {
    /// Queues one callback of interleaved input while a take is running. Whole frames
    /// only; frames that do not fit are counted as dropped.
    pub fn push(&mut self, interleaved: &[f32]) {
        if !self.shared.armed.load(Ordering::Acquire) {
            return;
        }
        let take = self.shared.take.load(Ordering::Acquire);
        if take != self.seen_take {
            self.seen_take = take;
            let start = self.playhead.load(Ordering::Relaxed);
            self.shared.start_sample.store(start, Ordering::Relaxed);
        }
        let frames = interleaved.len() / self.channels;
        let fit = frames.min(self.queue.slots() / self.channels);
        if let Ok(chunk) = self.queue.write_chunk_uninit(fit * self.channels) {
            chunk.fill_from_iter(interleaved.iter().copied());
        }
        if fit < frames {
            let lost = (frames - fit) as u64;
            self.shared
                .dropped_frames
                .fetch_add(lost, Ordering::Relaxed);
        }
    }
}

/// The ring handed back, samples written, and how the file ended.
type WriterResult = (Consumer<f32>, u64, Result<(), hound::Error>);

struct Writer {
    thread: JoinHandle<WriterResult>,
    stop: Arc<AtomicBool>,
    path: PathBuf,
}

/// Main-thread side: starts and stops takes. Each take is written to a 32-bit float
/// WAV by its own disk-writer thread, which drains the ring the input callback fills.
pub struct Recorder {
    /// Held here between takes, and by the writer thread during one.
    queue: Option<Consumer<f32>>,
    shared: Arc<Shared>,
    spec: hound::WavSpec,
    writer: Option<Writer>,
}

/// Both ends of a recorder for `channels`-channel input at `sample_rate`. `playhead`
/// is the engine's (`EngineHandle::playhead_clock`), used to place takes.
pub fn recorder(
    channels: u16,
    sample_rate: u32,
    playhead: Arc<AtomicU64>,
) -> (Recorder, RecordInput) {
    let channels = channels.max(1);
    let capacity = usize::from(channels) * sample_rate as usize * BUFFER_SECONDS;
    let (tx, rx) = RingBuffer::new(capacity);
    let shared = Arc::new(Shared {
        armed: AtomicBool::new(false),
        take: AtomicU32::new(0),
        start_sample: AtomicU64::new(0),
        dropped_frames: AtomicU64::new(0),
    });
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    (
        Recorder {
            queue: Some(rx),
            shared: shared.clone(),
            spec,
            writer: None,
        },
        RecordInput {
            queue: tx,
            channels: usize::from(channels),
            shared,
            playhead,
            seen_take: 0,
        },
    )
}

impl Recorder {
    pub fn is_recording(&self) -> bool {
        self.writer.is_some()
    }

    /// Creates the WAV at `path` and starts capturing into it.
    pub fn start(&mut self, path: impl Into<PathBuf>) -> Result<(), RecordError> {
        let Some(mut queue) = self.queue.take() else {
            return Err(RecordError::AlreadyRecording);
        };
        let path = path.into();
        let mut wav = match hound::WavWriter::create(&path, self.spec) {
            Ok(wav) => wav,
            Err(e) => {
                self.queue = Some(queue);
                return Err(e.into());
            }
        };
        // Drop anything left from an earlier take before arming.
        if let Ok(stale) = queue.read_chunk(queue.slots()) {
            stale.commit_all();
        }
        self.shared.dropped_frames.store(0, Ordering::Relaxed);
        self.shared.take.fetch_add(1, Ordering::Release);
        self.shared.armed.store(true, Ordering::Release);

        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let shared = self.shared.clone();
        let thread = thread::Builder::new()
            .name("canary-disk-writer".into())
            .spawn(move || {
                let mut written = 0u64;
                let result = loop {
                    let last = stopping.load(Ordering::Acquire);
                    let chunk = queue.read_chunk(queue.slots()).expect("slots are readable");
                    let (a, b) = chunk.as_slices();
                    if let Err(e) = a.iter().chain(b).try_for_each(|&s| wav.write_sample(s)) {
                        // Stop filling a ring nobody drains. hound finalizes the
                        // header on drop, so what was written stays readable.
                        shared.armed.store(false, Ordering::Release);
                        break Err(e);
                    }
                    written += chunk.len() as u64;
                    chunk.commit_all();
                    if last {
                        break wav.finalize();
                    }
                    if queue.is_empty() {
                        thread::sleep(WRITER_POLL);
                    }
                };
                (queue, written, result)
            })
            .expect("spawn disk writer thread");
        self.writer = Some(Writer { thread, stop, path });
        Ok(())
    }

    /// Stops capturing, waits for the writer to flush everything captured so far,
    /// and returns the take.
    pub fn stop(&mut self) -> Result<Take, RecordError> {
        let writer = self.writer.take().ok_or(RecordError::NotRecording)?;
        self.shared.armed.store(false, Ordering::Release);
        writer.stop.store(true, Ordering::Release);
        let (queue, samples, result) = writer
            .thread
            .join()
            .map_err(|_| RecordError::WriterPanicked)?;
        self.queue = Some(queue);
        let take = Take {
            path: writer.path,
            start_sample: self.shared.start_sample.load(Ordering::Relaxed),
            frames: samples / u64::from(self.spec.channels),
            dropped_frames: self.shared.dropped_frames.load(Ordering::Relaxed),
        };
        match result {
            Ok(()) => Ok(take),
            Err(error) => Err(RecordError::Wav {
                error,
                take: Some(take),
            }),
        }
    }
}

impl Drop for Recorder {
    /// Finishes a running take so its file is a valid WAV.
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.stop();
        }
    }
}

#[derive(Debug)]
pub enum InputError {
    NoDevice,
    UnsupportedFormat(SampleFormat),
    /// The input device runs at a different rate from the output; recordings would
    /// play off-pitch.
    RateMismatch {
        input: u32,
        output: u32,
    },
    Device(cpal::Error),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no default input device"),
            Self::UnsupportedFormat(format) => {
                write!(f, "input device uses {format:?}, only f32 is supported")
            }
            Self::RateMismatch { input, output } => {
                write!(f, "input runs at {input} Hz but output at {output} Hz")
            }
            Self::Device(e) => write!(f, "audio device error: {e}"),
        }
    }
}

impl std::error::Error for InputError {}

impl From<cpal::Error> for InputError {
    fn from(e: cpal::Error) -> Self {
        Self::Device(e)
    }
}

/// Opens the default input device (CoreAudio on macOS) next to the output stream
/// and feeds a recorder. `sample_rate` is the output's; the input must match it.
/// Capture stops when the returned `Stream` is dropped. On macOS the first call
/// asks the user for microphone access.
pub fn start_input(
    sample_rate: u32,
    playhead: Arc<AtomicU64>,
) -> Result<(Recorder, cpal::Stream, cpal::StreamConfig), InputError> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or(InputError::NoDevice)?;
    let supported = device.default_input_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(InputError::UnsupportedFormat(supported.sample_format()));
    }
    let config = supported.config();
    if config.sample_rate != sample_rate {
        return Err(InputError::RateMismatch {
            input: config.sample_rate,
            output: sample_rate,
        });
    }
    let (recorder, mut input) = recorder(config.channels, config.sample_rate, playhead);
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _| input.push(data),
        |e| eprintln!("input stream error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((recorder, stream, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_wav(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("canary-{}-{name}.wav", std::process::id()))
    }

    fn read(path: &PathBuf) -> (hound::WavSpec, Vec<f32>) {
        let mut r = hound::WavReader::open(path).unwrap();
        let samples = r.samples::<f32>().map(Result::unwrap).collect();
        (r.spec(), samples)
    }

    #[test]
    fn take_holds_exactly_what_was_pushed_while_armed() {
        let playhead = Arc::new(AtomicU64::new(0));
        let (mut rec, mut input) = recorder(2, 48_000, playhead.clone());
        input.push(&[9.0; 64]); // before start: ignored
        let path = temp_wav("exact");
        rec.start(&path).unwrap();
        assert!(matches!(
            rec.start(&path),
            Err(RecordError::AlreadyRecording)
        ));
        playhead.store(4_800, Ordering::Relaxed);
        let audio: Vec<f32> = (0..20_000).map(|i| i as f32).collect();
        for block in audio.chunks(256) {
            input.push(block);
        }
        let take = rec.stop().unwrap();
        input.push(&[9.0; 64]); // after stop: ignored

        assert_eq!(take.start_sample, 4_800);
        assert_eq!((take.frames, take.dropped_frames), (10_000, 0));
        let (spec, samples) = read(&take.path);
        assert_eq!((spec.channels, spec.sample_rate), (2, 48_000));
        assert_eq!(samples, audio);
        std::fs::remove_file(&take.path).unwrap();
        assert!(matches!(rec.stop(), Err(RecordError::NotRecording)));
    }

    #[test]
    fn overflow_drops_whole_frames_and_next_take_starts_clean() {
        let (mut rec, mut input) = recorder(2, 4, Arc::default()); // ring of 16 samples
        let path = temp_wav("overflow");
        rec.start(&path).unwrap();
        input.push(&[1.0; 15]); // 7 whole frames; the odd sample is ignored
        input.push(&[1.0; 40]); // more than fits unless the writer has drained

        // However fast the writer is, every frame is either written or counted.
        let take = rec.stop().unwrap();
        assert_eq!(take.frames + take.dropped_frames, 7 + 20);
        assert_eq!(read(&take.path).1.len() as u64, take.frames * 2);

        let playhead = Arc::new(AtomicU64::new(0));
        let (mut rec, mut input) = recorder(1, 48_000, playhead.clone());
        rec.start(&path).unwrap();
        input.push(&[1.0; 8]);
        rec.stop().unwrap();
        playhead.store(99, Ordering::Relaxed);
        rec.start(&path).unwrap(); // new take: new start stamp, no old samples
        input.push(&[2.0; 4]);
        let take = rec.stop().unwrap();
        assert_eq!((take.start_sample, take.dropped_frames), (99, 0));
        assert_eq!(read(&take.path).1, [2.0; 4]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn recorder_dropped_mid_take_leaves_a_valid_wav() {
        let (mut rec, mut input) = recorder(2, 48_000, Arc::default());
        let path = temp_wav("dropped");
        rec.start(&path).unwrap();
        let audio: Vec<f32> = (0..1_000).map(|i| i as f32).collect();
        input.push(&audio);
        drop(rec);

        let (spec, samples) = read(&path); // the header was finalized
        assert_eq!((spec.channels, spec.sample_rate), (2, 48_000));
        assert_eq!(samples, audio);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unwritable_path_errors_and_recorder_stays_usable() {
        let (mut rec, mut input) = recorder(1, 48_000, Arc::default());
        assert!(matches!(
            rec.start("/nonexistent-dir/take.wav"),
            Err(RecordError::Wav { take: None, .. })
        ));
        assert!(!rec.is_recording());
        let path = temp_wav("usable");
        rec.start(&path).unwrap();
        input.push(&[0.5; 3]);
        assert_eq!(rec.stop().unwrap().frames, 3);
        std::fs::remove_file(&path).unwrap();
    }
}
