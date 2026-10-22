use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use daw_core::SampleData;
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
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot open audio file: {e}"),
            Self::Decode(e) => write!(f, "cannot decode audio file: {e}"),
            Self::NoAudioTrack => write!(f, "file has no decodable audio track"),
            Self::Empty => write!(f, "file contains no audio"),
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
                Some(plane) => channel.extend_from_slice(plane),
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
}
