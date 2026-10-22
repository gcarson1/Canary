//! Loads a CLAP plugin and plays MIDI input port 0 through it for ten seconds.
//! `cargo run -p daw-host --example clap -- "/Library/Audio/Plug-Ins/CLAP/Surge XT.clap"`
//! With no plugin id, the bundle's first plugin is used.

use std::sync::Arc;
use std::{env, path::Path, thread, time::Duration};

use daw_core::{NodeId, TrackParams};
use daw_graph::{ExecutionPlan, Source, TrackNode};
use daw_host::ClapPlugin;

const MAX_BLOCK: usize = 4096;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args.next().ok_or("usage: clap <bundle.clap> [plugin id]")?;
    // SAFETY: the user chose this bundle.
    let entry = unsafe { daw_host::load_bundle(Path::new(&path))? };
    let found = daw_host::plugins(&entry);
    for p in &found {
        println!("{} ({}) by {}", p.name, p.id, p.vendor);
    }
    let id = match args.next() {
        Some(id) => id,
        None => found.first().ok_or("the bundle has no plugins")?.id.clone(),
    };

    let (mut handle, _stream, config) = daw_engine::start_output(4096)?;
    let (mut plugin, node) = ClapPlugin::new(&entry, &id, config.sample_rate as f32, MAX_BLOCK)?;
    println!(
        "{} params, {} samples latency",
        plugin.params().len(),
        plugin.latency_samples()
    );

    // Plugin (takes live MIDI) into a track, which is the master.
    let mut plan = ExecutionPlan::new(MAX_BLOCK);
    plan.insert(NodeId(0), Box::new(node));
    let params = Arc::new(TrackParams::new(Arc::default()));
    plan.insert(NodeId(1), Box::new(TrackNode::new(vec![], params)));
    let step = plan.add(NodeId(0), vec![], 2)?;
    plan.route_midi(step);
    let from = |channel| vec![Source { step, channel }];
    plan.add(NodeId(1), vec![from(0), from(1)], 2)?;
    handle.send_plan(Box::new(plan))?;

    let _midi = match daw_engine::midi_input_ports()?.first() {
        Some(name) => {
            println!("MIDI input: {name}");
            Some(daw_engine::connect_midi(handle.midi.take().unwrap(), 0)?)
        }
        None => {
            println!("no MIDI input ports; effects still process silence");
            None
        }
    };
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(100));
        handle.collect_garbage();
        plugin.idle();
    }

    // Take the plugin out of the graph and free it before the plugin handle goes.
    let mut empty = ExecutionPlan::new(MAX_BLOCK);
    empty.remove(NodeId(0));
    empty.remove(NodeId(1));
    handle.send_plan(Box::new(empty))?;
    thread::sleep(Duration::from_millis(200));
    handle.collect_garbage();
    Ok(())
}
