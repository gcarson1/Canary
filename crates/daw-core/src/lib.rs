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
