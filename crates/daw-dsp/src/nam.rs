//! Neural Amp Modeler: runs `.nam` amp and pedal captures (WaveNet models, the
//! format of the standard, lite, feather and nano trainer presets). A Rust port
//! of the inference in Steven Atkinson's NeuralAmpModelerCore (MIT; see
//! THIRD_PARTY_NOTICES.md), sample by sample with no allocation. Canary ships
//! no captures: drop a `.nam` file from the browser onto the module.
//!
//! A model is a chain of layer arrays. Each array re-channels its input, runs
//! dilated causal convolutions (plus the dry input mixed in as a condition)
//! through an activation with residual connections, and sums every layer's
//! activations into a head that feeds the next array.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};
use serde::Deserialize;

use crate::effects::{Effect, FileLoader, ParamSpec};
use crate::filter::db_to_gain;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Activation {
    Tanh,
    Hardtanh,
    Relu,
    LeakyRelu,
    Sigmoid,
    Silu,
}

impl Activation {
    fn parse(name: &str) -> Result<Self, String> {
        Ok(match name {
            "Tanh" | "Fasttanh" => Self::Tanh,
            "Hardtanh" => Self::Hardtanh,
            "ReLU" => Self::Relu,
            "LeakyReLU" => Self::LeakyRelu,
            "Sigmoid" => Self::Sigmoid,
            "SiLU" => Self::Silu,
            other => return Err(format!("unsupported activation {other}")),
        })
    }

    #[inline]
    fn apply(self, x: f32) -> f32 {
        match self {
            // NAM's own fast tanh, as its plugin runs by default.
            Self::Tanh => fast_tanh(x),
            Self::Hardtanh => x.clamp(-1.0, 1.0),
            Self::Relu => x.max(0.0),
            Self::LeakyRelu => {
                if x > 0.0 {
                    x
                } else {
                    0.01 * x
                }
            }
            Self::Sigmoid => 1.0 / (1.0 + (-x).exp()),
            Self::Silu => x / (1.0 + (-x).exp()),
        }
    }
}

/// NeuralAmpModelerCore's rational tanh approximation.
#[inline]
fn fast_tanh(x: f32) -> f32 {
    let ax = x.abs();
    let x2 = x * x;
    x * (2.455_507_5 + 2.455_507_5 * ax + (0.893_229_85 + 0.821_226_7 * ax) * x2)
        / (2.445_066_3 + (2.445_066_3 + x2) * (x + 0.814_642_7 * x * ax).abs())
}

/// One dilated layer: `z = act(conv(x) + mixin * condition)`, then
/// `x + layer1x1(z)` to the next layer and `z` to the head.
struct Layer {
    dilation: usize,
    kernel: usize,
    /// `conv[(k * ch + in) * ch + out]` (a column per input, so the inner loop
    /// is a vectorizable multiply-add); tap `k` reads `dilation * (K-1-k)` back.
    conv: Box<[f32]>,
    conv_bias: Box<[f32]>,
    mixin: Box<[f32]>,
    /// `one[in * ch + out]`.
    one: Box<[f32]>,
    one_bias: Box<[f32]>,
    /// The last inputs, `ring[t * ch + c]`, a power of two of them.
    ring: Box<[f32]>,
    mask: usize,
}

