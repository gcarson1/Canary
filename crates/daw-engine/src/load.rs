use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use daw_core::SampleData;
use rubato::audioadapter_buffers::direct::{SequentialSliceOfSlices, SequentialSliceOfVecs};
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Decode(Error),
    NoAudioTrack,
    /// The file decoded to no audio frames.
    Empty,
    Resample(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot open audio file: {e}"),
            Self::Decode(e) => write!(f, "cannot decode audio file: {e}"),
            Self::NoAudioTrack => write!(f, "file has no decodable audio track"),
            Self::Empty => write!(f, "file contains no audio"),
            Self::Resample(e) => write!(f, "cannot resample audio: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<Error> for LoadError {
    fn from(e: Error) -> Self {
        Self::Decode(e)
    }
}

/// Decodes a whole audio file (WAV, FLAC, MP3, AAC/M4A, Ogg Vorbis) into memory.
/// Blocking: call it on the main thread or a loader thread, never the audio thread.
/// The audio keeps the file's sample rate; nothing is resampled yet.
pub fn load_audio(path: &Path) -> Result<Arc<SampleData>, LoadError> {
    let file = File::open(path).map_err(LoadError::Io)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe().probe(
        &hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(LoadError::NoAudioTrack)?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(LoadError::NoAudioTrack)?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())?;

    let mut sample_rate = 0;
    let mut frames = 0;
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut packet_planes: Vec<Vec<f32>> = Vec::new();
    while let Some(packet) = format.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A corrupt packet costs a few milliseconds of audio, not the whole file.
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        sample_rate = decoded.spec().rate();
        decoded.copy_to_vecs_planar(&mut packet_planes);
        // A channel that appears mid-file starts with silence up to here; one that
        // disappears is padded with silence, so every channel stays in sync.
        if channels.len() < packet_planes.len() {
            channels.resize(packet_planes.len(), vec![0.0; frames]);
        }
        frames += decoded.frames();
        for (ch, channel) in channels.iter_mut().enumerate() {
            match packet_planes.get(ch) {
                // A corrupt float file can hold NaN or infinity; play it as silence.
                Some(plane) => {
                    channel.extend(plane.iter().map(|&s| if s.is_finite() { s } else { 0.0 }))
                }
                None => channel.resize(frames, 0.0),
            }
        }
    }

    if frames == 0 {
        return Err(LoadError::Empty);
    }
    SampleData::new(sample_rate, channels)
        .map(Arc::new)
        .ok_or(LoadError::Empty)
}

/// FFT block size for `resample`; rubato rounds it to suit the rate pair.
const RESAMPLE_CHUNK: usize = 1024;

