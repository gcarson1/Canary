//! Opens the default output device, plays for two seconds and prints the playhead.
//! Run with `cargo run -p daw-engine --example play`.

use std::{thread, time::Duration};

use daw_core::{Command, Telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut handle, _stream, config) = daw_engine::start_output(4096)?;
    println!("{} Hz, {} channels", config.sample_rate, config.channels);

    handle.commands.push(Command::Play)?;
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(500));
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
