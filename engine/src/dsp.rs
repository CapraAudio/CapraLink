//! Socket- and device-free pieces of the engine: packet format, RX loss handling,
//! playout (jitter/drift) decisions and the resampler. Kept here so they can be unit tested.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const RATE: u32 = 48_000;
pub const FRAME: usize = 480; // 10 ms per channel at 48 kHz
const MAGIC: u16 = 0xCA1A;
const VERSION: u8 = 1;
const HEADER: usize = 8;
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

/// TX side: 10 ms interleaved f32 frames in, finished UDP payloads out.
pub struct Packetizer {
    enc: opus::Encoder,
    channels: u8,
    seq: u32,
    buf: [u8; MAX_PACKET],
}

impl Packetizer {
    pub fn new(channels: u16, bitrate: i32) -> anyhow::Result<Self> {
        let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
        let mut enc = opus::Encoder::new(RATE, ch, opus::Application::Audio)?;
        enc.set_complexity(5)?;
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(5)?;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate.clamp(8_000, 96_000)))?;
        let mut buf = [0; MAX_PACKET];
        buf[..2].copy_from_slice(&MAGIC.to_be_bytes());
        buf[2] = VERSION;
        buf[3] = channels as u8;
        Ok(Packetizer { enc, channels: channels as u8, seq: 0, buf })
    }

    pub fn channels(&self) -> usize {
        self.channels as usize
    }

    /// `pcm` must be exactly one 10 ms frame (FRAME * channels samples).
    pub fn packet(&mut self, pcm: &[f32]) -> anyhow::Result<&[u8]> {
        self.buf[4..HEADER].copy_from_slice(&self.seq.to_be_bytes());
        self.seq = self.seq.wrapping_add(1);
        let n = self.enc.encode_float(pcm, &mut self.buf[HEADER..])?;
        Ok(&self.buf[..HEADER + n])
    }
}

/// Returns (channels, seq, opus payload) for a well-formed packet.
fn parse(p: &[u8]) -> Option<(u8, u32, &[u8])> {
    if p.len() <= HEADER || p[..2] != MAGIC.to_be_bytes() || p[2] != VERSION || !matches!(p[3], 1 | 2) {
        return None;
    }
    Some((p[3], u32::from_be_bytes([p[4], p[5], p[6], p[7]]), &p[HEADER..]))
}

/// RX side: packets in, 48 kHz stereo-interleaved PCM out (mono is duplicated to both sides).
pub struct Rx {
    dec: Option<(u8, opus::Decoder)>,
    expected: Option<u32>,
    pcm: Vec<f32>,
    stereo: Vec<f32>,
}

impl Default for Rx {
    fn default() -> Self {
        Rx { dec: None, expected: None, pcm: vec![0.0; MAX_OPUS_FRAME * 2], stereo: vec![0.0; MAX_OPUS_FRAME * 2] }
    }
}

impl Rx {
    /// Returns the stream's channel count for valid packets, None for junk.
    pub fn handle(&mut self, packet: &[u8], c: &Counters, out: &mut impl FnMut(&[f32])) -> Option<u8> {
        let (ch, seq, opus) = parse(packet)?;
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

pub const TARGET: usize = RATE as usize / 100; // 10 ms: starting / minimum cushion
const HEADROOM: usize = RATE as usize / 10; // fill beyond need + target + 100 ms is discarded
const GROW: usize = RATE as usize / 100; // +10 ms target per underrun
const SHRINK: usize = RATE as usize / 1000; // -1 ms target ...
const RELAX: usize = RATE as usize * 10; // ... per 10 s of clean playback
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
/// steers the sawtooth's low point (min cushion over 0.5 s) to `target` by playing very
/// slightly fast or slow, which also absorbs clock drift between machines. Each underrun
/// adds 10 ms to the target; it creeps back down while playback stays clean.
pub struct Playout {
    playing: bool,
    pub target: usize,
    clean: usize,
    low: usize,
    span: usize,
    adj: f32,
}

impl Default for Playout {
    fn default() -> Self {
        Playout { playing: false, target: TARGET, clean: 0, low: usize::MAX, span: 0, adj: 0.0 }
    }
}

impl Playout {
    /// `need`: 48 kHz frames the next callback will take from the ring.
    pub fn plan(&mut self, fill: usize, need: usize) -> Plan {
        let target = self.target;
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
            self.target = (target - SHRINK).max(TARGET);
            self.clean = 0;
        }
        Plan::Play { discard, ratio: 1.0 + self.adj as f64 }
    }

    /// The ring ran dry mid-playback: re-prebuffer against a deeper target.
    pub fn underrun(&mut self) {
        self.playing = false;
        self.target = (self.target + GROW).min(MAX_TARGET);
        self.clean = 0;
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
        let mut tx = Packetizer::new(1, 64_000).unwrap();
        let packets: Vec<Vec<u8>> = (0..50)
            .map(|f| {
                let pcm: Vec<f32> = (0..FRAME)
                    .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * (f * FRAME + i) as f32 / RATE as f32).sin())
                    .collect();
                tx.packet(&pcm).unwrap().to_vec()
            })
            .collect();
        let (mut rx, c, mut pcm) = (Rx::default(), Counters::default(), Vec::new());
        assert_eq!(rx.handle(b"junk-junk-junk", &c, &mut |_: &[f32]| {}), None);
        for (i, p) in packets.iter().enumerate() {
            if i != 20 {
                rx.handle(p, &c, &mut |s: &[f32]| pcm.extend_from_slice(s));
            }
        }
        rx.handle(&packets[10], &c, &mut |_: &[f32]| panic!("late packet must be dropped"));
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
        // an underrun grows the target: prebuffer now waits for a 20 ms cushion
        p.underrun();
        assert_eq!(p.target, TARGET + GROW);
        assert_eq!(p.plan(n + TARGET, n), Plan::Silence);
        ratio(p.plan(n + TARGET + GROW, n));
        // 10 s of clean playback gives 1 ms back
        for _ in 0..RELAX / n {
            p.plan(n + TARGET + GROW, n);
        }
        assert_eq!(p.target, TARGET + GROW - SHRINK);
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
