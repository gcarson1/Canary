//! Convolution reverb: the input convolved with a synthesized space (filtered,
//! decaying stereo noise, as Space Designer's synthesized mode makes). Building
//! an impulse response allocates, so a worker thread per instance builds it
//! whenever DECAY or DAMP change and hands it to the audio thread over a
//! lock-free queue; the old one keeps ringing out its tail, fed silence, so a
//! change never cuts the reverb off. A WAV impulse response dropped on the
//! device replaces the synthesized space (DECAY and DAMP then do nothing).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use fft_convolver::TwoStageFFTConvolver;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::effects::{Effect, FileLoader, ParamSpec};

/// The longest impulse response a file may hold, in seconds.
const MAX_IR_SECONDS: f32 = 20.0;

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
    normalize(&mut sides);
    sides
}

/// Half the input's power out for white noise: a tail sits under the dry.
fn normalize(sides: &mut [Vec<f32>; 2]) {
    let energy: f32 = sides.iter().flatten().map(|s| s * s).sum::<f32>() / 2.0;
    let scale = (0.25 / energy.max(1e-12)).sqrt();
    sides.iter_mut().flatten().for_each(|s| *s *= scale);
}

/// Reads a WAV impulse response as stereo at `sample_rate` (mono goes to both
/// sides; more channels: the first two), normalized like the synthesized one.
pub fn read_ir(path: &Path, sample_rate: f32) -> Result<[Vec<f32>; 2], String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| format!("not a WAV file: {e}"))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let frames = reader.duration() as usize;
    if frames == 0 || spec.sample_rate == 0 {
        return Err("the file is empty".into());
    }
    if frames as f32 / spec.sample_rate as f32 > MAX_IR_SECONDS {
        return Err(format!("longer than {MAX_IR_SECONDS} s"));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let full = (1i64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / full))
                .collect::<Result<_, _>>()
        }
    }
    .map_err(|e| format!("cannot read the samples: {e}"))?;
    let side = |c: usize| -> Vec<f32> {
        let c = c.min(channels - 1);
        samples
            .chunks_exact(channels)
            .map(|f| if f[c].is_finite() { f[c] } else { 0.0 })
            .collect()
    };
    let mut sides = [side(0), side(1)].map(|s| resample(&s, spec.sample_rate as f32, sample_rate));
    if sides[0].iter().chain(&sides[1]).all(|&s| s == 0.0) {
        return Err("the file is silent".into());
    }
    normalize(&mut sides);
    Ok(sides)
}

/// Windowed-sinc (Lanczos, 8 lobes) resampling of a whole signal from rate
/// `from` to `to`, low-passing when it goes down. Main thread or worker.
fn resample(x: &[f32], from: f32, to: f32) -> Vec<f32> {
    if (from - to).abs() < 0.5 {
        return x.to_vec();
    }
    const A: f64 = 8.0;
    let step = f64::from(from) / f64::from(to);
    let scale = (1.0 / step).min(1.0);
    let width = A / scale;
    let len = ((x.len() as f64) / step).ceil() as usize;
    (0..len)
        .map(|k| {
            let t = k as f64 * step;
            let first = (t - width).floor().max(0.0) as usize;
            let last = ((t + width).ceil() as usize).min(x.len() - 1);
            let mut sum = 0.0f64;
            for (i, &s) in x.iter().enumerate().take(last + 1).skip(first) {
                let d = (t - i as f64) * scale;
                let w = if d.abs() < 1e-12 {
                    1.0
                } else if d.abs() >= A {
                    0.0
                } else {
                    let p = std::f64::consts::PI * d;
                    A * p.sin() * (p / A).sin() / (p * p)
                };
                sum += w * f64::from(s);
            }
            (sum * scale) as f32
        })
        .collect()
}

fn build(decay: f32, damp: f32, sample_rate: f32) -> Box<Engine> {
    from_ir(synth_ir(decay, damp, sample_rate))
}

