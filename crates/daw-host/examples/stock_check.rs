//! Loads every bundled CLAP plugin (scripts/fetch_plugins.sh puts them in
//! third_party/plugins), plays a note through each instrument and noise through
//! each effect for one second, offline, and prints the output peak.
//! `cargo run -p daw-host --example stock_check`

use std::path::Path;

use daw_core::{MidiEvent, ProcessContext};
use daw_graph::AudioNode;
use daw_host::ClapPlugin;

const SR: f32 = 48_000.0;
const BLOCK: usize = 256;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/plugins");
    let mut bundles: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e} (run scripts/fetch_plugins.sh)", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "clap"))
        .collect();
    bundles.sort();
    let mut failed = 0;
    for path in bundles {
        // SAFETY: Canary's own bundled plugins.
        let entry = unsafe { daw_host::load_bundle(&path)? };
        for info in daw_host::plugins(&entry) {
            let result =
                ClapPlugin::new(&entry, &info.id, SR, BLOCK).map(|(mut plugin, mut node)| {
                    let peak = play(&mut node);
                    plugin.idle();
                    let kind = if plugin.has_audio_input() {
                        "effect"
                    } else {
                        "instrument"
                    };
                    (plugin.params().len(), peak, kind)
                });
            match result {
                Ok((params, peak, kind)) => println!(
                    "ok   {:<28} {:<40} {kind:<10} {params:>4} params, peak {peak:.3}",
                    info.name, info.id
                ),
                Err(e) => {
                    failed += 1;
                    println!("FAIL {:<28} {}: {e}", info.name, info.id);
                }
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} plugins failed").into());
    }
    Ok(())
}

/// One second: a held middle C for instruments, quiet noise for effects.
fn play(node: &mut dyn AudioNode) -> f32 {
    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut midi_out = Vec::with_capacity(64);
    let mut peak = 0.0f32;
    let mut seed = 1u32;
    for block in 0..(SR as usize / BLOCK) {
        let input: Vec<f32> = (0..BLOCK)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 24) as f32 * 0.2 - 0.1
            })
            .collect();
        let note = [MidiEvent {
            sample_offset: 0,
            data: [0x90, 60, 100],
        }];
        let ctx = ProcessContext {
            sample_rate: SR,
            num_samples: BLOCK,
            playhead: (block * BLOCK) as u64,
            playing: true,
            ..Default::default()
        };
        node.process(
            &[&input, &input],
            &mut [&mut l, &mut r],
            if block == 0 { &note } else { &[] },
            &mut midi_out,
            &ctx,
        );
        midi_out.clear();
        peak = l.iter().chain(&r).fold(peak, |m, s| m.max(s.abs()));
    }
    peak
}
