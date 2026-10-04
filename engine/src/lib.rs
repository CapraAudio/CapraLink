//! CapraLink audio engine: capture -> Opus -> UDP -> Opus -> playback.

mod dsp;
pub mod log;
mod node;
mod ptt;
mod rpc;
mod vdev;

pub use node::{Check, Device, Node, NodeState, Quality, RemoteConfig};
pub use ptt::{PttKey, PttMode};
pub use rpc::{daemon, daemon_exe, serve_rpc, Client, Devices};

/// A command for a system tool (pactl, systemctl, reg, hostname). Inside an AppImage,
/// LD_LIBRARY_PATH points at the bundled libraries, which break system binaries
/// (e.g. systemctl needs a newer OpenSSL than the bundled one), so it is dropped.
pub fn system_command(program: &str) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    if std::env::var_os("APPIMAGE").is_some() {
        c.env_remove("LD_LIBRARY_PATH");
    }
    c
}

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use dsp::{is_nack, Complexity, Counters, Jitter, Mode, Nacks, Packetizer, Plan, Playout, Resampler, Resend, Rx, FRAME, HIFI_BITRATE, JITTER_WINDOW_US, MAX_PACKET, MUSIC_JITTER_WINDOW_US, RATE, TARGET};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Settings device names that mean the CapraLink virtual devices (MASTER.md §3.4).
/// On macOS the drivers carry these names; on Linux `label` maps the Pulse devices to them.
/// This build's version as shown to people: the release version, plus ".alpha-build.N" on test
/// builds (CI sets CAPRALINK_BUILD on everything that isn't a release tag).
pub fn version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    option_env!("CAPRALINK_BUILD").map_or_else(|| v.to_string(), |b| format!("{v}.{b}"))
}

pub const VIRTUAL_OUTPUT: &str = "CapraLink Output";
pub const VIRTUAL_INPUT: &str = "CapraLink Input";

/// Windows Send-from name: loopback of the default playback device (MASTER.md §3.8).
pub const EVERYTHING: &str = "Everything this PC plays";
/// Send-from / Play-to value that turns that direction off (e.g. while another app such as a
/// remote-desktop client already carries this computer's audio).
pub const NO_DEVICE: &str = "none";

/// Windows: cpal creates one process-wide device enumerator (a COM object) on the first thread
/// that lists devices, and it dies when that thread exits and COM shuts down there. Create it on
/// a thread that never exits, before short-lived threads (per connection, RPC) list devices.
pub(crate) fn pin_audio_host() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            let pinned = std::thread::Builder::new().name("capralink-com".into()).spawn(move || {
                let _ = input_devices();
                let _ = tx.send(());
                loop {
                    std::thread::park();
                }
            });
            if pinned.is_ok() {
                let _ = rx.recv();
            }
        });
    }
}

/// One entry of a Send from / Play to list. `id` is what settings store: cpal's stable device id,
/// or for CapraLink's own devices and "Everything this PC plays" that fixed name (the same on every
/// computer, which remote configuration relies on). `name` is what the lists show.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

pub fn input_devices() -> Vec<AudioDevice> {
    devices(&cpal::default_host(), true).into_iter().map(|(e, _)| e).collect()
}

pub fn output_devices() -> Vec<AudioDevice> {
    devices(&cpal::default_host(), false).into_iter().map(|(e, _)| e).collect()
}

/// The listed devices; hidden plumbing left out.
fn devices(host: &cpal::Host, input: bool) -> Vec<(AudioDevice, cpal::Device)> {
    let all = if input { host.input_devices() } else { host.output_devices() };
    let shown = |d: &cpal::Device| match d.id() {
        Ok(id) if cfg!(target_os = "linux") => label(input, id.id(), name(d)),
        _ if cfg!(windows) => win_label(input, name(d)),
        _ => Some(name(d)),
    };
    let id = |d: &cpal::Device| d.id().map(|i| i.to_string()).unwrap_or_default();
    let mut v: Vec<_> = all.into_iter().flatten().filter_map(|d| Some(((id(&d), shown(&d)?), d))).collect();
    if cfg!(windows) && input {
        v.extend(host.default_output_device().map(|d| ((String::new(), EVERYTHING.to_string()), d))); // WASAPI records it in loopback mode
    }
    let (raw, devs): (Vec<_>, Vec<_>) = v.into_iter().unzip();
    listed(raw).into_iter().zip(devs).collect()
}

/// Entries from (cpal id, shown name) pairs in enumeration order: a repeated name gets " (2)",
/// " (3)"… so identical devices can be told apart, and CapraLink's own devices and "Everything
/// this PC plays" take their name as id. A device without an id is found by its name.
fn listed(raw: Vec<(String, String)>) -> Vec<AudioDevice> {
    let mut seen: Vec<&str> = Vec::new();
    raw.iter()
        .map(|(id, name)| {
            seen.push(name);
            let n = seen.iter().filter(|s| **s == name).count();
            let name = if n == 1 { name.clone() } else { format!("{name} ({n})") };
            let fixed = id.is_empty() || [VIRTUAL_INPUT, VIRTUAL_OUTPUT, EVERYTHING].contains(&name.as_str());
            AudioDevice { id: if fixed { name.clone() } else { id.clone() }, name }
        })
        .collect()
}

/// The entry a saved Send from / Play to value means: by id, else by name (versions before 0.2
/// saved names; the next save stores the id).
pub(crate) fn pick(list: &[AudioDevice], saved: &str) -> Option<usize> {
    list.iter().position(|d| d.id == saved).or_else(|| list.iter().position(|d| d.name == saved))
}

/// How a saved value reads while its device is missing: an old saved name as is, an id (which can
/// hold serial numbers and means nothing to people) as "Saved device".
pub(crate) fn missing_name(saved: &str) -> &str {
    if saved.parse::<cpal::DeviceId>().is_ok() { "Saved device" } else { saved }
}

fn name(d: &cpal::Device) -> String {
    d.description().map(|d| d.name().to_string()).unwrap_or_default()
}

/// Linux: the listed name of a device, from its Pulse name (cpal's device id on the Pulse host).
/// Our virtual devices get the special names; the internal plumbing is hidden (`None`), and so is
/// CapraLink Input as a send source / CapraLink Output as a play target (both would loop audio back).
fn label(input: bool, pulse_name: &str, name: String) -> Option<String> {
    match (input, pulse_name) {
        (true, "capralink_output.monitor") => Some(VIRTUAL_OUTPUT.into()),
        (false, "capralink_input_feed") => Some(VIRTUAL_INPUT.into()),
        (true, "capralink_input" | "capralink_input_feed.monitor") | (false, "capralink_output") => None,
        _ => Some(name),
    }
}

/// Windows: VB-Cable's playback side is CapraLink Input (apps record from "CABLE Output");
/// "CABLE Output" itself is hidden as a send source (it would loop audio back).
// ponytail: matches only the free VB-Cable's endpoint names (renamed endpoints or the A/B
// cables aren't recognised); match on the device's driver instead if that ever matters.
#[cfg_attr(not(windows), allow(dead_code))]
fn win_label(input: bool, name: String) -> Option<String> {
    // Windows keeps the "(VB-Audio Virtual Cable)" suffix when a user renames the endpoint
    // (e.g. the recording side to "CapraLink"), so match on that, not on the default names.
    if !name.ends_with("(VB-Audio Virtual Cable)") {
        return Some(name);
    }
    match input {
        true => None,                       // the cable's recording side: sending it would loop
        false if name.contains("16ch") => None, // the 16-channel variant: same cable, just clutter
        false => Some(VIRTUAL_INPUT.into()),
    }
}

/// Audio settings (saved in the node's config). `None` device = system default.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub input: Option<String>,
    pub output: Option<String>,
    pub bitrate: i32,
    pub channels: u16,
    /// Start `capralink --daemon` at login (MASTER.md §3.6).
    pub service: bool,
    /// Let paired computers read and change these settings (MASTER.md §3.6 M6b).
    pub remote_config: bool,
    /// Music Mode (MASTER.md §3.7): the link runs in it when either side has this on.
    pub music_mode: bool,
    /// Reconnect to the last computer this one connected to, after a drop or a restart (MASTER.md §3.9).
    pub auto_reconnect: bool,
    /// Volume (percent, 0–`MAX_VOLUME`) of what this computer sends / plays from the other one. Live.
    pub send_volume: u16,
    pub recv_volume: u16,
    /// Send silence (the stream keeps running). Live.
    pub mute: bool,
    /// Push-to-talk: send silence except while talking. Needs `ptt_key`.
    pub ptt: PttMode,
    pub ptt_key: Option<PttKey>,
    /// Hi-Fi: Music Mode sent lossless (24-bit PCM, ~2.3 Mbps) when either side wants it and both
    /// can. No effect while Music Mode is off. Live.
    pub hifi: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            input: None,
            output: None,
            bitrate: 64_000,
            channels: 1,
            service: false,
            remote_config: false,
            music_mode: false,
            auto_reconnect: true,
            send_volume: 100,
            recv_volume: 100,
            mute: false,
            ptt: PttMode::Off,
            ptt_key: None,
            hifi: false,
        }
    }
}

