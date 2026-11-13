//! The browser's preview voice: plays a whole sample once, whatever the
//! transport is doing, into the master.

use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext, SampleData};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::AudioNode;

/// Samples waiting to start, and finished ones going back to be freed.
const QUEUE: usize = 8;

/// Main-thread side: start a sample, and free the ones the node is done with.
pub struct Preview {
    start: Producer<Option<Arc<SampleData>>>,
    done: Consumer<Arc<SampleData>>,
}

impl Preview {
    /// Plays `audio` from its start, cutting off any preview playing; `None`
    /// stops. False if the queue is full.
    pub fn play(&mut self, audio: Option<Arc<SampleData>>) -> bool {
        self.collect();
        self.start.push(audio).is_ok()
    }

    /// Drops samples the node has let go of. Call it now and then.
    pub fn collect(&mut self) {
        while self.done.pop().is_ok() {}
    }
}

pub struct PreviewNode {
    start: Consumer<Option<Arc<SampleData>>>,
    done: Producer<Arc<SampleData>>,
    playing: Option<Arc<SampleData>>,
    pos: usize,
}

impl PreviewNode {
    pub fn new() -> (Self, Preview) {
        let (start_tx, start_rx) = RingBuffer::new(QUEUE);
        let (done_tx, done_rx) = RingBuffer::new(QUEUE * 2);
        (
            Self {
                start: start_rx,
                done: done_tx,
                playing: None,
                pos: 0,
            },
            Preview {
                start: start_tx,
                done: done_rx,
            },
        )
    }

    /// Hands a finished sample back to be dropped on the main thread. If the
    /// return queue is full it is kept playing silently until there is room.
    fn release(&mut self) -> bool {
        match self.playing.take() {
            Some(a) => match self.done.push(a) {
                Ok(()) => true,
                Err(rtrb::PushError::Full(a)) => {
                    // Parked at its end: silent, handed back next block.
                    self.pos = a.frames();
                    self.playing = Some(a);
                    false
                }
            },
            None => true,
        }
    }
}

impl AudioNode for PreviewNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        _context: &ProcessContext,
    ) {
        // Only take a new sample once the old one could be handed back.
        if self.start.peek().is_ok() && self.release() {
            if let Ok(next) = self.start.pop() {
                self.playing = next;
                self.pos = 0;
            }
        }
        let n = outputs.first().map_or(0, |o| o.len());
        for o in outputs.iter_mut() {
            o.fill(0.0);
        }
        let Some(audio) = self.playing.as_ref() else {
            return;
        };
        let left = audio.frames().saturating_sub(self.pos);
        let take = left.min(n);
        let last = audio.num_channels() - 1;
        for (ch, o) in outputs.iter_mut().take(2).enumerate() {
            let src = audio.channel(ch.min(last));
            o[..take].copy_from_slice(&src[self.pos..self.pos + take]);
        }
        self.pos += take;
        if self.pos >= audio.frames() {
            self.release();
        }
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_once_then_hands_the_sample_back() {
        let (mut node, mut preview) = PreviewNode::new();
        let audio = Arc::new(SampleData::new(48_000, vec![vec![0.5; 6]]).unwrap());
        assert!(preview.play(Some(audio.clone())));
        let ctx = ProcessContext {
            sample_rate: 48_000.0,
            num_samples: 4,
            playhead: 0,
            playing: false,
            ..Default::default()
        };
        let (mut l, mut r) = ([9.0; 4], [9.0; 4]);
        node.process(&[], &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx);
        assert_eq!(l, [0.5; 4]);
        node.process(&[], &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx);
        assert_eq!(l, [0.5, 0.5, 0.0, 0.0]);
        assert_eq!(Arc::strong_count(&audio), 2, "returned, not dropped");
        preview.collect();
        assert_eq!(Arc::strong_count(&audio), 1);
    }

    #[test]
    fn a_full_return_queue_parks_the_sample_silently() {
        let (mut node, mut preview) = PreviewNode::new();
        let audio = Arc::new(SampleData::new(48_000, vec![vec![0.5; 2]]).unwrap());
        let ctx = ProcessContext {
            num_samples: 4,
            ..Default::default()
        };
        let (mut l, mut r) = ([9.0; 4], [9.0; 4]);
        // Fill the return queue without collecting, then keep playing.
        for _ in 0..QUEUE * 3 {
            let _ = preview.start.push(Some(audio.clone()));
            node.process(&[], &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx);
            node.process(&[], &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx);
        }
        assert!(l.iter().all(|s| s.is_finite()));
        preview.collect();
    }
}
