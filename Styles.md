# Styles.md: Tactile Brutalist Telemetry (macOS Rust DAW)

A complete reference for the visual and interaction style of the macOS Rust DAW. Hand this file to any agent, UI engineer, or graphics programmer (working with WGPU, Metal, Vizia, Iced, or GPUI) so they can build views, custom widgets, and shaders in the exact same style without deviation.

---

## 0. The Feel in One Paragraph

A Digital Audio Workstation that feels like precision laboratory and broadcast instrumentation—not a skeuomorphic studio rack or a neon toy. The aesthetic fuses **Swiss-grid industrial brutalism** with **tactile modular hardware** (Softube/Console-80, Output Thermal, Audio Damage) and an **FL Studio pattern-first workflow**. Surfaces are matte carbon and warm slate separated strictly by `1px` structural hairlines with **zero rounded corners** (only floating modal dialogs receive an `8pt` radius). **Inter** handles all UI labels; **JetBrains Mono** (with tabular numerals enforced) handles all timecode, BPM, decibels, frequency readouts, and step indices. Color discipline is absolute: **90% monochrome**, **Warm Amber (`#E5853C`)** for active operational focus/selections, **Muted Teal (`#4CC3B5`)** for secondary modulation curves, and **Green/Red** strictly reserved for live audio signal telemetry and clipping.

---

## 1. Core Principles (Non-Negotiable)

| # | Principle | DAW Implementation Rule |
| :--- | :--- | :--- |
| **1** | **One Unified Scale** | Every length, gutter, and font size derives from a base logical unit (`1.0u = 16pt` at standard resolution) multiplied by a fluid viewport scalar (`1.0x` on laptop up to `1.625x` on 4K/5K displays). `1px` hairlines always snap to physical device pixels. |
| **2** | **Monochrome Foundation** | Panel backgrounds, unselected clips, waveforms, and grid lines render in matte carbon and slate grayscale. Zero decorative color gradients on the arrangement or mixer canvas. |
| **3** | **Color is Telemetry** | **Warm Amber (`accent_amber`)** = active tool, selected step, or active parameter arc. **Green (`good`)** = nominal audio signal / active sound playing LED. **Red (`bad`)** = `0.0 dBFS` clipping, Record-Armed, or DSP buffer underrun. |
| **4** | **Lines, Not Boxes** | Panels, tracks, browser rows, and synth sub-modules butt directly against one another, separated only by `1px` hairlines (`line`). Never put a bordered card inside another bordered box. |
| **5** | **Square by Default** | Corner radius is `0.0` on every button, step cell, fader cap, meter, clip, and text input. Corner radius is `8.0pt` (`0.5u`) strictly on top-level modal overlays and floating OS plugin windows. |
| **6** | **Hierarchy via Luminance & Weight** | Three text tiers (`text`, `muted`, `faint`). Pair giant stacked tabular numerals (`fs_68`) for master telemetry with micro `10pt`–`11pt` mono calibration labels around controls. |
| **7** | **Tactile Vector Hardware (No 3D Skeuomorphism)** | Faders are vertical dark troughs flanked by `1px` calibration ticks with flat horizontal bar caps. Knobs are `1.5pt` vector arc rings with crisp white pointer lines and direct monospace value readouts. |
| **8** | **Deterministic 4-Zone FL Workflow** | **Top Bar:** Global mode router & transport. **Left Rail:** Asset/sample browser & live spectral preview. **Center Canvas:** Active pattern/sequencer tools (top) + modular DSP rack (bottom). **Right Rail:** Project Sound Manifest (Channel Rack) + Console-80 Master dB Dock. |
| **9** | **60/120Hz Lock-Free Motion** | Audio meters, spectrograms, and playheads update at native refresh rate via lock-free ring buffers without easing lag. UI state transitions use `120ms`–`220ms` cubic-bezier (`0.2, 0.8, 0.2, 1.0`). |
| **10** | **Self-Contained Binary** | Fonts (`Inter`, `JetBrains Mono`), vector icons, and WGSL/Metal shaders are compiled directly into the binary. Zero runtime network calls or external asset fetches. |

---

## 2. Foundation & Tokens (Rust Theme Struct)

### 2.1 Fluid Resolution Scaling (`1.0u` Root Scalar)

* Up to `1440pt` logical viewport width: `1.0u = 16.0pt`.
* From `1440pt` to `3840pt` logical width: scale `1.0u` via `clamp(16.0, 0.45 * vw_pt + 9.5, 26.0)` (or user preference override `100%`–`160%`).
* All spacing (`sp_*`) and typography (`fs_*`) tokens below are defined as multiples of `u`.

