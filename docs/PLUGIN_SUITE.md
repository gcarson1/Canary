# Built-in Plugin Suite: Open-Source Options

Status: proposal, awaiting approval. Nothing here is integrated yet.
Researched 2026-11-13. License, last push and archive state were read from each project's GitHub repository or crates.io entry that day. This is engineering guidance, not legal advice.

Goal: ship Canary with a stock suite on par with FL Studio Producer/Signature (Parametric EQ 2, Maximus, Limiter, Reverb 2, Delay 3, Chorus, Flanger, Phaser, Gross Beat, Vocodex, Harmor/Sytrus-class synths, Slicex) and Logic Pro (Channel EQ, Compressor, Space Designer, ChromaVerb, Alchemy, Retro Synth, Sampler, Pitch Correction, Amp Designer/Pedalboard, Tape/Bitcrusher, spatial tools, Drum Machine Designer).

## 1. Constraints that decide everything

1. **Canary has no license.** The repo has no `LICENSE` file, so it is all-rights-reserved (closed) by default. Code under GPL, AGPL or LGPL cannot be copied into Canary's crates without relicensing Canary under that license. Permissive code (MIT, BSD, ISC, Apache-2.0, BSL-1.0) can be, keeping its notice.
2. **Built-ins are native Rust nodes.** Stock devices live in `daw_dsp::effects` / `daw_dsp::fx` and `daw_graph` (`Format::Builtin`, ids `canary.*`), get MIX/MATCH/DELTA from `EffectNode`, draw with Styles.md, and are tested with `assert_no_alloc`. Porting permissive DSP into `daw-dsp` keeps all of that. A hosted CLAP bundle gets its own UI, none of MIX/MATCH/DELTA, and Canary's crash exposure until out-of-process sandboxing (ARCHITECTURE.md section 2) is built.
3. **macOS arm64 only matters for hosted binaries.** Ported source compiles wherever Canary does. Many Linux-first suites (LSP, Calf, x42, Zam, Geonkick, ZynAddSubFX) ship no official macOS build, which removes them as hosted options.
4. **LGPL is not a shortcut in Rust.** LGPL needs the user to be able to relink the library; Cargo links statically, so LGPL code (LSP, Calf) is reference reading only.
5. **GPL plugins can still ship, separately.** Unmodified upstream GPL plugin bundles, distributed as a separate optional download with links to their source, are an aggregate, not a derived work of Canary. Keep them out of `Canary.app` itself, and run them out-of-process once the sandbox exists.

Canary already ships (ROADMAP.md Phase 10): EQ (cut, shelves, bell), dynamic EQ, de-esser, compressor (sidechain), saturator (ADAA), Freeverb-style reverb, ping-pong delay, chorus/flanger, brickwall limiter, gate, multimode filter, bitcrusher, utility (gain, width, pan, polarity), 16-voice subtractive synth, sampler with slicing, key/velocity zones and round robin, and master LUFS/true-peak/correlation metering. Slicex and Fruity Slicer are already covered by the sampler's slice mode.

## 2. Options per category

"Bundle in closed app" means: can the code or binary go inside `Canary.app` while Canary stays closed.