fn from_ir(irs: [Vec<f32>; 2]) -> Box<Engine> {
    let tail = irs[0].len().max(irs[1].len());
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
/// A file's impulse response, or the synthesized one again.
type FileIr = Option<[Vec<f32>; 2]>;

fn worker(
    wanted: Arc<Wanted>,
    sample_rate: f32,
    mut built: (f32, f32),
    mut to_audio: Producer<Box<Engine>>,
    mut retired: Consumer<Box<Engine>>,
    files: mpsc::Receiver<FileIr>,
) {
    // A file's response waiting for room in the queue, and whether one rules.
    let mut file: Option<[Vec<f32>; 2]> = None;
    let mut from_file = false;
    while !wanted.stop.load(Ordering::Relaxed) {
        while retired.pop().is_ok() {}
        while let Ok(ir) = files.try_recv() {
            from_file = ir.is_some();
            file = ir;
            built = (f32::NAN, f32::NAN); // rebuild the synthesized one if asked
        }
        let now = (
            f32::from_bits(wanted.decay.load(Ordering::Relaxed)),
            f32::from_bits(wanted.damp.load(Ordering::Relaxed)),
        );
        if to_audio.slots() > 0 {
            // The queue had room a moment ago and only this thread pushes.
            if let Some(ir) = file.take() {
                let _ = to_audio.push(from_ir(ir));
            } else if !from_file && now != built {
                built = now;
                let _ = to_audio.push(build(now.0, now.1, sample_rate));
            }
        }
        std::thread::sleep(POLL);
    }
}

/// The main thread's side of the convolver's file: reads and resamples an
/// impulse response and hands it to the worker.
pub struct IrLoader {
    to_worker: mpsc::Sender<FileIr>,
    sample_rate: f32,
    path: Option<PathBuf>,
    error: Option<String>,
}

impl FileLoader for IrLoader {
    fn load(&mut self, path: &Path) -> Result<(), String> {
        // Remembered even when it fails, so a project keeps pointing at it.
        self.path = Some(path.to_path_buf());
        let result = read_ir(path, self.sample_rate)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|ir| {
                self.to_worker
                    .send(Some(ir))
                    .map_err(|_| "the convolver is gone".to_string())
            });
        self.error = result.as_ref().err().cloned();
        result
    }
    fn file(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["wav"]
    }
    fn takes(&self) -> &'static str {
        "WAV IMPULSE RESPONSE"
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
    /// The worker's file queue, until `file_loader` hands it out.
    files: Option<mpsc::Sender<FileIr>>,
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
            files: None,
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
        let (files, from_main) = mpsc::channel();
        self.files = Some(files);
        std::thread::Builder::new()
            .name("canary-convolver".into())
            .spawn(move || {
                worker(
                    wanted,
                    sample_rate,
                    (decay, damp),
                    to_audio,
                    retired,
                    from_main,
                )
            })
            .expect("spawn the convolver's worker thread");
    }
    fn file_loader(&mut self) -> Option<Box<dyn FileLoader>> {
        let to_worker = self.files.take()?;
        Some(Box::new(IrLoader {
            to_worker,
            sample_rate: self.sr,
            path: None,
            error: None,
        }))
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

    /// A mono 16-bit WAV at 44.1 kHz: a click, silence, a second click at 0.1 s.
    fn write_ir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("canary-ir-{}-{name}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..8_820 {
            w.write_sample(if i == 0 || i == 4_410 { i16::MAX } else { 0 })
                .unwrap();
        }
        w.finalize().unwrap();
        path
    }

    #[test]
    fn a_wav_impulse_response_is_read_resampled_and_used() {
        let path = write_ir("echo");
        let ir = read_ir(&path, SR).unwrap();
        assert_eq!(ir[0], ir[1], "mono goes to both sides");
        assert!((ir[0].len() as i64 - 9_600).abs() <= 1, "{}", ir[0].len());
        // The second click lands 0.1 s in at the new rate.
        let peak = (4_000..ir[0].len())
            .max_by(|&a, &b| ir[0][a].abs().total_cmp(&ir[0][b].abs()))
            .unwrap();
        assert_eq!(peak, 4_800);

        let mut fx = effect("canary.convolver", SR).unwrap();
        fx.set_param(2, 0.0); // no predelay: the echo lands at 4800
        let mut loader = fx.file_loader().unwrap();
        assert_eq!(loader.extensions(), ["wav"]);
        loader.load(&path).unwrap();
        // The worker builds it, then a click comes back with its echo at 4800.
        let mut echo = 0.0f32;
        for _ in 0..200 {
            let mut l = vec![0.0; 9_600];
            l[0] = 1.0;
            let mut r = l.clone();
            fx.reset();
            fx.process(&mut l, &mut r);
            echo = l[4_800].abs();
            if echo > 0.01 {
                break;
            }
            std::thread::sleep(POLL);
        }
        assert!(echo > 0.01, "the file's response is in use");
        assert!(loader.load(Path::new("/nowhere.wav")).is_err());
        assert!(loader.error().is_some());
        std::fs::remove_file(&path).unwrap();
    }

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