impl Layer {
    /// One sample through the layer for `CH` channels: `x` becomes the next
    /// layer's input and `z` is added to the head sum `acc`.
    // Index loops over fixed widths are what unrolls and vectorizes here.
    #[allow(clippy::needless_range_loop)]
    #[inline]
    fn step<const CH: usize>(
        &mut self,
        x: &mut [f32],
        z: &mut [f32],
        acc: &mut [f32],
        input: f32,
        pos: usize,
        act: Activation,
    ) {
        let (Ok(x), Ok(z), Ok(acc)) = (
            <&mut [f32; CH]>::try_from(x),
            <&mut [f32; CH]>::try_from(z),
            <&mut [f32; CH]>::try_from(acc),
        ) else {
            return;
        };
        let slot = pos & self.mask;
        let (ring, _) = self.ring.as_chunks_mut::<CH>();
        ring[slot] = *x;
        for o in 0..CH {
            z[o] = self.conv_bias[o] + self.mixin[o] * input;
        }
        let (cols, _) = self.conv.as_chunks::<CH>();
        for t in 0..self.kernel {
            let at = pos.wrapping_sub(self.dilation * (self.kernel - 1 - t)) & self.mask;
            let hist = &ring[at];
            for (col, &h) in cols[t * CH..(t + 1) * CH].iter().zip(hist) {
                for o in 0..CH {
                    z[o] += col[o] * h;
                }
            }
        }
        for o in 0..CH {
            z[o] = act.apply(z[o]);
            acc[o] += z[o];
            x[o] += self.one_bias[o];
        }
        let (cols, _) = self.one.as_chunks::<CH>();
        for (col, &v) in cols.iter().zip(z.iter()) {
            for o in 0..CH {
                x[o] += col[o] * v;
            }
        }
    }

    /// `step` for any width.
    fn step_any(
        &mut self,
        x: &mut [f32],
        z: &mut [f32],
        acc: &mut [f32],
        input: f32,
        pos: usize,
        act: Activation,
    ) {
        let ch = x.len();
        let slot = pos & self.mask;
        self.ring[slot * ch..(slot + 1) * ch].copy_from_slice(x);
        for (o, zo) in z.iter_mut().enumerate() {
            *zo = self.conv_bias[o] + self.mixin[o] * input;
        }
        for t in 0..self.kernel {
            let at = pos.wrapping_sub(self.dilation * (self.kernel - 1 - t)) & self.mask;
            let hist = &self.ring[at * ch..(at + 1) * ch];
            let taps = &self.conv[t * ch * ch..(t + 1) * ch * ch];
            for (col, &h) in taps.chunks_exact(ch).zip(hist) {
                for (zo, w) in z.iter_mut().zip(col) {
                    *zo += w * h;
                }
            }
        }
        for ((zo, a), (xo, b)) in z
            .iter_mut()
            .zip(acc.iter_mut())
            .zip(x.iter_mut().zip(self.one_bias.iter()))
        {
            *zo = act.apply(*zo);
            *a += *zo;
            *xo += b;
        }
        for (col, &v) in self.one.chunks_exact(ch).zip(z.iter()) {
            for (xo, w) in x.iter_mut().zip(col) {
                *xo += w * v;
            }
        }
    }
}

struct LayerArray {
    input_size: usize,
    channels: usize,
    head_size: usize,
    activation: Activation,
    /// `rechannel[c * input_size + j]`, no bias.
    rechannel: Box<[f32]>,
    layers: Vec<Layer>,
    /// `head[h * channels + c]`, kernel 1.
    head: Box<[f32]>,
    head_bias: Box<[f32]>,
}

/// A loaded capture, ready to run.
pub struct Model {
    arrays: Vec<LayerArray>,
    head_scale: f32,
    /// The rate it was trained at, if the file says.
    pub sample_rate: Option<f32>,
    /// Scratch: the layers' input, their activations, the head sum, the
    /// array's output and head output.
    x: Box<[f32]>,
    z: Box<[f32]>,
    head_acc: Box<[f32]>,
    next: Box<[f32]>,
    head_out: Box<[f32]>,
    pos: usize,
}

#[derive(Deserialize)]
struct File {
    architecture: String,
    config: Config,
    weights: Vec<f32>,
    #[serde(default)]
    sample_rate: Option<f32>,
}

#[derive(Deserialize)]
struct Config {
    layers: Vec<ArrayConfig>,
    #[serde(default)]
    head: Option<serde_json::Value>,
    head_scale: f32,
}

