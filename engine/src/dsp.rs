//! Socket- and device-free pieces of the engine: packet format, RX loss handling,
//! playout (jitter/drift) decisions and the resampler. Kept here so they can be unit tested.

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const RATE: u32 = 48_000;
pub const FRAME: usize = 480; // 10 ms per channel at 48 kHz
const MAGIC: u16 = 0xCA1A;
const VERSION: u8 = 2;
const HEADER: usize = 7; // magic | version | seq, sent in clear and authenticated as AAD
const TAG: usize = 16;
pub const MAX_PACKET: usize = 1500;
const MAX_PLC: u32 = 5;
const MAX_OPUS_FRAME: usize = 5760; // 120 ms, the largest frame Opus can emit

#[derive(Default)]
pub struct Counters {
    pub sent: AtomicU64,
    pub received: AtomicU64,
    pub lost: AtomicU64,
    pub fec_recovered: AtomicU64,
    pub underruns: AtomicU64,
}

fn inc(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Relaxed);
}

fn cipher(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&(*key).into())
}

/// 8 zero bytes | seq. Keys are fresh per session, so a seq never repeats under one key.
fn nonce(seq: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[8..].copy_from_slice(&seq.to_be_bytes());
    n.into()
}

/// TX side: 10 ms interleaved f32 frames in, finished (encrypted) UDP payloads out.
/// Packet v2: `magic u16 | version u8 | seq u32 | AEAD(channels u8 | opus)`.
pub struct Packetizer {
    enc: opus::Encoder,
    aead: ChaCha20Poly1305,
    channels: u8,
    seq: u32,
    buf: [u8; MAX_PACKET],
}

impl Packetizer {
    pub fn new(channels: u16, bitrate: i32, key: &[u8; 32]) -> anyhow::Result<Self> {
        let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
        let mut enc = opus::Encoder::new(RATE, ch, opus::Application::Audio)?;
        enc.set_complexity(5)?;
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(5)?;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate.clamp(8_000, 96_000)))?;
        let mut buf = [0; MAX_PACKET];
        buf[..2].copy_from_slice(&MAGIC.to_be_bytes());
        buf[2] = VERSION;
        Ok(Packetizer { enc, aead: cipher(key), channels: channels as u8, seq: 0, buf })
    }

    pub fn channels(&self) -> usize {
        self.channels as usize
    }

    pub fn set_bitrate(&mut self, bitrate: i32) -> anyhow::Result<()> {
        Ok(self.enc.set_bitrate(opus::Bitrate::Bits(bitrate.clamp(8_000, 96_000)))?)
    }

    pub fn set_packet_loss_perc(&mut self, perc: u8) -> anyhow::Result<()> {
        Ok(self.enc.set_packet_loss_perc(perc.min(30).into())?)
    }

    pub fn set_complexity(&mut self, level: u8) -> anyhow::Result<()> {
        Ok(self.enc.set_complexity(level.min(10).into())?)
    }

    /// `pcm` must be exactly one 10 ms frame (FRAME * channels samples).
    pub fn packet(&mut self, pcm: &[f32]) -> anyhow::Result<&[u8]> {
        // ponytail: u32 seq wraps after ~497 days of one session (nonce reuse); rekey by reconnecting before then
        let seq = self.seq;
        self.seq = seq.wrapping_add(1);
        self.buf[3..HEADER].copy_from_slice(&seq.to_be_bytes());
        self.buf[HEADER] = self.channels;
        let n = self.enc.encode_float(pcm, &mut self.buf[HEADER + 1..MAX_PACKET - TAG])?;
        let end = HEADER + 1 + n;
        let (hdr, body) = self.buf.split_at_mut(HEADER);
        let tag = self.aead.encrypt_inout_detached(&nonce(seq), hdr, (&mut body[..1 + n]).into()).map_err(|_| anyhow::anyhow!("encrypt failed"))?;
        self.buf[end..end + TAG].copy_from_slice(&tag);
        Ok(&self.buf[..end + TAG])
    }
}

