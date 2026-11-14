//! Convolution reverb: the input convolved with a synthesized space (filtered,
//! decaying stereo noise, as Space Designer's synthesized mode makes). Building
//! an impulse response allocates, so a worker thread per instance builds it
//! whenever DECAY or DAMP change and hands it to the audio thread over a
//! lock-free queue; the old one keeps ringing out its tail, fed silence, so a
//! change never cuts the reverb off.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fft_convolver::TwoStageFFTConvolver;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::effects::{Effect, ParamSpec};

/// Samples per head block: the cost of a block, not latency (there is none).
const HEAD_BLOCK: usize = 128;
/// How often the worker looks for new settings.
const POLL: Duration = Duration::from_millis(15);

const PARAMS: [ParamSpec; 4] = [
    ParamSpec {
        name: "DECAY",
        min: 0.2,
        max: 10.0,
        default: 2.0,
        log: true,
    },
    ParamSpec {
        name: "DAMP",
        min: 0.0,
        max: 1.0,
        default: 0.5,
        log: false,
    },
    ParamSpec {
        name: "PREDELAY",
        min: 0.0,
        max: 200.0,
        default: 10.0,
        log: false,
    },
    ParamSpec {
        name: "WIDTH",
        min: 0.0,
        max: 1.0,
        default: 1.0,
        log: false,
    },
];

/// The shape of the space, as the worker last saw it asked for.
struct Wanted {
    decay: AtomicU32,
    damp: AtomicU32,
    stop: AtomicBool,
}

/// One impulse response, ready to run: a convolver per side.
struct Engine {
    sides: [TwoStageFFTConvolver<f32>; 2],
    /// Samples it rings on for once its input stops.
    tail: usize,
}

/// The stereo impulse response for `decay` seconds (to -60 dB) and `damp`
/// (0: highs last as long as lows; 1: they die about ten times sooner).
pub fn synth_ir(decay: f32, damp: f32, sample_rate: f32) -> [Vec<f32>; 2] {
    let len = ((decay * sample_rate) as usize).max(64);
    let fade_in = (0.002 * sample_rate) as usize + 1;
    let mut sides = [Vec::with_capacity(len), Vec::with_capacity(len)];
    for (side, ir) in sides.iter_mut().enumerate() {
        // Each side its own noise, so the tail is wide.
        let mut seed = 0x9E37_79B9u32 ^ (side as u32 * 0x85EB_CA6B);
        let mut lp = 0.0f32;
        for i in 0..len {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = seed as f32 / u32::MAX as f32 * 2.0 - 1.0;
            let t = i as f32 / len as f32;
            // The low-pass closes as the tail decays: from 16 kHz towards a
            // cutoff DAMP sets at the end.
            let end_hz = 16_000.0 * (1.0 - damp) + 800.0 * damp;
            let cutoff = (16_000.0 * (end_hz / 16_000.0).powf(t)).min(0.45 * sample_rate);
            let a = 1.0 - (-std::f32::consts::TAU * cutoff / sample_rate).exp();
            lp += (noise - lp) * a;
            let envelope = (-6.9 * t).exp() * (i.min(fade_in) as f32 / fade_in as f32);
            ir.push(lp * envelope);
        }
    }
    // Half the input's power out for white noise: a tail sits under the dry.
    let energy: f32 = sides.iter().flatten().map(|s| s * s).sum::<f32>() / 2.0;
    let scale = (0.25 / energy.max(1e-12)).sqrt();
    sides.iter_mut().flatten().for_each(|s| *s *= scale);
    sides
}

fn build(decay: f32, damp: f32, sample_rate: f32) -> Box<Engine> {
    let irs = synth_ir(decay, damp, sample_rate);
    let tail = irs[0].len();
    let sides = irs.map(|ir| {
        let mut c = TwoStageFFTConvolver::default();
        // Only fails for a zero block size or an empty response.
        c.init_default(HEAD_BLOCK, &ir)
            .expect("valid impulse response");
        c
    });
    Box::new(Engine { sides, tail })
}