#[derive(Deserialize)]
struct ArrayConfig {
    input_size: usize,
    condition_size: usize,
    channels: usize,
    #[serde(default)]
    bottleneck: Option<usize>,
    #[serde(default)]
    head_size: Option<usize>,
    #[serde(default)]
    head_bias: Option<bool>,
    #[serde(default)]
    head: Option<HeadConfig>,
    kernel_size: usize,
    dilations: Vec<usize>,
    activation: serde_json::Value,
    #[serde(default)]
    gated: Option<bool>,
}

#[derive(Deserialize)]
struct HeadConfig {
    out_channels: usize,
    kernel_size: usize,
    bias: bool,
}

/// `rows x cols` row-major to column-major.
fn transpose(m: &[f32], rows: usize, cols: usize) -> Box<[f32]> {
    (0..rows * cols)
        .map(|i| m[(i % rows) * cols + i / rows])
        .collect()
}

/// Reads weights in the order the trainer wrote them.
struct Weights<'a>(std::slice::Iter<'a, f32>);

impl Weights<'_> {
    fn take(&mut self, n: usize) -> Result<Box<[f32]>, String> {
        let out: Box<[f32]> = self.0.by_ref().take(n).copied().collect();
        if out.len() == n {
            Ok(out)
        } else {
            Err("the file has fewer weights than its layers need".into())
        }
    }
}

/// The receptive field the per-layer rings must hold, as a power of two.
fn ring_len(dilation: usize, kernel: usize) -> usize {
    (dilation * (kernel - 1) + 1).next_power_of_two()
}