/// Highest Send/Receive volume, percent.
pub const MAX_VOLUME: u16 = 150;

/// Gain for a volume percent; 0 when `silent` (mute, or push-to-talk while not talking).
fn gain(volume: u16, silent: bool) -> f32 {
    if silent { 0.0 } else { volume as f32 / 100.0 }
}

/// `x * g`; above 0.9 it bends smoothly towards ±1 instead of clipping hard. Only when `g`
/// isn't 1, so 100% stays bit-exact.
fn amplify(x: f32, g: f32) -> f32 {
    if g == 1.0 {
        return x;
    }
    let y = x * g;
    if y.abs() <= 0.9 { y } else { y.signum() * (0.9 + 0.1 * ((y.abs() - 0.9) / 0.1).tanh()) }
}

/// Per-session audio keys (ChaCha20-Poly1305): one per direction.
pub struct Keys {
    pub send: [u8; 32],
    pub recv: [u8; 32],
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub sent: u64,
    pub received: u64,
    pub lost: u64,
    pub fec_recovered: u64,
    pub underruns: u64,
    pub buffer_ms: f32,
    /// Current adaptive playout target; grows when the link is bursty.
    pub target_ms: f32,
    /// VU level (0..1) of audio captured / played: instant rise, ~26 dB/s fall.
    pub in_peak: f32,
    pub out_peak: f32,
    /// Longest gap between packets sent / received since the previous `stats()` call.
    /// Even ~10 ms means a smooth link; big gaps show where bursts come from.
    pub tx_gap_ms: f32,
    pub rx_gap_ms: f32,
    /// Currently applied Opus bitrate (bps) and complexity (MASTER.md §3.5).
    pub bitrate: i32,
    pub complexity: u8,
    /// The link is in Music Mode (either side has it on).
    #[serde(default)]
    pub music: bool,
    /// Audio packets the network stack refused (send queue full); dropped, never retried.
    #[serde(default)]
    pub send_dropped: u64,
    /// Slowest capture callback (µs) since the previous `stats()` call.
    #[serde(default)]
    pub callback_max_us: u32,
    /// Estimated one-way delay, ms: "you hear them" (None when Play to is off) and "they hear
    /// you" (None when Send from is off or the other computer runs an older version).
    #[serde(default)]
    pub delay_in_ms: Option<u32>,
    #[serde(default)]
    pub delay_out_ms: Option<u32>,
    /// This computer sends Hi-Fi (lossless) right now; `bitrate` then reads 2304 kbps.
    #[serde(default)]
    pub hifi: bool,
    /// Hi-Fi is on for this link but fell back to Music Mode (Opus) because the network couldn't
    /// keep up; it is tried again after a clean minute.
    #[serde(default)]
    pub hifi_fallback: bool,
}

#[derive(Default)]
struct Shared {
    c: Counters,
    buffer_ms: AtomicU32, // f32 bits
    target_ms: AtomicU32, // f32 bits
    in_peak: AtomicU32,   // f32 bits, VU level (see `meter`)
    out_peak: AtomicU32,  // f32 bits, VU level (see `meter`)
    rx_channels: AtomicU8,
    tx_gap_us: AtomicU32, // reset on read
    rx_gap_us: AtomicU32, // reset on read
    jitter_us: AtomicU32, // worst recent packet lateness
    target_bitrate: AtomicI32,   // written by the session thread from peer reports
    target_loss_perc: AtomicU8,
    bitrate: AtomicI32, // currently applied, for Stats
    complexity: AtomicU8,
    music: AtomicBool, // effective Music Mode, set by the node
    hifi: AtomicBool,  // send Hi-Fi (effective, not fallen back), set by the node
    mode: Mutex<Option<TxMode>>, // the encoder/resampler for `music`/`hifi`, prepared for `Tx::apply_mode`
    rx_pcm: AtomicBool, // the received stream is Hi-Fi
    // Hi-Fi packets sent, for resending; the capture callback only try_locks it (a packet it
    // can't store then can't be resent, which the receiver handles like a loss)
    resend: Mutex<Resend>,
    callback_us: AtomicU32, // slowest capture callback, reset on read
    failure: Mutex<Option<Failure>>, // see `err_cb`
    send_gain: AtomicU32, // f32 bits, applied before encoding (0 = silence)
    recv_gain: AtomicU32, // f32 bits, applied before playback
    chirp: AtomicU8,      // push-to-talk chirp for the playback callback to play: 1 = start, 2 = stop
    in_latency_us: AtomicU32,  // capture: the oldest sample's age when its callback runs
    out_latency_us: AtomicU32, // playback: how long until a written sample is heard
}

impl Shared {
    /// `tx`: (Channels setting, capture rate) when sending. The capture callback allocates
    /// nothing for a switch: its new encoder and resampler are built here and swapped in there.
    fn set_mode(&self, music: bool, hifi: bool, tx: Option<(u16, u32)>) {
        let hifi = hifi && music;
        if (self.music.swap(music, Relaxed) == music) & (self.hifi.swap(hifi, Relaxed) == hifi) {
            return;
        }
        if let Some((ch, rate)) = tx {
            drop(self.resend.lock()); // a first lock may allocate (lazily boxed on some OSes): not in the callback
            let ch = if music { 2 } else { ch };
            let m = Mode::new(ch, music, hifi).ok().map(|pk| TxMode { pk, rs: (rate != RATE).then(|| Resampler::new(rate, RATE, ch as usize)) });
            // whatever was in the slot (e.g. the encoder the callback swapped out) is freed here
            let _old = self.mode.lock().map(|mut slot| std::mem::replace(&mut *slot, m));
        }
    }

    fn gains(&self, send: f32, recv: f32) {
        self.send_gain.store(send.to_bits(), Relaxed);
        self.recv_gain.store(recv.to_bits(), Relaxed);
    }
}

struct TxMode {
    pk: Mode,
    rs: Option<Resampler>,
}

/// Why a link can't go on (`Link::failure`).
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// An audio stream must be rebuilt (e.g. its device's format changed): restart the link.
    Rebuild(String),
    /// An audio device is gone or refused: end the session.
    End(String),
}

/// A running TX + RX link. Dropping it stops everything.
pub struct Link {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    rx: Option<JoinHandle<()>>,
    tx: Option<(u16, u32)>,       // (Channels setting, capture rate) when sending
    _input: Option<cpal::Stream>, // None when Send from is off
    _output: Option<cpal::Stream>, // None when Play to is off
}

