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

pub const TARGET: usize = RATE as usize / 50; // 20 ms in frames
const BAND: usize = RATE as usize / 200; // 5 ms
const CEILING: usize = TARGET + RATE as usize / 10; // target + 100 ms

#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Output silence; `underrun` is true when playback just ran dry.
    Silence { underrun: bool },
    /// Drop `discard` frames, then pop `consume` frames and stretch/squash them to the requested length.
    Play { discard: usize, consume: usize },
}

/// Jitter/drift controller for the playback ring. All counts are 48 kHz frames.
/// Raw fill swings by a whole packet (10 ms) on every arrival, so drift decisions use a
/// smoothed fill (~1 s time constant at 10 ms callbacks); only real clock drift moves it.
#[derive(Default)]
pub struct Playout {
    playing: bool,
    avg: f32,
}

impl Playout {
    pub fn plan(&mut self, fill: usize, need: usize) -> Plan {
        let discard = fill.saturating_sub(TARGET) * (fill > CEILING) as usize;
        let fill = fill - discard;
        if !self.playing && fill < TARGET {
            return Plan::Silence { underrun: false };
        }
        if !self.playing || discard > 0 {
            self.avg = fill as f32;
        }
        self.avg += (fill as f32 - self.avg) * 0.01;
        let consume = match self.avg as usize {
            _ if need < 2 => need,
            f if f > TARGET + BAND => need + 1,
            f if f < TARGET - BAND => need - 1,
            _ => need,
        };
        if fill < consume {
            self.playing = false;
            return Plan::Silence { underrun: true };
        }
        self.playing = true;
        Plan::Play { discard, consume }
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
        assert_eq!(p.plan(TARGET - 1, 480), Plan::Silence { underrun: false }); // prebuffering
        assert_eq!(p.plan(TARGET, 480), Plan::Play { discard: 0, consume: 480 });
        // one packet of jitter must not trigger a correction
        assert_eq!(p.plan(TARGET + 480, 480), Plan::Play { discard: 0, consume: 480 });
        // a sustained offset (clock drift) does
        let last = (0..500).map(|_| p.plan(TARGET + BAND * 2, 240)).last().unwrap();
        assert_eq!(last, Plan::Play { discard: 0, consume: 241 });
        let last = (0..500).map(|_| p.plan(TARGET - BAND * 2, 240)).last().unwrap();
        assert_eq!(last, Plan::Play { discard: 0, consume: 239 });
        assert_eq!(p.plan(CEILING + 10, 480), Plan::Play { discard: CEILING + 10 - TARGET, consume: 480 });
        assert_eq!(p.plan(300, 480), Plan::Silence { underrun: true });
        assert_eq!(p.plan(TARGET - 1, 480), Plan::Silence { underrun: false }); // re-prebuffer
        assert_eq!(p.plan(TARGET, 480), Plan::Play { discard: 0, consume: 480 });
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
