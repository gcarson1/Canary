//! Lending the audio engine out of the device callback, so a worker thread can
//! render faster than real time (an offline export) with the very same plan,
//! plugins and state. The callback plays silence while it is lent.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::{engine, AudioEngine, EngineHandle};

/// What the device callback runs: the engine, unless it is lent out.
pub struct Renderer {
    engine: Option<Box<AudioEngine>>,
    ask: Consumer<()>,
    out: Producer<Box<AudioEngine>>,
    back: Consumer<Box<AudioEngine>>,
}

/// The main thread's side of lending.
pub struct Lending {
    /// A `borrow` timed out with its request still queued.
    stale: bool,
    ask: Producer<()>,
    out: Consumer<Box<AudioEngine>>,
    back: Producer<Box<AudioEngine>>,
}

/// `engine` plus the lending queues: the handle, and the callback's renderer.
pub fn lendable_engine(capacity: usize, sample_rate: f32) -> (EngineHandle, Lending, Renderer) {
    let (handle, audio) = engine(capacity, sample_rate);
    let (ask_tx, ask_rx) = RingBuffer::new(1);
    let (out_tx, out_rx) = RingBuffer::new(1);
    let (back_tx, back_rx) = RingBuffer::new(1);
    (
        handle,
        Lending {
            stale: false,
            ask: ask_tx,
            out: out_rx,
            back: back_tx,
        },
        Renderer {
            // Boxed here, on the main thread, so lending only moves a pointer.
            engine: Some(Box::new(audio)),
            ask: ask_rx,
            out: out_tx,
            back: back_rx,
        },
    )
}

impl Renderer {
    /// The device callback: hands the engine over when asked, takes it back
    /// when returned, and renders with it while it is here. Allocation-free.
    pub fn process(&mut self, output: &mut [f32], channels: usize) {
        if let Ok(engine) = self.back.pop() {
            self.engine = Some(engine);
        }
        if self.ask.pop().is_ok() {
            if let Some(engine) = self.engine.take() {
                // Capacity 1 and the main thread holds none: this always fits.
                if let Err(rtrb::PushError::Full(engine)) = self.out.push(engine) {
                    self.engine = Some(engine);
                }
            }
        }
        match self.engine.as_mut() {
            Some(engine) => engine.process(output, channels),
            None => output.fill(0.0),
        }
    }
}

impl Lending {
    /// Asks the callback for the engine and waits up to `wait` for it (a
    /// callback or two). `None` if the device is not running callbacks.
    pub fn borrow(&mut self, wait: std::time::Duration) -> Option<Box<AudioEngine>> {
        // A request from a timed-out borrow may still be queued: one is enough.
        if !self.stale {
            let _ = self.ask.push(());
        }
        let until = std::time::Instant::now() + wait;
        loop {
            if let Ok(engine) = self.out.pop() {
                self.stale = false;
                return Some(engine);
            }
            if std::time::Instant::now() >= until {
                // The request stays queued: if a callback runs later, `tend`
                // sends the engine straight back.
                self.stale = true;
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Gives the engine back to the callback.
    /// Call regularly: returns an engine lent after a `borrow` gave up waiting.
    pub fn tend(&mut self) {
        if self.stale {
            if let Ok(engine) = self.out.pop() {
                self.stale = false;
                self.give_back(engine);
            }
        }
    }

    pub fn give_back(&mut self, engine: Box<AudioEngine>) {
        if let Err(rtrb::PushError::Full(engine)) = self.back.push(engine) {
            // Cannot happen (one engine, capacity one); never drop it silently.
            panic!("engine returned twice: {:?}", std::ptr::from_ref(&*engine));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_core::Command;
    use std::time::Duration;

    #[test]
    fn a_lent_engine_renders_elsewhere_and_comes_back() {
        let (mut handle, mut lending, mut renderer) = lendable_engine(16, 48_000.0);
        let mut block = vec![0.0; 128];
        // Ask, then let the "callback" run once to hand it over.
        let _ = lending.ask.push(());
        assert_no_alloc::assert_no_alloc(|| renderer.process(&mut block, 2));
        let mut engine = lending.borrow(Duration::from_millis(10)).expect("lent");
        handle.commands.push(Command::Play).unwrap();
        let mut big = vec![0.0; 4_096];
        for _ in 0..10 {
            engine.process(&mut big, 2);
        }
        assert_eq!(
            engine.transport().playhead(),
            20_480,
            "rendered off the callback"
        );
        // Meanwhile the callback plays silence.
        block.fill(1.0);
        assert_no_alloc::assert_no_alloc(|| renderer.process(&mut block, 2));
        assert!(block.iter().all(|&s| s == 0.0));
        lending.give_back(engine);
        assert_no_alloc::assert_no_alloc(|| renderer.process(&mut block, 2));
        assert!(renderer.engine.is_some(), "back home");
        assert!(
            lending.borrow(Duration::from_millis(2)).is_none(),
            "no callback ran"
        );
        // The callback sees the stale request later; `tend` sends it home.
        assert_no_alloc::assert_no_alloc(|| renderer.process(&mut block, 2));
        assert!(renderer.engine.is_none());
        lending.tend();
        assert_no_alloc::assert_no_alloc(|| renderer.process(&mut block, 2));
        assert!(renderer.engine.is_some());
    }
}