/// Authenticates and decrypts a packet in place; returns (channels, seq, opus payload).
fn open<'a>(aead: &ChaCha20Poly1305, p: &'a mut [u8]) -> Option<(u8, u32, &'a [u8])> {
    if p.len() < HEADER + 1 + TAG || p[..2] != MAGIC.to_be_bytes() || p[2] != VERSION {
        return None;
    }
    let seq = u32::from_be_bytes([p[3], p[4], p[5], p[6]]);
    let (hdr, rest) = p.split_at_mut(HEADER);
    let (body, tag) = rest.split_at_mut(rest.len() - TAG);
    let tag = Tag::try_from(&*tag).ok()?;
    aead.decrypt_inout_detached(&nonce(seq), hdr, (&mut *body).into(), &tag).ok()?;
    matches!(body[0], 1 | 2).then(|| (body[0], seq, &body[1..]))
}

/// RX side: packets in, 48 kHz stereo-interleaved PCM out (mono is duplicated to both sides).
pub struct Rx {
    aead: ChaCha20Poly1305,
    dec: Option<(u8, opus::Decoder)>,
    expected: Option<u32>,
    pcm: Vec<f32>,
    stereo: Vec<f32>,
}

impl Rx {
    pub fn new(key: &[u8; 32]) -> Self {
        Rx { aead: cipher(key), dec: None, expected: None, pcm: vec![0.0; MAX_OPUS_FRAME * 2], stereo: vec![0.0; MAX_OPUS_FRAME * 2] }
    }

    /// Returns the stream's channel count for authentic packets, None for junk (dropped silently).
    /// Decrypts `packet` in place.
    // ponytail: an authentic packet replayed from far back makes the seq logic resync (a glitch);
    // add a replay window if spoofed-source replays ever matter on the LAN.
    pub fn handle(&mut self, packet: &mut [u8], c: &Counters, out: &mut impl FnMut(&[f32])) -> Option<u8> {
        let (ch, seq, opus) = open(&self.aead, packet)?;
        inc(&c.received, 1);
        if self.dec.as_ref().is_none_or(|(dch, _)| *dch != ch) {
            let chans = if ch == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            self.dec = Some((ch, opus::Decoder::new(RATE, chans).ok()?));
            self.expected = None;
        }
        let gap = self.expected.map_or(0, |e| seq.wrapping_sub(e) as i32);
        // A jump this big means the peer restarted (or ~10 s of loss): resync instead of dropping forever.
        let gap = if !(-50..=1000).contains(&gap) { 0 } else { gap };
        if gap < 0 {
            return Some(ch); // late or duplicate
        }
        if gap == 1 {
            self.decode(opus, true, ch, out);
            inc(&c.fec_recovered, 1);
        } else if gap > 1 {
            for _ in 0..(gap as u32).min(MAX_PLC) {
                self.decode(&[], false, ch, out);
            }
            inc(&c.lost, gap as u64);
        }
        self.decode(opus, false, ch, out);
        self.expected = Some(seq.wrapping_add(1));
        Some(ch)
    }

    fn decode(&mut self, opus: &[u8], fec: bool, ch: u8, out: &mut impl FnMut(&[f32])) {
        let ch = ch as usize;
        // FEC and PLC must be asked for exactly one missing frame; normal decode gets the full buffer.
        let len = if fec || opus.is_empty() { FRAME * ch } else { self.pcm.len() };
        let Some((_, dec)) = self.dec.as_mut() else { return };
        let Ok(n) = dec.decode_float(opus, &mut self.pcm[..len], fec) else { return };
        if ch == 2 {
            out(&self.pcm[..n * 2]);
        } else {
            for (i, s) in self.pcm[..n].iter().enumerate() {
                self.stereo[2 * i] = *s;
                self.stereo[2 * i + 1] = *s;
            }
            out(&self.stereo[..n * 2]);
        }
    }
}

