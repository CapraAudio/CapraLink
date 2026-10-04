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

<p align="center"><sub>Streaming to a Steam Deck with push-to-talk and Music Mode/Hi-Fi · a device's menu · changing another computer's settings remotely</sub></p>

## Features

- **Same app on every OS** — one tray app with the same window on macOS, Linux and Windows.
- **Pairing with a PIN** — computers find each other automatically; enter a 6-digit PIN once to pair.
  Not discovered? Pair by IP address.
- **Per-connection settings** — each computer remembers, for every paired device, what to send, where
  to play, volumes and push-to-talk, and switches automatically when you connect.
- **Low delay, light on resources** — Opus audio at 48 kHz with 10 ms frames; bitrate adapts from
  8 to 96 kbps as the network changes, so games aren't affected.
- **Music Mode** — stereo and higher quality (up to 160 kbps) for music, at the cost of a little more delay.
- **Music Mode/Hi-Fi** — lossless 24-bit audio on top of Music Mode, with automatic fallback when the network can't keep up.
- **Push-to-talk** — hold or toggle a key, mouse button or Steam Deck back grip; works while a game has focus.
- **Volume and mute** — send and receive volume (0–150%) and a mute button, per connection.
- **Delay readout** — see how much delay each direction has.
- **Steam Deck Game Mode** — runs in the background, with a Decky plugin in the Quick Access Menu.
- **Remote configuration** — when allowed, change another computer's devices and settings from this one.
- **Reconnects by itself** — after a Wi-Fi drop, an IP address change or a restart.
- **One-click updates** — the window shows its version and offers **Update now** when a newer release is out.
- **Troubleshooting built in** — connection quality, setup checks, a test sound, a microphone check and a
  diagnostics export (names and addresses hidden by default).
- **Keyboard and screen-reader friendly** — the whole window works without a mouse.
- **Encrypted** — pairing uses SPAKE2; every session is encrypted with fresh keys (Noise + ChaCha20-Poly1305).

## How to use it

**Pair two computers (once).** Open CapraLink on both. On the first, press **Show** next to its PIN
(pairing stays open for 2 minutes). On the second, find the first one under **Devices**, press
**Pair** and type the PIN. Not listed? Type its IP address and the PIN at the bottom of **Devices**.

**Connect.** Press **Connect** next to a paired device. Then choose:

- **Send from** — what this computer sends: a microphone, or **CapraLink Output** to send whatever
  apps play into it (on Windows, "Everything this PC plays").
- **Play to** — where the other computer's audio goes: your speakers or headphones, or **CapraLink
  Input** so apps (Discord, OBS, …) can use it as a microphone. **None** if this side only sends.

These choices are remembered for that device, so next time just press **Connect**.

**Volume and mute.** **Send volume** and **Receive volume** go from 0 to 150%. **Mute** sends silence
while keeping the connection up. Changes apply instantly.

