use std::fmt;
use std::time::Instant;

use daw_core::MidiEvent;
use rtrb::{Consumer, Producer, RingBuffer};

/// MIDI messages waiting for the audio thread, and the most it places per callback.
pub(crate) const MIDI_QUEUE_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TimedMidi {
    /// Arrival time, in nanoseconds since the engine's clock origin.
    nanos: u64,
    data: [u8; 3],
}

/// Queues live MIDI for the audio thread, stamped with its arrival time. Move it
/// into a MIDI callback, such as `connect_midi`'s.
pub struct MidiSender {
    queue: Producer<TimedMidi>,
    origin: Instant,
}

impl MidiSender {
    /// Stamps a raw message with the current time and queues it. Only channel voice
    /// messages (note, CC, program, pressure, pitch bend) of the right length are
    /// kept. Returns false if the message was dropped as another kind or because the
    /// queue is full. Never blocks.
    pub fn send(&mut self, bytes: &[u8]) -> bool {
        let len = match bytes.first() {
            Some(0xC0..=0xDF) => 2,
            Some(0x80..=0xEF) => 3,
            _ => return false,
        };
        if bytes.len() != len || bytes[1..].iter().any(|b| b & 0x80 != 0) {
            return false;
        }
        let mut data = [0; 3];
        data[..len].copy_from_slice(bytes);
        let nanos = self.origin.elapsed().as_nanos() as u64;
        self.queue.push(TimedMidi { nanos, data }).is_ok()
    }
}

#[cfg(test)]
impl MidiSender {
    /// Queues a message with an explicit arrival time, for deterministic tests.
    pub(crate) fn send_at(&mut self, nanos: u64, data: [u8; 3]) {
        self.queue.push(TimedMidi { nanos, data }).unwrap();
    }
}

/// Audio-thread side: turns arrival times into sample offsets.
pub(crate) struct MidiReceiver {
    queue: Consumer<TimedMidi>,
    origin: Instant,
    last_block: Option<u64>,
    events: Vec<MidiEvent>,
}

pub(crate) fn midi_queue() -> (MidiSender, MidiReceiver) {
    let (tx, rx) = RingBuffer::new(MIDI_QUEUE_CAPACITY);
    let origin = Instant::now();
    (
        MidiSender { queue: tx, origin },
        MidiReceiver {
            queue: rx,
            origin,
            last_block: None,
            events: Vec::with_capacity(MIDI_QUEUE_CAPACITY),
        },
    )
}

impl MidiReceiver {
    /// Pretends the previous callback began at `nanos`, so the next drain is deterministic.
    #[cfg(test)]
    pub(crate) fn set_reference(&mut self, nanos: u64) {
        self.last_block = Some(nanos);
    }

    /// This callback's events, offsets within its `frames` frames and in order.
    /// Allocation-free.
    pub(crate) fn drain(&mut self, frames: usize, sample_rate: f32) -> &mut [MidiEvent] {
        let now = self.origin.elapsed().as_nanos() as u64;
        self.drain_at(now, frames, sample_rate)
    }

    /// Events that arrived during the previous callback period land in this block at
    /// the same spacing, so live input has one block of latency but no jitter.
    fn drain_at(&mut self, now: u64, frames: usize, sample_rate: f32) -> &mut [MidiEvent] {
        let prev = self.last_block.replace(now).unwrap_or(now);
        self.events.clear();
        let last = frames.saturating_sub(1) as u64;
        // Anything past capacity waits for the next block rather than allocating.
        while self.events.len() < self.events.capacity() {
            let Ok(e) = self.queue.pop() else { break };
            let offset =
                (e.nanos.saturating_sub(prev) as f64 * f64::from(sample_rate) / 1e9) as u64;
            self.events.push(MidiEvent {
                sample_offset: offset.min(last) as u32,
                data: e.data,
            });
        }
        &mut self.events
    }
}

#[derive(Debug)]
pub enum MidiError {
    Init(midir::InitError),
    /// No input port at this index.
    NoPort(usize),
    Connect(midir::ConnectErrorKind),
}