pub const TARGET: usize = RATE as usize / 100; // 10 ms: minimum cushion
const MARGIN: usize = RATE as usize / 200; // 5 ms on top of measured jitter
const HEADROOM: usize = RATE as usize / 10; // fill beyond need + target + 100 ms is discarded
const GROW: usize = RATE as usize / 100; // +10 ms boost per underrun (spike jitter missed) ...
const SHRINK: usize = RATE as usize / 1000; // ... fading 1 ms ...
const RELAX: usize = RATE as usize; // ... per second of clean playback
const MAX_TARGET: usize = RATE as usize / 8; // 125 ms
const WINDOW: usize = RATE as usize / 2; // low-water mark measured over 0.5 s
const TAU: f32 = 2.0 * RATE as f32; // drift controller: remove a cushion error over ~2 s ...
const MAX_ADJ: f32 = 0.02; // ... playing at most 2% fast/slow (inaudible)

#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Output silence while prebuffering.
    Silence,
    /// Drop `discard` frames, then play at `ratio` × normal speed (resampled, so no clicks).
    Play { discard: usize, ratio: f64 },
}

/// Jitter/drift controller for the playback ring. All counts are 48 kHz frames.
///
/// The "cushion" is what is left in the ring after a callback takes its frames. Packets
/// arrive in 10 ms steps (more on bursty links), so the fill is a sawtooth; the controller
/// steers the sawtooth's low point (min cushion over 0.5 s) to `target()` by playing very
/// slightly fast or slow, which also absorbs clock drift between machines. The target is
/// sized from measured packet jitter (`set_jitter`); an underrun adds a boost that fades.
pub struct Playout {
    playing: bool,
    jitter: usize,
    boost: usize,
    clean: usize,
    low: usize,
    span: usize,
    adj: f32,
}

impl Default for Playout {
    fn default() -> Self {
        Playout { playing: false, jitter: 0, boost: 0, clean: 0, low: usize::MAX, span: 0, adj: 0.0 }
    }
}

impl Playout {
    /// Worst recent packet lateness, in frames (from `Jitter`).
    pub fn set_jitter(&mut self, frames: usize) {
        self.jitter = frames;
    }

    pub fn target(&self) -> usize {
        (TARGET.max(self.jitter + MARGIN) + self.boost).min(MAX_TARGET)
    }

    /// `need`: 48 kHz frames the next callback will take from the ring.
    pub fn plan(&mut self, fill: usize, need: usize) -> Plan {
        let target = self.target();
        let excess = fill.saturating_sub(need + target);
        let discard = excess * (excess > HEADROOM) as usize;
        let fill = fill - discard;
        if !self.playing && fill < need + target {
            return Plan::Silence;
        }
        if !self.playing || discard > 0 {
            (self.low, self.span, self.adj) = (usize::MAX, 0, 0.0);
        }
        self.playing = true;
        self.low = self.low.min(fill.saturating_sub(need));
        self.span += need;
        if self.span >= WINDOW {
            self.adj = ((self.low as f32 - target as f32) / TAU).clamp(-MAX_ADJ, MAX_ADJ);
            (self.low, self.span) = (usize::MAX, 0);
        }
        self.clean += need;
        if self.clean >= RELAX {
            self.boost = self.boost.saturating_sub(SHRINK);
            self.clean = 0;
        }
        Plan::Play { discard, ratio: 1.0 + self.adj as f64 }
    }

    /// The ring ran dry mid-playback: re-prebuffer against a deeper target.
    pub fn underrun(&mut self) {
        self.playing = false;
        self.boost = (self.boost + GROW).min(MAX_TARGET);
        self.clean = 0;
    }
}

