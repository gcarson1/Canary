//! Input monitoring: the input stream's callback queues what it captures, and
//! a plan node plays it into the tracks that monitor, through their effects.
//! The two devices run on their own clocks, so the node keeps a small cushion
//! queued and drops the oldest audio when the queue grows past it.

use daw_core::{MidiEvent, ProcessContext};
use rtrb::{Consumer, Producer, RingBuffer};

/// Frames of cushion the node waits for before it plays (and keeps at most
/// twice over): about 5 ms at 48 kHz, enough for uneven callback sizes.
const CUSHION: usize = 256;
/// Frames the queue holds.
const CAPACITY: usize = 8_192;

/// The input callback's side. `push` never allocates, locks or blocks.
pub struct MonitorInput {
    queue: Producer<f32>,
    channels: usize,
}

/// Plays the live input as stereo (a mono input on both sides).
pub struct MonitorNode {
    queue: Consumer<f32>,
    channels: usize,
    /// Waiting for the cushion to fill (at the start, and after running dry).
    priming: bool,
}

/// The two ends of a monitor queue for an input with `channels` channels.
pub fn monitor(channels: u16) -> (MonitorInput, MonitorNode) {
    let channels = usize::from(channels.max(1));
    let (tx, rx) = RingBuffer::new(CAPACITY * channels);
    (
        MonitorInput {
            queue: tx,
            channels,
        },
        MonitorNode {
            queue: rx,
            channels,
            priming: true,
        },
    )
}

impl MonitorInput {
    /// Queues whole frames of interleaved input; what does not fit is dropped
    /// (the node is not running, or is far behind).
    pub fn push(&mut self, interleaved: &[f32]) {
        let frames = interleaved.len() / self.channels;
        let fit = frames.min(self.queue.slots() / self.channels) * self.channels;
        if let Ok(chunk) = self.queue.write_chunk_uninit(fit) {
            chunk.fill_from_iter(interleaved.iter().copied());
        }
    }
}

impl daw_graph::AudioNode for MonitorNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        _context: &ProcessContext,
    ) {
        let n = outputs.first().map_or(0, |o| o.len());
        for out in outputs.iter_mut() {
            out.fill(0.0);
        }
        let ch = self.channels;
        let mut queued = self.queue.slots() / ch;
        // Drifted behind: drop the oldest, back to the cushion.
        if queued > 2 * CUSHION + n {
            let drop = (queued - CUSHION - n) * ch;
            if let Ok(chunk) = self.queue.read_chunk(drop) {
                chunk.commit_all();
            }
            queued = CUSHION + n;
        }
        if self.priming {
            if queued < CUSHION + n {
                return;
            }
            self.priming = false;
        }
        let take = n.min(queued);
        if let Ok(chunk) = self.queue.read_chunk(take * ch) {
            let (a, b) = chunk.as_slices();
            let at = |i: usize| if i < a.len() { a[i] } else { b[i - a.len()] };
            let right = ch.min(2) - 1;
            for i in 0..take {
                let (l, r) = (at(i * ch), at(i * ch + right));
                if let [left_out, right_out, ..] = outputs {
                    left_out[i] = l;
                    right_out[i] = r;
                }
            }
            chunk.commit_all();
        }
        // Ran dry: wait for the cushion again rather than crackle.
        if take < n {
            self.priming = true;
        }
    }

    fn reset(&mut self) {
        self.priming = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_graph::AudioNode;

    fn run(node: &mut MonitorNode, n: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
        let ctx = ProcessContext {
            num_samples: n,
            ..Default::default()
        };
        assert_no_alloc::assert_no_alloc(|| {
            node.process(&[], &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx);
        });
        (l, r)
    }

    #[test]
    fn the_input_plays_after_a_cushion_and_never_lags_far() {
        let (mut input, mut node) = monitor(2);
        // Stereo frames: left = i, right = -i.
        let frames: Vec<f32> = (0..64).flat_map(|i| [i as f32, -(i as f32)]).collect();
        input.push(&frames);
        let (l, _) = run(&mut node, 64);
        assert!(l.iter().all(|&x| x == 0.0), "waits for the cushion");
        for _ in 0..8 {
            input.push(&frames);
        }
        let (l, r) = run(&mut node, 64);
        assert_eq!((l[1], r[1]), (1.0, -1.0), "left and right in order");
        // A long stall on the output side: the queue fills, and the node
        // drops the oldest audio instead of playing it seconds late.
        for _ in 0..200 {
            input.push(&frames);
        }
        run(&mut node, 64);
        assert!(node.queue.slots() / 2 <= CUSHION + 64);
        // A mono input plays on both sides.
        let (mut mono, mut node) = monitor(1);
        mono.push(&vec![0.5; CUSHION + 64]);
        let (l, r) = run(&mut node, 64);
        assert!(l.iter().chain(&r).all(|&x| x == 0.5));
    }
}
