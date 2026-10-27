# Roadmap: Canary (macOS Rust DAW)

Canary is split into two strictly isolated worlds: a **non-blocking real-time audio thread** and an **asynchronous UI/state thread**. ARCHITECTURE.md describes the modular expansion patterns (DAG, sandboxing, multi-core, PDC, crate layout) layered on top of this plan.

## 1. Real-Time Golden Rules

* **Zero allocations on the audio thread.** No `Box::new()`, `Vec::push()` or `String` formatting in the render loop. Pre-allocate buffers, delay lines and graphs on the main thread.
* **No blocking primitives.** No `Mutex`, `RwLock`, file I/O or network calls on the audio thread. The deadline is about 2.9 ms at 128 samples / 44.1 kHz.
* **Lock-free communication.** UI ↔ engine traffic goes through SPSC ring buffers or atomics (`Acquire` / `Release`).
* **Garbage collection on a worker thread.** Never `Drop` removed nodes in the callback. Push them onto a lock-free garbage queue for a background thread to drop.

## 2. Crate Ecosystem

| Subsystem | Crates | Notes |
| --- | --- | --- |
| Audio I/O | `cpal` or `coreaudio-rs` | `coreaudio-rs` for AudioUnits, hardware timestamps, aggregate devices |
| MIDI I/O | `midir` + `wmidi` | CoreMIDI ports and virtual buses; allocation-free parsing |
| Lock-free IPC | `rtrb` or `ringbuf` | Commands in (Play, Seek, SetGain), telemetry out (meters, playhead) |
| Audio files | `symphonia` + `hound` | Decode WAV/FLAC/MP3/AAC; fast WAV writing for bounce/record |
| DSP and graph | `fundsp`, `dasp`, `realfft` | Composable blocks, frame abstractions, spectral analysis |
| Plugin hosting | `clack` (CLAP), `vst3-sys`, `objc2` | Start with CLAP or AudioUnits v2/v3 |

## 3. UI Framework Options

* **`vizia`:** built by audio developers (nih-plug ecosystem), retained-mode, Skia/FemtoVG rendering for knobs, meters and timelines.
* **`gpui`:** from the Zed team, macOS-first, renders through Metal directly for 120 FPS canvases.
* **Tauri v2 + Canvas/WebGL:** WKWebView UI with the audio in Rust. The timeline and waveforms go on canvas/WebGL fed from shared memory, not the DOM.

The UI follows Styles.md whichever framework is chosen.

## 4. The 4-Thread Topology

1. **UI / Main thread:** owns project state (`Project`, `TrackList`, zoom, undo/redo) and sends commands through a `CommandProducer`.
2. **Real-time audio thread (CoreAudio callback):** runs at Mach `THREAD_TIME_CONSTRAINT_POLICY`. At each block it drains `CommandConsumer`, runs the DSP graph, sums into the master bus, and pushes peak/RMS to a `TelemetryProducer`.
3. **Disk streaming / asset thread:** pre-fetches file chunks into pre-allocated `DiskStreamBuffer` rings.
4. **Garbage / background worker:** generates waveform peak files (`.pek`), runs offline bounces, and drops retired nodes.

## 5. Phased Implementation

### Phase 1: Real-Time Engine and Transport Skeleton
- [x] Cargo workspace. ARCHITECTURE.md's seven-crate layout replaces the three crates first planned.
- [x] `AudioNode` trait, buffer contract, `GainNode` with tests (`daw-graph`)
- [x] `cpal` / `coreaudio-rs` output stream
- [x] `Transport` (playhead in samples, BPM, time signature). Commands apply at block boundaries; per-sample command offsets come later
- [x] `rtrb` command/telemetry queues: Play/Stop and tempo changes reach the audio thread without locks

### Phase 2: Multi-Track Mixing and Clip Scheduling
- [x] `Track` with a pre-allocated scratch buffer sized to the max block (for example 4096 frames). Per ARCHITECTURE.md, tracks become `AudioNode`s in the DAG, not a flat `Vec<Track>`. `TrackNode` runs in an `ExecutionPlan` that owns every buffer; plans swap through an `rtrb` queue and retire to a garbage queue.
- [x] Load samples with `symphonia` into `Arc<SampleData>` (planar `f32`) on the main thread and send `Arc` clones over the queue (inside the plan). `resample` converts it to the device rate with `rubato` (FFT, band-limited).
- [x] Schedule clips by comparing `start_sample`/`end_sample` against the block's sample range
- [x] Lock-free / smoothed parameters for Gain, Pan, Mute, Solo