/// Worst packet lateness (arrival gap beyond the 10 ms packet period) seen over the last
/// 5–10 s, in µs: how much cushion the link needs to ride out its bursts.
#[derive(Default)]
pub struct Jitter {
    cur: u32,
    prev: u32,
    elapsed: u32,
}

impl Jitter {
    pub fn push(&mut self, gap_us: u32) -> u32 {
        // a pause this long is a peer restart or a stopped stream, not jitter
        if gap_us < 500_000 {
            self.cur = self.cur.max(gap_us.saturating_sub(10_000));
            self.elapsed += gap_us;
            if self.elapsed >= 5_000_000 {
                (self.prev, self.cur, self.elapsed) = (self.cur, 0, 0);
            }
        }
        self.cur.max(self.prev)
    }
}

/// AIMD bitrate controller (MASTER.md §3.5): the sender's reaction to the peer's periodic
/// receive-side reports. `ceiling` tracks the user's Bitrate setting.
pub struct RateControl {
    bitrate: i32,
    ceiling: i32,
}

impl RateControl {
    pub fn new(ceiling: i32) -> Self {
        let ceiling = ceiling.clamp(8_000, 96_000);
        RateControl { bitrate: ceiling, ceiling }
    }

    /// Feeds one peer report (deltas since its last report). Returns the bitrate to apply and
    /// the loss% to hand Opus (`packet_loss_perc`, so FEC scales with measured loss).
    /// A report with no packets (peer silent) leaves everything unchanged.
    pub fn on_report(&mut self, received: u64, lost: u64, underruns: u64) -> (i32, u8) {
        let total = received + lost;
        if total == 0 {
            return (self.bitrate, 0);
        }
        let loss_pct = lost as f64 * 100.0 / total as f64;
        if loss_pct > 5.0 || underruns > 0 {
            self.bitrate = (self.bitrate * 7 / 10).max(8_000);
        } else if loss_pct < 1.0 {
            self.bitrate = (self.bitrate + 8_000).min(self.ceiling);
        }
        (self.bitrate, loss_pct.round().clamp(0.0, 30.0) as u8)
    }
}

/// CPU-driven Opus complexity controller (MASTER.md §3.5): an EMA of encode time per frame,
/// dropping complexity fast under load and only raising it back after a sustained quiet spell.
pub struct Complexity {
    level: u8,
    ema_us: f32,
    hold: u32,
}

const COMPLEXITY_HOLD_FRAMES: u32 = 500; // ~5 s of 10 ms frames

impl Default for Complexity {
    fn default() -> Self {
        Complexity { level: 5, ema_us: 0.0, hold: 0 }
    }
}

impl Complexity {
    /// Feeds one frame's encode time (µs). Returns the new level when it changes.
    pub fn on_encode(&mut self, us: f32) -> Option<u8> {
        self.ema_us += 0.05 * (us - self.ema_us);
        if self.ema_us > 1500.0 {
            self.hold = 0;
            if self.level > 0 {
                self.level -= 1;
                return Some(self.level);
            }
        } else if self.ema_us < 300.0 {
            self.hold += 1;
            if self.hold >= COMPLEXITY_HOLD_FRAMES {
                self.hold = 0;
                if self.level < 5 {
                    self.level += 1;
                    return Some(self.level);
                }
            }
        } else {
            self.hold = 0;
        }
        None
    }
}