impl Link {
    /// Streams to/from `peer` over UDP `port`; only packets from exactly `peer` are accepted.
    pub fn start(cfg: &Settings, port: u16, peer: SocketAddr, keys: &Keys) -> anyhow::Result<Link> {
        anyhow::ensure!(matches!(cfg.channels, 1 | 2), "channels must be 1 or 2");
        let (in_dev, out_dev) = (find(true, &cfg.input)?.map(|(_, d)| d), find(false, &cfg.output)?.map(|(_, d)| d));

        let sock = UdpSocket::bind(("0.0.0.0", port)).with_context(|| format!("bind UDP port {port}"))?;
        // short, so Hi-Fi asks again for missing packets (and gives them up) while none arrive
        sock.set_read_timeout(Some(Duration::from_millis(20)))?;
        // A full send queue drops a packet (`send_dropped`) instead of stalling the capture
        // callback. Not `set_nonblocking`: that flag is shared with the RX thread's clone.
        // ponytail: still waits up to one timer tick (1–4 ms on Linux, 1 ms on Windows; macOS
        // UDP sends don't wait); move sending to its own thread if `callback_max_us` shows it.
        sock.set_write_timeout(Some(Duration::from_millis(1)))?;
        let shared = Arc::new(Shared::default());
        let initial_bitrate = cfg.bitrate.clamp(8_000, 96_000);
        shared.target_bitrate.store(initial_bitrate, Relaxed);
        shared.bitrate.store(initial_bitrate, Relaxed);
        shared.target_loss_perc.store(5, Relaxed);
        shared.complexity.store(5, Relaxed);
        // silent while push-to-talk is on, until the node says this side is talking
        shared.gains(gain(cfg.send_volume, cfg.mute || cfg.ptt != PttMode::Off), gain(cfg.recv_volume, false));
        let stop = Arc::new(AtomicBool::new(false));
        let (prod, cons) = HeapRb::<f32>::new(RATE as usize * 3).split(); // 1.5 s of stereo: Music Mode buffers up to 1 s + headroom

        // ponytail: with Play to = none, packets are still decoded into a ring nobody drains
        // (it just stays full); skip decoding in the RX thread if that CPU ever matters.
        // All fallible stream setup happens before the RX thread exists, so an error can't leak it.
        let output = out_dev
            .map(|d| -> anyhow::Result<cpal::Stream> {
                let s = build_output(&d, cons, shared.clone())?;
                s.play()?;
                Ok(s)
            })
            .transpose()?;
        let pk = Packetizer::new(cfg.channels, cfg.bitrate, &keys.send)?;
        // None when Send from is off: nothing captured or sent
        let input = in_dev
            .map(|d| -> anyhow::Result<(cpal::Stream, u32)> {
                let (s, rate) = build_input(&d, cfg, pk, sock.try_clone()?, peer, shared.clone())?;
                s.play()?;
                Ok((s, rate))
            })
            .transpose()?;
        let tx = input.as_ref().map(|(_, rate)| (cfg.channels, *rate));

        let rx = {
            let (shared, stop, rx, nacks) = (shared.clone(), stop.clone(), Rx::new(&keys.recv), Nacks::new(&keys.send));
            std::thread::Builder::new().name("capralink-rx".into()).spawn(move || receive(sock, peer, rx, nacks, prod, &shared, &stop))?
        };
        Ok(Link { shared, stop, rx: Some(rx), tx, _input: input.map(|(s, _)| s), _output: output })
    }

    /// Current (sending, receiving) VU levels, 0..1; cheap enough to poll many times a second.
    pub fn levels(&self) -> (f32, f32) {
        (f32::from_bits(self.shared.in_peak.load(Relaxed)), f32::from_bits(self.shared.out_peak.load(Relaxed)))
    }

    pub fn stats(&self) -> Stats {
        self.read(true)
    }

    /// Like `stats`, but leaves the gap and callback meters to the window (for the log and diagnostics).
    pub fn peek(&self) -> Stats {
        self.read(false)
    }

    fn read(&self, reset_gaps: bool) -> Stats {
        let c = &self.shared.c;
        let max = |g: &AtomicU32| if reset_gaps { g.swap(0, Relaxed) } else { g.load(Relaxed) };
        let gap = |g| max(g) as f32 / 1000.0;
        Stats {
            sent: c.sent.load(Relaxed),
            received: c.received.load(Relaxed),
            lost: c.lost.load(Relaxed),
            fec_recovered: c.fec_recovered.load(Relaxed),
            underruns: c.underruns.load(Relaxed),
            buffer_ms: f32::from_bits(self.shared.buffer_ms.load(Relaxed)),
            target_ms: f32::from_bits(self.shared.target_ms.load(Relaxed)),
            in_peak: f32::from_bits(self.shared.in_peak.load(Relaxed)),
            out_peak: f32::from_bits(self.shared.out_peak.load(Relaxed)),
            tx_gap_ms: gap(&self.shared.tx_gap_us),
            rx_gap_ms: gap(&self.shared.rx_gap_us),
            bitrate: self.shared.bitrate.load(Relaxed),
            complexity: self.shared.complexity.load(Relaxed),
            music: self.shared.music.load(Relaxed),
            send_dropped: c.send_dropped.load(Relaxed),
            callback_max_us: max(&self.shared.callback_us),
            delay_in_ms: None, // filled in by the node, which knows the network part
            delay_out_ms: None,
            hifi: self.shared.hifi.load(Relaxed),
            hifi_fallback: false, // filled in by the node
        }
    }

    /// Switches this link's sender and receiver into or out of Music Mode, and its sender into or
    /// out of Hi-Fi (only in Music Mode), live.
    pub fn set_mode(&self, music: bool, hifi: bool) {
        self.shared.set_mode(music, hifi, self.tx);
    }

    /// Sets the Send/Receive volumes (percent), live; `silent` sends silence (mute, or
    /// push-to-talk while not talking).
    pub fn set_volume(&self, send: u16, recv: u16, silent: bool) {
        self.shared.gains(gain(send, silent), gain(recv, false));
    }

    /// Mixes the push-to-talk chirp into this link's playback; false if Play to is off.
    pub fn chirp(&self, start: bool) -> bool {
        self.shared.chirp.store(if start { 1 } else { 2 }, Relaxed);
        self._output.is_some()
    }

    /// (sending, receiving) part of the delay, ms: capture + one frame, and playout buffer +
    /// playback; None for a direction that is off.
    pub fn latency(&self) -> (Option<f32>, Option<f32>) {
        let ms = |a: &AtomicU32| a.load(Relaxed) as f32 / 1000.0;
        let frame = if self.shared.hifi.load(Relaxed) {
            5.0
        } else if self.shared.music.load(Relaxed) {
            20.0
        } else {
            10.0
        };
        let recv = f32::from_bits(self.shared.buffer_ms.load(Relaxed)) + ms(&self.shared.out_latency_us);
        (self.tx.map(|_| ms(&self.shared.in_latency_us) + frame), self._output.is_some().then_some(recv))
    }

    /// Sets the sender's target bitrate/loss%, applied by the capture callback (MASTER.md §3.5).
    pub fn set_rate(&self, bitrate: i32, loss_perc: u8) {
        self.shared.target_bitrate.store(bitrate, Relaxed);
        self.shared.target_loss_perc.store(loss_perc, Relaxed);
    }

    /// Why the link can't go on (e.g. "Play to device stopped: …"), once an audio stream died.
    pub fn failure(&self) -> Option<Failure> {
        self.shared.failure.lock().ok()?.clone()
    }

    /// This link's own receive-side counters (cumulative) plus current jitter, for building
    /// the periodic peer report. Doesn't reset anything (unlike `stats()`).
    pub fn report_counters(&self) -> (u64, u64, u64, f32) {
        let c = &self.shared.c;
        (c.received.load(Relaxed), c.lost.load(Relaxed), c.underruns.load(Relaxed), self.shared.jitter_us.load(Relaxed) as f32 / 1000.0)
    }
}

/// The RX thread: the peer's audio into the playback ring; in Hi-Fi also NACKs out for what's
/// missing, and resends back for the peer's NACKs (from `Shared::resend`).
fn receive(sock: UdpSocket, peer: SocketAddr, mut rx: Rx, mut nacks: Nacks, mut prod: HeapProd<f32>, shared: &Shared, stop: &AtomicBool) {
    let (mut buf, mut last, mut jitter) = ([0u8; 2048], None, Jitter::default());
    let mut resend = [0u8; MAX_PACKET];
    while !stop.load(Relaxed) {
        // Errors are timeouts (checked above) or transient (e.g. ICMP resets on Windows).
        // ponytail: exact source match; a multi-homed peer replying from another
        // interface is ignored — relax to IP-only (packets are authenticated) if seen.
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == peer && is_nack(&buf[..n]) => {
                for seq in nacks.open(&mut buf[..n]).into_iter().flatten() {
                    // copied out, so the capture callback's try_lock isn't held up by the send
                    let len = shared.resend.lock().ok().and_then(|r| {
                        let p = r.get(seq)?;
                        resend[..p.len()].copy_from_slice(p);
                        Some(p.len())
                    });
                    if let Some(len) = len {
                        let _ = sock.send_to(&resend[..len], peer);
                    }
                }
            }
            Ok((n, from)) if from == peer => {
                if let Some(ch) = rx.handle(&mut buf[..n], &shared.c, &mut |pcm: &[f32]| {
                    prod.push_slice(pcm);
                }) {
                    let g = gap(&mut last, &shared.rx_gap_us);
                    shared.jitter_us.store(jitter.push(g, rx.period_us(), if shared.music.load(Relaxed) { MUSIC_JITTER_WINDOW_US } else { JITTER_WINDOW_US }), Relaxed);
                    shared.rx_channels.store(ch, Relaxed);
                }
            }
            _ => {}
        }
        // Hi-Fi: a missing packet whose playout time has come is given up; the others are asked for
        rx.release(prod.occupied_len() / 2, &shared.c, &mut |pcm: &[f32]| {
            prod.push_slice(pcm);
        });
        if let Some(nack) = rx.nack(Instant::now()) {
            let _ = sock.send_to(nack, peer);
        }
        shared.rx_pcm.store(rx.pcm(), Relaxed);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(rx) = self.rx.take() {
            let _ = rx.join();
        }
    }
}

