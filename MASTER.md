# CapraLink — Master Plan

Living document. The source of truth for goals, architecture, decisions and progress.
A new session reads this file, then `HANDOFF.md`, and can continue from there.

---

## 1. Goal

Share audio inputs and outputs between macOS, Linux and Windows machines over a LAN.

**Canonical use case:** Computer A is in a Discord call. From Computer B you hear the call and
speak into it, while still hearing B's own system audio mixed in.

**How that works:**

```
Computer A (host of the call)                 Computer B (where you sit)
─────────────────────────────                 ──────────────────────────
Discord output → "CapraLink Output" ──LAN──▶  played on B's normal speakers/headset
                                               (the OS mixes it with B's own audio)
Discord input  ← "CapraLink Input"  ◀──LAN──  captured from B's real microphone
```

A needs the virtual devices. B just uses its real devices. Every install ships both roles, so
any machine can be A or B.

## 2. Requirements (from the owner)

| # | Requirement | Status |
|---|---|---|
| R1 | macOS, Linux, Windows; same UI layout everywhere | Planned |
| R2 | Tray-based app | Planned |
| R3 | Pair devices with a PIN | Planned |
| R4 | Can run as a background service; remotely configurable when that option is on | Planned (see D6) |
| R5 | Virtual devices named exactly **CapraLink Input** and **CapraLink Output** | Planned (hardest part, see §4) |
| R6 | Low latency | Target ≤ 60 ms mouth-to-ear on wired LAN |
| R7 | Low CPU/RAM so gaming is not affected | Target < 1% of one core, < 40 MB RAM while streaming (UI closed) |
| R8 | Adaptive 8–96 kbps by CPU + network | Changed to 48 kHz (D3) |
| R9 | Written in Rust, suitable cross-platform frontend | Planned (see D1) |
| R10 | Keep the code minimal (ponytail) | Ongoing |

## 3. Architecture

One Rust program, two modes:

- **Tray mode** (default): tray icon + a small settings window.
- **Service mode** (`capralink --service`): same engine, no UI, starts at login.

```
┌─────────────────────────── capralink ───────────────────────────┐
│  UI (Tauri, only loaded when window is open)                    │
│  ───────────────────────────────────────────────────────────── │
│  Engine                                                         │
│   audio I/O ── Opus encode/decode ── jitter buffer              │
│   discovery (mDNS) ── pairing (PIN → PAKE) ── encrypted UDP     │
│   control channel (config, remote config)                       │
└─────────────────────────────────────────────────────────────────┘
        │ shared-memory ring buffer
┌───────┴───────────────────────┐
│ Virtual device (per OS)       │  shows up as "CapraLink Input/Output"
└───────────────────────────────┘
```

| Piece | Choice | Why |
|---|---|---|
| Real device audio | `cpal` crate | One API over CoreAudio / WASAPI / ALSA+PipeWire |
| Codec | Opus (libopus via `opus` crate) | Built for this: 6–510 kbps, low delay, FEC for packet loss |
| Frame size | 10 ms | Low latency; cheap at these bitrates |
| Transport | UDP, ChaCha20-Poly1305 per packet | Lowest latency; loss handled by Opus FEC/PLC |
| Discovery | mDNS (`mdns-sd`) | Zero config on a LAN |
| Pairing | 6-digit PIN → SPAKE2 → long-term keys saved | PIN can't be brute-forced offline; pair once |
| UI | Tauri 2 + plain HTML/CSS/JS (no framework) | Rust backend, native tray on all 3 OSes, identical layout |
| macOS virtual device | Core Audio HAL plug-in (AudioServerPlugIn), small C++ | The only supported way to add a device on macOS |
| Linux virtual device | PipeWire/PulseAudio virtual sink + source, created at runtime | Built into the OS, no driver |
| Windows virtual device | Existing open-source signed driver (D4) | Requires a kernel driver |

### 3.1 Repo layout

