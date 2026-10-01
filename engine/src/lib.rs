//! CapraLink audio engine: capture -> Opus -> UDP -> Opus -> playback.

mod dsp;
mod node;
mod rpc;
mod vdev;
#[cfg(windows)]
mod wincap;

pub use node::{Device, Node, NodeState, RemoteConfig};
pub use rpc::{daemon, daemon_exe, serve_rpc, Client, Devices};

/// A command for a system tool (pactl, systemctl, reg, hostname). Inside an AppImage,
/// LD_LIBRARY_PATH points at the bundled libraries, which break system binaries
/// (e.g. systemctl needs a newer OpenSSL than the bundled one), so it is dropped.
pub(crate) fn system_command(program: &str) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    if std::env::var_os("APPIMAGE").is_some() {
        c.env_remove("LD_LIBRARY_PATH");
    }
    c
}

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use dsp::{Complexity, Counters, Jitter, Packetizer, Plan, Playout, Resampler, Rx, FRAME, JITTER_WINDOW_US, MUSIC_JITTER_WINDOW_US, RATE, TARGET};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Settings device names that mean the CapraLink virtual devices (MASTER.md §3.4).
/// On macOS the drivers carry these names; on Linux `label` maps the Pulse devices to them.
pub const VIRTUAL_OUTPUT: &str = "CapraLink Output";
pub const VIRTUAL_INPUT: &str = "CapraLink Input";

/// Windows Send-from name: loopback of the default playback device (MASTER.md §3.8).
pub const EVERYTHING: &str = "Everything this PC plays";
/// Send-from / Play-to value that turns that direction off (e.g. while another app such as a
/// remote-desktop client already carries this computer's audio).
pub const NO_DEVICE: &str = "none";

pub fn input_devices() -> Vec<String> {
    #[allow(unused_mut)]
    let mut v: Vec<String> = devices(&cpal::default_host(), true).into_iter().map(|(n, _)| n).collect();
    // ponytail: app list is a snapshot taken when the lists load; refresh it live if people miss apps started later.
    #[cfg(windows)]
    v.extend(app_entries(wincap::sessions().into_iter().map(|(_, exe)| exe)));
    v
}

pub fn output_devices() -> Vec<String> {
    devices(&cpal::default_host(), false).into_iter().map(|(n, _)| n).collect()
}

/// Devices by the name the lists show (and settings store); hidden plumbing left out.
fn devices(host: &cpal::Host, input: bool) -> Vec<(String, cpal::Device)> {
    let all = if input { host.input_devices() } else { host.output_devices() };
    let shown = |d: &cpal::Device| match d.id() {
        Ok(id) if cfg!(target_os = "linux") => label(input, id.id(), name(d)),
        _ if cfg!(windows) => win_label(input, name(d)),
        _ => Some(name(d)),
    };
    let mut v: Vec<_> = all.into_iter().flatten().filter_map(|d| Some((shown(&d)?, d))).collect();
    if cfg!(windows) && input {
        v.extend(host.default_output_device().map(|d| (EVERYTHING.to_string(), d))); // WASAPI records it in loopback mode
    }
    v
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

/// Windows Send-from value for one app's audio: `app:Discord.exe` -> `Discord.exe`.
fn app_exe(name: &str) -> Option<&str> {
    name.strip_prefix("app:")
}

/// What people call an app: `Discord.exe` -> `Discord`.
#[cfg_attr(not(windows), allow(dead_code))]
fn app_title(exe: &str) -> &str {
    exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe)
}

/// Send-from values for the apps with audio sessions: deduped, sorted, never CapraLink itself
/// (it would capture its own playback), and always Discord (a saved choice shows while it's closed).
#[cfg_attr(not(windows), allow(dead_code))]
fn app_entries(exes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = exes.into_iter().chain(["Discord.exe".to_string()]).filter(|e| !e.to_lowercase().starts_with("capralink")).map(|e| format!("app:{e}")).collect();
    v.sort_by_key(|s| s.to_lowercase());
    v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    v
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings { input: None, output: None, bitrate: 64_000, channels: 1, service: false, remote_config: false, music_mode: false }
    }
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
}

/// A running TX + RX link. Dropping it stops everything.
pub struct Link {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    rx: Option<JoinHandle<()>>,
    _input: Box<dyn Send>, // cpal stream, or an app capture on Windows
    _output: Option<cpal::Stream>, // None when Play to is off
}