impl fmt::Display for MidiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init(e) => write!(f, "MIDI unavailable: {e}"),
            Self::NoPort(i) => write!(f, "no MIDI input port {i}"),
            Self::Connect(e) => write!(f, "could not open MIDI port: {e}"),
        }
    }
}

impl std::error::Error for MidiError {}

/// Names of the MIDI input ports (CoreMIDI sources on macOS), by index.
pub fn midi_input_ports() -> Result<Vec<String>, MidiError> {
    let input = midir::MidiInput::new("Canary").map_err(MidiError::Init)?;
    Ok(input
        .ports()
        .iter()
        .map(|p| input.port_name(p).unwrap_or_default())
        .collect())
}

/// Opens input port `index` and feeds its messages to the engine. Input stops when
/// the returned connection is dropped.
pub fn connect_midi(
    mut sender: MidiSender,
    index: usize,
) -> Result<midir::MidiInputConnection<()>, MidiError> {
    let mut input = midir::MidiInput::new("Canary").map_err(MidiError::Init)?;
    input.ignore(midir::Ignore::All);
    let port = input
        .ports()
        .into_iter()
        .nth(index)
        .ok_or(MidiError::NoPort(index))?;
    input
        .connect(
            &port,
            "Canary input",
            move |_, bytes, _| {
                sender.send(bytes);
            },
            (),
        )
        .map_err(|e| MidiError::Connect(e.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(nanos: u64) -> TimedMidi {
        TimedMidi {
            nanos,
            data: [0x90, 60, 100],
        }
    }

    #[test]
    fn arrivals_keep_their_spacing_one_block_late() {
        let (mut tx, mut rx) = midi_queue();
        rx.drain_at(1_000_000_000, 480, 48_000.0); // first block sets the reference

        // 10 ms block at 48 kHz: 1 ms in is frame 48, 5 ms is 240, late ones clamp.
        for nanos in [1_001_000_000, 1_005_000_000, 1_020_000_000, 900_000_000] {
            tx.queue.push(timed(nanos)).unwrap();
        }
        let offsets: Vec<u32> = rx
            .drain_at(1_010_000_000, 480, 48_000.0)
            .iter()
            .map(|e| e.sample_offset)
            .collect();
        assert_eq!(offsets, [48, 240, 479, 0]);
        assert!(rx.drain_at(1_020_000_000, 0, 48_000.0).is_empty());
    }

    #[test]
    fn send_keeps_only_well_formed_voice_messages() {
        let (mut tx, mut rx) = midi_queue();
        assert!(tx.send(&[0x90, 60, 100]));
        assert!(tx.send(&[0xC3, 5]));
        assert!(!tx.send(&[0xC3, 5, 0]), "program change is two bytes");
        assert!(!tx.send(&[0x90, 60]), "truncated note on");
        assert!(!tx.send(&[0x90, 0x80, 1]), "data byte with the top bit set");
        assert!(!tx.send(&[0xF8]), "clock");
        assert!(!tx.send(&[0xF0, 1, 2, 0xF7]), "sysex");
        assert!(!tx.send(&[]));
        let got: Vec<_> = rx.drain(64, 48_000.0).iter().map(|e| e.data).collect();
        assert_eq!(got, [[0x90, 60, 100], [0xC3, 5, 0]]);
    }

    #[test]
    fn full_queue_drops_and_drains_whole() {
        let (mut tx, mut rx) = midi_queue();
        for _ in 0..MIDI_QUEUE_CAPACITY {
            tx.queue.push(timed(0)).unwrap();
        }
        assert!(!tx.send(&[0x80, 60, 0]), "full queue drops");
        assert_eq!(rx.drain(64, 48_000.0).len(), MIDI_QUEUE_CAPACITY);
        assert!(tx.send(&[0x80, 60, 0]));
        assert_eq!(rx.drain(64, 48_000.0).len(), 1);
    }
}
