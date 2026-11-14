use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use crate::stream::{pick_config, StreamFaults};
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
    start_output_at(
        queue_capacity,
        None,
        &OutputChoice::default(),
        StreamFaults::default(),
    )
}

/// Which output to open and how: the system default device and buffer size
/// unless named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputChoice {
    /// A device name from `output_devices`; the default output if `None` or
    /// no longer there.
    pub device: Option<String>,
    /// Frames per callback; the device's own if `None` or out of its range.
    pub buffer: Option<u32>,
}

/// The output devices' names, the default one first.
pub fn output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_string());
    let mut names: Vec<String> = host
        .output_devices()
        .map(|ds| {
            ds.filter_map(|d| d.description().ok().map(|d| d.name().to_string()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(i) = default.and_then(|d| names.iter().position(|n| *n == d)) {
        let d = names.remove(i);
        names.insert(0, d);
    }
    names
}

/// `start_output`, asking the device for `rate` (a project's) if it offers it,
/// and reporting stream errors to `faults`.
pub fn start_output_at(
    queue_capacity: usize,
    rate: Option<u32>,
    choice: &OutputChoice,
    faults: StreamFaults,
) -> Result<(EngineHandle, cpal::Stream, cpal::StreamConfig), OutputError> {
    let host = cpal::default_host();
    let named = choice.device.as_ref().and_then(|want| {
        host.output_devices().ok()?.find(|d| {
            d.description()
                .is_ok_and(|desc| desc.name() == want.as_str())
        })
    });
    let device = named
        .or_else(|| host.default_output_device())
        .ok_or(OutputError::NoDevice)?;
    let default = device.default_output_config()?;
    let ranges = if rate.is_some() {
        device.supported_output_configs()?.collect()
    } else {
        Vec::new()
    };
    let supported = pick_config(default, ranges, rate);
    if supported.sample_format() != SampleFormat::F32 {
        return Err(OutputError::UnsupportedFormat(supported.sample_format()));
    }
    let mut config = supported.config();
    if let (Some(frames), cpal::SupportedBufferSize::Range { min, max }) =
        (choice.buffer, supported.buffer_size())
    {
        if (*min..=*max).contains(&frames) {
            config.buffer_size = cpal::BufferSize::Fixed(frames);
        }
    }
    let channels = config.channels as usize;

    let (handle, mut audio) = engine(queue_capacity, config.sample_rate as f32);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| audio.process(data, channels),
        // May run on the render thread (xruns): only flags the fault.
        move |e| faults.record(&e),
        None,
    )?;
    stream.play()?;
    Ok((handle, stream, config))
}