impl Model {
    /// Parses a `.nam` file's JSON. Main thread: allocates.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let file: File = serde_json::from_str(text).map_err(|e| format!("not a .nam file: {e}"))?;
        if file.architecture != "WaveNet" {
            return Err(format!(
                "{} models are not supported yet (WaveNet only)",
                file.architecture
            ));
        }
        if file.config.head.as_ref().is_some_and(|h| !h.is_null()) {
            return Err("models with a post-stack head are not supported yet".into());
        }
        let mut w = Weights(file.weights.iter());
        let mut arrays = Vec::new();
        for (i, a) in file.config.layers.iter().enumerate() {
            let ch = a.channels;
            if a.gated == Some(true) {
                return Err("gated WaveNet models are not supported yet".into());
            }
            if a.bottleneck.is_some_and(|b| b != ch) {
                return Err("models with a bottleneck are not supported yet".into());
            }
            if a.condition_size != 1 || a.kernel_size == 0 || ch == 0 || a.dilations.is_empty() {
                return Err(format!("layer array {i} has an unsupported shape"));
            }
            let activation = match &a.activation {
                serde_json::Value::String(s) => Activation::parse(s)?,
                serde_json::Value::Object(o) => {
                    Activation::parse(o.get("type").and_then(|t| t.as_str()).unwrap_or("?"))?
                }
                _ => return Err("unsupported activation".into()),
            };
            let (head_size, head_bias) = match (&a.head, a.head_size) {
                (Some(h), _) if h.kernel_size == 1 => (h.out_channels, h.bias),
                (Some(_), _) => return Err("heads wider than one tap are not supported yet".into()),
                (None, Some(n)) => (n, a.head_bias.unwrap_or(false)),
                (None, None) => return Err(format!("layer array {i} has no head")),
            };
            if let Some(prev) = arrays.last() {
                let prev: &LayerArray = prev;
                if prev.channels != a.input_size || prev.head_size != ch {
                    return Err(format!("layer array {i} does not fit the one before it"));
                }
            } else if a.input_size != 1 {
                return Err("the first layer array must take one input".into());
            }
            let rechannel = w.take(ch * a.input_size)?;
            let mut layers = Vec::new();
            for &dilation in &a.dilations {
                if dilation == 0 {
                    return Err("a dilation of 0".into());
                }
                let k = a.kernel_size;
                // The file stores conv weights out, in, tap; keep them tap-major.
                let raw = w.take(ch * ch * k)?;
                let mut conv = vec![0.0; ch * ch * k].into_boxed_slice();
                for o in 0..ch {
                    for j in 0..ch {
                        for t in 0..k {
                            conv[(t * ch + j) * ch + o] = raw[(o * ch + j) * k + t];
                        }
                    }
                }
                let ring = ring_len(dilation, k);
                layers.push(Layer {
                    dilation,
                    kernel: k,
                    conv,
                    conv_bias: w.take(ch)?,
                    mixin: w.take(ch)?,
                    one: transpose(&w.take(ch * ch)?, ch, ch),
                    one_bias: w.take(ch)?,
                    ring: vec![0.0; ring * ch].into_boxed_slice(),
                    mask: ring - 1,
                });
            }
            let head = w.take(head_size * ch)?;
            let head_bias = if head_bias {
                w.take(head_size)?
            } else {
                vec![0.0; head_size].into_boxed_slice()
            };
            arrays.push(LayerArray {
                input_size: a.input_size,
                channels: ch,
                head_size,
                activation,
                rechannel,
                layers,
                head,
                head_bias,
            });
        }
        let Some(last) = arrays.last() else {
            return Err("the model has no layers".into());
        };
        if last.head_size != 1 {
            return Err("the model must end in one output".into());
        }
        // The trainer writes head_scale as the last weight too.
        if w.0.next().is_none() {
            return Err("the file has fewer weights than its layers need".into());
        }
        if w.0.next().is_some() {
            return Err("the file has more weights than its layers use".into());
        }
        let widest = arrays
            .iter()
            .map(|a| a.channels.max(a.head_size))
            .max()
            .unwrap_or(1);
        let scratch = || vec![0.0; widest].into_boxed_slice();
        let mut model = Self {
            arrays,
            head_scale: file.config.head_scale,
            sample_rate: file.sample_rate,
            x: scratch(),
            z: scratch(),
            head_acc: scratch(),
            next: scratch(),
            head_out: scratch(),
            pos: 0,
        };
        model.prewarm();
        Ok(model)
    }

    /// Runs silence through the receptive field, so the first real sample
    /// starts from the model's settled state (as NAM does).
    fn prewarm(&mut self) {
        let field: usize = self
            .arrays
            .iter()
            .flat_map(|a| &a.layers)
            .map(|l| l.dilation * (l.kernel - 1))
            .sum::<usize>()
            + 1;
        for _ in 0..field {
            self.tick(0.0);
        }
    }

    /// One sample in, one out. No allocation.
    pub fn tick(&mut self, input: f32) -> f32 {
        let pos = self.pos;
        self.pos = self.pos.wrapping_add(1);
        // The previous array's head output starts this one's head sum.
        let mut prev_head = 0usize;
        for (ai, a) in self.arrays.iter_mut().enumerate() {
            let ch = a.channels;
            // Re-channel this array's input into `x`.
            let source: &[f32] = if ai == 0 {
                std::slice::from_ref(&input)
            } else {
                &self.next[..a.input_size]
            };
            for c in 0..ch {
                let row = &a.rechannel[c * a.input_size..(c + 1) * a.input_size];
                self.x[c] = row.iter().zip(source).map(|(w, s)| w * s).sum();
            }
            if ai == 0 {
                self.head_acc[..ch].fill(0.0);
            } else {
                self.head_acc[..ch].copy_from_slice(&self.head_out[..prev_head]);
            }
            let act = a.activation;
            for layer in &mut a.layers {
                let (x, z, acc) = (
                    &mut self.x[..ch],
                    &mut self.z[..ch],
                    &mut self.head_acc[..ch],
                );
                // Fixed widths let the compiler keep a layer in registers.
                match ch {
                    16 => layer.step::<16>(x, z, acc, input, pos, act),
                    8 => layer.step::<8>(x, z, acc, input, pos, act),
                    4 => layer.step::<4>(x, z, acc, input, pos, act),
                    2 => layer.step::<2>(x, z, acc, input, pos, act),
                    _ => layer.step_any(x, z, acc, input, pos, act),
                }
            }
            for h in 0..a.head_size {
                let row = &a.head[h * ch..(h + 1) * ch];
                self.head_out[h] = a.head_bias[h]
                    + row
                        .iter()
                        .zip(&self.head_acc[..ch])
                        .map(|(w, v)| w * v)
                        .sum::<f32>();
            }
            prev_head = a.head_size;
            self.next[..ch].copy_from_slice(&self.x[..ch]);
        }
        let y = self.head_out[0] * self.head_scale;
        if y.is_finite() {
            y
        } else {
            0.0
        }
    }

    /// Clears the history, then settles again.
    fn clear(&mut self) {
        for l in self.arrays.iter_mut().flat_map(|a| &mut a.layers) {
            l.ring.fill(0.0);
        }
        self.prewarm();
    }
}

