use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use crate::{engine, EngineHandle};

#[derive(Debug)]
pub enum OutputError {
    NoDevice,
    UnsupportedFormat(SampleFormat),
    Device(cpal::Error),
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no default output device"),
            Self::UnsupportedFormat(format) => {
                write!(f, "output device uses {format:?}, only f32 is supported")
            }
            Self::Device(e) => write!(f, "audio device error: {e}"),
        }
    }
}

impl std::error::Error for OutputError {}

impl From<cpal::Error> for OutputError {
    fn from(e: cpal::Error) -> Self {
        Self::Device(e)
    }
}

/// Opens the default output device (CoreAudio on macOS) and starts rendering.
/// Audio stops when the returned `Stream` is dropped.
pub fn start_output(
    queue_capacity: usize,
) -> Result<(EngineHandle, cpal::Stream, cpal::StreamConfig), OutputError> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or(OutputError::NoDevice)?;
    let supported = device.default_output_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(OutputError::UnsupportedFormat(supported.sample_format()));
    }
    let config = supported.config();
    let channels = config.channels as usize;

    let (handle, mut audio) = engine(queue_capacity);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| audio.process(data, channels),
        // Called off the render callback, so printing here is allowed.
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((handle, stream, config))
}
