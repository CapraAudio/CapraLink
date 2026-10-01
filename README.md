<p align="center">
  <img src="app/icons/128x128@2x.png" width="128" alt="CapraLink icon">
</p>

<h1 align="center">CapraLink</h1>

<p align="center">
  Share microphones and speakers between computers on your home network.<br>
  macOS · Linux (incl. Steam Deck) · Windows
</p>

---

## What it does

CapraLink connects two computers on the same network and streams audio between them, in both
directions, with low delay. Each side picks what it sends (a microphone, or everything an app plays)
and where incoming audio goes (headphones, or a virtual microphone other apps can use).

It installs two virtual audio devices on each computer:

- **CapraLink Output** — a speaker that apps can play into. Whatever goes in is sent to the other computer.
- **CapraLink Input** — a microphone that apps can record from. Whatever the other computer sends comes out of it.

So, for example, Discord on one computer can use the other computer's headset as if it were plugged in.

## Why it exists

It started with two everyday setups:

- **Discord on the Mac, game on the Steam Deck.** The voice call runs on the MacBook, while the
  game is played on the Steam Deck across the room. The Deck should hear the call and talk into it,
  and still hear its own game.
- **The MacBook's microphone on the Windows desktop.** The desktop borrows the laptop's mic, and
  nothing needs to come back.

Cables, USB switches and audio mixers can do this, but they are fiddly and stop at the desk.
Existing network-audio tools tend to be one-way, single-platform, or too heavy to leave running
next to a game. CapraLink is meant to be set up once, stay in the tray, and get out of the way.

## Screenshots

<p align="center">
  <img src="docs/screenshots/main.png" width="260" alt="Main window while streaming">
  <img src="docs/screenshots/device-menu.png" width="260" alt="Device menu">
  <img src="docs/screenshots/configure.png" width="260" alt="Configuring another computer remotely">
</p>

<p align="center"><sub>Streaming to a Steam Deck · a device's menu · changing another computer's settings remotely</sub></p>

## Features

- **Same app on every OS** — one tray app with the same window layout on macOS, Linux and Windows.
- **Pairing with a PIN** — computers find each other automatically; enter a 6-digit PIN once to pair.
  Not discovered? Pair by IP address.
- **Per-connection settings** — each computer remembers what to send and where to play for each
  paired device, and switches automatically when you connect.
- **Low delay, light on resources** — Opus audio at 48 kHz with 10 ms frames; bitrate adapts from
  8 to 96 kbps as the network changes, so games aren't affected.
- **Music Mode** — stereo and higher quality (up to 160 kbps) for music, at the cost of slightly more delay.
- **Runs in the background** — optional login service, including Steam Deck Game Mode, with no window open.
- **Remote configuration** — when allowed, change another computer's devices and settings from this one.
- **Reconnects by itself** — after a Wi-Fi drop, an IP address change or a restart.
- **Encrypted** — pairing uses SPAKE2; every session is encrypted with fresh keys (Noise + ChaCha20-Poly1305).

## Platform notes

| | Virtual devices | Notes |
|---|---|---|
| **macOS** | Built-in audio driver (based on BlackHole), installed once with `sudo drivers/macos/install.sh` | Grant microphone access on first run |
| **Linux** | Created automatically with PipeWire / PulseAudio | Works in Steam Deck Game Mode with the background service on |
| **Windows** | Uses [VB-Cable](https://vb-audio.com/Cable/) (free) as CapraLink Input; "Everything this PC plays" captures system audio without a driver | Pick "CABLE Output" as the microphone in your apps |

Wired Ethernet gives the smoothest audio; Wi-Fi works, with a slightly larger buffer on busy networks.

## Status

Working day to day between macOS, a Steam Deck and Windows 11. One-click installers and signed
releases are still to come. Until then, build from source or grab the latest build from the
[Actions](../../actions) tab (macOS app, Linux AppImage/deb/rpm, Windows exe).

## Building from source

Needs [Rust](https://rustup.rs) and CMake. On Linux, also the Tauri system packages (see
`.github/workflows/ci.yml`).

```bash
cargo install tauri-cli --version "^2"
cd app && cargo tauri build
```

macOS virtual devices:

```bash
drivers/macos/build.sh && sudo drivers/macos/install.sh
```

## License

GPL-3.0 — see [LICENSE](LICENSE). The macOS driver is derived from
[BlackHole](https://github.com/ExistentialAudio/BlackHole) (GPL-3.0); see `drivers/macos/NOTICE`.