**Push-to-talk.** Under **Sending**, press **Set button…**, then press the key or button you want:
a keyboard key, a mouse side button, or on a Steam Deck one of the back grips (L4, L5, R4, R5).
Choose **Hold to talk** (talks while held, plus 200 ms so word endings aren't cut) or **Press to
toggle**. You'll hear a short chirp when talking starts and stops; the other side hears silence
otherwise. It keeps working while a game has focus and never takes the key away from the game.

- **macOS:** allow CapraLink under System Settings → Privacy & Security → **Input Monitoring** when asked.
- **Windows:** nothing to set up. CapraLink only listens to input (no keyboard hooks).
- **Steam Deck:** the back grips work directly, with no Steam Input mapping.

**Music Mode and Music Mode/Hi-Fi.** Tick **Music Mode** for stereo, higher-quality audio (it applies to both
directions, and either computer can turn it on). Tick **Hi-Fi** next to it for **Music Mode/Hi-Fi**: lossless 24-bit
audio, about 2.3 Mbit/s each way and up to 1 s of delay. Lost packets are re-sent; if the network
can't keep up, it falls back to Music Mode by itself and tries again after a clean minute. Both
computers need CapraLink 0.3.0 or later for Music Mode/Hi-Fi.

**Delay and connection quality.** Under the window's controls you'll see the connection quality and
"You hear them: … ms · They hear you: … ms". Normal mode is usually 30–60 ms; Music Mode and Music Mode/Hi-Fi
keep a larger buffer on purpose.

**Steam Deck Game Mode.** In desktop mode, open CapraLink, press ⚙ and turn on **Run in background at login**,
then pair and set up your devices. Back in Game Mode, CapraLink keeps running. To control it there,
install the Decky plugin:

1. Install [Decky Loader](https://decky.xyz) if you haven't.
2. Download `CapraLink-Decky-<version>.zip` from the [Releases](../../releases) page to the Deck.
3. In the Quick Access Menu (**…** button) → Decky → ⚙: turn on **Developer mode**, then
   **Developer → Install Plugin from ZIP File** and pick the zip.

The CapraLink panel shows the connection and has Connect/Disconnect, Music Mode, Music Mode/Hi-Fi, Mute,
volumes, push-to-talk and the delay readout.

**Changing the other computer's settings.** On the other computer, press ⚙ and turn on **Allow paired
computers to change these settings**. Then use **⋯ → Configure** next to it here.

**Troubleshooting.** ⚙ → **Troubleshooting…** shows the connection quality, setup checks, a test
sound and a microphone check, and exports one diagnostics text file (names and addresses hidden
unless you untick that) to attach to a bug report.

## Install

Download from the [Releases](../../releases) page. Check a download against `SHA256SUMS.txt`
if you like (`shasum -a 256 <file>` / `certutil -hashfile <file> SHA256`).

| | Download | Supported |
|---|---|---|
| **macOS** | `CapraLink-<version>-macOS.pkg`: the app plus the CapraLink Input/Output audio driver | macOS 12.3 or later, Apple silicon and Intel |
| **Windows** | `CapraLink_<version>_x64-setup.exe` | Windows 10 and 11, 64-bit |
| **Linux** | `.AppImage` (any distro, incl. SteamOS), `.deb` or `.rpm` | x86-64 with PipeWire or PulseAudio |

The installers aren't signed by Apple or Microsoft, so the first launch needs one extra step:

- **macOS:** if the package won't open, go to System Settings → Privacy & Security and
  click **Open Anyway**. Allow microphone access when CapraLink asks.
- **Windows:** if SmartScreen appears, click **More info → Run anyway**. For a CapraLink microphone,
  install [VB-Cable](https://vb-audio.com/Cable/) (free). "Everything this PC plays" needs no driver.
  The installer isn't code-signed, so if **Smart App Control** is on, Windows may block it outright
  (it has no "run anyway" for this); CapraLink can't be installed on that PC until a release passes
  Microsoft's reputation check.
- **Linux:** make the AppImage executable (`chmod +x`). The virtual devices are created automatically.
  On a Steam Deck, see [Steam Deck Game Mode](#how-to-use-it) above.

**Updating:** when a newer version is out, the window shows **Update now**: one click downloads
it, checks its signature, installs it and restarts CapraLink. Not available for `.deb`/`.rpm`
installs, nor on macOS when a release changes the audio drivers; there, install the new download
the usual way.

Wired Ethernet gives the smoothest audio. Wi-Fi works, with a slightly larger buffer on busy networks.

To remove the macOS audio driver: `sudo drivers/macos/uninstall.sh` from this repository, or delete
`/Library/Audio/Plug-Ins/HAL/CapraLink*.driver` and restart.

## Status

Working day to day between macOS, a Steam Deck and Windows 11. Every push also builds the
installers: see the latest run in the [Actions](../../actions) tab.

## Building from source

Needs [Rust](https://rustup.rs) (the version is pinned in `rust-toolchain.toml`) and CMake. On
Linux, also the Tauri system packages (see `.github/workflows/ci.yml`).

```bash
cargo install tauri-cli --version 2.12.0 --locked
cd app && cargo tauri build
```

macOS virtual devices:

```bash
drivers/macos/build.sh && sudo drivers/macos/install.sh
```

How it works, the security model, and what CapraLink stores and shares: [ARCHITECTURE.md](ARCHITECTURE.md).
Reporting a security problem: [SECURITY.md](SECURITY.md).

## Versioning

CapraLink follows [Semantic Versioning](https://semver.org/): versions are MAJOR.MINOR.PATCH.

- **PATCH** (0.2.0 → 0.2.1): bug fixes only.
- **MINOR** (0.2.1 → 0.3.0): new features that keep working with what came before.
- **MAJOR** (0.x → 1.0.0, then 1.x → 2.0.0): a change that breaks the public API below.

The public API, meaning what a version number promises to keep compatible, is:

1. **Computers on different versions can pair and connect** (network protocol and remote configuration).
2. **Your settings and pairings survive an update** (`config.json`).
3. **The command-line options** of `capralinkd` and of `capralink` (below).

`capralink` itself is a small client of the running engine (it never opens a window or starts the engine, and exits non-zero with a message on stderr if it can't do what you asked):

```text
capralink --status              one JSON line: connected {id, name} or null, devices [{id, name, online, connected}],
                                music_mode, quality ("good"/"fair"/"poor" or null), error, mute, send_volume, recv_volume,
                                ptt, ptt_key (name or null), talking, ptt_error, hifi, hifi_active,
                                delay_in_ms, delay_out_ms (null when not connected)
capralink --connect NAME_OR_ID  connect to a paired device
capralink --disconnect
capralink --music on|off        Music Mode
capralink --hifi on|off         Music Mode/Hi-Fi (needs Music Mode)
capralink --mute on|off
capralink --volume send|recv N  0 to 150
capralink --ptt off|hold|toggle push-to-talk mode
capralink --ptt-set             wait up to 10 s for a key or button press, make it the push-to-talk
                                button (turning push-to-talk on in hold mode if it was off), print its name
```

Before 1.0.0, a breaking change bumps MINOR instead of MAJOR, and the release notes call it out.

## License

GPL-3.0 — see [LICENSE](LICENSE). The macOS driver is derived from
[BlackHole](https://github.com/ExistentialAudio/BlackHole) (GPL-3.0); see `drivers/macos/NOTICE`.