| Category | FL / Logic targets | Best option | License | Form | macOS arm64 | Maintained | Bundle in closed app | Use it as |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| EQ | Parametric EQ 2, Channel EQ, Linear Phase EQ | Own 8-band + analyzer (already on roadmap); Signalsmith DSP filters as reference | MIT | C++ header library | n/a (port) | yes (2026-10) | yes | Port/own code |
| EQ (alt) | | ZL Equalizer | AGPL-3.0 | VST3/AU | yes | yes (2026-11) | no | Extras only |
| EQ (ref) | | LSP Parametric/Graphic EQ | LGPL-3.0 | LV2/VST/CLAP | no official build | yes | no | Reference |
| Dynamics | Maximus, Limiter, Compressor, Multipressor | Own lookahead true-peak limiter and multiband (reuse `TruePeak`, crossover filters); Airwindows Pressure/Density as character options | MIT | C++ source | n/a | yes (2026-11) | yes | Port/own code |
| Dynamics (ref) | | x42-dpl, ZaMaximX2, LSP | GPL-2.0 / LGPL-3.0 | LV2 | Linux-first | yes | no | Reference |
| Algorithmic reverb | Reverb 2, ChromaVerb | **Cloud Seed** (Valdemar Erlingsson) | MIT | C++ source | n/a | stable (last push 2024-05) | yes | Port to Rust |
| | | Airwindows Galactic and friends | MIT | C++ source | n/a | yes | yes | Port |
| | | Dragonfly Reverb | GPL-3.0 | CLAP/VST3/LV2 | macOS builds published | yes (2026-05) | no | Extras |
| Convolution | Space Designer, Fruity Convolver | **fft-convolver** crate | MIT | Rust crate | n/a | yes (2026-06) | yes | Dependency |
| | | IR content: OpenAIR library and self-recorded IRs | per-IR (CC BY / CC0) | WAV | n/a | n/a | yes, with attribution | Content |
| Delay | Delay 3, Tape Delay, Delay Designer | Own tempo-synced delay (roadmap); ChowMatrix for multi-tap ideas | BSD-3-Clause | C++ | n/a | dormant (2022) | yes | Port/own code |
| Modulation | Chorus, Flanger, Phaser, Ensemble | Own phaser (roadmap); Airwindows chorus/ensemble ports; stone-phaser | MIT; BSL-1.0 | C++/Faust | n/a | yes; dormant | yes | Port |
| Volume/time shaper | Gross Beat, Fruity Gate | Own build. tiagolr gate12/time12 define the behavior well | GPL-3.0 | VST3/CLAP | yes | yes (2026-01) | no | Behavior reference only |
| Vocoder | Vocodex, EVOC 20 | Own filterbank vocoder (bandpass bank plus envelope followers, keyed by the existing sidechain) | own | Rust | n/a | n/a | yes | Own code |
| Pitch shift / correction | Pitch Correction, Flex Pitch, Pitcher, NewTone-lite | **Signalsmith Stretch** + **pitch-detection** crate | MIT; MIT/Apache-2.0 | C++ header (Rust bindings exist) + Rust crate | n/a | yes (2026-10); stable (2022) | yes | Port Stretch to Rust; crate as dependency |
| | | x42 fat1 (Zita-at1 autotune) | GPL-2.0 | LV2 | Linux-first | yes | no | Reference |
| Amp / pedal sim | Amp Designer, Pedalboard | **Neural Amp Modeler core** + cab IRs through the convolver | MIT (Eigen is MPL-2.0, file-level copyleft, fine to link) | C++ library | yes | yes (2026-11) | yes | FFI or Rust port |
| | | BYOD (ChowDSP), AIDA-X | GPL-3.0 | VST3/AU/CLAP | yes | yes / 2024 | no | Extras |
| Tape / lo-fi | Tape, Bitcrusher, Lo-Fi | Airwindows ToTape and IronOxide family; existing bitcrusher | MIT | C++ source | n/a | yes | yes | Port |
| | | ChowTape (AnalogTapeModel) | GPL-3.0 | VST3/AU/CLAP | yes | yes (2025-04) | no | Extras |
| Spatial | Stereo Spread, Binauralizer, Directional Mixer | Own stereo tools (width exists); Spatial Audio Framework core for binaural/ambisonics | ISC (optional modules GPL-2.0: keep them disabled) | C library | n/a | yes (2026-05) | yes, core modules only | Port selected parts, later |
| Wavetable / hybrid synth | Harmor, Sytrus, Alchemy, Retro Synth | **Surge XT** | GPL-3.0 | CLAP/VST3/AU | yes (universal) | very active (2026-11) | no | Extras flagship |
| | | Vital | GPL-3.0 (name is trademarked) | VST3/AU | yes | stale (2023-05) | no | Skip; Surge covers it |
| FM synth | Sytrus, Retro Synth FM | Dexed | GPL-3.0 | CLAP/VST3/AU | yes | active (2026-11) | no | Extras |
| Analog-style poly | Retro Synth, ES2-lite | OB-Xf; Odin 2 | GPL-3.0 | CLAP/VST3/AU | yes | active (2026-11); slowing (2025-09) | no | Extras |
| Modular | (none built-in) | Cardinal | GPL-3.0 | CLAP/VST3/AU | yes | very active | no | Extras, optional |
| Own synth upgrade | | Roadmap items (LFO, noise/sub, unison, presets) plus wavetable oscillator; fundsp for prototyping | MIT OR Apache-2.0 | Rust crate | n/a | yes (2026-03) | yes | Own code |
| Sampler / SFZ | Sampler, DirectWave, Quick Sampler | Own SFZ import into the existing sampler (the format is an open spec) | own | Rust | n/a | n/a | yes | Own code |
| | | sfizz | BSD-2-Clause | library + plugins | yes | **archived 2025-03** | yes | Reference only |
| | | Shortcircuit XT | source MIT outside `libs/`; shipped binary AGPL-3.0 | CLAP/VST3/AU | yes | active, beta | binary no; MIT source yes | Reference for sampler features |
| Drum machine | FPC, Drum Machine Designer | Own, on the sampler (roadmap item); kits from CC0 sample packs | own | Rust | n/a | n/a | yes | Own code + content |
| | | Geonkick (drum synth) | GPL-3.0 | LV2/VST3 | Linux/Windows only | yes | no | Skip |
| Metering / analysis | Wave Candy, MultiMeter | Already built (LUFS, true peak, correlation); add spectrum and phase scope from existing `realfft` path | own | Rust | n/a | n/a | yes | Own code |
| Plugin frameworks | | nih-plug (ISC; its VST3 export is GPL) | ISC | Rust | yes | yes (2026-09) | yes, CLAP only | Only if Canary's effects are later sold as standalone CLAPs |

