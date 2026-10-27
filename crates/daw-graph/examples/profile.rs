//! Times each built-in node on 60 s of stereo at 48 kHz in 256-frame blocks, to
//! find the hot loops worth vectorizing. Run with `--release` on a quiet machine;
//! each node runs three times and the fastest counts.

use std::sync::Arc;
use std::time::Instant;

use daw_core::{Clip, MidiEvent, ProcessContext, SampleData, TrackParams};
use daw_dsp::effects::{effect, EFFECTS};
use daw_graph::{AudioNode, EffectNode, MasterNode, SamplerData, SamplerNode, TrackNode, Zone};

const SR: f32 = 48_000.0;
const BLOCK: usize = 256;
const SECONDS: usize = 60;

fn time(name: &str, node: &mut dyn AudioNode, inputs: usize, notes: bool) {
    let signal: Vec<f32> = (0..BLOCK).map(|i| 0.5 * (i as f32 * 0.05).sin()).collect();
    let ins: Vec<&[f32]> = (0..inputs).map(|_| &signal[..]).collect();
    let (mut l, mut r) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
    let mut midi_out = Vec::with_capacity(64);
    let blocks = SECONDS * SR as usize / BLOCK;
    let mut elapsed = f64::INFINITY;
    for _ in 0..3 {
        let start = Instant::now();
        for b in 0..blocks {
            let ctx = ProcessContext {
                sample_rate: SR,
                num_samples: BLOCK,
                playhead: (b * BLOCK) as u64,
                playing: true,
            };
            // A new note every ~100 ms keeps the sampler's voices busy.
            let note = [MidiEvent {
                sample_offset: 0,
                data: [0x90, 48 + (b % 24) as u8, 100],
            }];
            let midi: &[MidiEvent] = if notes && b % 19 == 0 { &note } else { &[] };
            node.process(&ins, &mut [&mut l, &mut r], midi, &mut midi_out, &ctx);
        }
        elapsed = elapsed.min(start.elapsed().as_secs_f64());
    }
    let per_sample = elapsed * 1e9 / (blocks * BLOCK) as f64;
    println!(
        "{name:<14} {:>8.1}x real time  {per_sample:>6.1} ns/frame  (sum {:.3})",
        SECONDS as f64 / elapsed,
        l.iter().sum::<f32>()
    );
}

fn main() {
    let audio = Arc::new(
        SampleData::new(
            48_000,
            vec![
                (0..SR as usize * 4)
                    .map(|i| (i as f32 * 0.01).sin())
                    .collect();
                2
            ],
        )
        .unwrap(),
    );
    let params = Arc::new(TrackParams::new(Arc::default()));
    let clips = (0..8)
        .map(|i| Clip::new(i * 48_000 * 4, audio.clone()))
        .collect();
    time("track", &mut TrackNode::new(clips, params), 0, false);
    for (id, _) in EFFECTS {
        let (mut node, _) = EffectNode::new(effect(id, SR).unwrap(), SR, BLOCK);
        time(id.trim_start_matches("canary."), &mut node, 2, false);
    }
    let (mut sampler, _) = SamplerNode::new();
    let zone = Zone {
        sample: audio.clone(),
        start: 0,
        end: audio.frames(),
        keys: (0, 127),
        velocities: (1, 127),
        root: 60,
        cell: 0,
        one_shot: true,
    };
    sampler.exchange(&mut SamplerData { zones: vec![zone] });
    time("sampler", &mut sampler, 0, true);
    let (mut master, _) = MasterNode::new(SR);
    time("master", &mut master, 2, false);
}