### 2.2 Spacing Tokens (`4pt` Grid at `16pt` Base)

| Token | Unit (`u`) | Base (`@16pt`) | Primary Usage |
| :--- | :--- | :--- | :--- |
| `sp_px` | `0.0625u` | `1pt` | Structural hairlines, grid bar lines, tick marks |
| `sp_0h` | `0.125u` | `2pt` | Active tab underline, selected row left bar, stereo meter gap |
| `sp_1` | `0.25u` | `4pt` | Step sequencer cell gap, kbd badge padding |
| `sp_2` | `0.5u` | `8pt` | Standard button gap, module status dot diameter |
| `sp_3` | `0.75u` | `12pt` | Browser/Manifest row horizontal padding |
| `sp_4` | `1.0u` | `16pt` | Standard panel padding, toolbar group spacing |
| `sp_6` | `1.5u` | `24pt` | Major DSP rack module padding, modal padding |
| `sp_8` | `2.0u` | `32pt` | Outer workspace gutters, large section breaks |

### 2.3 Color Tokens (Dark Control Room & Daylight Studio)

| Token | Dark Theme (Default) | Light Theme (Daylight) | Role in DAW |
| :--- | :--- | :--- | :--- |
| `bg_void` | `#0B0B0D` | `#F4F4F5` | Arrangement void, XY pad wells, FFT background, fader troughs |
| `bg_panel` | `#161618` | `#FFFFFF` | Left Sample Browser, Right Sound Manifest, Top Bar |
| `bg_rack` | `#232326` | `#FAFAFA` | Lower Hardware DSP Rack faceplate, Bottom-Right Master Console |
| `sel` | `#1D1D21` | `#F0F0F2` | Selected browser/manifest row fill, active clip background |
| `line` | `#2E2E33` | `#EBEBEE` | Primary `1px` panel and row dividers |
| `line_2` | `#3E3E45` | `#D9D9DE` | Button/input borders, fader ladder calibration ticks, inactive dial tracks |
| `text` | `#F4F4F6` | `#09090B` | Primary labels, active readouts, fader caps, dial pointers |
| `muted` | `#9A9AA2` | `#6E6E78` | Module headers (`DCO`, `VCF`), inactive tabs, secondary labels |
| `faint` | `#5E5E66` | `#A1A1AA` | Calibration tick numbers (`35`, `110`, `-10`), units (`Hz`, `dB`, `ms`) |
| `ink` | `#F4F4F6` | `#18181B` | High-contrast monochrome fill (active Play button, unaccented steps) |
| `ink_text` | `#0B0B0D` | `#FFFFFF` | Foreground text/icons inside solid `ink` or `accent_amber` buttons |
| `accent_amber` | `#E5853C` | `#D96B27` | **Primary Operational Accent:** Active tools, selected steps, drive arcs, `[15][30]` switches |
| `accent_cyan` | `#4CC3B5` | `#0D9488` | **Secondary Modulation Accent:** LFO curves, stereo spread vectors, synth bus tags |
| `good` | `#56D38A` | `#15803D` | **Nominal Audio Telemetry:** Active Sound Manifest play LEDs, healthy dB meter bars |
| `bad` | `#F04E3E` | `#C42A1C` | **Critical Telemetry:** `0.0 dBFS` clip cap, Record Arm state, buffer overload |

### 2.4 Typography Scale & Features

* **`font_sans` (`Inter`):** Variable `100..900`, OpenType `'cv02', 'cv03', 'cv04', 'cv11'`.
* **`font_mono` (`JetBrains Mono`):** Variable `100..800`, OpenType `'tnum'` (tabular numerals mandatory).
* **Sizes:**
  * `fs_10` (`0.625u`) & `fs_11` (`0.6875u`): Eyebrow headers (`01 // ASSET_BROWSER`), dial calibration ticks, FFT pitch/Hz overlay.
  * `fs_12` (`0.75u`) & `fs_13` (`0.8125u`): Browser filenames, Sound Manifest channel rows, button labels.
  * `fs_14` (`0.875u`) & `fs_16` (`1.0u`): Module sub-headers, Console-80 status values (`Analog Drive`, `-96.0 dB`).
  * `fs_18` (`1.125u`): Primary transport BPM (`142.00`) and Bar:Beat:Tick clock (`004 : 01 : 02`).
  * `fs_68` (`4.25u`): Giant stacked Console-80 Master Peak dBFS and Integrated LUFS readouts (`weight: 800`, `letter-spacing: -0.04em`).

