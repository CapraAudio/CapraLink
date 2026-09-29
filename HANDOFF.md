# CapraLink — Session Handoff

Read `MASTER.md` first. This file is the live position.

## Current position
- Phase: M1 (engine) in progress, M0 (tray app) next.
- M1 engine being built (engine/ crate, delegated). Then M0 Tauri app + CI.

## Machine facts
- GitHub: private repo CapraAudio/CapraLink, branch main. Repo-local git identity set.
- No brew, no cmake, no pkg-config on the Mac; Xcode CLT present.
- Dev machine: macOS 26.6.2 (Apple Silicon assumed), project at `~/Documents/CapraLink`.
- Rust 1.98.1 via rustup in ~/.cargo (installed without PATH edit; use ~/.cargo/bin or `. ~/.cargo/env`).
- Test machines: Windows 10/11 PC and PipeWire Linux PC on the LAN (D10).

## Pending owner actions / questions
None blocking. Rounds 1–2 answered (D3–D10).

## Next step
Verify engine build/tests, commit, then build app/ (Tauri tray) against the engine API, add CI.