impl Link {
    /// Streams to/from `peer` over UDP `port`; only packets from exactly `peer` are accepted.
    pub fn start(cfg: &Settings, port: u16, peer: SocketAddr, keys: &Keys) -> anyhow::Result<Link> {
        anyhow::ensure!(matches!(cfg.channels, 1 | 2), "channels must be 1 or 2");
        let host = cpal::default_host();
        let find = |input: bool, want: &Option<String>, default: Option<cpal::Device>| match want {
            Some(n) => devices(&host, input).into_iter().find(|(s, _)| s == n).map(|(_, d)| d).ok_or_else(|| anyhow!("device not found: {n}")),
            None => default.ok_or_else(|| anyhow!("no default device")),
        };
        let off = |want: &Option<String>| want.as_deref() == Some(NO_DEVICE);
        let app = cfg.input.as_deref().and_then(app_exe).filter(|_| cfg!(windows));
        let in_dev = match app {
            _ if off(&cfg.input) => None,
            Some(_) => None,
            None => Some(find(true, &cfg.input, host.default_input_device())?),
        };
        let out_dev = if off(&cfg.output) { None } else { Some(find(false, &cfg.output, host.default_output_device())?) };

        let sock = UdpSocket::bind(("0.0.0.0", port)).with_context(|| format!("bind UDP port {port}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let shared = Arc::new(Shared::default());
        let initial_bitrate = cfg.bitrate.clamp(8_000, 96_000);
        shared.target_bitrate.store(initial_bitrate, Relaxed);
        shared.bitrate.store(initial_bitrate, Relaxed);
        shared.target_loss_perc.store(5, Relaxed);
        shared.complexity.store(5, Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        let (mut prod, cons) = HeapRb::<f32>::new(RATE as usize).split(); // 500 ms of stereo

        // ponytail: with Play to = none, packets are still decoded into a ring nobody drains
        // (it just stays full); skip decoding in the RX thread if that CPU ever matters.
        let output = out_dev.map(|d| build_output(&d, cons, shared.clone())).transpose()?;
        let pk = Packetizer::new(cfg.channels, cfg.bitrate, &keys.send)?;
        let input: Box<dyn Send> = match (in_dev, app) {
            _ if off(&cfg.input) => Box::new(()), // sending off: nothing captured or sent
            (Some(d), _) => {
                let s = build_input(&d, cfg, pk, sock.try_clone()?, peer, shared.clone())?;
                s.play()?;
                Box::new(s)
            }
            #[cfg(windows)]
            (None, Some(exe)) => {
                let mut tx = Tx::new(cfg, pk, sock.try_clone()?, peer, shared.clone(), 2, RATE);
                Box::new(wincap::start(exe, move |d: &[f32]| tx.process(d))?)
            }
            _ => unreachable!("app capture is Windows-only"),
        };

        let rx = {
            let (shared, stop, mut rx) = (shared.clone(), stop.clone(), Rx::new(&keys.recv));
            std::thread::Builder::new().name("capralink-rx".into()).spawn(move || {
                let (mut buf, mut last, mut jitter) = ([0u8; 2048], None, Jitter::default());
                while !stop.load(Relaxed) {
                    // Errors are timeouts (checked above) or transient (e.g. ICMP resets on Windows).
                    // ponytail: exact source match; a multi-homed peer replying from another
                    // interface is ignored — relax to IP-only (packets are authenticated) if seen.
                    let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                    if from != peer {
                        continue;
                    }
                    if let Some(ch) = rx.handle(&mut buf[..n], &shared.c, &mut |pcm: &[f32]| {
                        prod.push_slice(pcm);
                    }) {
                        let g = gap(&mut last, &shared.rx_gap_us);
                        shared.jitter_us.store(jitter.push(g, rx.period_us(), if shared.music.load(Relaxed) { MUSIC_JITTER_WINDOW_US } else { JITTER_WINDOW_US }), Relaxed);
                        shared.rx_channels.store(ch, Relaxed);
                    }
                }
            })?
        };
        if let Some(o) = &output {
            o.play()?;
        }
        Ok(Link { shared, stop, rx: Some(rx), _input: input, _output: output })
    }

    /// Current (sending, receiving) VU levels, 0..1; cheap enough to poll many times a second.
    pub fn levels(&self) -> (f32, f32) {
        (f32::from_bits(self.shared.in_peak.load(Relaxed)), f32::from_bits(self.shared.out_peak.load(Relaxed)))
    }

    pub fn stats(&self) -> Stats {
        let c = &self.shared.c;
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
            tx_gap_ms: self.shared.tx_gap_us.swap(0, Relaxed) as f32 / 1000.0,
            rx_gap_ms: self.shared.rx_gap_us.swap(0, Relaxed) as f32 / 1000.0,
            bitrate: self.shared.bitrate.load(Relaxed),
            complexity: self.shared.complexity.load(Relaxed),
            music: self.shared.music.load(Relaxed),
        }
    }

    /// Switches this link's sender and receiver into or out of Music Mode, live.
    pub fn set_music(&self, on: bool) {
        self.shared.music.store(on, Relaxed);
    }

    /// Sets the sender's target bitrate/loss%, applied by the capture callback (MASTER.md §3.5).
    pub fn set_rate(&self, bitrate: i32, loss_perc: u8) {
        self.shared.target_bitrate.store(bitrate, Relaxed);
        self.shared.target_loss_perc.store(loss_perc, Relaxed);
    }

    /// This link's own receive-side counters (cumulative) plus current jitter, for building
    /// the periodic peer report. Doesn't reset anything (unlike `stats()`).
    pub fn report_counters(&self) -> (u64, u64, u64, f32) {
        let c = &self.shared.c;
        (c.received.load(Relaxed), c.lost.load(Relaxed), c.underruns.load(Relaxed), self.shared.jitter_us.load(Relaxed) as f32 / 1000.0)
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

fn err_cb(e: cpal::Error) {
    eprintln!("audio stream error: {e}");
}

struct Tx {
    pk: Packetizer,
    sock: UdpSocket,
    peer: SocketAddr,
    dev_ch: usize,
    rate: u32,
    user_ch: u16,
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
}

impl Tx {
    // ponytail: encode + send run inside the capture callback; move them to a dedicated
    // encode thread fed by a ring if callbacks ever overrun.
    fn process<T: SizedSample>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
        self.apply_mode();
        self.apply_rate();
        let (dev_ch, out_ch) = (self.dev_ch, self.pk.channels());
        self.mixed.clear();
        for f in data.chunks_exact(dev_ch) {
            let s = |i: usize| f32::from_sample(f[i.min(dev_ch - 1)]);
            if out_ch == 1 {
                self.mixed.push((0..dev_ch).map(s).sum::<f32>() / dev_ch as f32);
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
                if let Ok(p) = self.pk.packet(&self.frame) {
                    if self.sock.send_to(p, self.peer).is_ok() {
                        self.shared.c.sent.fetch_add(1, Relaxed);
                        gap(&mut self.last_send, &self.shared.tx_gap_us);
                    }
                }
                // The controller thinks in 10 ms frames: a 20 ms frame counts as two, each with
                // half the encode time (same CPU share, same ~5 s hold).
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
    }

    /// Follows the node's Music Mode flag: forced stereo, 20 ms frames, complexity up to 10.
    /// The encoder is rebuilt in place (seq continues); a partial frame is dropped.
    fn apply_mode(&mut self) {
        let music = self.shared.music.load(Relaxed);
        if music == self.music {
            return;
        }
        self.music = music;
        let ch = if music { 2 } else { self.user_ch };
        // ponytail: allocates (new Opus encoder, resampler, buffers) in the audio callback; fine for a rare user toggle
        let _ = self.pk.set_mode(ch, music);
        let level = self.cx.set_max(if music { 10 } else { 5 });
        if self.pk.set_complexity(level).is_ok() {
            self.shared.complexity.store(level, Relaxed);
        }
        let ch = self.pk.channels();
        if self.rs.is_some() {
            self.rs = Some(Resampler::new(self.rate, RATE, ch));
        }
        self.frame.clear();
        self.frame.reserve(self.pk.frame() * ch);
    }

    /// Applies the session thread's latest bitrate/loss% target, only when it actually changed.
    fn apply_rate(&mut self) {
        let bitrate = self.shared.target_bitrate.load(Relaxed);
        if bitrate != self.last_bitrate && self.pk.set_bitrate(bitrate).is_ok() {
            self.last_bitrate = bitrate;
            self.shared.bitrate.store(bitrate, Relaxed);
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
        Tx {
            pk,
            sock,
            peer,
            dev_ch,
            rate,
            user_ch: cfg.channels,
            music: false,
            rs: (rate != RATE).then(|| Resampler::new(rate, RATE, ch)),
            mixed: Vec::with_capacity(16_384 * ch),
            resampled: Vec::with_capacity(32_768 * ch),
            frame: Vec::with_capacity(4 * FRAME), // room for a 20 ms stereo frame
            last_send: None,
            shared,
            last_bitrate,
            last_loss_perc,
            cx: Complexity::default(),
        }
    }
}

fn build_input(dev: &cpal::Device, cfg: &Settings, pk: Packetizer, sock: UdpSocket, peer: SocketAddr, shared: Arc<Shared>) -> anyhow::Result<cpal::Stream> {
    // "Everything this PC plays" is a playback device: its mix format is the one loopback takes.
    let sc = if cfg.input.as_deref() == Some(EVERYTHING) { dev.default_output_config()? } else { pick_config(dev.default_input_config()?, dev.supported_input_configs()?) };
    let (dev_ch, rate, fmt) = (sc.channels() as usize, sc.sample_rate(), sc.sample_format());
    let mut tx = Tx::new(cfg, pk, sock, peer, shared, dev_ch, rate);
    let mut c = sc.config();
    // Ask for 10 ms capture buffers so packets leave evenly instead of in bursts
    // (ALSA/PipeWire default to ~40 ms periods, which forces a deeper jitter buffer on the peer).
    if let cpal::SupportedBufferSize::Range { min, max } = *sc.buffer_size() {
        c.buffer_size = cpal::BufferSize::Fixed((rate / 100).clamp(min, max));
    }
    Ok(match fmt {
        SampleFormat::F32 => dev.build_input_stream(c, move |d: &[f32], _: &_| tx.process(d), err_cb, None)?,
        SampleFormat::I16 => dev.build_input_stream(c, move |d: &[i16], _: &_| tx.process(d), err_cb, None)?,
        SampleFormat::I32 => dev.build_input_stream(c, move |d: &[i32], _: &_| tx.process(d), err_cb, None)?,
        SampleFormat::U16 => dev.build_input_stream(c, move |d: &[u16], _: &_| tx.process(d), err_cb, None)?,
        f => anyhow::bail!("unsupported input sample format {f}"),
    })
}

struct Playback {
    cons: HeapCons<f32>,
    plan: Playout,
    rs: Resampler, // 48 kHz -> device rate, speed nudged by the drift controller
    base_step: f64,
    scratch: Vec<f32>, // 48 kHz stereo pulled from the ring
    staged: Vec<f32>,  // device-rate stereo waiting to be written
    dev_ch: usize,
    shared: Arc<Shared>,
}

impl Playback {
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let frames = out.len() / self.dev_ch;
        let fill_ms = (self.cons.occupied_len() / 2) as f32 * 1000.0 / RATE as f32;
        self.shared.buffer_ms.store(fill_ms.to_bits(), Relaxed);
        while self.staged.len() / 2 < frames {
            let missing = frames - self.staged.len() / 2;
            let avail = self.cons.occupied_len() / 2;
            self.scratch.clear();
            let base_need = (missing as f64 * self.base_step).ceil() as usize + 1;
            self.plan.set_music(self.shared.music.load(Relaxed));
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
        meter(&self.shared.out_peak, &self.staged[..frames * 2]);
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
}

fn build_output(dev: &cpal::Device, cons: HeapCons<f32>, shared: Arc<Shared>) -> anyhow::Result<cpal::Stream> {
    let sc = pick_config(dev.default_output_config()?, dev.supported_output_configs()?);
    let (rate, fmt) = (sc.sample_rate(), sc.sample_format());
    let mut pb = Playback {
        cons,
        plan: Playout::default(),
        rs: Resampler::new(RATE, rate, 2),
        base_step: RATE as f64 / rate as f64,
        scratch: Vec::with_capacity(TARGET * 40),
        staged: Vec::with_capacity(TARGET * 40),
        dev_ch: sc.channels() as usize,
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
    Ok(match fmt {
        SampleFormat::F32 => dev.build_output_stream(c, move |d: &mut [f32], _: &_| pb.fill(d), err_cb, None)?,
        SampleFormat::I16 => dev.build_output_stream(c, move |d: &mut [i16], _: &_| pb.fill(d), err_cb, None)?,
        SampleFormat::I32 => dev.build_output_stream(c, move |d: &mut [i32], _: &_| pb.fill(d), err_cb, None)?,
        SampleFormat::U16 => dev.build_output_stream(c, move |d: &mut [u16], _: &_| pb.fill(d), err_cb, None)?,
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
    assert_eq!(shown(true, &outs), outs, "direction matters");
}

#[cfg(test)]
#[test]
fn app_entries_and_names() {
    let e = app_entries(["chrome.exe", "Spotify.exe", "chrome.exe", "CapraLink.exe", "discord.exe"].map(String::from));
    assert_eq!(e, ["app:chrome.exe", "app:discord.exe", "app:Spotify.exe"]);
    assert_eq!(app_entries([]), ["app:Discord.exe"], "Discord always listed");
    assert_eq!(app_exe("app:Discord.exe"), Some("Discord.exe"));
    assert_eq!(app_exe("Microphone"), None);
    assert_eq!(app_title("Discord.exe"), "Discord");
    assert_eq!(app_title("GAME.EXE"), "GAME");
}

#[cfg(test)]
#[test]
fn link_is_send() {
    fn send<T: Send>() {}
    send::<Link>();
}
