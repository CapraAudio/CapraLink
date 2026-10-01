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
- **Jitter/clock drift:** controller steers the ring's low-water cushion (min over 0.5 s, after the
  callback's take) to a target = max(10 ms, measured jitter + 5 ms) + underrun boost. Jitter = worst
  packet lateness (arrival gap − 10 ms) over the last 5–10 s, measured in the RX thread. Underrun adds
  a 10 ms boost that fades 1 ms/s. Drift absorbed by resampling playback ±≤2% (P-control, τ≈2 s) —
  no sample skip/dup. Capture requests 10 ms device buffers when the range allows.
- **Packet (M1, unencrypted):** `magic u16 | version u8 | channels u8 | seq u32 | opus bytes`.
  Encryption wraps this in M2.
- Default UDP port 47800.

### 3.3 Discovery, pairing, encryption (M2 design)

**Pieces:** one `Node` per process (tray app and `capralinkd` both use it) owning: persistent
config, mDNS advertise+browse, a TCP control listener, and at most one `Link` (D7).

- **Config file** `<OS config dir>/CapraLink/config.json` (`dirs::config_dir()`):
  `device_id` (random 128-bit hex), `name` (hostname default), audio settings
  (input, output, bitrate, channels), `peers: [{id, name, secret}]` (secret = 32-byte hex).
  Written atomically (temp file + rename). Secrets never leave the file except as keys.
- **Discovery:** mDNS `_capralink._udp.local.` with TXT `id`, `name`, `v=2`; port = control/audio
  port (47800). Browse list = discovered devices, marked paired/unpaired.
- **Control channel:** TCP on the same port number. Frames: `u16 length | payload`.
- **PIN:** each node shows a standing 6-digit PIN (window; `capralinkd` prints it). Regenerated on
  every start and after each successful pairing. 5 failed attempts → PIN rotates.
- **Pairing (initiator types the target's PIN):** TCP → cleartext hello {id, name} both ways →
  SPAKE2 (symmetric, password = PIN, identity = "capralink-pair-v1") → key confirmation (each side
  sends HMAC-SHA256(K, "confirm"|role|both ids)) → pairing secret = HKDF-SHA256(K, info
  "capralink pairing secret") → both store {peer id, name, secret}.
- **Session (connect):** initiator TCP → cleartext hello {id} → Noise `NNpsk0_25519_ChaChaPoly_SHA256`
  with psk = pairing secret (unknown id → close). Inside the encrypted channel: JSON messages,
  first `{"type":"link","channels":N}` → responder auto-accepts (paired peers are trusted), starts its
  own `Link` back to the initiator's address, replies `{"type":"ok"}`. Either side sends
  `{"type":"stop"}` or closes TCP → both stop. Control connection stays open for the session
  (M6 remote config rides on it).
- **Audio encryption:** per-session keys from Noise's raw split (includes ephemeral DH → forward
  secrecy): key_dir = HKDF-SHA256(ikm = split key for that direction, salt = handshake hash,
  info "capralink audio v2"). (Originally derived from hh alone — no forward secrecy; fixed 2026-09-29.) ChaCha20-Poly1305, nonce = 8 zero bytes | seq u32 BE.
  Fresh keys every session → seq-as-nonce never repeats. Packet v2:
  `magic u16 | version u8 (2) | seq u32 | AEAD(channels u8 | opus)` with the 7-byte header as AAD.
  Packets failing auth are dropped silently. UDP from any address other than the session peer ignored.