```
Cargo.toml        workspace: engine, app
engine/           audio engine library + `capralinkd` headless CLI (service mode, testing)
app/              Tauri tray app (depends on engine); UI in app/ui/ (plain HTML/JS)
.github/workflows CI: build + test on macOS, Linux, Windows
```

### 3.2 Engine data flow (M1)

A node runs one **TX** stream and one **RX** stream. Which devices they use is config:

| Node | TX captures from | RX plays into |
|---|---|---|
| A (runs Discord) | CapraLink Output (virtual) | CapraLink Input (virtual) |
| B (you) | real mic | real speakers/headset |

- **TX:** cpal input callback → f32 → mix to configured channels → resample to 48 kHz if needed →
  accumulate 10 ms frames → Opus encode (complexity 5, FEC on) → UDP send. All in the callback.
- **RX:** recv thread → packet seq gap? decode next packet's FEC for the missing one, else PLC →
  decode → push PCM into lock-free ring. Playback callback pulls from the ring.
- **Jitter/clock drift:** ring target fill 20 ms. Underrun → silence + re-prebuffer.
  Fill drifting beyond ±5 ms of target → skip/duplicate single samples (inaudible), which
  absorbs clock-rate differences between machines.
- **Packet (M1, unencrypted):** `magic u16 | version u8 | channels u8 | seq u32 | opus bytes`.
  Encryption wraps this in M2.
- Default UDP port 47800.

## 4. Feasibility notes (owner asked for honest feedback)

1. **Virtual devices on Windows are the hardest part.** Windows has no user-mode way to create
   an audio device. It needs a kernel driver, and Windows 10/11 only load drivers signed through
   Microsoft (needs an EV code-signing certificate, roughly $250–500/yr, plus a Microsoft
   Partner Center account). Options are in the open questions.
2. **macOS virtual devices are doable** but the installer needs admin rights once, and
   distributing outside your own machines needs an Apple Developer account ($99/yr) for
   signing + notarization, otherwise Gatekeeper blocks it.
3. **Linux is easy** — PipeWire/PulseAudio can create named virtual devices with no driver.
4. **16 kHz is great for voice, poor for game/music audio.** 16 kHz cuts everything above
   8 kHz (sounds like a phone call). Also, 64 kbps at 16 kHz is more bits than that bandwidth
   can use. Recommendation: run Opus at 48 kHz and let the adaptive bitrate (8–96 kbps) decide
   quality — Opus automatically narrows bandwidth when bitrate drops. CPU cost difference is
   negligible.
5. **"Service" on Windows/macOS can't be a true system service.** System services run outside
   your login session and cannot reach your audio devices. Service mode will be a login agent
   (launchd LaunchAgent / Windows startup task / systemd user unit): headless, auto-start,
   no window.
6. **Echo:** if B plays the call through speakers, B's mic hears it and the call hears itself.
   Discord's echo cancellation on A can't fix this. Headphones on B, or an optional echo
   canceller on B's mic (costs some CPU).
7. **Latency budget (wired LAN):** 10 ms frame + ~20 ms jitter buffer + ~10–20 ms device
   buffers each side ≈ 40–60 ms. Wi-Fi adds jitter; the buffer grows adaptively.

### 4.1 Virtual device research (2026-09-29)

- **macOS:** BlackHole (GPL-3.0, compatible with D8) supports compile-time renaming
  (`kDriver_Name`, `kPlugIn_BundleID`, `kNumber_Of_Channels`). Each build is one loopback device,
  so we ship two builds: "CapraLink Input" and "CapraLink Output". Installs to
  `/Library/Audio/Plug-Ins/HAL`, then coreaudiod restart (admin once). Distribution to others
  needs Developer ID signing + notarization.
- **Linux:** PipeWire/Pulse `module-null-sink` (Audio/Sink for Output, `media.class=Audio/Source/Virtual`
  for Input), created at runtime via `pactl`, or persisted in `~/.config/pipewire/pipewire.conf.d/`.
  cpal has `pipewire`/`pulseaudio` features for opening nodes by name.