### Phase 3: Arrangement Timeline UI and Waveform Rendering
- [x] Mip-mapped waveform pyramid (min/max at 64, 256, 1024 and 4096 samples per pixel), 60+ FPS at any zoom. `daw_dsp::WaveformPyramid`: `view()` costs O(pixels) at any zoom; `.pek` caching and drawing land with `daw-ui`
- [x] Non-destructive clip editing (split, trim via `offset_samples`, crossfade) through metadata, never by mutating audio. `Clip` is a window (offset, length, equal-power fades) over shared `Arc<SampleData>`
- [x] Arrangement timeline UI (`daw-ui`, per Styles.md): egui, custom-painted 4-zone shell; playlist with waveform clips, move across tracks, trim, slice, snap, zoom, seek, undo. Section 6 QA runs as `cargo test -p daw-ui` (renders to `target/qa/`, audits corners and colors). `.pek` caching via `daw_engine::cached_waveform`. Runs as the `canary` app

### Phase 4: Recording, MIDI and Plugin Graph
- [x] Recording: `start_input` opens a separate default input stream (`cpal`); `RecordInput::push` fills an SPSC ring of whole frames, and a per-take disk-writer thread (`Recorder`) drains it to a 32-bit float WAV (`hound`). Takes are placed by `EngineHandle::playhead_clock`. Still to come: a true duplex/aggregate device and input/output latency compensation of `Take::start_sample`
- [x] `midir` capture: `connect_midi` stamps messages with a shared clock; the audio thread places them one block late at exact spacing (`MidiReceiver`), split across plan chunks, and delivers them to steps marked `ExecutionPlan::route_midi`. An instrument track is a `SynthNode` (built-in 16-voice sine) feeding a `TrackNode`'s inputs
- [x] CLAP hosting (`daw-host`, via `clack-host`), in-process, per ARCHITECTURE.md section 2: `load_bundle`/`plugins` list a bundle; `ClapPlugin` is the main-thread handle (params with id, name, range, default; `set_param` over a lock-free queue; `save_state`/`load_state`; `latency_samples`; `idle`); `PluginNode` is the `AudioNode` (live MIDI as CLAP note/MIDI events, param events at block start, no allocation). Still open: re-activation on `request_restart` or sample-rate change, transport info to plugins, plugins with a sidechain or extra ports (rejected for now), plugin output events, plugin GUIs, loading a real bundle (untested here, no CLAP plugins installed)
- [x] Audio Unit hosting (macOS, AU v2 API via `objc2-audio-toolbox`): `audio_units` lists effects, instruments and MIDI effects; `AuPlugin` (params, `save_state`/`load_state` as binary plists, latency) and `AuNode` (stereo render callback in, `MusicDeviceMIDIEvent` for MIDI). Tested on Apple's AUDelay and DLSMusicDevice
- [x] VST3 hosting (`vst3` crate bindings; the SDK is MIT): `Vst3Bundle` loads a `.vst3` bundle and lists its audio modules; `Vst3Plugin` (controller: normalized params, component + controller state, latency) and `Vst3Node` (`IParameterChanges` from a lock-free queue, notes as `IEventList` events, no allocation). Tested against an in-process test plugin; no real VST3 bundle installed here to try. Plugins with sidechain or extra buses are rejected for now; no IMessage support yet
- [x] Plugin devices in the app: `daw_host::Catalog` scans AU/CLAP/VST3; the DSP rack shows the selected track's devices with arc dials and a picker to add more. A track plays instrument → clips/mixer → effects; plugin nodes persist across plan rebuilds and their state saves in the project. No delay compensation between tracks yet (latencies are reported, the PDC compiler is not built)
- [x] Signed `.app` bundle: `scripts/bundle.sh` builds `target/release/Canary.app` with `NSMicrophoneUsageDescription`, signed ad-hoc or with `SIGN_IDENTITY` (hardened runtime, mic and plugin entitlements). No `cargo-bundle` needed. Notarization is a manual step

### Phase 5: Production Toolkit (later)
- [x] PDC compiler: delay lines so tracks with plugin latency stay in sync. `ExecutionPlan::compensate` (run by `send_plan`) delays faster edges; delays keep their audio across plan swaps
- [ ] ARA2 on timeline clips (needs the ARA SDK; no Rust bindings yet)
- [x] Every built-in effect has dry/wet and automatic level matching; insert racks can be reordered by drag and drop. `daw_graph::EffectNode` adds MIX/MATCH/DELTA to every `daw_dsp::effects::Effect`; drag a module's title to move it
- [x] Built-in dynamics and tone: dynamic EQ (incl. threshold-triggered low cut), de-esser, compressor, saturator. In the device picker as `CANARY`; saturator uses ADAA against aliasing
- [x] Sampler: one-shot/classic/slice modes (transient, grid, manual), slice to pads, key and velocity zones, round robin. Built-in `Sampler` instrument: drop files from 01 (Shift round robin, Option velocity zone, Cmd key zone), click the waveform to add slice points, right-click to remove. Pads do not preview yet (no MIDI path from the UI)
- [ ] Metering and QC: LUFS (integrated/short-term), oversampled true peak, delta monitoring, mid/side, level-matched A/B reference
- [ ] Plugin GUIs drawn through Metal; SIMD where profiling shows a hot loop
