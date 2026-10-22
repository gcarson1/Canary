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

---

## 3. Transitioning from Single-Core to Multi-Core DSP

Initially, the CoreAudio callback runs on a single high-priority thread. Once a user loads 50 tracks with heavy reverb and virtual instruments, a single CPU core will bottleneck even when the other 9 cores on an M-series chip are idle.

* **Why the DAG matters here:** Because the engine is a Directed Acyclic Graph (DAG), independent branches of the graph (e.g., Track 1 through Track 32 before they hit the Master Bus) have zero data dependencies on each other during a single buffer tick.
* **The Upgrade Path:** Later on, you can replace the single-threaded topological loop inside the Audio Engine with a real-time work-stealing thread pool (anchored by Mach real-time thread policies and atomic dependency counters) without rewriting any DSP nodes, UI, or disk streaming code.

---

## 4. Automatic Plugin Delay Compensation (PDC)

Lookahead limiters or linear-phase EQs introduce a fixed sample delay (e.g., 512 samples). Without modularity for this, tracks with plugins will play out of phase with dry tracks.

* **Building it into the trait now:** By including `fn latency_samples(&self) -> u32` on the `AudioNode` trait from day one, the graph compiler on the Main Thread can walk the DAG, calculate the maximum latency path, and automatically insert hidden `DelayCompensationNode` ring buffers onto parallel tracks with lower latency.

---

## 5. Recommended Cargo Workspace Layout for Long-Term Modularity

To enforce strict boundaries at compile time (so a UI developer or contributor can never accidentally import a blocking mutex into the DSP crate), structure the Rust workspace into decoupled crates:

| Crate Name | Responsibilities | Allowed Dependencies |
| --- | --- | --- |
| **`daw-core`** | Pure data types: `Sample`, `TimeSignature`, `MidiEvent`, `NodeId`, `ParameterId`, Command/Telemetry enums. | Zero heavy dependencies (`no_std` compatible if desired). |
| **`daw-dsp`** | Pure signal processing primitives (biquad filters, envelopes, oscillators, delay lines, FFT). | `daw-core`, `realfft`, `dasp`. **Zero OS or UI dependencies.** |
| **`daw-graph`** | The `AudioNode` trait, topological sorter, PDC calculator, and `ExecutionPlan` compiler. | `daw-core`, `daw-dsp`. |
| **`daw-engine`** | Real-time thread coordinator, `cpal`/`coreaudio-rs` stream management, `rtrb` queues, disk streamer, and garbage collector. | `daw-core`, `daw-graph`, `rtrb`, `symphonia`. |
| **`daw-host`** | CLAP (`clack`), VST3, and AudioUnit wrappers that implement `AudioNode`. | `daw-core`, `daw-graph`, `clack`, `objc2`. |
| **`daw-project`** | Undo/Redo command pattern (`Action` trait), serialization (`serde`), `.daw` project file bundles, and state management. | `daw-core`, `serde`. |
| **`daw-ui`** | macOS windowing, timeline canvas, mixer views, and user input translation into `daw-project` actions. | `daw-core`, `daw-project`, UI framework (`vizia`/`gpui`). |

### Why this workspace structure pays off

1. **Headless Testing & CI:** Because `daw-graph` and `daw-engine` don't depend on `daw-ui` or even a physical soundcard when running an offline buffer loop, you can run automated unit tests and benchmarks (`cargo bench`) on the entire mixing engine in GitHub Actions without needing a macOS display server.
2. **Swappable Frontends:** If you start with a quick prototype UI in `iced` or `egui` and decide a year later to rewrite the visual interface in `gpui` or native Swift/AppKit via FFI, 80% of the codebase (`core`, `dsp`, `graph`, `engine`, `host`, `project`) remains completely untouched.
3. **Real-Time Sanitization:** You can attach custom global allocators or tools like `assert_no_alloc` specifically inside `daw-engine`'s test suite to mathematically guarantee no future feature accidentally introduces a heap allocation on the audio thread.