/// The device (and its listed name) a Send from (`input`) / Play to setting names: `None` =
/// system default, `Ok(None)` = that direction is off.
/// The system's default output is CapraLink's own virtual output, so what plays there is sent
/// over the link (e.g. a Mac whose sound is heard on the other computer).
pub(crate) fn default_output_is_virtual() -> bool {
    find(false, &None).ok().flatten().is_some_and(|(n, _)| n == VIRTUAL_OUTPUT)
}

fn find(input: bool, want: &Option<String>) -> anyhow::Result<Option<(String, cpal::Device)>> {
    let host = cpal::default_host();
    Ok(Some(match want.as_deref() {
        Some(NO_DEVICE) => return Ok(None),
        Some(n) => {
            let (list, mut devs): (Vec<_>, Vec<_>) = devices(&host, input).into_iter().unzip();
            let i = pick(&list, n).ok_or_else(|| anyhow!("device not found: {}", missing_name(n)))?;
            (list[i].name.clone(), devs.swap_remove(i))
        }
        None => if input { host.default_input_device() } else { host.default_output_device() }.map(|d| (name(&d), d)).ok_or_else(|| anyhow!("no default device"))?,
    }))
}

/// The system's default playback device's name (Windows setup check).
/// "macOS 26.0", "Windows 10.0.26100", "SteamOS" …, for diagnostics; falls back to the OS family.
pub(crate) fn os_version() -> String {
    let out = |prog: &str, args: &[&str]| {
        let mut c = system_command(prog);
        c.args(args).stdin(std::process::Stdio::null());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
        c.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
    };
    let v = if cfg!(target_os = "macos") {
        out("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}"))
    } else if cfg!(windows) {
        out("cmd", &["/c", "ver"]) // "Microsoft Windows [Version 10.0.26100.1]"
    } else {
        std::fs::read_to_string("/etc/os-release").ok().and_then(|s| s.lines().find_map(|l| Some(l.strip_prefix("PRETTY_NAME=")?.trim_matches('"').to_string())))
    };
    v.unwrap_or_else(|| std::env::consts::OS.to_string())
}

#[cfg(windows)]
pub(crate) fn default_output_name() -> Option<String> {
    cpal::default_host().default_output_device().map(|d| name(&d))
}

/// Plays about a second of a soft 440 Hz tone on the Play to device; returns when it's done.
pub fn test_tone(output: &Option<String>) -> anyhow::Result<()> {
    // 1 s, 50 ms fades
    play(output, 1.0, |t| 0.2 * (t.min(1.0 - t) / 0.05).clamp(0.0, 1.0) * (std::f32::consts::TAU * 440.0 * t).sin())
}

/// Plays `secs` of `wave(t)` (t in seconds; it must be silent from `secs` on) on the Play to
/// device `output`; returns when it's done.
pub(crate) fn play(output: &Option<String>, secs: f32, wave: fn(f32) -> f32) -> anyhow::Result<()> {
    let (_, dev) = find(false, output)?.ok_or_else(|| anyhow!("Play to is set to None"))?;
    let sc = pick_config(dev.default_output_config()?, dev.supported_output_configs()?);
    let (ch, rate, c) = (sc.channels() as usize, sc.sample_rate(), sc.config());
    let err = |e: cpal::Error| crate::log::log(&format!("playing a sound: {e}"));
    let mut n = 0u32;
    let s = match sc.sample_format() {
        SampleFormat::F32 => dev.build_output_stream(c, move |d: &mut [f32], _: &_| render(d, &mut n, ch, rate, wave), err, None)?,
        SampleFormat::I16 => dev.build_output_stream(c, move |d: &mut [i16], _: &_| render(d, &mut n, ch, rate, wave), err, None)?,
        SampleFormat::I32 => dev.build_output_stream(c, move |d: &mut [i32], _: &_| render(d, &mut n, ch, rate, wave), err, None)?,
        SampleFormat::U16 => dev.build_output_stream(c, move |d: &mut [u16], _: &_| render(d, &mut n, ch, rate, wave), err, None)?,
        f => anyhow::bail!("unsupported output sample format {f}"),
    };
    s.play()?;
    std::thread::sleep(Duration::from_secs_f32(secs + 0.2)); // the sound plus the device's buffer
    Ok(())
}

/// Next frames of `wave` (`n` = frames played so far).
fn render<T: SizedSample + FromSample<f32>>(out: &mut [T], n: &mut u32, ch: usize, rate: u32, wave: fn(f32) -> f32) {
    for f in out.chunks_exact_mut(ch) {
        f.fill(T::from_sample(wave(*n as f32 / rate as f32)));
        *n = n.saturating_add(1);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct MicCheck {
    pub peak_db: f32,
    /// "silent", "clipping" or "ok"
    pub verdict: String,
    pub message: String,
}

/// Records 3 s from the Send from device and judges its level.
pub fn mic_check(input: &Option<String>) -> anyhow::Result<MicCheck> {
    let (label, dev) = find(true, input)?.ok_or_else(|| anyhow!("Send from is set to None"))?;
    // "Everything this PC plays" is a playback device: its mix format is the one loopback takes.
    let sc = if input.as_deref() == Some(EVERYTHING) { dev.default_output_config()? } else { dev.default_input_config()? };
    let (peak, c) = (Arc::new(AtomicU32::new(0)), sc.config());
    let err = |e: cpal::Error| crate::log::log(&format!("mic check: {e}"));
    let p = peak.clone();
    let s = match sc.sample_format() {
        SampleFormat::F32 => dev.build_input_stream(c, move |d: &[f32], _: &_| meter_max(&p, d), err, None)?,
        SampleFormat::I16 => dev.build_input_stream(c, move |d: &[i16], _: &_| meter_max(&p, d), err, None)?,
        SampleFormat::I32 => dev.build_input_stream(c, move |d: &[i32], _: &_| meter_max(&p, d), err, None)?,
        SampleFormat::U16 => dev.build_input_stream(c, move |d: &[u16], _: &_| meter_max(&p, d), err, None)?,
        f => anyhow::bail!("unsupported input sample format {f}"),
    };
    s.play()?;
    std::thread::sleep(Duration::from_secs(3));
    drop(s);
    let peak_db = (20.0 * f32::from_bits(peak.load(Relaxed)).log10()).max(-120.0);
    let (verdict, message) = if peak_db < -60.0 {
        ("silent", format!("No sound from {label} — check it isn't muted. On macOS, allow microphone access in System Settings → Privacy & Security."))
    } else if peak_db >= -0.5 {
        ("clipping", format!("{label} is too loud — lower its input volume"))
    } else {
        ("ok", format!("Microphone OK — peak {} dB", format!("{peak_db:.0}").replace('-', "\u{2212}")))
    };
    Ok(MicCheck { peak_db, verdict: verdict.into(), message })
}

/// Raises a max-peak meter (positive f32 bit patterns order like the floats).
fn meter_max<T: Sample>(peak: &AtomicU32, d: &[T])
where
    f32: FromSample<T>,
{
    let p = d.iter().fold(0f32, |m, s| m.max(f32::from_sample(*s).abs()));
    peak.fetch_max(p.to_bits(), Relaxed);
}

/// Default config, switched to 48 kHz when the device supports it (avoids resampling).
fn pick_config(def: cpal::SupportedStreamConfig, mut all: impl Iterator<Item = cpal::SupportedStreamConfigRange>) -> cpal::SupportedStreamConfig {
    if def.sample_rate() == RATE {
        return def;
    }
    all.find_map(|r| (r.channels() == def.channels() && r.sample_format() == def.sample_format()).then(|| r.try_with_sample_rate(RATE)).flatten())
        .unwrap_or(def)
}

/// Records the time since `last` into a reset-on-read max-gap meter.
/// Returns the gap in µs (0 for the first call).
fn gap(last: &mut Option<Instant>, max_us: &AtomicU32) -> u32 {
    let now = Instant::now();
    let us = last.replace(now).map_or(0, |prev| (now - prev).as_micros() as u32);
    max_us.fetch_max(us, Relaxed);
    us
}

/// Raises a reset-on-read peak meter. Positive f32 bit patterns order like the floats.
/// VU-style level: jumps to a new peak at once, then falls back about 26 dB/s
/// (×0.97 per ~10 ms callback), so any reader at any rate sees smooth motion.
/// Each level has a single writer (its audio callback), so load/store is enough.
fn meter(level: &AtomicU32, samples: &[f32]) {
    let p = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    let old = f32::from_bits(level.load(Relaxed));
    level.store(p.max(old * 0.97).to_bits(), Relaxed);
}

/// Logs stream errors; one the stream can't survive is also recorded as the link's `failure()`
/// (an `End` is never downgraded to a `Rebuild`).
fn err_cb(shared: Arc<Shared>, input: bool) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e| {
        crate::log::log(&format!("audio stream error ({}): {e}", side(input)));
        if let (Some(new), Ok(mut f)) = (failure(&e, input), shared.failure.lock()) {
            if !matches!(*f, Some(Failure::End(_))) {
                *f = Some(new);
            }
        }
    }
}

fn side(input: bool) -> &'static str {
    if input { "Send from device" } else { "Play to device" }
}

