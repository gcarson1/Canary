# Canary

A native macOS Digital Audio Workstation written in Rust, built for producers who want an FL-style pattern workflow on precision, telemetry-style hardware UI.
Vault project: canary

## How this project is run
The Claude Code template (github.com/gcarson1/claude-code-template) is the operating system here. Its subagents and skills are installed globally in `~/.claude/`; a file in this repo's `.claude/` would only override one:
- `code-reviewer` after every non-trivial change. Fix its Must-fix items; ask before acting on the rest.
- `test-writer` for new risky logic (graph compiler, PDC math, lock-free queues, anything on the audio thread) and every bug fix.
- `docs-writer` when behavior changes that `ARCHITECTURE.md` or `Styles.md` describe.
- Ponytail (level: full) on every coding task. `/vault load` at the start of a session, `/vault save` at the end, `/vault decide` for architecture calls.

## Build order
Architecture first, UI last. Do not start UI work until the engine below it runs headless.
Phases and progress live in ROADMAP.md: tick its checkboxes as work lands. The steps below say how to build, ROADMAP.md says what comes next.
1. `ARCHITECTURE.md` is the source of truth for the engine: `AudioNode` DAG, lock-free plan swapping, PDC, plugin sandboxing, and the Cargo workspace crates.
2. Build crates bottom up: `daw-core` → `daw-dsp` → `daw-graph` → `daw-engine` → `daw-host` / `daw-project`. Each must build and pass `cargo test` without a display or soundcard.
3. Only then `daw-ui`, following `Styles.md` ("Tactile Brutalist Telemetry") exactly, and running its QA protocol (Section 6) before any UI change is called done.

## Commands
- Build: `cargo build --workspace` (crates live in `crates/`)
- Test: `cargo test --workspace` (single crate: `cargo test -p <crate>`)
- Lint/format: `cargo clippy --workspace -- -D warnings` and `cargo fmt --all`
- Run the app: `cargo run --release -p canary -- [project.daw] [--samples DIR]` (samples default to `~/Music/Canary`)
- Signed app bundle: `scripts/bundle.sh` (optional `SIGN_IDENTITY="Developer ID Application: …"`)
- `cargo test -p daw-ui` runs the Styles.md QA protocol and writes renders to `target/qa/`

## Conventions
- Crate boundaries in `ARCHITECTURE.md` section 5 are hard rules: `daw-dsp` has no OS or UI dependencies, and nothing below `daw-ui` depends on a UI framework.
- Audio thread code never allocates, locks, or blocks. Prove it with `assert_no_alloc` in `daw-engine` tests.
- Every `AudioNode` reports `latency_samples()` honestly so PDC stays correct.

## Gotchas
- Gemini MCP (`.mcp.json`) needs `GEMINI_API_KEY` in your shell profile. Don't send anything confidential to it.
- Fonts, icons and shaders are compiled into the binary; no runtime network fetches.