/// Converts `audio` to `sample_rate` with rubato's FFT resampler (band-limited, so
/// downsampling does not alias). Returns the same `Arc` when the rate already
/// matches. Blocking and allocating: call it off the audio thread, for example after
/// `load_audio`, and again from the original if the device rate changes.
pub fn resample(audio: &Arc<SampleData>, sample_rate: u32) -> Result<Arc<SampleData>, LoadError> {
    if audio.sample_rate() == sample_rate {
        return Ok(audio.clone());
    }
    let channels = audio.num_channels();
    let mut resampler = Fft::<f32>::new(
        audio.sample_rate() as usize,
        sample_rate as usize,
        RESAMPLE_CHUNK,
        channels,
        FixedSync::Both,
    )
    .map_err(|e| LoadError::Resample(e.into()))?;

    let frames = audio.frames();
    let wanted = (frames as u64 * u64::from(sample_rate)).div_ceil(u64::from(audio.sample_rate()));

    let planes: Vec<&[f32]> = (0..channels).map(|ch| audio.channel(ch)).collect();
    let input = SequentialSliceOfSlices::new(&planes, channels, frames)
        .map_err(|e| LoadError::Resample(e.into()))?;

    let out_len = resampler.process_all_needed_output_len(frames);
    let mut out_channels = vec![vec![0.0; out_len]; channels];
    let mut output = SequentialSliceOfVecs::new_mut(&mut out_channels, channels, out_len)
        .map_err(|e| LoadError::Resample(e.into()))?;
    let (_, written) = resampler
        .process_all_into_buffer(&input, &mut output, frames, None)
        .map_err(|e| LoadError::Resample(e.into()))?;
    let len = written.min(wanted as usize);
    for channel in &mut out_channels {
        channel.truncate(len); // into_boxed_slice in SampleData::new frees the slack
    }
    SampleData::new(sample_rate, out_channels)
        .map(Arc::new)
        .ok_or(LoadError::Empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_stereo_wav_as_planar_f32() {
        let path = std::env::temp_dir().join(format!("canary-load-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..1_000i16 {
            writer.write_sample(i * 16).unwrap(); // left ramps up
            writer.write_sample(-i * 16).unwrap(); // right ramps down
        }
        writer.finalize().unwrap();

        let data = load_audio(&path);
        std::fs::remove_file(&path).unwrap();
        let data = data.unwrap();

        assert_eq!(data.sample_rate(), 44_100);
        assert_eq!((data.num_channels(), data.frames()), (2, 1_000));
        let expected = 999.0 * 16.0 / 32_768.0;
        assert!((data.channel(0)[999] - expected).abs() < 1e-4);
        assert!((data.channel(1)[999] + expected).abs() < 1e-4);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        assert!(matches!(
            load_audio(Path::new("/nonexistent/canary.wav")),
            Err(LoadError::Io(_))
        ));
    }

    #[test]
    fn resample_converts_rate_and_keeps_pitch() {
        // One second of a 1 kHz sine at 44.1 kHz becomes one second at 48 kHz.
        let sine = |rate: f32, frames: usize| -> Vec<f32> {
            (0..frames)
                .map(|i| (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / rate).sin())
                .collect()
        };
        let audio = Arc::new(SampleData::new(44_100, vec![sine(44_100.0, 44_100); 2]).unwrap());

        let out = resample(&audio, 48_000).unwrap();

        assert_eq!(out.sample_rate(), 48_000);
        assert_eq!(out.num_channels(), 2);
        assert!(
            (out.frames() as i64 - 48_000).abs() <= 1,
            "{} frames",
            out.frames()
        );
        // Same pitch: a 1 kHz sine crosses zero 2,000 times a second at any rate.
        let mid = &out.channel(1)[1_000..47_000];
        let crossings = mid
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let expected = 2_000.0 * mid.len() as f32 / 48_000.0;
        assert!(
            (crossings as f32 - expected).abs() <= 2.0,
            "{crossings} crossings"
        );
        // Same level.
        let peak = mid.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!((peak - 1.0).abs() < 0.02, "peak {peak}");
    }

    #[test]
    fn resample_to_same_rate_is_a_no_op() {
        let audio = Arc::new(SampleData::new(48_000, vec![vec![0.5; 16]]).unwrap());
        assert!(Arc::ptr_eq(&resample(&audio, 48_000).unwrap(), &audio));
    }

    #[test]
    fn resample_keeps_short_clips_in_time() {
        // A drum-hit-length clip shorter than one FFT block: the impulse at frame
        // 100 (44.1 kHz) must land near frame 109 at 48 kHz, not vanish into the
        // resampler's startup delay.
        let mut click = vec![0.0; 256];
        click[100] = 1.0;
        let audio = Arc::new(SampleData::new(44_100, vec![click]).unwrap());

        let out = resample(&audio, 48_000).unwrap();

        assert_eq!(out.frames(), 279); // ceil(256 * 48000 / 44100)
        let peak = (0..out.frames())
            .max_by(|&a, &b| out.channel(0)[a].abs().total_cmp(&out.channel(0)[b].abs()))
            .unwrap();
        assert!((107..=111).contains(&peak), "impulse moved to frame {peak}");
    }
}
