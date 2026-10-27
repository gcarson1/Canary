# Architecture: Modular Expansion Patterns

The 4-thread topology is the foundational standard used by modern commercial DAWs (like Reaper, Bitwig, and Ableton Live), and it provides a solid base for modularity. However, to prevent the codebase from turning into a tangled monolith as it scales from a basic 4-track player to a full production environment (supporting complex routing, sidechaining, multi-core DSP, and unstable third-party plugins), the design needs **five specific modular expansion patterns** right now.

---

## 1. Shifting from a "Track List" to a Directed Acyclic Graph (DAG)

In Phase 2, starting with a flat `Vec<Track>` that sums into a Master Bus is fine, but it breaks down the moment you want **aux sends/returns, group buses, sidechain compression, or folder tracks**.

* **The Modular Solution:** Represent *everything*—tracks, buses, plugins, hardware inputs, and master outputs—as a uniform trait: `AudioNode`.
* **How it scales:** The audio engine doesn't care what a node does; it only traverses a compiled **execution schedule** (a topologically sorted flat array of node IDs and buffer indices).
* **Lock-Free Graph Swapping:** Nodes live in a persistent `NodeArena` (`MAX_NODES` = 4096 slots, indexed by `NodeId`) owned by the **Audio Thread**, so node state (smoothers, later delay lines and plugins) survives graph edits. An `ExecutionPlan` holds the schedule plus all its pre-allocated scratch buffers, and the node changes to make when it goes live:
    * **Schedule:** steps by `NodeId`, acyclic by construction (a step may only read from earlier steps; the last step is the master output).
    * **Node changes:** `insert`, `remove`, and `update` (new data swapped into a live node in place via `AudioNode::exchange`; the old data stays in the plan).
    * **Main Thread:** `LiveNodes` mirrors the arena. `EngineHandle::send_plan` validates the plan against it, then queues the `Box<ExecutionPlan>` through the `rtrb` command queue.
    * **Audio Thread:** at the top of a block, and only if the garbage queue has room, it calls `NodeArena::apply` (removals, inserts, updates), moves removed nodes into the retiring plan, and pushes the old plan to the garbage queue. `EngineHandle::collect_garbage` frees it on the Main Thread. Nothing is allocated or dropped on the Audio Thread.

```rust
pub trait AudioNode: Send {
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        midi_out: &mut Vec<MidiEvent>, // Pre-allocated capacity!
        context: &ProcessContext,
    );
    fn latency_samples(&self) -> u32 { 0 }
    fn reset(&mut self);
    fn exchange(&mut self, _data: &mut (dyn Any + Send)) {} // Audio thread: no alloc, no drop
}
```

Live MIDI reaches only steps marked with `ExecutionPlan::route_midi`; every other node gets an empty `midi_in`. `TrackNode` sums its inputs before gain/pan, so an instrument node feeds a track through its inputs.

### Buffer Contract

Scratch buffers are allocated to `max_block_size` and reused across the schedule, so they always hold stale audio.

* **Compiler/Executor:** slices every input and output to exactly `context.num_samples` frames before `process()`, pre-zeroes summing buses and any buffer passed to an untrusted third-party plugin, and matches channel layouts (mono into stereo passes the same buffer twice, or inserts an adapter node).
* **Node:** every output channel it does not actively write is filled with `0.0`.

---

## 2. Out-of-Process Plugin Sandboxing (Crucial for macOS)

Third-party AU and VST3/CLAP plugins are the #1 cause of DAW crashes. Furthermore, on Apple Silicon Macs, users frequently run older `x86_64` plugins inside an `arm64` native DAW.