- **Manual fallback (added 2026-09-29, owner's Wi-Fi drops Mac→Deck multicast):** "Pair by IP address"
  (`pair_ip`), and each peer's last working control address is stored (`Peer.addr`, pair hello carries the
  initiator's port) so Connect works without discovery.
- **Not in M2:** multiple simultaneous peers (D7),
  remote config (M6).

### 3.4 Virtual devices (M3 design)

Settings keep using device *names*. Two names are special and mean the virtual devices:
`Settings.input = "CapraLink Output"` → capture what apps play into CapraLink Output (the A role),
`Settings.output = "CapraLink Input"` → feed CapraLink Input, which apps record from. The engine maps
these to the real per-OS device names in one small function; the UI lists them first with a hint.

- **Linux:** cpal `pulseaudio` feature (pure-Rust client; default host when a Pulse/PipeWire-pulse
  server runs, ALSA fallback). Node start creates, via the Pulse protocol (or `pactl` fallback):
  `module-null-sink sink_name=capralink_output` (description "CapraLink Output"; we capture its
  `.monitor`), and `module-null-sink sink_name=capralink_input_feed` (description
  "CapraLink Input (internal)") + `module-remap-source master=capralink_input_feed.monitor
  source_name=capralink_input` (description "CapraLink Input"). Modules unloaded on shutdown;
  leftovers from a crash are reused/replaced on start.
- **macOS:** BlackHole (GPL-3) built twice as HAL plug-ins "CapraLink Output" / "CapraLink Input",
  2 ch, 48 kHz, source in `drivers/macos/`, built with clang (no Xcode needed), installed to
  `/Library/Audio/Plug-Ins/HAL` by a script the owner runs with sudo (+ coreaudiod restart).
  Device names equal the special names, so no mapping. Directions exposed to apps: as narrow as
  BlackHole's flags allow while cpal can still open our side.
- **Windows:** M4 (test-signed VirtualDrivers fork, D4).

### 3.5 Adaptive bitrate and CPU (M5 design)

- **Feedback:** each side's node sends `{"type":"report","received","lost","underruns","jitter_ms"}`
  (deltas since the last report) over the existing encrypted control channel every 1 s. Old peers
  ignore unknown messages (already handled).
- **Bitrate controller (sender, pure fn in dsp.rs, AIMD):** on each report from the peer:
  loss > 5 % or new underruns → bitrate × 0.7; loss < 1 % and no underruns → +8 kbps.
  Clamped to [8 kbps, the user's Bitrate setting] — the slider is now the ceiling, not a fixed value.
  Opus `packet_loss_perc` follows measured loss (0–30 %) so FEC scales with loss; at lower bitrates
  Opus switches to SILK/hybrid where in-band FEC is actually effective (see M1 note).
  Applied by the capture callback via atomics (`set_bitrate` only when the value changes).
- **CPU controller (sender, in the capture path):** EMA of Opus encode time per 10 ms frame. EMA >
  1.5 ms (15 % of the frame budget — the machine is loaded, e.g. a game) → complexity −1 (min 0);
  EMA < 0.3 ms for ~5 s → +1 (max 5). Bitrate is not a CPU lever for Opus.
- **Visibility:** Stats gain `bitrate` (current kbps) and `complexity`; UI stats line shows them.

### 3.6 Service mode and remote configuration (M6 design)

**M6a — engine/UI split + service mode.**
- One binary per OS (`capralink`, the Tauri app). `capralink --daemon` runs the headless engine:
  the `Node` plus a local RPC server; it returns before any Tauri/GTK/webview init, so it runs
  without a display (SteamOS Game Mode, login agents). `capralinkd` stays as a dev/CLI tool.
- Default launch = UI client: tray + window. On start it connects to the local daemon; if none
  answers it spawns `<self> --daemon` detached and retries for ~3 s. The UI never embeds a Node.
- **Local RPC:** TCP 127.0.0.1:(port+1, default 47801), one JSON request/response per connection:
  `{"token","cmd","args"}` → `{"ok":…}|{"err":"…"}`. Token = 32 random bytes hex in
  `<config dir>/rpc.token` (0600), created by the daemon; the UI reads it. Commands mirror Node:
  state, devices, pair, connect, disconnect, forget, set_settings, set_service, shutdown.
- **Service toggle** (`Settings.service: bool`, UI "Run in background at login"): the daemon
  installs/removes a login agent that runs `<exe> --daemon` (on Linux AppImage: `$APPIMAGE`, not the
  ephemeral mount path). macOS `~/Library/LaunchAgents/com.capraaudio.capralink.plist` (RunAtLoad,
  KeepAlive on crash only); Linux `~/.config/systemd/user/capralink.service` (enable, Restart=on-failure);
  Windows `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\CapraLink`. Takes effect next login
  (the running daemon keeps running).
- **Quit** in the tray: service off → also shut the daemon down; service on → only the UI exits.

**M6b — remote configuration.** `Settings.remote_config: bool` (default off; UI checkbox "Allow paired
computers to change these settings"). A paired peer opens a normal Noise session but sends
`{"type":"manage"}` instead of `link`; it never touches the target's audio session. If the target has
remote_config off → `error` "remote configuration is off on <name>" and close. Otherwise requests on
that channel: `get_config` → `{name, settings, inputs, outputs}` (target's own device lists) and
`set_settings {settings}` → applied via the normal set_settings path, EXCEPT `service` and
`remote_config` keep the target's local values (only changeable locally). One request per connection;
the initiator dials fresh each time (addrs incl. remembered address). RPC/App: `remote_get {id}`,
`remote_set {id, settings}`. UI: "Configure" on paired + reachable devices opens a panel with that
device's Send from / Play to / Channels / Bitrate.

### 3.7 Music Mode (design, owner-approved answers 2026-09-29)

**What it is:** a link-wide high-quality mode. `Settings.music_mode: bool` per machine; a link runs in
Music Mode when **either** side has it on (both directions switch together — owner choice).

| Dial | Normal | Music Mode |
|---|---|---|
| Channels | user's Mono/Stereo | forced stereo |
| Opus frame | 10 ms | 20 ms (better quality per bit, +10 ms) |
| Opus signal hint | auto | music (bandwidth auto: fullband at music rates, narrows only under back-off) |
| Bitrate ceiling | Bitrate slider (≤ 96 kbps) | 160 kbps |
| Bitrate floor on bad network | 8 kbps (AIMD) | same — stay smooth (owner choice) |
| Encoder complexity max | 5 | 10, same CPU back-off under load |
| Receive cushion minimum | 10 ms | 40 ms (+30 ms; owner accepted +40–60 ms total) |
| Playback speed correction (drift/cushion) | ≤ 2% | ≤ 0.5% (music is pitch-sensitive; owner heard 2% as "slower") |
| Cushion ceiling / stall memory / step per hiccup | 125 ms / 5–10 s / +10 ms | 300 ms / 30–60 s / +30 ms (owner: prefer more delay over touching Wi-Fi settings; Deck Wi-Fi stalls 100–150 ms every ~12 s) |

**Signalling:** a new control message `{"type":"mode","music":bool}` sent at session start and whenever
the local setting changes; each side computes `effective = local || peer` and applies it to its own
sender (frame/channels/bitrate ceiling/complexity) and receiver (cushion). Old peers ignore the message
(treated as off). Switching is live — no reconnect: the sender rebuilds its Opus encoder on the next
frame (seq continues, so nonces never repeat); the receiver already recreates its decoder on a channel
change and must size PLC/FEC to the incoming frame duration (10 or 20 ms).

**UI:** "Music Mode" checkbox in the main window (next to Channels/Bitrate; the Mono/Stereo control and
bitrate label show the forced values while it's on), in the remote Configure panel, and as a checkable
item in the tray menu. Status pill just says "Streaming" / "Idle" (owner request).

### 3.8 Windows (M4 plan, owner answers 2026-09-29)

Driverless first (owner choice; keeps Secure Boot and anti-cheat working). Test PC: Windows 11, owner
is admin, remote access via Windows' built-in OpenSSH Server. Role: both A and B.

- **M4a — native smoke test as B** (no new code): CapraLink.exe runs, window + tray, pair with the Mac/Deck,
  audio both ways, service Run key, firewall prompt. SSH sessions run outside the user's desktop session,
  so the app is launched in the interactive session via a scheduled task (`schtasks /create … /it` +
  `schtasks /run`).
- **M4b — driverless capture ("CapraLink Output" side):** WASAPI process loopback (Windows 10 2004+,
  `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, include the process
  tree) and whole-system loopback of the default render device. Send-from list on Windows gains
  "<app> (app audio)" entries for running processes with audio sessions (e.g. Discord) and
  "Everything this PC plays". Settings stores the choice by process name (`app:Discord.exe`) so it
  survives restarts. Implemented with the `windows` crate (already a cpal dependency).
- **M4c — virtual mic via VB-Cable (if installed):** Play-to "CapraLink Input" maps to the "CABLE Input"
  render device; the hint tells the user to pick "CABLE Output" as the microphone in Discord. If VB-Cable
  isn't installed the entry is absent and the UI links to vb-audio.com. Nothing bundled (licence).
- **Later (optional):** own signed driver (D4 path) only if VB-Cable proves insufficient.
- Installer (NSIS/MSI via the Tauri bundler) belongs to M7.

### 3.9 Auto-reconnect and address editing (design, owner-approved 2026-10-01)

- `Settings.auto_reconnect: bool` (default **true**; UI "Reconnect automatically" in the background
  section; remote config may change it). Config gains `last_peer: Option<String>` (device id).
- **Who retries:** only the side that initiated the session (avoids both sides dialing and replacing
  each other's sessions). `last_peer` is set when this node's own `connect` succeeds and cleared on a
  manual `disconnect`, `forget`, or when the peer sends `stop` (the other side chose to end it).
- **When:** a session lost without a stop (keepalive timeout / TCP error) and auto_reconnect on → a
  retry loop in its own thread: dial `addrs(id)` (mDNS addresses + remembered `addr`) after 2 s, then
  backoff ×2 up to 30 s, forever, until connected, the user acts (connect elsewhere / disconnect /
  forget), auto_reconnect is turned off, or the node shuts down. At node start, if `last_peer` is set
  and auto_reconnect on → the same loop (covers app restarts and reboots).
- **Visible state:** `Device.reconnecting: bool`; UI row status "Reconnecting…"; status pill stays Idle.
- **Edit address:** `Node::set_peer_addr(id, addr)` — paired peers only; accepts ip / ip:port / host
  (default port 47800), stores `Peer.addr`. No PIN: the Noise handshake with the stored secret still
  authenticates the peer. RPC `set_peer_addr {id, addr}`; UI "Edit address" link on paired rows → inline
  input + Save/Cancel (poll-safe like the other inline editors).
- Responder side unchanged: it already records the initiator's current IP on each session.
- As built: node shutdown and lost sessions do NOT send `stop` (so a peer's quit/reboot reads as a loss and is retried); an incoming session clears the receiver's `last_peer` (only the dialer owns reconnecting). Retry loop = generation counter + Condvar; at most one live.

### 3.10 Per-connection audio settings (owner-approved 2026-10-01, built)

Owner examples: Deck↔Mac = Discord on the Mac, game on the Deck; Mac→Windows = Windows uses the
Mac's mic, nothing comes back. Answers: each side stores its own half; the main window's
Sending/Receiving sections edit the current connection; per-connection = Send from, Play to,
Channels, Bitrate. Music Mode, service, remote config, auto-reconnect stay per-computer.

- **Storage:** `Peer.audio: Option<Audio>` where `Audio { input, output, channels, bitrate }`
  (serde default → old configs load unchanged). `Settings` keeps those four fields as the *working
  copy* = the current connection's audio, so the UI, RPC, and `start_link` don't change.
- **Current connection:** the session's peer, else the last device connected to (either direction);
  config gains `current: Option<String>` (separate from `last_peer`, which only the dialer owns).
- **Session start (both directions, the one `start_link` call site):** if the peer has `audio`, copy it
  into the working settings before starting the link; if not, save the working settings as its
  `audio` (a new pairing starts from whatever is set now). `current` = that peer.
- **`set_settings`:** unchanged, plus the four audio fields are also written to `current`'s `audio`.
- **Remote config:** a `set_settings` from peer X updates X's `audio` (it is configuring "how you talk
  to me"), and the working copy only if X is current; `GetConfig` from X returns X's audio. Non-audio
  fields behave as today.
- **UI:** section titles show which connection is being edited ("Sending to Steam Deck"); nothing else
  moves. Forget deletes that peer's audio with it.
- **Tests:** two peers with different saved audio → connect to each, check the link's settings; remote
  set from X lands in X's profile only; old config without `audio` loads.
- **Missing device** (saved mic unplugged): same as today's behavior for a missing device.
- As built: `use_peer` in `activate` (both directions); `on_manage(ctl, id)`; `NodeState.current`; UI reloads the selects when `current` changes. Re-pairing a device resets its saved audio. Test `audio_settings_are_per_connection`.

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
| D20 | 2026-10-01 | Per-connection audio: each computer saves Send from / Play to / Channels / Bitrate per paired device; main window edits the current connection; Music Mode stays per-computer | Owner switches setups by partner (Deck↔Mac Discord+game, Mac mic→Windows one-way) |
| D19 | 2026-10-01 | Auto-reconnect by the initiating side only (backoff 2→30 s, forever; also after restart via last_peer); paired peers' addresses editable without a PIN | IP changes / Wi-Fi drops / reboots shouldn't need a click; initiator-only avoids dueling reconnects |
| D18 | 2026-10-01 | Per-app (process loopback) capture removed; Windows Output side = "Everything this PC plays" | Owner: VB-Cable + Everything cover the use case; less unsafe Win32 code |
| D17 | 2026-09-29 | Windows goes driverless first (supersedes D4 for now): per-app / whole-system WASAPI loopback for the Output side; VB-Cable (user-installed, signed) for the Input side | Keeps Secure Boot + anti-cheat; no signing cost; D4 test-signed driver kept as a fallback |
| D16 | 2026-09-29 | Tray: single click shows the menu on all OSes (owner choice); double-click opens the window where reported (Windows only) | Keeps Quit/Music Mode discoverable on macOS |
| D15 | 2026-09-29 | Music Mode = link-wide (either side on → both directions): stereo, 20 ms frames, 160 kbps ceiling, complexity 10 w/ back-off, 40 ms min cushion; AIMD floor unchanged | Owner answers (quality over latency, smooth over pristine on bad Wi-Fi) |
| D14 | 2026-09-29 | Device names: each computer has an editable name (config.name) that peers see (mDNS TXT + stored peer name refresh); changeable locally or via remote config | Owner chose global rename over per-machine nicknames |
| D13 | 2026-09-29 | M6: single binary with `--daemon` engine mode + UI client over token-authenticated loopback RPC; service = OS login agent running the daemon | Game Mode/headless support, zero webview while gaming, one download per OS, keeps macOS mic permission |
| D12 | 2026-09-29 | M5: network drives bitrate (AIMD on receiver loss/underrun reports, slider = ceiling); CPU drives Opus complexity (encode-time EMA) | Opus bitrate ≠ CPU cost; complexity is the real CPU knob |
| D11 | 2026-09-29 | M2 security: SPAKE2 PIN pairing → stored secret; Noise NNpsk0 control channel; per-session ChaCha20-Poly1305 audio keys; standing PIN on target, auto-accept from paired peers | Standard, audited crates; no PKI; headless-friendly |
| D8 | 2026-09-29 | Open-source release, GPL-3.0 | Owner choice; lets us reuse GPL drivers (e.g. BlackHole on macOS) |
| D9 | 2026-09-29 | No echo canceller; headphones required on B | Owner choice; zero extra CPU |
| D10 | 2026-09-29 | Test rigs: this Mac + a Windows 10/11 PC + a PipeWire Linux PC on the LAN | Owner has them |

## 6. Milestones

| M | Scope | Done when |
|---|---|---|
| M0 | Toolchain, repo, Tauri tray skeleton on macOS | Tray icon runs on this Mac |
| M1 | Engine: mic → Opus → UDP → speaker, two hard-coded peers | Hear yourself across two machines; latency measured |
| M2 | mDNS discovery, PIN pairing, encryption, saved pairings | Pair two machines from the UI — **done 2026-09-29**, owner-verified Mac↔Linux (pair, connect, audio both ways, disconnect/reconnect) |
| M3 | Virtual devices: Linux, then macOS HAL plug-in | Discord on A can pick CapraLink Input/Output |
| M4 | Windows: native test as B; driverless app/system loopback capture; VB-Cable as virtual mic (§3.8, D17) | Same, on Windows |
| M5 | Adaptive bitrate (loss/RTT/CPU), jitter tuning | Holds quality on Wi-Fi; CPU target met — code done 2026-09-29, field test pending |
| M6 | Service mode + remote configuration | Configure a headless machine from another — **done 2026-09-29** (Mac LaunchAgent login check pending owner) |
| M7 | Installers + signing for all 3 OSes | One-click install per OS |

## 7. Open questions

See `HANDOFF.md` → "Pending owner actions" for the live list. Answers get moved into §5.

## 8. Progress log

| Date | Change |
|---|---|
| 2026-09-29 | Project started. Master plan written. No Rust toolchain on the Mac yet. |
| 2026-10-01 | UI: device rows = name + ⋯ menu (Configure / Edit address / Forget) + status with Connect at right; meters inside Sending/Receiving titles; ⚙ dropdown in This device for background/remote/reconnect options (owner-approved previews). |
| 2026-10-01 | Auto-reconnect + edit address landed (D19): initiator-only retry 2→30 s, last_peer reconnect at start, 'Reconnecting…' state, set_peer_addr. 26 tests ×20 runs stable. |
| 2026-10-01 | "Refresh devices…" as the last entry of every Send from / Play to list (main window + Configure panel): re-reads devices, keeps selections. |
| 2026-10-01 | Per-connection audio settings (§3.10, D20): Send from / Play to / Channels / Bitrate saved per paired device, applied on connect; remote config edits the requester's profile; section titles name the connection. |
| 2026-10-01 | Per-app capture removed (wincap.rs + windows crate dep deleted, D18). Owner verified None and Everything on Windows. |
| 2026-10-01 | "None" option for Send from / Play to (value `none`): turns that direction off, e.g. alongside Moonlight. VB-Cable 16ch entry hidden. |
| 2026-09-30 | M4b/M4c code (engine/src/wincap.rs): process loopback for `app:<exe>`, system loopback "Everything this PC plays" (cpal), VB-Cable → CapraLink Input mapping, UI hints/note. Compiled + type-checked for Windows; runtime untested. |
| 2026-09-30 | M4a passed: CI Windows build runs on Win 11 Pro (window, tray, daemon, mDNS, pairing, audio both ways with Mac). Windows default devices were Steam Streaming → pick headset explicitly. |
| 2026-09-29 | Music sounded slow: cushion refills played up to 2% slow → capped at 0.5% in Music Mode. Wi-Fi at the time: stalls 548–892 ms, 15 lost packets. |
| 2026-09-29 | UI polish: status pill just Streaming/Idle; VU gradient blends green→yellow→red; window auto-fits content height (self-correcting for title bar); tray double-click (Windows). |
| 2026-09-29 | Music Mode field test: occasional hiccups = Deck Wi-Fi stalls 100–150 ms every ~12 s (iwd re-enables power save; owner declined iwd config). Music Mode buffer now remembers stalls 30–60 s, ceiling 300 ms, +30 ms per hiccup. |
| 2026-09-29 | Music Mode landed (D15): live link-wide switch via `mode` message, encoder rebuilt in-callback with continuing seq, 20 ms/stereo/160k/cx10, 40 ms min cushion; tray check item. Encode cost 0.65% of a core (vs 0.23% normal). 22 tests. Known: old remote configurers reset music_mode on save; no FEC at music bitrates. |
| 2026-09-29 | Device rename verified by owner. Remaining milestones: M4 (Windows), M7 (installers/signing). |
| 2026-09-29 | Device rename (D14): set_name + mDNS re-advertise, stored peer names refresh from mDNS/remote_get, remote rename via Configure (remote_config-gated). 18 tests. |
| 2026-09-29 | M6b verified by owner (Mac configured Deck remotely). M6 done except owner's Mac login check. Next: device rename (owner request). |
| 2026-09-29 | M6b landed: Manage sessions (get_config/set_settings) gated by target's remote_config; service/remote_config only changeable locally; UI Configure panel + checkbox. 17 tests. Field test pending. |
| 2026-09-29 | Deck reboot check passed (service auto-starts). M6b started. |
| 2026-09-29 | M6a verified on Deck: systemd user service runs the engine in Game Mode; Mac↔Deck audio works with Deck in Game Mode (owner confirmed). Pair-by-IP used to re-pair. |
| 2026-09-29 | Repo made PUBLIC (private Actions minutes exhausted; aligns with D8). Wi-Fi name scrubbed from notes + history (filter-branch, force-push; local branch backup-before-scrub kept), 12 CI runs linked to old commits deleted. |
| 2026-09-29 | M6a landed (engine/src/rpc.rs): `capralink --daemon` headless engine + loopback RPC (token 0600) + UI client that auto-spawns the daemon; Settings.service installs LaunchAgent / systemd user unit / HKCU Run key. Daemon idle 14.6 MB, 0% CPU. 15 tests. Untested: UI mode, agents on real machines. |
| 2026-09-29 | M5 landed: 1 s receiver reports over the control channel (replace 5 s ping as keepalive); AIMD RateControl (slider = ceiling), loss-driven FEC %, encode-time Complexity controller; stats show kbps + cx. 12 tests. |
| 2026-09-29 | End-to-end Discord call through CapraLink virtual devices (Mac=A, Linux=B) works. |
| 2026-09-29 | M3-macOS: BlackHole v0.7.1-derived HAL plug-ins (drivers/macos, clang build, ad-hoc signed) installed by owner; load fine on macOS 26 without Developer ID. |
| 2026-09-29 | M3-Linux code: cpal pulseaudio host on Linux; pactl creates capralink_output / capralink_input_feed + remap source capralink_input (reuses leftovers, unloads on shutdown incl. Ctrl-C/SIGTERM); special names mapped in lib.rs; pulse playback requests 10 ms buffers. Untested on Linux — owner test pending. |
| 2026-09-29 | M2 signed off by owner. Starting M3 (Linux virtual devices first). |
| 2026-09-29 | M2 landed (engine/src/node.rs): mDNS discovery (all LAN IPv4 addrs, ranked dialing), SPAKE2 PIN pairing w/ rate limit, Noise NNpsk0 control + keepalive, forward-secret per-session ChaCha20-Poly1305 audio (packet v2), config.json (0600, atomic). App UI: PIN, device list, pair/connect/forget, settings in node. CLI = headless node. 8 tests; 2-process smoke test OK. Awaiting owner 2-machine test. |
| 2026-09-29 | M1 signed off by owner: Mac↔Linux both directions, no clicks, low latency, jitter-sized cushion. |
| 2026-09-29 | Two-machine tuning (Mac↔Linux): mic permission fix (.app + usage string); Linux ALSA 40 ms bursts → 10 ms capture buffers (0 underruns); ~0.3% clock drift → resampling drift control on low-water cushion. Stats gained levels, target, tx/rx gaps. |
| 2026-09-29 | M0 tray app landed (Tauri 2.12, single main.rs + ui/index.html). Idle in tray: 0% CPU, 21 MB phys footprint (RSS 84 MB incl. shared WebKit). Window visual check pending owner. |
| 2026-09-29 | M1 engine landed: cpal 0.18 + opus 0.4 (static libopus via cmake) + ringbuf. Loopback: 0 loss, 0 underruns, 18 MB RSS, 0.2% CPU. Drift controller smoothed (EMA) to avoid per-callback skip/dup. CI added. |
| 2026-09-29 | Owner answered round 1: D3, D4, D7, D8 recorded. |
| 2026-09-29 | Private repo created: github.com/CapraAudio/CapraLink. Git identity = Capra Audio (GitHub noreply). |
| 2026-09-29 | Round 2: GPL-3.0, no AEC, test rigs recorded (D8–D10). Rust install approved. |
