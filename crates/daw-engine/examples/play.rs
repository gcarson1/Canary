//! Opens the default output device and plays for two seconds, printing the playhead.
//! Pass an audio file to hear it on a track:
//! `cargo run -p daw-engine --example play -- path/to/file.wav`.

use std::sync::Arc;
use std::{env, path::Path, thread, time::Duration};

use daw_core::{Clip, Command, Telemetry, TrackParams};
use daw_graph::{ExecutionPlan, GainNode, Source, TrackNode};

/// Frames per plan buffer; larger device callbacks are rendered in chunks.
const MAX_BLOCK: usize = 4096;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut handle, _stream, config) = daw_engine::start_output(4096)?;
    println!("{} Hz, {} channels", config.sample_rate, config.channels);

    if let Some(path) = env::args().nth(1) {
        let audio = daw_engine::load_audio(Path::new(&path))?;
        if audio.sample_rate() != config.sample_rate {
            println!(
                "file is {} Hz; it plays at the wrong pitch until resampling lands",
                audio.sample_rate()
            );
        }
        let params = Arc::new(TrackParams::new(Arc::default()));
        let mut plan = ExecutionPlan::new(MAX_BLOCK);
        let track = plan.add(
            Box::new(TrackNode::new(
                vec![Clip {
                    start_sample: 0,
                    audio,
                }],
                params,
            )),
            vec![],
            2,
        )?;
        let from = |channel| {
            vec![Source {
                step: track,
                channel,
            }]
        };
        // Master bus: the last node added is what the device hears.
        plan.add(Box::new(GainNode { gain: 1.0 }), vec![from(0), from(1)], 2)?;
        if handle.plans.push(Box::new(plan)).is_err() {
            return Err("plan queue full".into());
        }
    }

    handle.commands.push(Command::Play)?;
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(500));
        handle.collect_garbage();
        let mut last = None;
        while let Ok(t) = handle.telemetry.pop() {
            last = Some(t);
        }
        if let Some(Telemetry::Playhead { sample, playing }) = last {
            println!("playhead {sample} samples, playing {playing}");
        }
    }
    Ok(())
}
