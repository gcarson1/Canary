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
- [x] Load samples with `symphonia` into `Arc<SampleData>` (planar `f32`) on the main thread and send `Arc` clones over the queue (inside the plan). Resampling to the device rate is not done yet.
- [x] Schedule clips by comparing `start_sample`/`end_sample` against the block's sample range
- [x] Lock-free / smoothed parameters for Gain, Pan, Mute, Solo

### Phase 3: Arrangement Timeline UI and Waveform Rendering
- [ ] Mip-mapped waveform pyramid (min/max at 64, 256, 1024 and 4096 samples per pixel), 60+ FPS at any zoom
- [ ] Non-destructive clip editing (split, trim via `offset_samples`, crossfade) through metadata, never by mutating audio

### Phase 4: Recording, MIDI and Plugin Graph
- [ ] Duplex CoreAudio stream; input goes to an SPSC buffer drained by the disk writer thread
- [ ] `midir` capture, events stamped with sample offsets and routed to instrument tracks
- [ ] Plugin hosting (`daw-host`), in-process first, per ARCHITECTURE.md section 2
- [ ] Signed `.app` bundle via `cargo-bundle` with `NSMicrophoneUsageDescription` in `Info.plist`