---

## 3. The 4-Zone Layout Architecture

```text
┌────────────────────────────────────────────── TOP GLOBAL BAR & TRANSPORT (3.5u height) ──────────────────────────────────────────────┐
│ ● DAW_01 │ [SEQUENCER] [PLAYLIST] [PIANO ROLL] [PATCHER] [MIXER] │ PAT 04 │ 142.00 BPM │ 004:01:02 │ [■] [▶ PLAY] [● REC] │ DSP 11% │
├──────────────────────────┬─────────────────────────────────────────────────────────────────────┬─────────────────────────────────────┤
│ 01 // ASSET_BROWSER      │ 02A // CENTER WORKSPACE: TOOLS & ACTIVE PATTERN / TIMELINE CANVAS   │ 03 // SOUND_MANIFEST (CHANNEL RACK) │
│ (17.5u width, bg_panel)  │ (Fluid 1fr width, bg_void)                                          │ (21.25u width, bg_panel)            │
│ ──────────────────────── │ ─────────────────────────────────────────────────────────────────── │ ─────────────────────────────────── │
│ [FILTER // 808, tape...] │ [DRAW] [PAINT] [SLICE] [MUTE] │ GRID SNAP: [1/16] [1/8] [BAR]       │ ID ACT GENERATOR / SAMPLE   M/S BUS │
│ [ALL] [DRUMS] [SYNTH]    │ ─────────────────────────────────────────────────────────────────── │ ─────────────────────────────────── │
│ ──────────────────────── │ 01 808_Thermal   │ [■][□][□][■][□][□][■][□][■][□][□][■][□][□][■][□] │ 01 [■] 808_Thermal_Sub      [M] M01 │
│ ▼ DRUMS // ANALOG_TAPE   │ 02 Kick_Console  │ [■][□][□][□][■][□][□][□][■][□][□][□][■][□][□][■] │ 02 [■] Kick_Console80       [M] M02 │
│   ├─ 808_Thermal_Sub  →  │ 03 Snare_A820    │ [□][□][□][□][■][□][□][■][□][□][□][□][■][□][□][□] │ 03 [□] Snare_Tape_A820      [M] M03 │
│   ├─ Kick_Console80   →  │ 04 Hat_Closed    │ [■][■][■][■][■][■][■][■][■][■][■][■][■][■][■][■] │ 04 [■] Hat_Closed_15ips     [M] M04 │
│   └─ Snare_A820_Sat   →  │ ─────────────────────────────────────────────────────────────────── │ 05 [■] Mayo_Juno_Poly       [M] SYN │
│ ──────────────────────── │ VELOCITY LANE    │  █        █        █     █        █        ▄     │ 06 [□] Desert_Delay_Tex     [■] M06 │
│ SPECTRAL_PREVIEW         ├═════════════════════════════════════════════════════════════════════┼─────────────────────────────────────┤
│ [GPU Thermal Waterfall]  │ ● 02B // LOWER HARDWARE DSP RACK (25u height, bg_rack)              │ ● 04 // CONSOLE=80 MASTER BUS       │
│ -1.20dB | 32.7Hz | C1    │ ────────────────┬─────────────────┬────────────────┬─────────────── │ PEAK_DBFS         0  ┌──┐ ┌──┐      │
│ [High-Contrast FFT Plot] │ THERMAL // XY   │ DCO / VCF       │ ENVELOPE       │ TAPE_A820      │  -0.4            -6  │██│ │██│      │
│ 48.0kHz · 24-BIT · 1.84s │   ( Thermal )   │ [16'][8'][SUB]  │  │─│ │─│ │─│   │ Input   Output │ INTEGRATED_LUFS -12  │██│ │██│      │
│                          │   (   Orb   )   │  │─│  │─│  │─│  │  │█│ │ │ │█│   │ +3.0dB  -3.0dB │  -11.2          -24  │██│ │██│      │
│                          │ DRIVE / WIDTH   │  PWM  FREQ RES  │   A   D   S  R │ IPS: [15] [30] │ Summing: Analog      └──┘ └──┘      │
└──────────────────────────┴─────────────────┴─────────────────┴────────────────┴────────────────┴─────────────────────────────────────┘
```