- **Windows:** VirtualDrivers/Virtual-Audio-Driver (MIT, speaker + mic) is beta, has no releases,
  and needs **test-signing mode** — which major anti-cheats (Vanguard, FACEIT, EAC) refuse to run
  under. Scream is speaker-only and also test-signed. VB-Cable is signed but closed and not
  renameable. **No off-the-shelf signed OSS option exists → D4 must be revisited.**
- **Driverless capture exists for the Output side:** Windows 10 2004+ process loopback
  (WASAPI `PROCESS_LOOPBACK`) and macOS 14.2+ Core Audio process taps can capture a chosen app's
  audio (e.g. Discord) with no virtual device. The **Input side (virtual mic) always needs a driver.**

## 5. Decisions log

| ID | Date | Decision | Reason |
|---|---|---|---|
| D1 | 2026-09-29 | UI = Tauri 2 + vanilla HTML/JS; window destroyed when closed | Same layout on all OSes; zero UI cost while gaming |
| D2 | 2026-09-29 | Codec = Opus, 10 ms frames, FEC on | Industry standard for low-latency voice/audio |
| D3 | 2026-09-29 | Opus @ 48 kHz, adaptive 8–96 kbps (start 64 kbps) | 16 kHz too muffled for game/music; owner agreed |
| D4 | 2026-09-29 | Windows: fork VirtualDrivers/Virtual-Audio-Driver (MIT), rename devices to CapraLink Input/Output, test-sign ourselves; users enable test-signing (`bcdedit /set testsigning on`, Secure Boot off) | Owner choice over phased/driverless. Known cost: anti-cheat games (Vanguard, FACEIT, EAC) won't run on that Windows machine. Upgrade path: attestation signing with EV cert |
| D5 | 2026-09-29 | UDP + per-packet encryption keyed by PIN pairing | Low latency, LAN-safe |
| D6 | 2026-09-29 | "Service" = headless login agent, not system service | OS audio is per-user-session |
| D7 | 2026-09-29 | One peer at a time (1:1 link) | Covers the use case; simplest |
| D8 | 2026-09-29 | Open-source release, GPL-3.0 | Owner choice; lets us reuse GPL drivers (e.g. BlackHole on macOS) |
| D9 | 2026-09-29 | No echo canceller; headphones required on B | Owner choice; zero extra CPU |
| D10 | 2026-09-29 | Test rigs: this Mac + a Windows 10/11 PC + a PipeWire Linux PC on the LAN | Owner has them |

## 6. Milestones

| M | Scope | Done when |
|---|---|---|
| M0 | Toolchain, repo, Tauri tray skeleton on macOS | Tray icon runs on this Mac |
| M1 | Engine: mic → Opus → UDP → speaker, two hard-coded peers | Hear yourself across two machines; latency measured |
| M2 | mDNS discovery, PIN pairing, encryption, saved pairings | Pair two machines from the UI |
| M3 | Virtual devices: Linux, then macOS HAL plug-in | Discord on A can pick CapraLink Input/Output |
| M4 | Virtual devices: Windows — fork + rename + test-sign VirtualDrivers driver, built in CI (D4) | Same, on Windows |
| M5 | Adaptive bitrate (loss/RTT/CPU), jitter tuning | Holds quality on Wi-Fi; CPU target met |
| M6 | Service mode + remote configuration | Configure a headless machine from another |
| M7 | Installers + signing for all 3 OSes | One-click install per OS |

## 7. Open questions

See `HANDOFF.md` → "Pending owner actions" for the live list. Answers get moved into §5.

## 8. Progress log

| Date | Change |
|---|---|
| 2026-09-29 | Project started. Master plan written. No Rust toolchain on the Mac yet. |
| 2026-09-29 | Owner answered round 1: D3, D4, D7, D8 recorded. |
| 2026-09-29 | Private repo created: github.com/CapraAudio/CapraLink. Git identity = Capra Audio (GitHub noreply). |
| 2026-09-29 | Round 2: GPL-3.0, no AEC, test rigs recorded (D8–D10). Rust install approved. |