const PARAMS: [ParamSpec; 2] = [
    ParamSpec {
        name: "INPUT",
        min: -24.0,
        max: 24.0,
        default: 0.0,
        log: false,
    },
    ParamSpec {
        name: "OUTPUT",
        min: -24.0,
        max: 24.0,
        default: 0.0,
        log: false,
    },
];

/// The amp: runs the loaded capture on the mid signal (amps are mono) and
/// sends it to both sides. Silent until a capture is loaded.
pub struct NamAmp {
    input: f32,
    output: f32,
    model: Option<Box<Model>>,
    incoming: Option<Consumer<Box<Model>>>,
    retired: Option<Producer<Box<Model>>>,
    loader: Option<NamLoader>,
}

impl Default for NamAmp {
    fn default() -> Self {
        let (to_audio, incoming) = RingBuffer::new(2);
        let (retired, from_audio) = RingBuffer::new(4);
        Self {
            input: 1.0,
            output: 1.0,
            model: None,
            incoming: Some(incoming),
            retired: Some(retired),
            loader: Some(NamLoader {
                to_audio,
                from_audio,
                path: None,
                error: None,
                trained_rate: None,
                _keep: Arc::new(()),
            }),
        }
    }
}

/// The main thread's side: parses captures and hands them over.
pub struct NamLoader {
    to_audio: Producer<Box<Model>>,
    from_audio: Consumer<Box<Model>>,
    path: Option<PathBuf>,
    error: Option<String>,
    trained_rate: Option<f32>,
    _keep: Arc<()>,
}

impl FileLoader for NamLoader {
    fn load(&mut self, path: &Path) -> Result<(), String> {
        // Models the audio thread let go of are dropped here.
        while self.from_audio.pop().is_ok() {}
        // Remembered even when it fails, so a project keeps pointing at it.
        self.path = Some(path.to_path_buf());
        let result = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))
            .and_then(|text| Model::from_json(&text))
            .and_then(|model| {
                self.trained_rate = model.sample_rate;
                self.to_audio
                    .push(Box::new(model))
                    .map_err(|_| "busy: try again in a moment".to_string())
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
        &["nam"]
    }
}