/// What a stream error means for the link; `None` = log only (busy, rerouted, xruns …).
fn failure(e: &cpal::Error, input: bool) -> Option<Failure> {
    use cpal::ErrorKind::*;
    let what = side(input);
    Some(match e.kind() {
        StreamInvalidated => Failure::Rebuild(format!("{what} must be restarted: {e}")),
        DeviceNotAvailable | HostUnavailable => Failure::End(format!("{what} stopped: {e}")),
        PermissionDenied if input => Failure::End(format!("{what}: permission denied — on macOS allow microphone access in System Settings → Privacy & Security")),
        PermissionDenied => Failure::End(format!("{what}: permission denied: {e}")),
        _ => return None,
    })
}

struct Tx {
    pk: Packetizer,
    sock: UdpSocket,
    peer: SocketAddr,
    dev_ch: usize,
    rate: u32, // capture rate
    music: bool,
    rs: Option<Resampler>,
    mixed: Vec<f32>,
    resampled: Vec<f32>,
    frame: Vec<f32>,
    last_send: Option<Instant>,
    shared: Arc<Shared>,
    last_bitrate: i32,
    last_loss_perc: u8,
    cx: Complexity,
    gain: f32, // send gain now: moves to the target over `GAIN_RAMP_S`, so mute/push-to-talk don't click
}

/// How long a send-gain change takes (0 → 1): an instant jump would click.
const GAIN_RAMP_S: f32 = 0.01;

impl Tx {
    // ponytail: encode + send run inside the capture callback; move them to a dedicated
    // encode thread fed by a ring if callbacks ever overrun (`callback_max_us`).
    /// `age_us` = the device's age of the first sample (callback - capture timestamp), 0 if unknown.
    fn process<T: SizedSample>(&mut self, data: &[T], age_us: u32)
    where
        f32: FromSample<T>,
    {
        let start = Instant::now();
        self.apply_mode();
        self.apply_rate();
        let (dev_ch, out_ch) = (self.dev_ch, self.pk.channels());
        // at least this callback's own buffer, when the host's timestamps don't say more
        let buffer_us = (data.len() / dev_ch) as u64 * 1_000_000 / self.rate as u64;
        self.shared.in_latency_us.store(age_us.max(buffer_us as u32), Relaxed);
        let target = f32::from_bits(self.shared.send_gain.load(Relaxed));
        let step = 1.0 / (self.rate as f32 * GAIN_RAMP_S);
        self.mixed.clear();
        for f in data.chunks_exact(dev_ch) {
            self.gain += (target - self.gain).clamp(-step, step); // lands exactly on the target
            let g = self.gain;
            let s = |i: usize| amplify(f32::from_sample(f[i.min(dev_ch - 1)]), g);
            if out_ch == 1 {
                self.mixed.push(amplify((0..dev_ch).map(|i| f32::from_sample(f[i])).sum::<f32>() / dev_ch as f32, g));
            } else {
                self.mixed.extend([s(0), s(1)]);
            }
        }
        meter(&self.shared.in_peak, &self.mixed);
        let pcm = match self.rs.as_mut() {
            Some(rs) => {
                self.resampled.clear();
                rs.process(&self.mixed, &mut self.resampled);
                &self.resampled
            }
            None => &self.mixed,
        };
        let frame_len = self.pk.frame() * out_ch;
        for chunk in pcm.chunks(frame_len) {
            let take = chunk.len().min(frame_len - self.frame.len());
            self.frame.extend_from_slice(&chunk[..take]);
            if self.frame.len() == frame_len {
                let t0 = Instant::now();
                let pcm = self.pk.pcm();
                if let Ok(p) = self.pk.packet(&self.frame) {
                    if pcm {
                        if let Ok(mut r) = self.shared.resend.try_lock() {
                            r.keep(p);
                        }
                    }
                    if self.sock.send_to(p, self.peer).is_ok() {
                        self.shared.c.sent.fetch_add(1, Relaxed);
                        gap(&mut self.last_send, &self.shared.tx_gap_us);
                    } else {
                        self.shared.c.send_dropped.fetch_add(1, Relaxed);
                    }
                }
                // The controller thinks in 10 ms frames: a 20 ms frame counts as two, each with
                // half the encode time (same CPU share, same ~5 s hold). Hi-Fi (5 ms): no
                // encoding, nothing to steer.
                let tens = self.pk.frame() / FRAME;
                let us = t0.elapsed().as_micros() as f32 / tens as f32;
                for _ in 0..tens {
                    if let Some(level) = self.cx.on_encode(us) {
                        if self.pk.set_complexity(level).is_ok() {
                            self.shared.complexity.store(level, Relaxed);
                        }
                    }
                }
                self.frame.clear();
                self.frame.extend_from_slice(&chunk[take..]);
            }
        }
        self.shared.callback_us.fetch_max(start.elapsed().as_micros() as u32, Relaxed);
    }

    /// Follows the node's Music Mode flag: forced stereo, 20 ms frames, complexity up to 10;
    /// and its Hi-Fi flag: 5 ms PCM frames.
    /// Swaps in the encoder and resampler `Shared::set_mode` prepared (seq continues; the old
    /// ones go back into the slot, freed there); a partial frame is dropped.
    fn apply_mode(&mut self) {
        let (music, hifi) = (self.shared.music.load(Relaxed), self.shared.hifi.load(Relaxed));
        if (music, hifi) == (self.music, self.pk.pcm()) {
            return;
        }
        let Ok(mut slot) = self.shared.mode.try_lock() else { return }; // retried next callback
        let Some(m) = slot.as_mut().filter(|m| (m.pk.music, m.pk.pcm) == (music, hifi)) else { return };
        if self.pk.set_mode(&mut m.pk).is_err() {
            return;
        }
        std::mem::swap(&mut self.rs, &mut m.rs);
        drop(slot);
        self.music = music;
        let level = self.cx.set_max(if music { 10 } else { 5 });
        if self.pk.set_complexity(level).is_ok() {
            self.shared.complexity.store(level, Relaxed);
        }
        self.shared.bitrate.store(self.shown(self.last_bitrate), Relaxed);
        self.frame.clear(); // has room for a 20 ms stereo frame
    }

    /// The bitrate Stats show: Hi-Fi's, or the encoder's.
    fn shown(&self, bitrate: i32) -> i32 {
        if self.pk.pcm() { HIFI_BITRATE } else { bitrate }
    }

    /// Applies the session thread's latest bitrate/loss% target, only when it actually changed.
    /// In Hi-Fi it goes to the idle encoder, ready for a fallback.
    fn apply_rate(&mut self) {
        let bitrate = self.shared.target_bitrate.load(Relaxed);
        if bitrate != self.last_bitrate && self.pk.set_bitrate(bitrate).is_ok() {
            self.last_bitrate = bitrate;
            self.shared.bitrate.store(self.shown(bitrate), Relaxed);
        }
        let loss_perc = self.shared.target_loss_perc.load(Relaxed);
        if loss_perc != self.last_loss_perc && self.pk.set_packet_loss_perc(loss_perc).is_ok() {
            self.last_loss_perc = loss_perc;
        }
    }
}