1. **Top Bar** (`3.5u` / `56pt` height): Sticky global header. Left: `8pt` amber status dot + view switcher tabs (`SEQUENCER`, `PLAYLIST`, `PIANO ROLL`, `PATCHER`, `MIXER`) with `2pt` active bottom bar. Center: Pattern counter (`PAT 04`), `PAT`/`SONG` segmented switch, tabular BPM & Clock, and square transport controls (`PLAY`, `STOP`, `REC`). Right: DSP CPU/buffer load and dirty-project indicator square.

2. **Left Rail** (`17.5u` / `280pt` width): Top: Square monospace search input and segmented category filter. Middle: Zero-box hairline file rows (`.rowbtn`) with hover underline and `→` nudge. Bottom: Pinned Spectral Preview Dock rendering a live GPU thermal spectrogram waterfall (`#190A28` → `#6B114D` → `#E5853C` → `#F6B44E`) and high-contrast spike FFT analyzer with pitch/frequency telemetry (`-1.20dB | 32.70Hz | C1`).

3. **Center Stage** (Fluid `1fr` width):
   * **Upper Half** (`bg_void`): Topped by a `2.5u` Tool Strip (`DRAW`, `PAINT`, `SLICE`, `MUTE` + `GRID SNAP`). Displays the active view (16-step square matrix, arrangement playlist, or piano roll) with a collapsible bottom Velocity/Automation bar lane.
   * **Lower Half** (`25u` / `400pt` height, `bg_rack`): Tactile Hardware/DSP Rack divided into sub-modules (`THERMAL`, `DCO`, `VCF`, `ENVELOPE`, `TAPE_A820`) by `1px` vertical hairlines (`line_2`).

4. **Right Rail** (`21.25u` / `340pt` width):
   * **Upper Half** (`bg_panel`): Sound Manifest (Channel Rack)—every sound, sampler, and synth in the project listed in `46pt` hairline rows with channel index, live `12pt × 12pt` square play LED, name, `M`/`S` toggles, and mixer bus badge (`M01`).
   * **Lower Half** (`25u` / `400pt` height, `bg_rack`): Console-80 Master Bus Dock—giant `68pt` stacked Peak dBFS and Integrated LUFS numbers on the left, paired with dual calibrated vertical Peak/RMS stereo meters on the right.

---

## 4. Custom Widget & Shader Specifications

### 4.1 Buttons & Segmented Controls (Three Tiers)

* **Primary Active Block:** Solid `accent_amber` (`#E5853C`) or solid `ink` (`#F4F4F6`) fill with dark `ink_text` (`#0B0B0D`), `0pt` corner radius. Used for the active tool (`DRAW`), active tape speed (`[15]`), active oscillator octave (`[8']`), or active `PLAY` transport.
* **Secondary Outlined Button:** `1px` `line_2` border on `bg_void` or `bg_panel`, `0pt` radius, `muted` text. Hover shifts border to `text`.
* **Text Action (`tbtn`):** Borderless `muted` text; hover underlines (`1px`).

### 4.2 Calibrated Ladder Faders (Mayo Synth Style)

* **Trough:** `6pt` wide vertical track filled with `bg_void` (`#0B0B0D`).
* **Calibration Ticks:** `8pt` wide horizontal `1px` ticks in `faint` (`#5E5E66`) flanking both left and right sides of the trough at regular dB/percentage intervals.
* **Fader Cap:** `30pt × 10pt` sharp rectangle (`0pt` radius) in solid `text` (`#F4F4F6`)—or `accent_amber` for primary cutoff/frequency sliders—with a `1.5pt` horizontal center groove in `bg_void`.

### 4.3 Minimal Vector Arc Dials (Tape / Desert Cities Style)

* **Track Ring:** `270°` open circle, `2.0pt` stroke in `line_2` (`#3E3E45`).
* **Active Value Arc:** `2.5pt` stroke in `accent_amber` (for drive/saturation/filter) or `accent_cyan` (for delay/wet/stereo) sweeping from the minimum angle (or center `12 o'clock` for bipolar pan/gain).
* **Pointer & Readout:** `2.0pt` straight radial line in `text` (`#F4F4F6`) from center to rim, paired with a `fs_12` `font_mono` value readout (`+3.0 dB`, `110 Hz`) directly underneath.

### 4.4 Sound Manifest Rows & Activity LEDs

