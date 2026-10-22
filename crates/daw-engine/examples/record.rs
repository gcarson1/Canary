//! Records the default input for four seconds while playing MIDI input port 0
//! (if there is one) through the built-in synth, then prints the take.
//! `cargo run -p daw-engine --example record -- take.wav`.
//! macOS asks for microphone access the first time.

use std::sync::Arc;
use std::{env, thread, time::Duration};

use daw_core::{Command, NodeId, TrackParams};
use daw_graph::{ExecutionPlan, Source, SynthNode, TrackNode};

const MAX_BLOCK: usize = 4096;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).unwrap_or_else(|| "take.wav".into());
    let (mut handle, _output, config) = daw_engine::start_output(4096)?;

    // Instrument track: synth (takes live MIDI) into a track, which is the master.
    let mut plan = ExecutionPlan::new(MAX_BLOCK);
    plan.insert(NodeId(0), Box::new(SynthNode::new()));
    let params = Arc::new(TrackParams::new(Arc::default()));
    plan.insert(NodeId(1), Box::new(TrackNode::new(vec![], params)));
    let synth = plan.add(NodeId(0), vec![], 1)?;
    plan.route_midi(synth);
    plan.add(
        NodeId(1),
        vec![vec![Source {
            step: synth,
            channel: 0,
        }]],
        2,
    )?;
    handle.send_plan(Box::new(plan))?;

    let ports = daw_engine::midi_input_ports()?;
    let _midi = match ports.first() {
        Some(name) => {
            println!("MIDI input: {name}");
            Some(daw_engine::connect_midi(handle.midi.take().unwrap(), 0)?)
        }
        None => {
            println!("no MIDI input ports");
            None
        }
    };

    let (mut recorder, _input, input_config) =
        daw_engine::start_input(config.sample_rate, handle.playhead_clock())?;
    println!(
        "recording {} channels at {} Hz",
        input_config.channels, input_config.sample_rate
    );
    handle.commands.push(Command::Play)?;
    recorder.start(&path)?;
    for _ in 0..8 {
        thread::sleep(Duration::from_millis(500));
        handle.collect_garbage();
    }
    let take = recorder.stop()?;
    println!(
        "{}: {} frames from sample {}, {} dropped",
        take.path.display(),
        take.frames,
        take.start_sample,
        take.dropped_frames
    );
    Ok(())
}
