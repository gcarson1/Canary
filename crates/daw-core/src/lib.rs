//! Pure data types shared by every other crate. No heavy dependencies.

/// Identifies a node in the audio graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// A raw MIDI message, timestamped within the current block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    /// Offset in samples from the start of the block.
    pub sample_offset: u32,
    /// Status byte followed by up to two data bytes.
    pub data: [u8; 3],
}

/// Per-block information passed to every `AudioNode::process` call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessContext {
    pub sample_rate: f32,
    /// Frames in this tick. Every buffer passed to `process` is exactly this long;
    /// it can vary per callback, up to the plan's max block size.
    pub num_samples: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u8,
    pub denominator: u8,
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self {
            numerator: 4,
            denominator: 4,
        }
    }
}

/// Main thread → audio thread. Applied at the start of the next block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Play,
    Stop,
    /// Move the playhead to an absolute sample position.
    Seek(u64),
    SetTempo(f64),
    SetTimeSignature(TimeSignature),
}

/// Audio thread → main thread. Sent once per block; dropped if the queue is full.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Telemetry {
    Playhead { sample: u64, playing: bool },
}