/// Streaming linear resampler for interleaved audio, state carried across calls.
// ponytail: linear resampler, swap for a windowed-sinc (rubato) if aliasing is audible
pub struct Resampler {
    pub step: f64, // input frames per output frame
    pos: f64,      // read position; 0 = `prev`, 1.. = current chunk
    prev: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, prev: vec![0.0; channels] }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.prev.len();
        let n = input.len() / ch;
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let a = if i == 0 { self.prev[c] } else { input[(i - 1) * ch + c] };
                out.push(a + (input[i * ch + c] - a) * f);
            }
            self.pos += self.step;
        }
        if n > 0 {
            self.pos -= n as f64;
            self.prev.copy_from_slice(&input[(n - 1) * ch..n * ch]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_fec() {
        let key = [7u8; 32];
        let mut tx = Packetizer::new(1, 64_000, &key).unwrap();
        let packets: Vec<Vec<u8>> = (0..50)
            .map(|f| {
                let pcm: Vec<f32> = (0..FRAME)
                    .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * (f * FRAME + i) as f32 / RATE as f32).sin())
                    .collect();
                tx.packet(&pcm).unwrap().to_vec()
            })
            .collect();
        let (mut rx, c, mut pcm) = (Rx::new(&key), Counters::default(), Vec::new());
        let never = &mut |_: &[f32]| panic!("must be dropped");
        assert_eq!(rx.handle(&mut b"junk-junk-junk-junk-junk-junk".to_vec(), &c, never), None);
        // any flipped bit (header = AAD, body, or tag) fails authentication
        for i in [2, 4, HEADER, packets[0].len() - 1] {
            let mut p = packets[0].clone();
            p[i] ^= 1;
            assert_eq!(rx.handle(&mut p, &c, never), None);
        }
        assert_eq!(Rx::new(&[8u8; 32]).handle(&mut packets[0].clone(), &c, never), None, "wrong key");
        assert_eq!(c.received.load(Relaxed), 0);
        for (i, p) in packets.iter().enumerate() {
            if i != 20 {
                rx.handle(&mut p.clone(), &c, &mut |s: &[f32]| pcm.extend_from_slice(s));
            }
        }
        rx.handle(&mut packets[10].clone(), &c, never); // late packet
        assert_eq!(c.fec_recovered.load(Relaxed), 1);
        assert_eq!(c.lost.load(Relaxed), 0);
        assert_eq!(c.received.load(Relaxed), 50);
        assert_eq!(pcm.len(), 50 * FRAME * 2);
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt();
        assert!(rms > 0.2, "rms {rms}");
    }

    #[test]
    fn drift_controller() {
        let mut p = Playout::default();
        let ratio = |plan| match plan {
            Plan::Play { ratio, .. } => ratio,
            other => panic!("expected Play, got {other:?}"),
        };
        let n = 480; // 10 ms callbacks
        assert_eq!(p.plan(n + TARGET - 1, n), Plan::Silence); // prebuffering
        assert_eq!(p.plan(n + TARGET, n), Plan::Play { discard: 0, ratio: 1.0 });
        // low point held on target: normal speed
        let r = (0..200).map(|i| ratio(p.plan(n + TARGET + (i % 2) * n, n))).last().unwrap();
        assert!((r - 1.0).abs() < 1e-6, "{r}");
        // cushion never dips (sender clock fast, excess piling up): play faster, bounded
        let fast = (0..200).map(|_| ratio(p.plan(n + TARGET + 960, n))).last().unwrap();
        assert!(fast > 1.004 && fast <= 1.02, "{fast}");
        // cushion running thin (sender clock slow): play slower, bounded
        let slow = (0..200).map(|_| ratio(p.plan(n + TARGET / 2, n))).last().unwrap();
        assert!((0.98..1.0).contains(&slow), "{slow}");
        let over = n + TARGET + HEADROOM + 10;
        assert!(matches!(p.plan(over, n), Plan::Play { discard, .. } if discard == HEADROOM + 10));
        // measured jitter sizes the target: 18 ms late packets -> 23 ms cushion
        p.set_jitter(RATE as usize * 18 / 1000);
        assert_eq!(p.target(), RATE as usize * 23 / 1000);
        p.set_jitter(0);
        // an underrun boosts the target: prebuffer now waits for a 20 ms cushion
        p.underrun();
        assert_eq!(p.target(), TARGET + GROW);
        assert_eq!(p.plan(n + TARGET, n), Plan::Silence);
        ratio(p.plan(n + TARGET + GROW, n));
        // each second of clean playback fades the boost by 1 ms
        for _ in 0..RELAX / n {
            p.plan(n + TARGET + GROW, n);
        }
        assert_eq!(p.target(), TARGET + GROW - SHRINK);
    }

    #[test]
    fn jitter_window() {
        let mut j = Jitter::default();
        assert_eq!(j.push(10_000), 0); // on time
        assert_eq!(j.push(28_000), 18_000); // 18 ms late
        assert_eq!(j.push(3_000_000), 18_000); // restart pause ignored
        for _ in 0..1000 {
            j.push(10_000); // 10 s of smooth packets ages the spike out
        }
        assert_eq!(j.push(10_000), 0);
    }

    #[test]
    fn rate_control() {
        let mut r = RateControl::new(64_000);
        assert_eq!(r.on_report(0, 0, 0), (64_000, 0), "zero-packet report is a no-op");
        // a lossy report cuts x0.7 off the ceiling-starting bitrate
        let (bitrate, loss) = r.on_report(90, 10, 0); // 10% loss
        assert_eq!(bitrate, 44_800);
        assert_eq!(loss, 10);
        // clean reports then ramp back up by 8k, clamped at the ceiling
        assert_eq!(r.on_report(100, 0, 0), (52_800, 0));
        assert_eq!(r.on_report(100, 0, 0), (60_800, 0));
        for _ in 0..10 {
            r.on_report(100, 0, 0);
        }
        assert_eq!(r.on_report(100, 0, 0), (64_000, 0), "stops at the ceiling");
        // underruns cut even with clean loss
        let (bitrate, _) = r.on_report(100, 0, 1);
        assert_eq!(bitrate, 64_000 * 7 / 10);
        // floor holds
        let mut r = RateControl::new(8_000);
        for _ in 0..5 {
            r.on_report(0, 100, 0);
        }
        assert_eq!(r.on_report(0, 100, 0).0, 8_000);
        // loss_perc clamps to 30
        let mut r = RateControl::new(64_000);
        assert_eq!(r.on_report(1, 99, 0).1, 30);
        // ceiling change (M5: applied by restarting the link with a fresh controller) clamps
        assert_eq!(RateControl::new(96_000).on_report(0, 0, 0).0, 96_000);
        assert_eq!(RateControl::new(16_000).on_report(0, 0, 0).0, 16_000, "starting bitrate is the new ceiling");
    }

    #[test]
    fn complexity_control() {
        let mut c = Complexity::default();
        // sustained high encode time steps all the way down to 0
        for _ in 0..400 {
            c.on_encode(3000.0);
        }
        assert_eq!(c.level, 0);
        // let the EMA decay well under the 300 us threshold, then start the hold count fresh
        for _ in 0..80 {
            c.on_encode(50.0);
        }
        c.hold = 0;
        // sustained low time doesn't move it until the hold period elapses
        for _ in 0..COMPLEXITY_HOLD_FRAMES - 1 {
            assert_eq!(c.on_encode(50.0), None);
        }
        assert_eq!(c.on_encode(50.0), Some(1), "steps up only after the hold period");
        // never rises above 5
        for _ in 0..COMPLEXITY_HOLD_FRAMES * 20 {
            c.on_encode(50.0);
        }
        assert_eq!(c.level, 5);
    }

    #[test]
    fn resample_length() {
        let mut r = Resampler::new(44_100, 48_000, 2);
        let (mut out, mut total_in) = (Vec::new(), 0);
        for chunk in [441, 512, 1000, 7, 4410] {
            r.process(&vec![0.1; chunk * 2], &mut out);
            total_in += chunk;
            let expect = total_in as f64 * 48_000.0 / 44_100.0;
            assert!((out.len() as f64 / 2.0 - expect).abs() <= 1.0, "{} vs {expect}", out.len() / 2);
        }
    }
}