* **The Modular Solution:** Design `PluginNode` so it can operate either **In-Process** (direct function pointer call) or **Out-of-Process** (via macOS XPC / POSIX shared memory `shm_open` + Mach semaphores).
* **Future Growth:** By abstracting plugin hosting behind the `AudioNode` trait now, you can start by running plugins in-process and later move them into separate worker processes (like Bitwig's sandboxing) without touching a single line of the core Audio Engine or UI code. If a buggy synth segfaults, only the child process dies, and the DAW stays open.
* **In-Process Split (CLAP, `daw-host`):** `ClapPlugin` is not `Send` and stays on the Main Thread (params, state, callbacks). Its `PluginNode` runs in the arena and gets pre-zeroed buffers. The node is dropped on the Main Thread through garbage collection, so it must be removed from the graph and collected before its `ClapPlugin` is dropped, or the plugin leaks.
* **Formats (`daw-host`):** CLAP (`clack`), Audio Units (AU v2 via `objc2-audio-toolbox`) and VST3 (`vst3` crate) each pair a main-thread handle with an `AudioNode`. `daw_host::Catalog` scans all three and instantiates any of them behind the `Hosted` trait. Plugins with more than one audio bus/port per direction are rejected for now.
* **Built-in effects (`Format::Builtin`):** compressor, saturator, dynamic EQ (bell or threshold-triggered low cut) and de-esser are pure DSP in `daw_dsp::effects`. `daw_graph::EffectNode` wraps any of them and adds MIX (dry/wet), MATCH (automatic level matching for A/B, off by default: the wet signal is held at the dry signal's loudness over a 1 s window, within ±24 dB, frozen below -70 dBFS) and DELTA (output wet minus dry). Parameters are atomics shared with the main thread; level knobs glide over 20 ms; non-finite input is zeroed. State saves as `NAME=value` lines. Racks reorder by dragging a module's title (`Action::MoveDevice`).
* **Sampler (`canary.sampler`, an instrument):** `daw_graph::SamplerNode` plays `Zone`s (a sample region, key and velocity range, root key, round-robin cell, one-shot flag) with 32 voices, cubic interpolation, attack/release and a retrigger choke. The main-thread `daw_host::Sampler` owns the layers (files by key zone and velocity zone; layers in the same cell take turns) and slicing (`daw_dsp::slice`: transients, grid, manual markers, snapped to zero crossings). It rebuilds the zone list on any shaping change and sends it over an rtrb queue; the node swaps it in at block start and sends the old list back to be dropped. MODE 2 maps slice *n* to key ROOT + *n* (the pads). The app loads layer files with the same loader as clips and hands the audio over (`Devices::feed_samplers`). State: param lines, `LAYER=key,velocity,path`, `MARKER=fraction`.
* **Master and metering (`daw_graph::MasterNode`, node 0):** inserted once and never rebuilt, so meters keep their history. It meters the mix only: BS.1770-4 loudness (`daw_dsp::Loudness`: K-weighting at any rate, momentary 400 ms, short-term 3 s, integrated with absolute and relative gates over a fixed histogram), 4x oversampled true peak (`daw_dsp::TruePeak`, hold until reset) and correlation. Readings and controls go through the shared `Monitor` atomics. It outputs stereo, mid or side, and A/B-switches (10 ms crossfade) to the project's reference track, a `TrackNode` at node 2047 feeding the master's inputs 2-3. The reference is gain-matched by its offline integrated loudness against the mix's integrated (else short-term) loudness, within ±24 dB. Delta monitoring is per effect (DELTA).
* **In the app:** plugin nodes persist across plan rebuilds (ids from 2048 up). A removed plugin's handle is dropped only after its node has left the engine.

---

## 3. Transitioning from Single-Core to Multi-Core DSP

Initially, the CoreAudio callback runs on a single high-priority thread. Once a user loads 50 tracks with heavy reverb and virtual instruments, a single CPU core will bottleneck even when the other 9 cores on an M-series chip are idle.

* **Why the DAG matters here:** Because the engine is a Directed Acyclic Graph (DAG), independent branches of the graph (e.g., Track 1 through Track 32 before they hit the Master Bus) have zero data dependencies on each other during a single buffer tick.
* **The Upgrade Path:** Later on, you can replace the single-threaded topological loop inside the Audio Engine with a real-time work-stealing thread pool (anchored by Mach real-time thread policies and atomic dependency counters) without rewriting any DSP nodes, UI, or disk streaming code.

---

## 4. Automatic Plugin Delay Compensation (PDC)

Lookahead limiters or linear-phase EQs introduce a fixed sample delay (e.g., 512 samples). Without modularity for this, tracks with plugins will play out of phase with dry tracks.

* **Building it into the trait now:** By including `fn latency_samples(&self) -> u32` on the `AudioNode` trait from day one, the graph compiler on the Main Thread can walk the DAG, calculate the maximum latency path, and automatically insert hidden delay ring buffers onto parallel paths with lower latency.
* **Built (`ExecutionPlan::compensate`):** `EngineHandle::send_plan` runs it on every plan. Each step's inputs arrive when its slowest source is ready; every faster source edge gets a ring buffer of the difference, and a step's output is ready that much plus its node's latency. Nodes inserted by the plan report their own `latency_samples()`; nodes live from an earlier plan (persistent plugins) need `ExecutionPlan::set_latency`. Latencies clamp at `MAX_LATENCY` (2^20 samples). On a plan swap `adopt_delays` hands each edge's in-flight audio to the new plan when the edge (step node, input, source node, channel) and its delay are unchanged, so edits during playback do not drop audio (track node ids are positional, so inserting or removing a track while playing re-keys the tracks after it, and their in-flight audio restarts). `ExecutionPlan::latency()` is the master's total lag; `EngineHandle::plan_latency` exposes it and recorded takes are moved earlier by it. Live MIDI input is not compensated, so instrument steps report 0. AU and VST3 latency is read live, and the app rebuilds the plan when an effect's latency changes.

---

## 5. Recommended Cargo Workspace Layout for Long-Term Modularity

To enforce strict boundaries at compile time (so a UI developer or contributor can never accidentally import a blocking mutex into the DSP crate), structure the Rust workspace into decoupled crates:

| Crate Name | Responsibilities | Allowed Dependencies |
| --- | --- | --- |
| **`daw-core`** | Pure data types: `Sample`, `TimeSignature`, `MidiEvent`, `NodeId`, `ParameterId`, Command/Telemetry enums. | Zero heavy dependencies (`no_std` compatible if desired). |
| **`daw-dsp`** | Pure signal processing primitives (biquad filters, envelopes, oscillators, delay lines, FFT). | `daw-core`, `realfft`, `dasp`. **Zero OS or UI dependencies.** |
| **`daw-graph`** | The `AudioNode` trait, topological sorter, PDC calculator, and `ExecutionPlan` compiler. | `daw-core`, `daw-dsp`. |
| **`daw-engine`** | Real-time thread coordinator, `cpal`/`coreaudio-rs` stream management, `rtrb` queues, disk streamer, and garbage collector. | `daw-core`, `daw-dsp` (`.pek` waveform cache), `daw-graph`, `rtrb`, `symphonia`, `hound`, `midir`. |
| **`daw-host`** | CLAP (`clack`), VST3, and AudioUnit wrappers that implement `AudioNode`; Canary's built-in effects behind the same `Hosted` handle. | `daw-core`, `daw-dsp`, `daw-graph`, `clack-host`/`clack-extensions`, `vst3`, `libloading`, `rtrb`; macOS: `objc2-audio-toolbox`, `objc2-core-audio-types`, `objc2-core-foundation`. |
| **`daw-project`** | Undo/Redo command pattern (`Action` trait), serialization (`serde`), `.daw` project file bundles, and state management. Stores track devices (plugin format, bundle, id, base64 state). | `daw-core`, `serde`, `serde_json` (`.daw` files). |
| **`daw-ui`** | macOS windowing, timeline canvas, mixer views, and user input translation into `daw-project` actions. | `daw-core`, `daw-dsp` (pure data only: `WaveformPyramid`/`MinMax`), `daw-project`, `egui` (custom-painted). |
| **`canary`** | The `eframe` app binary: wires `daw-ui`, `daw-engine` and `daw-project`; builds engine plans from the project, loads audio on worker threads, records takes, saves. | `daw-core`, `daw-dsp`, `daw-graph`, `daw-engine`, `daw-host`, `daw-project`, `daw-ui`, `eframe`, `egui`, `cpal`. |

### Why this workspace structure pays off

1. **Headless Testing & CI:** Because `daw-graph` and `daw-engine` don't depend on `daw-ui` or even a physical soundcard when running an offline buffer loop, you can run automated unit tests and benchmarks (`cargo bench`) on the entire mixing engine in GitHub Actions without needing a macOS display server.
2. **Swappable Frontends:** If you start with a quick prototype UI in `iced` or `egui` and decide a year later to rewrite the visual interface in `gpui` or native Swift/AppKit via FFI, 80% of the codebase (`core`, `dsp`, `graph`, `engine`, `host`, `project`) remains completely untouched.
3. **Real-Time Sanitization:** You can attach custom global allocators or tools like `assert_no_alloc` specifically inside `daw-engine`'s test suite to mathematically guarantee no future feature accidentally introduces a heap allocation on the audio thread.
