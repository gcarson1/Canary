//! Real-time thread coordinator: audio streams, lock-free queues, plan swapping, disk streaming and garbage collection.

mod output;
mod transport;

pub use output::{start_output, OutputError};
pub use transport::Transport;

use daw_core::{Command, Telemetry};
use rtrb::{Consumer, Producer, RingBuffer};

/// Main-thread side of the engine queues.
pub struct EngineHandle {
    pub commands: Producer<Command>,
    pub telemetry: Consumer<Telemetry>,
}

/// Audio-thread side. `process` never allocates, locks or blocks.
pub struct AudioEngine {
    commands: Consumer<Command>,
    telemetry: Producer<Telemetry>,
    transport: Transport,
}

/// Creates both ends of the command and telemetry queues, each holding `capacity` messages.
pub fn engine(capacity: usize) -> (EngineHandle, AudioEngine) {
    let (command_tx, command_rx) = RingBuffer::new(capacity);
    let (telemetry_tx, telemetry_rx) = RingBuffer::new(capacity);
    (
        EngineHandle {
            commands: command_tx,
            telemetry: telemetry_rx,
        },
        AudioEngine {
            commands: command_rx,
            telemetry: telemetry_tx,
            transport: Transport::default(),
        },
    )
}

impl AudioEngine {
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Renders one block of interleaved audio with `channels` channels.
    /// Commands queued before the call take effect at the start of the block.
    pub fn process(&mut self, output: &mut [f32], channels: usize) {
        while let Ok(command) = self.commands.pop() {
            self.transport.apply(command);
        }

        // No graph yet: the engine renders silence.
        output.fill(0.0);

        let frames = output.len() / channels.max(1);
        self.transport.advance(frames as u64);

        // A full queue means the main thread is behind; dropping one update is fine.
        let _ = self.telemetry.push(Telemetry::Playhead {
            sample: self.transport.playhead(),
            playing: self.transport.is_playing(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::{assert_no_alloc, AllocDisabler};
    use daw_core::TimeSignature;

    #[global_allocator]
    static ALLOCATOR: AllocDisabler = AllocDisabler;

    #[test]
    fn commands_reach_transport_and_telemetry_comes_back() {
        let (mut handle, mut audio) = engine(16);
        handle.commands.push(Command::SetTempo(90.0)).unwrap();
        handle.commands.push(Command::Play).unwrap();

        let mut block = [1.0; 256]; // 128 stereo frames of stale data
        audio.process(&mut block, 2);

        assert_eq!(block, [0.0; 256]);
        assert_eq!(audio.transport().bpm(), 90.0);
        assert_eq!(
            handle.telemetry.pop(),
            Ok(Telemetry::Playhead {
                sample: 128,
                playing: true
            })
        );
    }

    #[test]
    fn full_telemetry_queue_does_not_block() {
        let (mut handle, mut audio) = engine(1);
        handle.commands.push(Command::Play).unwrap();
        let mut block = [0.0; 64];
        audio.process(&mut block, 1);
        audio.process(&mut block, 1); // queue already full

        assert_eq!(audio.transport().playhead(), 128);
        assert_eq!(
            handle.telemetry.pop(),
            Ok(Telemetry::Playhead {
                sample: 64,
                playing: true
            })
        );
        assert!(handle.telemetry.pop().is_err());
    }

    #[test]
    fn process_does_not_allocate() {
        let (mut handle, mut audio) = engine(16);
        let mut block = vec![0.0; 1024];
        for command in [
            Command::Play,
            Command::Seek(1_000),
            Command::SetTempo(128.0),
            Command::SetTimeSignature(TimeSignature {
                numerator: 7,
                denominator: 8,
            }),
        ] {
            handle.commands.push(command).unwrap();
        }

        assert_no_alloc(|| {
            audio.process(&mut block, 2);
            audio.process(&mut block, 2);
        });

        assert_eq!(audio.transport().playhead(), 1_000 + 1024);
    }
}