* Each channel row is `46pt` (`2.875u`) tall, separated by a `1px` bottom hairline (`line`).
* **Selected Channel:** Background shifts to `sel` (`#1D1D21`) with a `3pt` solid `accent_amber` inset bar on the far left edge.
* **Activity LED (`ACT` Column):** `12pt × 12pt` square (`0pt` radius).
  * **Silent:** `bg_void` fill with `1px` `line_2` border.
  * **Playing / Triggered:** Instantaneously fills with solid `good` Green (`#56D38A`).
  * **Muted:** Dashed `1px` `line_2` border, `faint` label text.

### 4.5 Master Stereo Meters (Console-80 Style)

* Dual `22pt` wide vertical troughs in `bg_void` with `1px` `line_2` borders, flanked by monospace dB scale numbers (`0.0`, `-3`, `-6`, `-12`, `-24`, `-48`).
* **Signal Zones:**
  * `-60.0 dB` to `-3.0 dB`: Solid `good` Green (`#56D38A`).
  * `-3.0 dB` to `-0.1 dB`: Solid `accent_amber` (`#E5853C`) pre-clip headroom zone.
  * `>= 0.0 dBFS`: Top `10pt` cap latches to solid `bad` Red (`#F04E3E`) until clicked to reset.

---

## 5. Copy, Labeling & Interaction Rules

1. **Exposed Technical Headers:** Prefix top-level zones and DSP sub-modules with uppercase monospace coordinates or double-slash tags (`01 // ASSET_BROWSER`, `02A // PATTERN_MATRIX`, `VCF // FILTER`, `CONSOLE=80 // MASTER BUS`).
2. **Zero Marketing or Conversational Copy:** Never write "Welcome" screens or full-sentence empty states. Use terse technical directives (`NO CHANNELS LOADED · DRAG SAMPLE FROM 01 OR PRESS ⌘T`).
3. **Direct Value Scrubbing:** Every monospace numeric readout (`142.00 BPM`, `+3.0 dB`, `M01`) can be clicked and dragged vertically to increment/decrement, or double-clicked for direct keyboard entry.
4. **Focus Lock on Destructive/Live Tasks:** When recording audio/MIDI or rendering offline stems, lock the Top Bar into Focus Mode (`← ABORT`, live take timer, and a single solid button to commit).

---

## 6. QA Protocol (Run Before Merging Any UI PR)

1. **Multi-DPI Viewport Sweep:** Render and inspect at `1440×900`, `2560×1440`, and `3840×2160` in both Dark and Light themes. Verify the `1.0u` fluid scalar scales fonts, faders, and step cells proportionally and all `1px` hairlines remain crisp.
2. **Zero Box-in-Box Audit:** Verify no panel, synth module, or channel row uses nested bordered boxes or drop shadows. Every division must be a single `1px` full-bleed hairline.
3. **Corner Radius Audit:** Assert `border_radius == 0.0` on all buttons, step blocks, meters, inputs, and panels (only floating modal dialogs may use `8.0pt`).
4. **Color Discipline Check:** Verify that outside of GPU spectral/thermal analyzer wells, the only non-grayscale colors are `accent_amber` (`#E5853C`), `accent_cyan` (`#4CC3B5`), `good` (`#56D38A`), and `bad` (`#F04E3E`).
5. **Tabular Numerals Check:** Confirm `tnum` is active on all `JetBrains Mono` text runs so rapidly changing dB meters, playhead clocks, and frequency counters never jitter horizontally.

---

## 7. Prompt Snippet for Coding Agents

```text
Build <component/view> for our macOS Rust DAW following Styles.md ("Tactile Brutalist Telemetry"). Hard rules:

1. Layout uses the 4-zone FL-style shell (Top Bar router/transport, Left Asset Browser + GPU Spectrogram, Center Pattern Matrix + Lower Hardware DSP Rack, Right Sound Manifest + Console-80 Master Bus).
2. All sizes use logical `u` tokens (1u = 16pt base, fluid-scaled for 4K); all dividers are 1px full-bleed hairlines (#2E2E33) with zero nested boxes and 0.0 corner radius on all controls.
3. Surfaces use matte carbon (#0B0B0D, #161618) and warm slate (#232326).
4. Color is strictly telemetry: Warm Amber (#E5853C) for active tools/steps/arcs, Green (#56D38A) for playing channel LEDs and nominal dB meters, Red (#F04E3E) for 0dBFS clipping and record-arm.
5. Use Inter for labels and JetBrains Mono (tabular-nums) for all clocks, BPM, dB, and calibration ticks, including oversized 68pt stacked numerals in the Master dock.
```
