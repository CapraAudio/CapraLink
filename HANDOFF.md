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
1. Two-machine test: Mac runs target/release/capralink; PC runs capralinkd from CI artifacts (latest: https://github.com/CapraAudio/CapraLink/actions/runs/36623187667). Owner reports sound/latency/stats + window layout.

## Owner test results (2026-09-29, Mac app ↔ Linux capralinkd)
- Linux mic → Mac headphones: WORKS, latency "not much at all".
- Mac mic → Linux speakers: was SILENT; FIXED by bundled .app + NSMicrophoneUsageDescription (retest 2026-09-29: both directions work, Sending meter moves).
- (orig note) Mac mic → Linux speakers: SILENT. Suspect macOS mic permission (app launched from a terminal → TCC attributes to the terminal's host app; unbundled binary has no NSMicrophoneUsageDescription → silence).
- Occasional slight click. Cause unknown (underrun vs drift skip/dup vs resampler) — need stats.
- Mac window: stats line + error text cut off at bottom (body fixed 540px, overflow hidden).
- Fixed 2026-09-29: in/out peak meters (stats + UI + CLI dB), window 600px tall + scrollable, app/Info.plist with NSMicrophoneUsageDescription; .app built via `cargo tauri build --bundles app` (tauri-cli 2.12 installed in ~/.cargo/bin) → target/release/bundle/macos/CapraLink.app. Retest done: bidirectional OK. Mac stats (Linux→Mac): lost 0, ~9 underruns/40 s, buf swings 26–53 ms vs 20 ms target → bursty arrival (Linux ALSA capture ~40 ms periods). Fix: adaptive playout target + 10 ms capture buffer request. Retest after: underruns ~1/20 s, target grew to 66 ms, fill peaks 119 ms → 50–100 ms stalls (suspect Wi-Fi power save or Linux still old build). Owner: Linux had been old build. With new Linux: 0 underruns, target 20 ms, but fill 43–49 ms (≈0.3% clock drift beyond skip/dup capacity). → Replaced skip/dup with resampling drift control on low-water cushion (commit after this). Awaiting owner retest. Open: Mac stats line not visible to owner; clicks still undiagnosed.

## Next step
Verify engine build/tests, commit, then build app/ (Tauri tray) against the engine API, add CI.

## In flight (2026-09-29)
If a session resumes and these are missing, re-run them from MASTER §3.2 / §6.