## 3. Recommended approach

**Two tiers.**

1. **Canary Stock (inside the app, permissive or own code, native Rust in `daw-dsp`).** Every FL/Logic effect category, plus the synth, sampler and drum machine. This is where the product quality lives: one UI language (Styles.md), MIX/MATCH/DELTA on everything, `assert_no_alloc`-tested, honest `latency_samples()`, no crash surface.
2. **Canary Extras (optional separate download, unmodified upstream GPL CLAP bundles).** Surge XT, Dexed, OB-Xf, Dragonfly Reverb, optionally Cardinal and Odin 2. These fill the Harmor/Sytrus/Alchemy gap that would take a year to write. The download page links each project's source and license. The `Catalog` already finds CLAP bundles, so this needs no engine work, only a curated installer and an "Extras" filter in the device picker. Recommend gating it on the out-of-process sandbox for safety, but it works in-process today.

**Port, don't wrap, for Stock.** C++ FFI would bring a C++ toolchain into `daw-dsp` and break "pure DSP, no OS deps" in spirit. Cloud Seed, Signalsmith Stretch and the chosen Airwindows plugins are small enough to port by hand. NAM core is the exception (Eigen-based neural inference): wrap it behind a `nam` feature in a new small crate rather than in `daw-dsp`, or port its WaveNet/LSTM inference later.

**Keep a `THIRD_PARTY_NOTICES.md`** listing every ported MIT/BSD/ISC source with its copyright line, and each bundled IR or sample pack with its license. Required by those licenses, and cheap to keep from day one.

## 4. Integration order

Ordered by user value per unit of work, finishing roadmap gaps first.

1. **Close Phase 10 effect gaps:** 8-band EQ with analyzer, lookahead true-peak limiter (Maximus/Limiter tier, multiband next), tempo-synced delay, phaser. Own code; no new dependencies.
2. **Convolution reverb** on `fft-convolver`, with a small bundled IR set. Unlocks Space Designer, and cabinet IRs for step 6.
3. **Cloud Seed port** as the flagship algorithmic reverb (ChromaVerb / Reverb 2 tier). The current Freeverb stays as the light option.
4. **Pitch toolkit:** port Signalsmith Stretch for real-time pitch and formant shift, add `pitch-detection`; build pitch correction (key/scale snap, retune speed), then a harmonizer. Also lets offline `stretch` upgrade from WSOLA.
5. **Character pack from Airwindows (MIT):** a curated handful (tape, console, density/pressure, a chorus/ensemble, Galactic) behind Canary UIs, not dozens.
6. **Amp/pedal sim:** NAM core plus cab IRs from step 2. Needs a decision on which amp captures ship (each capture has its own license; safest is capturing our own).
7. **Shaper and vocoder:** Gross Beat-style volume/time shaper and a sidechain-keyed vocoder. Own designs.
8. **Instruments:** synth roadmap items plus wavetable oscillator, drum machine on the sampler with CC0 kits, SFZ import.
9. **Canary Extras installer** (Surge XT, Dexed, OB-Xf, Dragonfly), ideally after out-of-process sandboxing. Can move earlier if instruments matter more than effects.
10. **Later:** spatial (binaural panner from SAF core), multiband dynamics, spectrum/phase scope.

## 5. Ranked recommendation (for approval)

1. Keep Canary closed (or permissively licensed) and build Stock only from own code plus MIT/BSD/ISC/Apache sources. **Decision needed:** if Gabriel would rather make Canary GPL-3.0 open source, Surge XT, Dexed, Dragonfly and the ChowDSP plugins could be built in directly and steps 8-9 change; everything else holds.
2. Highest-value permissive picks, in order: Cloud Seed (reverb), fft-convolver (convolution), Signalsmith Stretch + pitch-detection (pitch), Neural Amp Modeler core (amp sim), Airwindows (character).
3. Ship Surge XT as the headline instrument through an optional Extras download, with Dexed, OB-Xf and Dragonfly Reverb alongside.
4. Skip as hosted options: Vital (stale since 2023, covered by Surge), sfizz (archived), Helm (archived), LSP/Calf/x42/Zam/Geonkick (no official macOS builds, LGPL/GPL anyway).
5. Start with integration step 1, which needs no new dependencies and closes items already on the roadmap.
