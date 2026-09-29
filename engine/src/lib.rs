//! CapraLink audio engine: capture -> Opus -> UDP -> Opus -> playback.

mod dsp;
mod node;
mod vdev;

pub use node::{Device, Node, NodeState};

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use dsp::{Counters, Jitter, Packetizer, Plan, Playout, Resampler, Rx, FRAME, RATE, TARGET};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Settings device names that mean the CapraLink virtual devices (MASTER.md §3.4).
/// On macOS the drivers carry these names; on Linux `label` maps the Pulse devices to them.
pub const VIRTUAL_OUTPUT: &str = "CapraLink Output";
pub const VIRTUAL_INPUT: &str = "CapraLink Input";

pub fn input_devices() -> Vec<String> {
    devices(&cpal::default_host(), true).into_iter().map(|(n, _)| n).collect()
}

pub fn output_devices() -> Vec<String> {
    devices(&cpal::default_host(), false).into_iter().map(|(n, _)| n).collect()
}

/// Devices by the name the lists show (and settings store); hidden plumbing left out.
fn devices(host: &cpal::Host, input: bool) -> Vec<(String, cpal::Device)> {
    let all = if input { host.input_devices() } else { host.output_devices() };
    let shown = |d: &cpal::Device| match d.id() {
        Ok(id) if cfg!(target_os = "linux") => label(input, id.id(), name(d)),
        _ => Some(name(d)),
    };
    all.into_iter().flatten().filter_map(|d| Some((shown(&d)?, d))).collect()
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

/// Audio settings (saved in the node's config). `None` device = system default.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub input: Option<String>,
    pub output: Option<String>,
    pub bitrate: i32,
    pub channels: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { input: None, output: None, bitrate: 64_000, channels: 1 }
    }
}

/// Per-session audio keys (ChaCha20-Poly1305): one per direction.
pub struct Keys {
    pub send: [u8; 32],
    pub recv: [u8; 32],
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stats {
    pub sent: u64,
    pub received: u64,
    pub lost: u64,
    pub fec_recovered: u64,
    pub underruns: u64,
    pub buffer_ms: f32,
    /// Current adaptive playout target; grows when the link is bursty.
    pub target_ms: f32,
    /// Peak sample (0..1) captured / played since the previous `stats()` call.
    pub in_peak: f32,
    pub out_peak: f32,
    /// Longest gap between packets sent / received since the previous `stats()` call.
    /// Even ~10 ms means a smooth link; big gaps show where bursts come from.
    pub tx_gap_ms: f32,
    pub rx_gap_ms: f32,
}

#[derive(Default)]
struct Shared {
    c: Counters,
    buffer_ms: AtomicU32, // f32 bits
    target_ms: AtomicU32, // f32 bits
    in_peak: AtomicU32,   // f32 bits, reset on read
    out_peak: AtomicU32,  // f32 bits, reset on read
    rx_channels: AtomicU8,
    tx_gap_us: AtomicU32, // reset on read
    rx_gap_us: AtomicU32, // reset on read
    jitter_us: AtomicU32, // worst recent packet lateness
}

/// A running TX + RX link. Dropping it stops everything.
pub struct Link {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    rx: Option<JoinHandle<()>>,
    _input: cpal::Stream,
    _output: cpal::Stream,
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
        let in_dev = find(true, &cfg.input, host.default_input_device())?;
        let out_dev = find(false, &cfg.output, host.default_output_device())?;

        let sock = UdpSocket::bind(("0.0.0.0", port)).with_context(|| format!("bind UDP port {port}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (mut prod, cons) = HeapRb::<f32>::new(RATE as usize).split(); // 500 ms of stereo

        let input = build_input(&in_dev, cfg, Packetizer::new(cfg.channels, cfg.bitrate, &keys.send)?, sock.try_clone()?, peer, shared.clone())?;
        let output = build_output(&out_dev, cons, shared.clone())?;

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
                        shared.jitter_us.store(jitter.push(g), Relaxed);
                        shared.rx_channels.store(ch, Relaxed);
                    }
                }
            })?
        };
        input.play()?;
        output.play()?;
        Ok(Link { shared, stop, rx: Some(rx), _input: input, _output: output })
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
            in_peak: f32::from_bits(self.shared.in_peak.swap(0, Relaxed)),
            out_peak: f32::from_bits(self.shared.out_peak.swap(0, Relaxed)),
            tx_gap_ms: self.shared.tx_gap_us.swap(0, Relaxed) as f32 / 1000.0,
            rx_gap_ms: self.shared.rx_gap_us.swap(0, Relaxed) as f32 / 1000.0,
        }
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
fn meter(peak: &AtomicU32, samples: &[f32]) {
    let p = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    peak.fetch_max(p.to_bits(), Relaxed);
}

fn err_cb(e: cpal::Error) {
    eprintln!("audio stream error: {e}");
}

struct Tx {
    pk: Packetizer,
    sock: UdpSocket,
    peer: SocketAddr,
    dev_ch: usize,
    rs: Option<Resampler>,
    mixed: Vec<f32>,
    resampled: Vec<f32>,
    frame: Vec<f32>,
    last_send: Option<Instant>,
    shared: Arc<Shared>,
}

impl Tx {
    // ponytail: encode + send run inside the capture callback; move them to a dedicated
    // encode thread fed by a ring if callbacks ever overrun.
    fn process<T: SizedSample>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
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
        for chunk in pcm.chunks(FRAME * out_ch) {
            let take = chunk.len().min(FRAME * out_ch - self.frame.len());
            self.frame.extend_from_slice(&chunk[..take]);
            if self.frame.len() == FRAME * out_ch {
                if let Ok(p) = self.pk.packet(&self.frame) {
                    if self.sock.send_to(p, self.peer).is_ok() {
                        self.shared.c.sent.fetch_add(1, Relaxed);
                        gap(&mut self.last_send, &self.shared.tx_gap_us);
                    }
                }
                self.frame.clear();
                self.frame.extend_from_slice(&chunk[take..]);
            }
        }
    }
}

fn build_input(dev: &cpal::Device, cfg: &Settings, pk: Packetizer, sock: UdpSocket, peer: SocketAddr, shared: Arc<Shared>) -> anyhow::Result<cpal::Stream> {
    let sc = pick_config(dev.default_input_config()?, dev.supported_input_configs()?);
    let (dev_ch, rate, fmt) = (sc.channels() as usize, sc.sample_rate(), sc.sample_format());
    let ch = cfg.channels as usize;
    let mut tx = Tx {
        pk,
        sock,
        peer,
        dev_ch,
        rs: (rate != RATE).then(|| Resampler::new(rate, RATE, ch)),
        mixed: Vec::with_capacity(16_384 * ch),
        resampled: Vec::with_capacity(32_768 * ch),
        frame: Vec::with_capacity(FRAME * ch),
        last_send: None,
        shared,
    };
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
fn link_is_send() {
    fn send<T: Send>() {}
    send::<Link>();
}