impl Tx {
    fn new(cfg: &Settings, pk: Packetizer, sock: UdpSocket, peer: SocketAddr, shared: Arc<Shared>, dev_ch: usize, rate: u32) -> Tx {
        let ch = cfg.channels as usize;
        let (last_bitrate, last_loss_perc) = (shared.bitrate.load(Relaxed), shared.target_loss_perc.load(Relaxed));
        let gain = f32::from_bits(shared.send_gain.load(Relaxed)); // a link that starts silent stays so
        Tx {
            pk,
            sock,
            peer,
            dev_ch,
            rate,
            music: false,
            rs: (rate != RATE).then(|| Resampler::new(rate, RATE, ch)),
            mixed: Vec::with_capacity(16_384 * 2), // stereo: Music Mode can switch to it
            resampled: Vec::with_capacity(32_768 * 2),
            frame: Vec::with_capacity(4 * FRAME), // room for a 20 ms stereo frame
            last_send: None,
            shared,
            last_bitrate,
            last_loss_perc,
            cx: Complexity::default(),
            gain,
        }
    }
}

/// The capture stream and its sample rate.
fn build_input(dev: &cpal::Device, cfg: &Settings, pk: Packetizer, sock: UdpSocket, peer: SocketAddr, shared: Arc<Shared>) -> anyhow::Result<(cpal::Stream, u32)> {
    // "Everything this PC plays" is a playback device: its mix format is the one loopback takes.
    let sc = if cfg.input.as_deref() == Some(EVERYTHING) { dev.default_output_config()? } else { pick_config(dev.default_input_config()?, dev.supported_input_configs()?) };
    let (dev_ch, rate, fmt) = (sc.channels() as usize, sc.sample_rate(), sc.sample_format());
    let err_cb = err_cb(shared.clone(), true);
    let mut tx = Tx::new(cfg, pk, sock, peer, shared, dev_ch, rate);
    let mut c = sc.config();
    // Ask for 10 ms capture buffers so packets leave evenly instead of in bursts
    // (ALSA/PipeWire default to ~40 ms periods, which forces a deeper jitter buffer on the peer).
    if let cpal::SupportedBufferSize::Range { min, max } = *sc.buffer_size() {
        c.buffer_size = cpal::BufferSize::Fixed((rate / 100).clamp(min, max));
    }
    let age = |i: &cpal::InputCallbackInfo| {
        let t = i.timestamp();
        t.callback.duration_since(t.capture).as_micros().min(1_000_000) as u32
    };
    let s = match fmt {
        SampleFormat::F32 => dev.build_input_stream(c, move |d: &[f32], i: &_| tx.process(d, age(i)), err_cb, None)?,
        SampleFormat::I16 => dev.build_input_stream(c, move |d: &[i16], i: &_| tx.process(d, age(i)), err_cb, None)?,
        SampleFormat::I32 => dev.build_input_stream(c, move |d: &[i32], i: &_| tx.process(d, age(i)), err_cb, None)?,
        SampleFormat::U16 => dev.build_input_stream(c, move |d: &[u16], i: &_| tx.process(d, age(i)), err_cb, None)?,
        f => anyhow::bail!("unsupported input sample format {f}"),
    };
    Ok((s, rate))
}

struct Playback {
    cons: HeapCons<f32>,
    plan: Playout,
    rs: Resampler, // 48 kHz -> device rate, speed nudged by the drift controller
    base_step: f64,
    scratch: Vec<f32>, // 48 kHz stereo pulled from the ring
    staged: Vec<f32>,  // device-rate stereo waiting to be written
    dev_ch: usize,
    rate: u32, // device rate
    chirp: Option<(bool, u32)>, // push-to-talk chirp playing: (start, frames played)
    shared: Arc<Shared>,
}

impl Playback {
    /// `ahead_us` = how long until the first frame is heard (playback - callback timestamp), 0 if unknown.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], ahead_us: u32) {
        let frames = out.len() / self.dev_ch;
        // at least this callback's own buffer, when the host's timestamps don't say more
        let buffer_us = frames as u64 * 1_000_000 / self.rate as u64;
        self.shared.out_latency_us.store(ahead_us.max(buffer_us as u32), Relaxed);
        let fill_ms = (self.cons.occupied_len() / 2) as f32 * 1000.0 / RATE as f32;
        self.shared.buffer_ms.store(fill_ms.to_bits(), Relaxed);
        while self.staged.len() / 2 < frames {
            let missing = frames - self.staged.len() / 2;
            let avail = self.cons.occupied_len() / 2;
            self.scratch.clear();
            let base_need = (missing as f64 * self.base_step).ceil() as usize + 1;
            self.plan.set_music(self.shared.music.load(Relaxed));
            self.plan.set_hifi(self.shared.rx_pcm.load(Relaxed));
            self.plan.set_jitter(self.shared.jitter_us.load(Relaxed) as usize * RATE as usize / 1_000_000);
            match self.plan.plan(avail, base_need) {
                Plan::Play { discard, ratio } => {
                    self.rs.step = self.base_step * ratio;
                    let need = (missing as f64 * self.rs.step).ceil() as usize + 1;
                    if avail - discard >= need {
                        self.cons.skip(discard * 2);
                        self.scratch.resize(need * 2, 0.0);
                        self.cons.pop_slice(&mut self.scratch);
                        self.rs.process(&self.scratch, &mut self.staged);
                        continue;
                    }
                    self.plan.underrun();
                    self.shared.c.underruns.fetch_add(1, Relaxed);
                }
                Plan::Silence => {}
            }
            // silence goes straight to the device, bypassing the resampler
            self.staged.resize(frames * 2, 0.0);
        }
        let g = f32::from_bits(self.shared.recv_gain.load(Relaxed));
        if g != 1.0 {
            self.staged[..frames * 2].iter_mut().for_each(|s| *s = amplify(*s, g));
        }
        meter(&self.shared.out_peak, &self.staged[..frames * 2]);
        self.mix_chirp(frames);
        let target_ms = self.plan.target() as f32 * 1000.0 / RATE as f32;
        self.shared.target_ms.store(target_ms.to_bits(), Relaxed);
        let mono = self.shared.rx_channels.load(Relaxed) == 1;
        for (f, s) in out.chunks_exact_mut(self.dev_ch).zip(self.staged.as_chunks::<2>().0) {
            if self.dev_ch == 1 {
                f[0] = T::from_sample((s[0] + s[1]) * 0.5);
                continue;
            }
            for (i, o) in f.iter_mut().enumerate() {
                *o = T::from_sample(match i {
                    0 => s[0],
                    1 => s[1],
                    _ if mono => s[0],
                    _ => 0.0,
                });
            }
        }
        self.staged.drain(..frames * 2);
    }

    /// Adds the push-to-talk chirp (if one was asked for or is playing) to the next `frames`.
    fn mix_chirp(&mut self, frames: usize) {
        match self.shared.chirp.swap(0, Relaxed) {
            0 => {}
            n => self.chirp = Some((n == 1, 0)),
        }
        let Some((start, n)) = self.chirp.as_mut() else { return };
        for f in self.staged[..frames * 2].as_chunks_mut::<2>().0 {
            let c = ptt::chirp(*start, *n as f32 / self.rate as f32);
            f[0] += c;
            f[1] += c;
            *n += 1;
        }
        if *n as f32 >= ptt::CHIRP_SECS * self.rate as f32 {
            self.chirp = None;
        }
    }
}

fn build_output(dev: &cpal::Device, cons: HeapCons<f32>, shared: Arc<Shared>) -> anyhow::Result<cpal::Stream> {
    let sc = pick_config(dev.default_output_config()?, dev.supported_output_configs()?);
    let (rate, fmt) = (sc.sample_rate(), sc.sample_format());
    let err_cb = err_cb(shared.clone(), false);
    let mut pb = Playback {
        cons,
        plan: Playout::default(),
        rs: Resampler::new(RATE, rate, 2),
        base_step: RATE as f64 / rate as f64,
        scratch: Vec::with_capacity(TARGET * 40),
        staged: Vec::with_capacity(TARGET * 40),
        dev_ch: sc.channels() as usize,
        rate,
        chirp: None,
        shared,
    };
    let mut c = sc.config();
    // PulseAudio's default playback buffer is ~2 s; ask for 10 ms periods there (20 ms total).
    // Other hosts keep their default (validated) behaviour.
    if dev.id().is_ok_and(|i| i.host().name() == "PulseAudio") {
        if let cpal::SupportedBufferSize::Range { min, max } = *sc.buffer_size() {
            c.buffer_size = cpal::BufferSize::Fixed((rate / 100).clamp(min, max));
        }
    }
    let ahead = |i: &cpal::OutputCallbackInfo| {
        let t = i.timestamp();
        t.playback.duration_since(t.callback).as_micros().min(1_000_000) as u32
    };
    Ok(match fmt {
        SampleFormat::F32 => dev.build_output_stream(c, move |d: &mut [f32], i: &_| pb.fill(d, ahead(i)), err_cb, None)?,
        SampleFormat::I16 => dev.build_output_stream(c, move |d: &mut [i16], i: &_| pb.fill(d, ahead(i)), err_cb, None)?,
        SampleFormat::I32 => dev.build_output_stream(c, move |d: &mut [i32], i: &_| pb.fill(d, ahead(i)), err_cb, None)?,
        SampleFormat::U16 => dev.build_output_stream(c, move |d: &mut [u16], i: &_| pb.fill(d, ahead(i)), err_cb, None)?,
        f => anyhow::bail!("unsupported output sample format {f}"),
    })
}

