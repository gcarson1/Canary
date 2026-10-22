//! Opens the default output device and plays for two seconds, printing the playhead.
//! Pass an audio file to hear it on a track:
//! `cargo run -p daw-engine --example play -- path/to/file.wav`.

use std::sync::Arc;
use std::{env, path::Path, thread, time::Duration};

use daw_core::{Clip, Command, NodeId, Telemetry, TrackParams};
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
        let (track, master) = (NodeId(0), NodeId(1));
        let mut plan = ExecutionPlan::new(MAX_BLOCK);
        let clips = vec![Clip {
            start_sample: 0,
            audio,
        }];
        plan.insert(track, Box::new(TrackNode::new(clips, params)));
        plan.insert(master, Box::new(GainNode { gain: 1.0 }));
        let track_step = plan.add(track, vec![], 2)?;
        let from = |channel| {
            vec![Source {
                step: track_step,
                channel,
            }]
        };
        // The last step added is what the device hears.
        plan.add(master, vec![from(0), from(1)], 2)?;
        handle.send_plan(Box::new(plan))?;
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