impl Effect for NamAmp {
    fn params(&self) -> &'static [ParamSpec] {
        &PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.input = db_to_gain(value),
            1 => self.output = db_to_gain(value),
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, _sample_rate: f32) {}
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if let Some(new) = self.incoming.as_mut().and_then(|c| c.pop().ok()) {
            if let (Some(old), Some(back)) = (self.model.replace(new), self.retired.as_mut()) {
                // Room is kept: the loader drains this before every load.
                let _ = back.push(old);
            }
        }
        let Some(model) = &mut self.model else {
            left.fill(0.0);
            right.fill(0.0);
            return;
        };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let y = model.tick(0.5 * (*l + *r) * self.input) * self.output;
            (*l, *r) = (y, y);
        }
    }
    fn reset(&mut self) {
        if let Some(m) = &mut self.model {
            m.clear();
        }
    }
    fn file_loader(&mut self) -> Option<Box<dyn FileLoader>> {
        self.loader
            .take()
            .map(|l| Box::new(l) as Box<dyn FileLoader>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-array model in the trainer's format, every weight set by `f`.
    fn json(f: impl Fn(usize) -> f32) -> String {
        // Array 1: input 1 -> 2 channels, kernel 2, dilations [1, 2], head 2.
        // Array 2: input 2 -> 2 channels, kernel 2, dilations [1], head 1, bias.
        let count = (2)
            + 2 * (2 * 2 * 2 + 2 + 2 + 4 + 2)
            + (2 * 2)
            + (2 * 2)
            + (2 * 2 * 2 + 2 + 2 + 4 + 2)
            + (2 + 1)
            + 1;
        let weights: Vec<String> = (0..count).map(|i| format!("{}", f(i))).collect();
        format!(
            r#"{{"version":"0.5.4","architecture":"WaveNet","sample_rate":48000,
            "config":{{"layers":[
              {{"input_size":1,"condition_size":1,"channels":2,"head_size":2,"kernel_size":2,
                "dilations":[1,2],"activation":"Tanh","gated":false,"head_bias":false}},
              {{"input_size":2,"condition_size":1,"channels":2,"head_size":1,"kernel_size":2,
                "dilations":[1],"activation":"Tanh","gated":false,"head_bias":true}}],
              "head":null,"head_scale":0.5}},
            "weights":[{}]}}"#,
            weights.join(",")
        )
    }

    #[test]
    fn loads_the_trainer_format_and_runs() {
        let mut m = Model::from_json(&json(|i| ((i * 37 % 11) as f32 - 5.0) * 0.07)).unwrap();
        let out: Vec<f32> = (0..256)
            .map(|i| m.tick((i as f32 * 0.1).sin() * 0.5))
            .collect();
        assert!(out.iter().all(|y| y.is_finite()));
        assert!(out.iter().any(|y| y.abs() > 1e-4), "it makes sound");
        // Too few weights, or another architecture, are refused with a reason.
        let short = json(|_| 0.1).replace(",0.1]", "]");
        assert!(Model::from_json(&short).is_err());
        let lstm = json(|_| 0.1).replace("\"WaveNet\"", "\"LSTM\"");
        assert!(Model::from_json(&lstm)
            .err()
            .is_some_and(|e| e.contains("LSTM")));
    }

    #[test]
    fn a_hand_built_model_computes_what_the_layout_says() {
        // All conv, mixin and 1x1 weights zero except: rechannel 1, head 1,
        // head bias 0. Then each layer's z = tanh(0) = 0 and the output is the
        // head of the second array: its head sum starts from the first array's
        // head output (all zero) plus zeros, times head weights, plus bias.
        // With the final head bias at 0.25 and head_scale 0.5: 0.125.
        let text = json(|i| match i {
            // The final array's head bias: count - 2 (head_scale is last).
            _ if i == 2 + 36 + 4 + 4 + 18 + 2 => 0.25,
            _ => 0.0,
        });
        let mut m = Model::from_json(&text).unwrap();
        assert!((m.tick(0.7) - 0.125).abs() < 1e-6);
    }

    #[test]
    fn the_amp_is_silent_until_a_capture_arrives() {
        let mut amp = NamAmp::default();
        let mut loader = amp.file_loader().unwrap();
        let (mut l, mut r) = (vec![0.5; 64], vec![0.5; 64]);
        amp.process(&mut l, &mut r);
        assert!(l.iter().all(|&x| x == 0.0));
        let path = std::env::temp_dir().join(format!("canary-nam-{}.nam", std::process::id()));
        std::fs::write(&path, json(|i| ((i % 7) as f32 - 3.0) * 0.1)).unwrap();
        loader.load(&path).unwrap();
        amp.process(&mut l, &mut r);
        assert!(l.iter().any(|&x| x != 0.0) && l == r);
        assert_eq!(loader.file(), Some(path.as_path()));
        std::fs::remove_file(&path).unwrap();
        assert!(loader.load(&path).is_err() && loader.error().is_some());
    }
}