/// Builds engines when the wanted space changes; drops the retired ones.
fn worker(
    wanted: Arc<Wanted>,
    sample_rate: f32,
    mut built: (f32, f32),
    mut to_audio: Producer<Box<Engine>>,
    mut retired: Consumer<Box<Engine>>,
) {
    while !wanted.stop.load(Ordering::Relaxed) {
        while retired.pop().is_ok() {}
        let now = (
            f32::from_bits(wanted.decay.load(Ordering::Relaxed)),
            f32::from_bits(wanted.damp.load(Ordering::Relaxed)),
        );
        if now != built && to_audio.slots() > 0 {
            built = now;
            // The queue had room a moment ago and only this thread pushes.
            let _ = to_audio.push(build(now.0, now.1, sample_rate));
        }
        std::thread::sleep(POLL);
    }
}

/// Convolution reverb on a synthesized space. Wet only; the node's MIX blends.
pub struct Convolver {
    sr: f32,
    values: [f32; 4],
    wanted: Arc<Wanted>,
    from_worker: Option<Consumer<Box<Engine>>>,
    to_worker: Option<Producer<Box<Engine>>>,
    engine: Option<Box<Engine>>,
    /// The previous engine, fed silence until its tail has rung out.
    draining: Option<(Box<Engine>, usize)>,
    pre: [Box<[f32]>; 2],
    pre_pos: usize,
    pre_len: usize,
    /// Scratch for one block: delayed input, a draining tail, silence.
    scratch: [Box<[f32]>; 3],
}

impl Default for Convolver {
    fn default() -> Self {
        Self {
            sr: 0.0,
            values: PARAMS.map(|p| p.default),
            wanted: Arc::new(Wanted {
                decay: AtomicU32::new(PARAMS[0].default.to_bits()),
                damp: AtomicU32::new(PARAMS[1].default.to_bits()),
                stop: AtomicBool::new(false),
            }),
            from_worker: None,
            to_worker: None,
            engine: None,
            draining: None,
            pre: Default::default(),
            pre_pos: 0,
            pre_len: 0,
            scratch: Default::default(),
        }
    }
}

impl Drop for Convolver {
    fn drop(&mut self) {
        self.wanted.stop.store(true, Ordering::Relaxed);
    }
}

/// The longest block `process` takes in one go; longer ones are split.
const MAX_CHUNK: usize = 1024;

impl Convolver {
    /// Takes a new engine if the worker has one. The current one starts
    /// draining; one already draining is cut and sent back to be dropped.
    fn swap_in(&mut self) {
        let Some(new) = self.from_worker.as_mut().and_then(|c| c.pop().ok()) else {
            return;
        };
        if let Some(old) = self.engine.replace(new) {
            let cut = self.draining.replace((old, 0)).map(|(e, _)| e);
            if let (Some(e), Some(back)) = (cut, self.to_worker.as_mut()) {
                // Room is reserved: at most two engines are ever out here.
                let _ = back.push(e);
            }
            if let Some((old, left)) = &mut self.draining {
                *left = old.tail + MAX_CHUNK;
            }
        }
    }

    fn chunk(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        let [input, tail, silence] = &mut self.scratch;
        let len = self.pre[0].len();
        let width = self.values[3];
        let mut wet = [[0.0f32; MAX_CHUNK]; 2];
        for (side, buf) in [&*left, &*right].into_iter().enumerate() {
            // Predelay: read what came in `pre_len` samples ago.
            let mut pos = self.pre_pos;
            for (i, &x) in buf.iter().enumerate() {
                let read = (pos + len - self.pre_len) % len;
                self.pre[side][pos] = x;
                input[i] = self.pre[side][read];
                pos = (pos + 1) % len;
            }
            if let Some(engine) = &mut self.engine {
                let _ = engine.sides[side].process(&input[..n], &mut wet[side][..n]);
            }
            if let Some((old, _)) = &mut self.draining {
                silence[..n].fill(0.0);
                if old.sides[side]
                    .process(&silence[..n], &mut tail[..n])
                    .is_ok()
                {
                    for (w, t) in wet[side][..n].iter_mut().zip(&tail[..n]) {
                        *w += t;
                    }
                }
            }
        }
        self.pre_pos = (self.pre_pos + n) % len;
        if let Some((_, left_samples)) = &mut self.draining {
            *left_samples = left_samples.saturating_sub(n);
            if *left_samples == 0 {
                if let (Some((e, _)), Some(back)) = (self.draining.take(), self.to_worker.as_mut())
                {
                    let _ = back.push(e);
                }
            }
        }
        for i in 0..n {
            let (l, r) = (wet[0][i], wet[1][i]);
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5 * width;
            left[i] = mid + side;
            right[i] = mid - side;
        }
    }
}

