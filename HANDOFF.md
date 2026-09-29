# CapraLink — Session Handoff

Read `MASTER.md` first. This file is the live position.

## Current position
- Phase: M0 + M1 done. Next: owner two-machine test (Mac ↔ Windows/Linux), then M2 (discovery + PIN pairing + encryption).

## Machine facts
- GitHub: private repo CapraAudio/CapraLink, branch main. Repo-local git identity set.
- No brew/pkg-config. cmake 4.4 installed user-level at ~/Library/Python/3.9/bin (NOT on PATH): `export PATH=$HOME/Library/Python/3.9/bin:$HOME/.cargo/bin:$PATH` before cargo.
- Dev machine: macOS 26.6.2 (Apple Silicon assumed), project at `~/Documents/CapraLink`.
- Rust 1.98.1 via rustup in ~/.cargo (installed without PATH edit; use ~/.cargo/bin or `. ~/.cargo/env`).
- Test machines: Windows 10/11 PC and PipeWire Linux PC on the LAN (D10).

## Pending owner actions / questions
1. Two-machine test: Mac runs target/release/capralink; PC runs capralinkd from CI artifacts (https://github.com/CapraAudio/CapraLink/actions/runs/36612019404). Owner reports sound/latency/stats + window layout.

## Owner test results (2026-09-29, Mac app ↔ Linux capralinkd)
- Linux mic → Mac headphones: WORKS, latency "not much at all".
- Mac mic → Linux speakers: SILENT. Suspect macOS mic permission (app launched from a terminal → TCC attributes to the terminal's host app; unbundled binary has no NSMicrophoneUsageDescription → silence).
- Occasional slight click. Cause unknown (underrun vs drift skip/dup vs resampler) — need stats.
- Mac window: stats line + error text cut off at bottom (body fixed 540px, overflow hidden).
- Fixes in progress: level meters in stats, window layout, bundled .app with mic usage string.

## Next step
Verify engine build/tests, commit, then build app/ (Tauri tray) against the engine API, add CI.

## In flight (2026-09-29)
If a session resumes and these are missing, re-run them from MASTER §3.2 / §6.