#[cfg(test)]
#[test]
fn linux_labels() {
    let l = |input, pulse: &str| label(input, pulse, "desc".into());
    assert_eq!(l(true, "capralink_output.monitor").as_deref(), Some(VIRTUAL_OUTPUT));
    assert_eq!(l(false, "capralink_input_feed").as_deref(), Some(VIRTUAL_INPUT));
    for (input, hidden) in [(true, "capralink_input"), (true, "capralink_input_feed.monitor"), (false, "capralink_output")] {
        assert_eq!(l(input, hidden), None, "{hidden}");
    }
    assert_eq!(l(true, "alsa_input.usb-mic").as_deref(), Some("desc"));
    assert_eq!(l(false, "capralink_output.monitor").as_deref(), Some("desc"), "direction matters");
}

#[cfg(test)]
#[test]
fn windows_labels() {
    let shown = |input, names: &[&str]| names.iter().filter_map(|n| win_label(input, n.to_string())).collect::<Vec<_>>();
    let outs = ["Speakers (Realtek(R) Audio)", "CABLE Input (VB-Audio Virtual Cable)"];
    assert_eq!(shown(false, &outs), ["Speakers (Realtek(R) Audio)", VIRTUAL_INPUT]);
    assert!(shown(false, &["CABLE In 16ch (VB-Audio Virtual Cable)"]).is_empty(), "16-channel variant hidden");
    assert!(shown(true, &["CapraLink (VB-Audio Virtual Cable)"]).is_empty(), "renamed recording side still hidden");
    assert_eq!(shown(false, &["Discord mic feed (VB-Audio Virtual Cable)"]), [VIRTUAL_INPUT], "renamed playback side still mapped");
    assert_eq!(shown(false, &outs[..1]), ["Speakers (Realtek(R) Audio)"], "no VB-Cable, no CapraLink Input");
    let ins = ["Microphone (USB Mic)", "CABLE Output (VB-Audio Virtual Cable)"];
    assert_eq!(shown(true, &ins), ["Microphone (USB Mic)"]);
    assert_eq!(shown(true, &outs), ["Speakers (Realtek(R) Audio)"], "no VB-Cable endpoint is ever offered as Send from");
}

#[cfg(test)]
#[test]
fn device_ids() {
    let raw = |v: &[(&str, &str)]| v.iter().map(|(i, n)| (i.to_string(), n.to_string())).collect();
    let l = listed(raw(&[
        ("coreaudio:usb-1", "USB Mic"),
        ("coreaudio:builtin", "MacBook Pro Microphone"),
        ("coreaudio:usb-2", "USB Mic"),
        ("coreaudio:BlackHoleUID", VIRTUAL_OUTPUT),
        ("", EVERYTHING),
        ("coreaudio:usb-3", "USB Mic"),
    ]));
    let shown: Vec<_> = l.iter().map(|d| (d.id.as_str(), d.name.as_str())).collect();
    assert_eq!(
        shown,
        [
            ("coreaudio:usb-1", "USB Mic"),
            ("coreaudio:builtin", "MacBook Pro Microphone"),
            ("coreaudio:usb-2", "USB Mic (2)"),
            (VIRTUAL_OUTPUT, VIRTUAL_OUTPUT),
            (EVERYTHING, EVERYTHING),
            ("coreaudio:usb-3", "USB Mic (3)"),
        ],
        "duplicates told apart; CapraLink's devices and Everything keep fixed ids"
    );
    for (i, d) in l.iter().enumerate() {
        assert_eq!(pick(&l, &d.id), Some(i), "{} resolves to itself", d.id);
    }
    // the same devices enumerated in another order still resolve by id
    let moved = listed(raw(&[("coreaudio:usb-2", "USB Mic"), ("coreaudio:usb-1", "USB Mic")]));
    assert_eq!(moved[pick(&moved, "coreaudio:usb-2").unwrap()].id, "coreaudio:usb-2");
    // settings saved by name (0.1.x) still resolve, to the first match as before
    assert_eq!(pick(&l, "USB Mic"), Some(0));
    assert_eq!(pick(&l, "MacBook Pro Microphone"), Some(1));
    assert_eq!(pick(&l, VIRTUAL_OUTPUT), Some(3));
    assert_eq!(pick(&l, "coreaudio:gone"), None);
    // Linux/Windows virtual devices get their fixed id from the label, whatever cpal calls them
    let lin = listed(raw(&[("pulseaudio:capralink_input_feed", &label(false, "capralink_input_feed", "x".into()).unwrap())]));
    let win = listed(raw(&[("wasapi:{0.0.0.00000000}.{abc}", &win_label(false, "CABLE Input (VB-Audio Virtual Cable)".into()).unwrap())]));
    assert_eq!((lin[0].id.as_str(), win[0].id.as_str()), (VIRTUAL_INPUT, VIRTUAL_INPUT));
    assert_eq!(missing_name(&format!("{}:gone", cpal::default_host().id())), "Saved device");
    assert_eq!(missing_name("USB Mic"), "USB Mic");
}

#[cfg(test)]
#[test]
fn stream_error_failures() {
    use cpal::{Error, ErrorKind::*};
    let f = |k, input| failure(&Error::new(k), input);
    assert!(matches!(f(StreamInvalidated, true), Some(Failure::Rebuild(m)) if m.starts_with("Send from device")));
    assert!(matches!(f(DeviceNotAvailable, false), Some(Failure::End(m)) if m.starts_with("Play to device stopped")));
    assert!(matches!(f(HostUnavailable, true), Some(Failure::End(_))));
    assert!(matches!(f(PermissionDenied, true), Some(Failure::End(m)) if m.contains("microphone access")));
    assert!(matches!(f(PermissionDenied, false), Some(Failure::End(m)) if !m.contains("microphone")));
    for k in [DeviceBusy, DeviceChanged, Xrun, RealtimeDenied, BackendError, Other] {
        assert_eq!(f(k, true), None, "{k:?} is log-only");
    }
}

/// Test builds count heap allocations per thread, to prove the capture callback makes none.
#[cfg(test)]
mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    thread_local!(static N: Cell<usize> = const { Cell::new(0) });
    struct Counting;
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let _ = N.try_with(|n| n.set(n.get() + 1));
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) }
        }
    }
    #[global_allocator]
    static A: Counting = Counting;
    pub fn allocations() -> usize {
        N.with(Cell::get)
    }
}

#[cfg(test)]
#[test]
fn capture_callback_doesnt_allocate() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (shared, rate) = (Arc::new(Shared::default()), 96_000); // resampled, through the anti-alias filter
    let pk = Packetizer::new(1, 64_000, &[7; 32]).unwrap();
    let mut tx = Tx::new(&Settings::default(), pk, UdpSocket::bind("127.0.0.1:0").unwrap(), peer.local_addr().unwrap(), shared.clone(), 2, rate);
    let chunk = [0.1f32; 2 * 960]; // 10 ms of device stereo
    tx.process(&chunk, 0);
    shared.set_mode(true, false, Some((1, rate))); // what `Link::set_mode` does
    shared.gains(1.5, 1.0); // 150%: through the soft clip
    let before = alloc_count::allocations();
    for _ in 0..10 {
        tx.process(&chunk, 0);
    }
    assert_eq!(alloc_count::allocations() - before, 0, "the capture callback allocated");
    assert!(tx.music && tx.pk.channels() == 2 && tx.pk.frame() == 2 * FRAME, "switched to Music Mode");
    assert_eq!(shared.c.sent.load(Relaxed), 1 + 5, "one 10 ms mono packet, then five 20 ms stereo ones");
    assert!(shared.callback_us.load(Relaxed) > 0);
    // Hi-Fi: PCM packets, kept for resending, still without allocating
    shared.set_mode(true, true, Some((1, rate)));
    let (before, sent) = (alloc_count::allocations(), shared.c.sent.load(Relaxed));
    for _ in 0..10 {
        tx.process(&chunk, 0);
    }
    assert_eq!(alloc_count::allocations() - before, 0, "the capture callback allocated in Hi-Fi");
    assert!(tx.pk.pcm() && tx.pk.frame() == dsp::PCM_FRAME, "switched to Hi-Fi");
    let sent = shared.c.sent.load(Relaxed) - sent;
    assert!((19..=20).contains(&sent), "5 ms packets: {sent}");
    assert_eq!(shared.bitrate.load(Relaxed), HIFI_BITRATE);
    let last = u32::from_be_bytes(tx.pk.packet(&[0.0; 2 * dsp::PCM_FRAME]).unwrap()[3..7].try_into().unwrap()) - 1;
    assert!(shared.resend.lock().unwrap().get(last).is_some(), "kept for resending");
}

