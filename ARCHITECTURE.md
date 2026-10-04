# CapraLink architecture

How CapraLink is built, how it protects your audio, and what it stores and shares.

## Pieces

| Path | What it is |
|---|---|
| `engine/` | The engine (`capralink_engine`): discovery, pairing, sessions, audio. Also `capralinkd`, a headless build of it. |
| `app/` | The tray app (Tauri 2). `capralink --daemon` runs the engine. Plain `capralink` is the window and tray, a client of that engine. |
| `app/ui/` | The window: one HTML page plus `ui.js`. Same layout on every OS. |
| `drivers/macos/` | The CapraLink Input/Output audio driver for macOS, derived from BlackHole. |

The engine runs in the background, as a login service if you turn that on. The window talks to it over a
loopback-only connection on port 47801. Both sides prove they know a per-user secret (`rpc.token`,
readable only by you) before anything else is exchanged.

## Audio path

capture device → resample to 48 kHz → Opus (10 ms frames, 20 ms in Music Mode) → encrypt → UDP
→ decrypt → jitter buffer → Opus decode (with FEC / loss concealment) → drift-corrected resample →
playback device

- **Bitrate:** the receiver reports loss and underruns every second. The sender adjusts its bitrate from
  8 to 96 kbps (up to 160 kbps in Music Mode), with the user's setting as the ceiling.
- **Encoder load:** Opus complexity follows how long encoding takes, so CapraLink stays light next to games.
- **Playout buffer:** it grows when the network is bursty and shrinks back when it calms down. Clock
  drift between the two computers is absorbed by tiny speed changes: at most 2%, or 0.5% in Music Mode.
- **Volume and mute:** per connection, 0–150% each way. Send volume is applied before encoding,
  receive volume before playback; above 100% loud peaks are bent softly towards full scale instead
  of clipping. Mute sends silence (the stream keeps running). Both change live, without reconnecting.
- **Push-to-talk:** per connection, Hold (talks while the key is held, plus 200 ms) or Toggle. While
  not talking the sender sends silence, the same way as mute. A short two-tone chirp plays on this
  computer when talking starts (rising) and stops (falling).
- **Delay readout:** each direction's one-way delay is estimated as capture buffer + one frame +
  half the control channel's round trip + playout buffer + playback buffer. The 1 s reports carry
  a timestamp echo for the round trip and each side's own part of the delay.

## Push-to-talk key listener

The engine listens for the push-to-talk key only while one is set for the current connection (or
while the window is waiting for "Set button"). It only listens: games and other apps still get
every key, and no low-level keyboard hooks are installed.

- **Linux:** reads the `/dev/input/event*` devices this user can read (controllers usually are;
  keyboards and mice need the `input` group).
- **Windows:** Raw Input on a hidden window, keyboard and mouse (middle, back and forward buttons).
- **macOS:** a listen-only event tap, which needs the Input Monitoring permission (macOS asks the
  first time).

Keys are compared with the chosen one in memory and never logged or sent anywhere.

## Virtual devices

- **macOS:** the HAL plug-ins in `drivers/macos` add **CapraLink Output** (a speaker apps play into)
  and **CapraLink Input** (a microphone apps record from).
- **Linux:** PipeWire/PulseAudio null sinks, plus a remapped source, created at start and removed at exit.
- **Windows:** VB-Cable stands in for CapraLink Input. "Everything this PC plays" uses WASAPI
  loopback, so no driver is needed for sending.

## Security model

- **Pairing:** each computer shows a 6-digit PIN.
  - The PIN only works for 2 minutes after you press Show.
  - A wrong PIN changes the PIN and locks pairing for a while. The lock doubles with each further miss, up to an hour.
  - Pairing runs SPAKE2 on the PIN, then each side proves it holds the same key (HMAC). The PIN never
    crosses the network, and a passive listener learns nothing from the exchange.
  - Both computers then store a 32-byte pairing secret derived with HKDF.
- **Sessions:** only paired computers can connect.
  - Each session opens with a Noise `NNpsk0_25519_ChaChaPoly_SHA256` handshake, keyed by the pairing secret.
  - The control channel (start/stop, reports, remote configuration) runs inside that handshake.
- **Audio:** fresh ChaCha20-Poly1305 keys per session and per direction, derived from the Noise
  handshake, which gives forward secrecy.
  - Packet headers are authenticated.
  - Replayed or forged packets are dropped.
  - A session ends before its packet counter could ever repeat a nonce.
- **Remote configuration:** off by default. When on, paired computers can change audio settings and
  the device name, but never the background-service or remote-configuration switches.
- **Limits:** incoming connections must finish their handshake within a few seconds, and only a few
  may be pending at once.

## Privacy

- **On the local network:** CapraLink announces itself over mDNS with its device name (your hostname by
  default; rename it in the window) and a random device ID. Nothing leaves your network.
- **Stored on disk:** settings, paired devices, and their pairing secrets, in
  `<config dir>/CapraLink/config.json`. That's `~/Library/Application Support` on macOS,
  `~/.config` on Linux and `%APPDATA%` on Windows. The file is readable only by you on macOS and Linux.
- **Log file:** `capralink.log` (plus one older `capralink.1.log`) in the same folder, capped at
  1 MB. It records events (start/stop, pairing results, sessions, device errors, a quality line every
  30 s while streaming), never audio, PINs, keys or tokens. It stays on your computer unless you
  export diagnostics (Troubleshooting → Export), which hides names and addresses by default. A paired
  computer can fetch these diagnostics only when remote configuration is on.
- **No telemetry,** no accounts. The only internet request is the update check: when the window
  opens, it asks GitHub's public API for the latest CapraLink release (a plain web request, nothing
  about you or your devices), and offers a Download link if a newer one exists. Audio and control
  traffic never leave your network.

## Known limits

- One connection at a time, and IPv4 only.
- `Node::shutdown` ends the session and announces departure, but doesn't join its listener threads.
  The binaries exit right after it.