impl Effect for Convolver {
    fn params(&self) -> &'static [ParamSpec] {
        &PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        let Some(v) = self.values.get_mut(index) else {
            return;
        };
        *v = value;
        match index {
            0 => self.wanted.decay.store(value.to_bits(), Ordering::Relaxed),
            1 => self.wanted.damp.store(value.to_bits(), Ordering::Relaxed),
            2 => {
                let max = self.pre[0].len().max(1) - 1;
                self.pre_len = ((value / 1000.0 * self.sr) as usize).min(max);
            }
            _ => {}
        }
    }
    /// Main thread: builds the first engine here, then starts the worker.
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        let pre = (0.2 * sample_rate) as usize + 1;
        self.pre = [
            vec![0.0; pre].into_boxed_slice(),
            vec![0.0; pre].into_boxed_slice(),
        ];
        self.scratch = std::array::from_fn(|_| vec![0.0; MAX_CHUNK].into_boxed_slice());
        // A worker for the old rate stops; this one starts with a fresh flag.
        self.wanted.stop.store(true, Ordering::Relaxed);
        let (decay, damp) = (self.values[0], self.values[1]);
        self.wanted = Arc::new(Wanted {
            decay: AtomicU32::new(decay.to_bits()),
            damp: AtomicU32::new(damp.to_bits()),
            stop: AtomicBool::new(false),
        });
        self.engine = Some(build(decay, damp, sample_rate));
        self.draining = None;
        let (to_audio, from_worker) = RingBuffer::new(1);
        let (to_worker, retired) = RingBuffer::new(4);
        self.from_worker = Some(from_worker);
        self.to_worker = Some(to_worker);
        let wanted = self.wanted.clone();
        std::thread::Builder::new()
            .name("canary-convolver".into())
            .spawn(move || worker(wanted, sample_rate, (decay, damp), to_audio, retired))
            .expect("spawn the convolver's worker thread");
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.pre[0].is_empty() {
            return;
        }
        self.swap_in();
        for (l, r) in left.chunks_mut(MAX_CHUNK).zip(right.chunks_mut(MAX_CHUNK)) {
            self.chunk(l, r);
        }
    }
    fn reset(&mut self) {
        if let Some(e) = &mut self.engine {
            e.sides.iter_mut().for_each(TwoStageFFTConvolver::reset);
        }
        if let (Some((e, _)), Some(back)) = (self.draining.take(), self.to_worker.as_mut()) {
            let _ = back.push(e);
        }
        self.pre.iter_mut().for_each(|p| p.fill(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::effect;

    const SR: f32 = 48_000.0;

    fn tail_energy(fx: &mut Box<dyn Effect>, from: usize) -> f32 {
        let mut click = vec![0.0; 96_000];
        click[0] = 1.0;
        let (mut l, mut r) = (click.clone(), click);
        for (a, b) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
            fx.process(a, b);
        }
        l[from..].iter().map(|s| s * s).sum()
    }

    #[test]
    fn rings_for_its_decay_and_rebuilds_when_it_changes() {
        let mut fx = effect("canary.convolver", SR).unwrap();
        fx.set_param(2, 0.0);
        fx.reset();
        // 2 s decay: something at 1 s, nothing (below -60 dB) past 2 s.
        let late = tail_energy(&mut fx, 48_000);
        assert!(late > 1e-6, "{late}");
        // A 0.3 s decay, once the worker has built it.
        fx.set_param(0, 0.3);
        std::thread::sleep(Duration::from_millis(300));
        fx.reset();
        let mut quiet = vec![0.0; 1024];
        let mut quiet_r = quiet.clone();
        fx.process(&mut quiet, &mut quiet_r); // takes the new engine
        fx.reset(); // and drops the draining old one
        let short = tail_energy(&mut fx, 24_000);
        assert!(short < late * 1e-3, "{short} vs {late}");
    }

    #[test]
    fn the_ir_decays_to_minus_60_db_and_damp_darkens_it() {
        let [l, _] = synth_ir(1.0, 0.0, SR);
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let head = rms(&l[..4_800]);
        let end = rms(&l[43_200..]);
        assert!(crate::gain_to_db(end / head) < -45.0);
        // Damped: the end has less high end, so it changes less sample to sample.
        let diff = |x: &[f32]| rms(&x.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>()) / rms(x);
        let [dark, _] = synth_ir(1.0, 1.0, SR);
        assert!(diff(&dark[40_000..]) < 0.5 * diff(&l[40_000..]));
    }
}