/// a's sender → a relay that loses some packets → b's receiver; b's NACKs go back through the
/// relay to a's receive thread, which resends: b ends up with every packet, in order.
#[cfg(test)]
#[test]
fn hifi_loopback_resends_fill_losses() {
    let bind = || {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let a = s.local_addr().unwrap();
        (s, a)
    };
    let ((sa, a_addr), (sb, b_addr), (relay, relay_addr)) = (bind(), bind(), bind());
    let (key, other) = ([7u8; 32], [9u8; 32]); // a → b, b → a
    let (a_sh, b_sh, stop) = (Shared::default(), Shared::default(), AtomicBool::new(false));
    a_sh.gains(1.0, 1.0);
    let a_shared = Arc::new(a_sh);
    a_shared.set_mode(true, true, Some((1, RATE)));
    let pk = Packetizer::new(1, 64_000, &key).unwrap();
    let mut tx = Tx::new(&Settings::default(), pk, sa.try_clone().unwrap(), relay_addr, a_shared.clone(), 2, RATE);
    let (a_prod, _a_cons) = HeapRb::<f32>::new(RATE as usize).split();
    let (mut b_prod, mut b_cons) = HeapRb::<f32>::new(RATE as usize * 3).split();
    let cushion = RATE as usize / 10 * 2; // 100 ms already buffered: no deadline hits
    b_prod.push_slice(&vec![0.0; cushion]);
    let lose = |seq: u32| seq % 10 == 3 || (100..110).contains(&seq); // 29 of 200, each once
    let tone: Vec<f32> = (0..2 * RATE as usize).map(|i| (i as f32 * 0.013).sin() * 0.7).collect(); // 1 s, stereo
    std::thread::scope(|s| {
        let (a, b, st) = (&*a_shared, &b_sh, &stop);
        s.spawn(move || receive(sa, relay_addr, Rx::new(&other), Nacks::new(&key), a_prod, a, st));
        s.spawn(move || receive(sb, relay_addr, Rx::new(&key), Nacks::new(&other), b_prod, b, st));
        s.spawn(move || {
            let (mut buf, mut seen) = ([0u8; 2048], std::collections::HashSet::new());
            while !st.load(Relaxed) {
                let Ok((n, from)) = relay.recv_from(&mut buf) else { continue };
                let seq = u32::from_be_bytes(buf[3..7].try_into().unwrap());
                if from == a_addr && seen.insert(seq) && lose(seq) {
                    continue; // lost, the first time
                }
                let _ = relay.send_to(&buf[..n], if from == a_addr { b_addr } else { a_addr });
            }
        });
        for chunk in tone.chunks(2 * 480) {
            tx.process(chunk, 0);
            std::thread::sleep(Duration::from_millis(2));
        }
        let t = Instant::now();
        while b.c.received.load(Relaxed) < 200 && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Relaxed);
    });
    let c = &b_sh.c;
    assert_eq!(a_shared.c.sent.load(Relaxed), 200);
    assert_eq!((c.received.load(Relaxed), c.fec_recovered.load(Relaxed), c.lost.load(Relaxed)), (200, 29, 0));
    assert!(b_sh.rx_pcm.load(Relaxed));
    let mut got = vec![0.0; b_cons.occupied_len()];
    b_cons.pop_slice(&mut got);
    let want: Vec<f32> = tone.iter().map(|x| (x * 8_388_608.0).round() / 8_388_608.0).collect();
    assert!(got[cushion..] == want[..], "every packet, in order, lossless to 24 bits");
}

#[cfg(test)]
#[test]
fn volume_and_soft_clip() {
    assert_eq!((gain(100, false), gain(150, false), gain(0, false), gain(150, true)), (1.0, 1.5, 0.0, 0.0));
    for x in [-1.3f32, -0.95, 0.0, 0.5, 0.91, 1.0] {
        assert_eq!(amplify(x, 1.0), x, "100% is untouched");
    }
    assert_eq!(amplify(0.5, 0.0), 0.0);
    assert!((amplify(0.4, 1.5) - 0.6).abs() < 1e-6, "linear below 0.9");
    let mut prev = 0.0;
    for i in 1..=100 {
        let y = amplify(i as f32 / 100.0, 1.5);
        assert!(y > prev && y < 1.0, "rises smoothly and never reaches full scale: {y}");
        assert!((y - prev) <= 0.015 + 1e-6, "no jump at the knee");
        prev = y;
    }
    assert_eq!(amplify(-0.8, 1.5), -amplify(0.8, 1.5));
}

#[cfg(test)]
#[test]
fn send_gain_mute_and_capture_latency() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let shared = Arc::new(Shared::default());
    let pk = Packetizer::new(1, 64_000, &[7; 32]).unwrap();
    let mut tx = Tx::new(&Settings::default(), pk, UdpSocket::bind("127.0.0.1:0").unwrap(), peer.local_addr().unwrap(), shared.clone(), 2, RATE);
    let level = || f32::from_bits(shared.in_peak.swap(0, Relaxed));
    shared.gains(gain(150, false), 1.0);
    tx.process(&[0.4f32; 2 * 480], 0); // fades up from silence over 10 ms (no click)...
    assert!(level() < 0.45, "still fading in");
    tx.process(&[0.4f32; 2 * 480], 0);
    assert!((level() - 0.6).abs() < 1e-6, "...then 150%");
    assert_eq!(shared.in_latency_us.load(Relaxed), 10_000, "no timestamps: the 10 ms buffer itself");
    tx.process(&[0.4f32; 2 * 480], 25_000);
    assert_eq!(shared.in_latency_us.load(Relaxed), 25_000, "the host's timestamps when they say more");
    level();
    shared.gains(gain(150, true), 1.0); // muted: fades out over 10 ms, then silence, still sent
    tx.process(&[0.4f32; 2 * 480], 0);
    let fading = tx.mixed.iter().step_by(2).copied().collect::<Vec<_>>();
    assert!(fading[0] > 0.5 && fading.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < 0.01), "a smooth fade, no jump");
    tx.process(&[0.4f32; 2 * 480], 0); // 150% → 0 takes 15 ms
    level();
    let sent = shared.c.sent.load(Relaxed);
    tx.process(&[0.4f32; 2 * 480], 0);
    assert_eq!((level(), shared.c.sent.load(Relaxed)), (0.0, sent + 1));
}

#[cfg(test)]
#[test]
fn chirp_mixes_into_playback() {
    let (_, cons) = HeapRb::<f32>::new(RATE as usize).split();
    let shared = Arc::new(Shared::default());
    shared.gains(1.0, 1.0);
    let mut pb = Playback { cons, plan: Playout::default(), rs: Resampler::new(RATE, RATE, 2), base_step: 1.0, scratch: vec![], staged: vec![], dev_ch: 2, rate: RATE, chirp: None, shared: shared.clone() };
    let mut out = vec![0f32; 2 * 480];
    pb.fill(&mut out, 0);
    assert!(out.iter().all(|s| *s == 0.0), "nothing received: silence");
    assert_eq!(shared.out_latency_us.load(Relaxed), 10_000);
    shared.chirp.store(1, Relaxed); // what `Link::chirp(true)` does
    let mut heard: Vec<f32> = vec![];
    for _ in 0..20 {
        pb.fill(&mut out, 0);
        heard.extend(out.iter().step_by(2));
    }
    let want: Vec<f32> = (0..heard.len()).map(|i| ptt::chirp(true, i as f32 / RATE as f32)).collect();
    assert_eq!(heard, want, "the start chirp, from its first sample");
    assert!(pb.chirp.is_none(), "done after {} s", ptt::CHIRP_SECS);
}

#[cfg(test)]
#[test]
fn link_is_send() {
    fn send<T: Send>() {}
    send::<Link>();
}
