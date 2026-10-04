//! Socket- and device-free pieces of the engine: packet format, RX loss handling,
//! playout (jitter/drift) decisions and the resampler. Kept here so they can be unit tested.

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

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
    pub send_dropped: AtomicU64,
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

/// A NACK's nonce: 1 | 7 zero bytes | counter. It is sent under the audio key of the direction it
/// asks about (by that direction's receiver, so one writer per key) and can't collide with an
/// audio nonce.
fn nack_nonce(counter: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = 1;
    n[8..].copy_from_slice(&counter.to_be_bytes());
    n.into()
}

/// Hi-Fi (lossless Music Mode): 48 kHz stereo as 24-bit little-endian PCM, 5 ms per packet
/// (1440 bytes, so a packet stays under 1500). Losses are resent on request (NACK), not concealed.
pub const PCM_FRAME: usize = RATE as usize / 200;
const PCM_BYTES: usize = PCM_FRAME * 2 * 3;
/// body[0] of a Hi-Fi packet: 0.2.x receivers accept only 1 | 2 there, so they drop it as junk.
const PCM: u8 = 0x80 | 2;
pub const HIFI_BITRATE: i32 = RATE as i32 * 2 * 24; // 2304 kbps
/// NACK datagram: `magic u16 | NACK u8 | counter u32 | AEAD(ranges)`. A distinct byte where audio
/// has its version, so no audio receiver (old or new) mistakes one for audio.
const NACK: u8 = b'N';
const SLOTS: usize = 256; // ring size of the reorder window and the resend store (> REORDER)
const REORDER: u32 = 200; // 1 s of 5 ms packets: how late a resend may still be played
const RESYNC: u32 = 1000; // a ~5 s jump: start over instead of playing it out as silence
const RENACK: Duration = Duration::from_millis(30); // ask again for a still-missing packet after this
/// A missing packet is given up (5 ms of faded silence) once the playback ring holds less than this.
pub const GIVE_UP: usize = RATE as usize * 30 / 1000;
const FADE: usize = RATE as usize / 1000; // 1 ms in/out around a given-up packet

/// Bitrate ceiling in Music Mode (MASTER.md §3.7); normally the user's Bitrate setting (≤ 96 kbps).
pub const MUSIC_BITRATE: i32 = 160_000;

/// The sender's bitrate ceiling: the user's setting, or the Music Mode ceiling.
pub fn ceiling(user: i32, music: bool) -> i32 {
    if music { MUSIC_BITRATE } else { user.clamp(8_000, 96_000) }
}

fn encoder(channels: u16, music: bool, bitrate: i32, complexity: i32, loss_perc: i32) -> anyhow::Result<opus::Encoder> {
    let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
    let mut enc = opus::Encoder::new(RATE, ch, opus::Application::Audio)?;
    enc.set_complexity(complexity)?;
    enc.set_inband_fec(true)?;
    enc.set_packet_loss_perc(loss_perc)?;
    enc.set_bitrate(opus::Bitrate::Bits(bitrate.clamp(8_000, MUSIC_BITRATE)))?;
    if music {
        // bandwidth stays automatic: Opus picks fullband at music bitrates and narrows only
        // when bad-network back-off drops the rate, which sounds better than forcing it
        enc.set_signal(opus::Signal::Music)?;
    }
    Ok(enc)
}

/// TX side: interleaved f32 frames (10 ms, or 20 ms in Music Mode) in, finished (encrypted)
/// UDP payloads out. Packet v2: `magic u16 | version u8 | seq u32 | AEAD(channels u8 | opus)`,
/// in Hi-Fi `AEAD(PCM | 240 stereo frames of i24 LE)`.
pub struct Packetizer {
    enc: opus::Encoder,
    aead: ChaCha20Poly1305,
    channels: u8,
    music: bool,
    pcm: bool,
    seq: u32,
    buf: [u8; MAX_PACKET],
}

impl Packetizer {
    pub fn new(channels: u16, bitrate: i32, key: &[u8; 32]) -> anyhow::Result<Self> {
        let enc = encoder(channels, false, bitrate, 5, 5)?;
        let mut buf = [0; MAX_PACKET];
        buf[..2].copy_from_slice(&MAGIC.to_be_bytes());
        buf[2] = VERSION;
        Ok(Packetizer { enc, aead: cipher(key), channels: channels as u8, music: false, pcm: false, seq: 0, buf })
    }

    /// Switches channels / Music Mode live by swapping in `m`'s encoder (bitrate, complexity
    /// and loss% carry over); `m` gets the old one back. Doesn't allocate, so it can run in the
    /// capture callback. The AEAD key and `seq` stay, so nonces keep counting up.
    pub fn set_mode(&mut self, m: &mut Mode) -> anyhow::Result<()> {
        m.enc.set_bitrate(self.enc.get_bitrate()?)?;
        m.enc.set_complexity(self.enc.get_complexity()?)?;
        m.enc.set_packet_loss_perc(self.enc.get_packet_loss_perc()?)?;
        std::mem::swap(&mut self.enc, &mut m.enc);
        ((self.channels, m.channels), (self.music, m.music)) = ((m.channels, self.channels), (m.music, self.music));
        std::mem::swap(&mut self.pcm, &mut m.pcm);
        Ok(())
    }

    pub fn channels(&self) -> usize {
        self.channels as usize
    }

    /// Hi-Fi: packets carry PCM (the encoder stays, idle, for falling back to Music Mode).
    pub fn pcm(&self) -> bool {
        self.pcm
    }

    /// Samples per channel in one frame: 10 ms, 20 ms in Music Mode, 5 ms in Hi-Fi.
    pub fn frame(&self) -> usize {
        if self.pcm {
            PCM_FRAME
        } else if self.music {
            2 * FRAME
        } else {
            FRAME
        }
    }

    pub fn set_bitrate(&mut self, bitrate: i32) -> anyhow::Result<()> {
        Ok(self.enc.set_bitrate(opus::Bitrate::Bits(bitrate.clamp(8_000, MUSIC_BITRATE)))?)
    }

    pub fn set_packet_loss_perc(&mut self, perc: u8) -> anyhow::Result<()> {
        Ok(self.enc.set_packet_loss_perc(perc.min(30).into())?)
    }

    pub fn set_complexity(&mut self, level: u8) -> anyhow::Result<()> {
        Ok(self.enc.set_complexity(level.min(10).into())?)
    }

    /// `pcm` must be exactly one frame (`frame()` * channels samples).
    pub fn packet(&mut self, pcm: &[f32]) -> anyhow::Result<&[u8]> {
        // seq is the nonce: never wrap it (~497 days of one session); reconnecting rekeys
        let seq = self.seq;
        anyhow::ensure!(seq != u32::MAX, "sequence numbers used up: reconnect to rekey");
        self.seq = seq + 1;
        self.buf[3..HEADER].copy_from_slice(&seq.to_be_bytes());
        let n = if self.pcm {
            self.buf[HEADER] = PCM;
            pack(pcm, &mut self.buf[HEADER + 1..][..PCM_BYTES]);
            PCM_BYTES
        } else {
            self.buf[HEADER] = self.channels;
            self.enc.encode_float(pcm, &mut self.buf[HEADER + 1..MAX_PACKET - TAG])?
        };
        let end = HEADER + 1 + n;
        let (hdr, body) = self.buf.split_at_mut(HEADER);
        let tag = self.aead.encrypt_inout_detached(&nonce(seq), hdr, (&mut body[..1 + n]).into()).map_err(|_| anyhow::anyhow!("encrypt failed"))?;
        self.buf[end..end + TAG].copy_from_slice(&tag);
        Ok(&self.buf[..end + TAG])
    }
}

/// An encoder for `Packetizer::set_mode`, built off the audio thread (creating one allocates).
pub struct Mode {
    enc: opus::Encoder,
    channels: u8,
    pub music: bool,
    pub pcm: bool,
}

impl Mode {
    /// `pcm` (Hi-Fi) is Music Mode sent as PCM: stereo, with a Music Mode encoder to fall back to.
    pub fn new(channels: u16, music: bool, pcm: bool) -> anyhow::Result<Self> {
        let (channels, music) = if pcm { (2, true) } else { (channels, music) };
        Ok(Mode { enc: encoder(channels, music, 64_000, 5, 5)?, channels: channels as u8, music, pcm })
    }
}

/// f32 → 24-bit signed little-endian, rounded and clamped (at 24 bits dither isn't needed).
fn pack(pcm: &[f32], out: &mut [u8]) {
    for (s, o) in pcm.iter().zip(out.as_chunks_mut::<3>().0) {
        let v = (s * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
        o.copy_from_slice(&v.to_le_bytes()[..3]);
    }
}

fn unpack(b: &[u8], out: &mut [f32]) {
    for (o, [x, y, z]) in out.iter_mut().zip(b.as_chunks::<3>().0) {
        *o = (i32::from_le_bytes([0, *x, *y, *z]) >> 8) as f32 / 8_388_608.0;
    }
}

/// Authenticates and decrypts a packet in place; returns (body[0], seq, payload): body[0] is the
/// channel count (Opus) or `PCM`. (0.2.x accepted only 1 | 2 here.)
fn open<'a>(aead: &ChaCha20Poly1305, p: &'a mut [u8]) -> Option<(u8, u32, &'a [u8])> {
    if p.len() < HEADER + 1 + TAG || p[..2] != MAGIC.to_be_bytes() || p[2] != VERSION {
        return None;
    }
    let seq = u32::from_be_bytes([p[3], p[4], p[5], p[6]]);
    let (hdr, rest) = p.split_at_mut(HEADER);
    let (body, tag) = rest.split_at_mut(rest.len() - TAG);
    let tag = Tag::try_from(&*tag).ok()?;
    aead.decrypt_inout_detached(&nonce(seq), hdr, (&mut *body).into(), &tag).ok()?;
    Some((body[0], seq, &body[1..]))
}

/// Whether a datagram is a NACK (see `Nacks`), not audio.
pub fn is_nack(p: &[u8]) -> bool {
    p.len() > 2 && p[..2] == MAGIC.to_be_bytes() && p[2] == NACK
}

/// TX side of Hi-Fi resends: authenticates the peer's NACKs (each counter only once, so a replayed
/// NACK can't make this side resend) and lists the seqs asked for.
pub struct Nacks {
    aead: ChaCha20Poly1305,
    last: Option<u32>,
}

impl Nacks {
    /// `key`: this side's send key (the peer sends its NACKs under it).
    pub fn new(key: &[u8; 32]) -> Self {
        Nacks { aead: cipher(key), last: None }
    }

    pub fn open<'a>(&mut self, p: &'a mut [u8]) -> Option<impl Iterator<Item = u32> + 'a> {
        if p.len() < HEADER + TAG || !is_nack(p) {
            return None;
        }
        let counter = seq_of(p);
        if self.last.is_some_and(|l| counter <= l) {
            return None;
        }
        let (hdr, rest) = p.split_at_mut(HEADER);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG);
        let tag = Tag::try_from(&*tag).ok()?;
        self.aead.decrypt_inout_detached(&nack_nonce(counter), hdr, (&mut *body).into(), &tag).ok()?;
        self.last = Some(counter);
        // ranges: first seq u32 | count u16
        Some(body.as_chunks::<6>().0.iter().flat_map(|r| {
            let first = u32::from_be_bytes([r[0], r[1], r[2], r[3]]);
            first..first.saturating_add(u16::from_be_bytes([r[4], r[5]]).min(REORDER as u16) as u32)
        }))
    }
}

/// The last ~1.3 s of sent Hi-Fi packets, as sent (same seq, same ciphertext), for resending.
pub struct Resend {
    buf: Vec<u8>,
    len: Vec<usize>,
}

impl Default for Resend {
    fn default() -> Self {
        Resend { buf: vec![0; SLOTS * MAX_PACKET], len: vec![0; SLOTS] }
    }
}

impl Resend {
    pub fn keep(&mut self, p: &[u8]) {
        let i = slot(seq_of(p));
        self.buf[i * MAX_PACKET..][..p.len()].copy_from_slice(p);
        self.len[i] = p.len();
    }

    pub fn get(&self, seq: u32) -> Option<&[u8]> {
        let p = &self.buf[slot(seq) * MAX_PACKET..][..self.len[slot(seq)]];
        (p.len() > HEADER && seq_of(p) == seq).then_some(p)
    }
}

/// The seq (or NACK counter) in a datagram's header.
fn seq_of(p: &[u8]) -> u32 {
    u32::from_be_bytes([p[3], p[4], p[5], p[6]])
}

fn slot(seq: u32) -> usize {
    seq as usize % SLOTS
}

/// RX side of Hi-Fi: a 1 s reorder window. PCM packets are held by seq until they can be played
/// in order; a missing one is asked for again (NACK) until its playout deadline, then played as
/// 5 ms of silence faded in and out. Packets already played, duplicates and replays are refused.
struct Reorder {
    on: bool,       // the stream is PCM
    base: u32,      // next seq to play
    top: u32,       // highest seq taken + 1; top - base ≤ REORDER
    have: Vec<bool>,
    nacked: Vec<Option<Instant>>, // last asked for
    pcm: Vec<f32>,  // SLOTS packets of stereo f32
    last: [f32; 2], // last frame played: a given-up packet fades out from it
    silent: bool,   // the last packet was given up: fade the next one in
    nacks: u32,     // NACK counter (its nonce)
    nack: [u8; MAX_PACKET],
}

impl Reorder {
    fn new() -> Self {
        let mut nack = [0; MAX_PACKET];
        nack[..2].copy_from_slice(&MAGIC.to_be_bytes());
        nack[2] = NACK;
        let pcm = vec![0.0; SLOTS * PCM_FRAME * 2];
        Reorder { on: false, base: 0, top: 0, have: vec![false; SLOTS], nacked: vec![None; SLOTS], pcm, last: [0.0; 2], silent: false, nacks: 0, nack }
    }

    fn reset(&mut self, seq: u32) {
        (self.on, self.base, self.top) = (true, seq, seq);
    }

    /// Takes authentic packet `seq`; false if it is refused.
    fn insert(&mut self, seq: u32, body: &[u8], c: &Counters, out: &mut impl FnMut(&[f32])) -> bool {
        if seq < self.base || (seq < self.top && self.have[slot(seq)]) {
            return false; // played, duplicate or replayed
        }
        if seq - self.base > RESYNC {
            self.reset(seq);
        }
        while seq - self.base >= REORDER {
            // the oldest can't wait any longer; past all we know of, the rest is simply lost
            if !self.step(true, c, out) {
                inc(&c.lost, (seq + 1 - REORDER - self.base) as u64);
                self.reset(seq + 1 - REORDER);
            }
        }
        for s in self.top..=seq {
            (self.have[slot(s)], self.nacked[slot(s)]) = (false, None);
        }
        let i = slot(seq);
        if self.nacked[i].is_some() {
            inc(&c.fec_recovered, 1); // asked for again, and here it is
        }
        unpack(body, &mut self.pcm[i * PCM_FRAME * 2..][..PCM_FRAME * 2]);
        (self.have[i], self.nacked[i], self.top) = (true, None, self.top.max(seq + 1));
        true
    }

    /// Plays the next packet: held as is, missing (with `give_up`) as faded silence, counted lost.
    /// False if there's nothing to play yet.
    fn step(&mut self, give_up: bool, c: &Counters, out: &mut impl FnMut(&[f32])) -> bool {
        let i = slot(self.base);
        if self.base >= self.top || !(self.have[i] || give_up) {
            return false;
        }
        let p = &mut self.pcm[i * PCM_FRAME * 2..][..PCM_FRAME * 2];
        if self.have[i] {
            if self.silent {
                for (n, f) in p.as_chunks_mut::<2>().0.iter_mut().take(FADE).enumerate() {
                    f.iter_mut().for_each(|s| *s *= n as f32 / FADE as f32);
                }
            }
            self.last.copy_from_slice(&p[p.len() - 2..]);
            self.silent = false;
        } else {
            for (n, f) in p.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let g = if self.silent { 0.0 } else { 1.0 - (n as f32 / FADE as f32).min(1.0) };
                *f = self.last.map(|s| s * g);
            }
            self.silent = true;
            inc(&c.lost, 1);
        }
        out(p);
        (self.have[i], self.nacked[i]) = (false, None);
        self.base += 1;
        true
    }

    /// Plays what is in order; a missing packet is given up once the playback ring (`fill` frames)
    /// holds less than `GIVE_UP` (`usize::MAX`: never).
    fn release(&mut self, mut fill: usize, c: &Counters, out: &mut impl FnMut(&[f32])) {
        while self.step(fill < GIVE_UP, c, out) {
            fill = fill.saturating_add(PCM_FRAME);
        }
    }

    /// A NACK (ranges: first seq u32 | count u16) for the missing packets not asked for in the
    /// last `RENACK`, if any.
    fn nack(&mut self, aead: &ChaCha20Poly1305, now: Instant) -> Option<&[u8]> {
        if !self.on || self.nacks == u32::MAX {
            return None;
        }
        let (mut n, mut prev) = (HEADER, None);
        for s in self.base..self.top {
            let i = slot(s);
            if self.have[i] || self.nacked[i].is_some_and(|t| now - t < RENACK) {
                continue;
            }
            self.nacked[i] = Some(now);
            if prev.is_some_and(|p: u32| p + 1 == s) {
                let count = u16::from_be_bytes([self.nack[n - 2], self.nack[n - 1]]) + 1;
                self.nack[n - 2..n].copy_from_slice(&count.to_be_bytes());
            } else {
                self.nack[n..n + 4].copy_from_slice(&s.to_be_bytes());
                self.nack[n + 4..n + 6].copy_from_slice(&1u16.to_be_bytes());
                n += 6; // ≤ 100 ranges in a 200-packet window: fits
            }
            prev = Some(s);
        }
        if n == HEADER {
            return None;
        }
        let counter = self.nacks;
        self.nacks += 1;
        self.nack[3..HEADER].copy_from_slice(&counter.to_be_bytes());
        let (hdr, body) = self.nack.split_at_mut(HEADER);
        let tag = aead.encrypt_inout_detached(&nack_nonce(counter), hdr, (&mut body[..n - HEADER]).into()).ok()?;
        self.nack[n..n + TAG].copy_from_slice(&tag);
        Some(&self.nack[..n + TAG])
    }
}

/// RX side: packets in, 48 kHz stereo-interleaved PCM out (mono is duplicated to both sides).
pub struct Rx {
    aead: ChaCha20Poly1305,
    dec: Option<(u8, opus::Decoder)>,
    expected: Option<u32>,
    last_len: usize, // samples per channel of the last decoded packet: the size PLC/FEC fill
    pcm: Vec<f32>,
    stereo: Vec<f32>,
    reorder: Reorder, // Hi-Fi
}

impl Rx {
    pub fn new(key: &[u8; 32]) -> Self {
        let reorder = Reorder::new();
        Rx { aead: cipher(key), dec: None, expected: None, last_len: FRAME, pcm: vec![0.0; MAX_OPUS_FRAME * 2], stereo: vec![0.0; MAX_OPUS_FRAME * 2], reorder }
    }

    /// Returns the stream's channel count for authentic packets, None for junk and for late,
    /// duplicate or replayed packets (dropped silently). Decrypts `packet` in place.
    /// Hi-Fi packets go through the reorder window: in-order audio comes out at once, the rest
    /// waits for `release`.
    pub fn handle(&mut self, packet: &mut [u8], c: &Counters, out: &mut impl FnMut(&[f32])) -> Option<u8> {
        let (ch, seq, opus) = open(&self.aead, packet)?;
        if ch == PCM && opus.len() == PCM_BYTES {
            return self.take_pcm(seq, opus, c, out).then_some(2);
        }
        if !matches!(ch, 1 | 2) {
            return None;
        }
        // seq never wraps under one key (`Packetizer::packet`), so older than expected is never played
        let gap = match self.expected {
            Some(e) if seq < e => return None,
            Some(e) => seq - e,
            None => 0,
        };
        if self.reorder.on {
            // back from Hi-Fi: what it still holds comes first (missing packets as silence)
            while self.reorder.step(true, c, out) {}
            self.reorder.on = false;
        }
        inc(&c.received, 1);
        if self.dec.as_ref().is_none_or(|(dch, _)| *dch != ch) {
            let chans = if ch == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            self.dec = Some((ch, opus::Decoder::new(RATE, chans).ok()?));
        }
        if gap == 1 {
            self.decode(opus, true, ch, out);
            inc(&c.fec_recovered, 1);
        } else if gap > 1 && gap <= 1000 {
            for _ in 0..gap.min(MAX_PLC) {
                self.decode(&[], false, ch, out);
            }
        }
        // A bigger jump is ~10 s of loss: resync without concealing or counting it.
        if gap <= 1000 {
            inc(&c.lost, gap as u64);
        }
        self.decode(opus, false, ch, out);
        self.expected = Some(seq.saturating_add(1));
        Some(ch)
    }

    fn take_pcm(&mut self, seq: u32, body: &[u8], c: &Counters, out: &mut impl FnMut(&[f32])) -> bool {
        let r = &mut self.reorder;
        if !r.on {
            if self.expected.is_some_and(|e| seq < e) {
                return false;
            }
            r.reset(seq);
        }
        if !r.insert(seq, body, c, out) {
            return false;
        }
        inc(&c.received, 1);
        r.release(usize::MAX, c, out);
        self.expected = Some(r.top); // everything below is closed to Opus packets
        true
    }

    /// The stream is Hi-Fi (PCM).
    pub fn pcm(&self) -> bool {
        self.reorder.on
    }

    /// Hi-Fi: plays what is in order, giving up a missing packet once the playback ring holds
    /// less than `GIVE_UP` (`fill`: 48 kHz frames in it).
    pub fn release(&mut self, fill: usize, c: &Counters, out: &mut impl FnMut(&[f32])) {
        if self.reorder.on {
            self.reorder.release(fill, c, out);
        }
    }

    /// Hi-Fi: a NACK datagram for the sender, if packets are missing (again, after `RENACK`).
    pub fn nack(&mut self, now: Instant) -> Option<&[u8]> {
        self.reorder.nack(&self.aead, now)
    }

    /// The stream's current packet period in µs (10 ms, 20 ms in Music Mode, 5 ms in Hi-Fi).
    pub fn period_us(&self) -> u32 {
        if self.reorder.on {
            return (PCM_FRAME * 1_000_000 / RATE as usize) as u32;
        }
        (self.last_len * 1_000_000 / RATE as usize) as u32
    }

    fn decode(&mut self, opus: &[u8], fec: bool, ch: u8, out: &mut impl FnMut(&[f32])) {
        let ch = ch as usize;
        // FEC and PLC must be asked for exactly one missing frame (assumed the size of the last
        // one); normal decode gets the full buffer.
        let (conceal, len) = (fec || opus.is_empty(), self.last_len * ch);
        let len = if conceal { len } else { self.pcm.len() };
        let Some((_, dec)) = self.dec.as_mut() else { return };
        let Ok(n) = dec.decode_float(opus, &mut self.pcm[..len], fec) else { return };
        if !conceal {
            self.last_len = n;
        }
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

pub const TARGET: usize = RATE as usize / 100; // 10 ms: minimum cushion ...
pub const MUSIC_TARGET: usize = 15 * TARGET; // ... 150 ms in Music Mode (covers a typical Wi-Fi stall from the start)
pub const HIFI_TARGET: usize = 30 * TARGET; // ... 300 ms in Hi-Fi: time for resends to arrive
const MARGIN: usize = RATE as usize / 200; // 5 ms on top of measured jitter
const HEADROOM: usize = RATE as usize / 10; // fill beyond need + target + 100 ms is discarded
// Music Mode never drops audio to shrink the buffer (an audible skip): the ≤ 0.5% speed-up drains it
// instead (300 ms in about a minute). Only fill that wouldn't fit the 1.5 s playback ring is dropped:
// beyond 400 ms over the 1 s ceiling, not over the current target (a forgotten stall can shrink that
// by ~0.85 s at once).
const MUSIC_HEADROOM: usize = RATE as usize * 2 / 5;
const GROW: usize = RATE as usize / 100; // +10 ms boost per underrun (spike jitter missed) ...
const SHRINK: usize = RATE as usize / 1000; // ... fading 1 ms ...
const RELAX: usize = RATE as usize; // ... per second of clean playback
const MAX_TARGET: usize = RATE as usize / 8; // 125 ms
// Music Mode trades delay for never hiccuping on stall-prone Wi-Fi: a deeper ceiling, a bigger
// step after a surprise stall, and (in `Jitter`) a much longer memory of past stalls.
const MUSIC_MAX_TARGET: usize = RATE as usize; // 1 s: rides out a ~0.7 s Wi-Fi dropout (owner OK, Hi-Fi later)
const MUSIC_GROW: usize = RATE as usize * 3 / 100; // +30 ms per underrun
pub const JITTER_WINDOW_US: u32 = 5_000_000; // stall memory 5–10 s ...
pub const MUSIC_JITTER_WINDOW_US: u32 = 120_000_000; // ... 2–4 min in Music Mode (Wi-Fi stalls ~every 60–90 s)
const WINDOW: usize = RATE as usize / 2; // low-water mark measured over 0.5 s
const TAU: f32 = 2.0 * RATE as f32; // drift controller: remove a cushion error over ~2 s ...
const MAX_ADJ: f32 = 0.02; // ... playing at most 2% fast/slow (inaudible on speech)
// Music is pitch-sensitive: 2% is ~1/3 semitone. 0.5% (~9 cents) is below most listeners'
// threshold and still covers real clock drift between machines (~0.3% measured).
const MUSIC_MAX_ADJ: f32 = 0.005;

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
    min: usize,
    max: usize,
    grow: usize,
    max_adj: f32,
    headroom: usize,
    drain: bool, // Music Mode: `headroom` counts from `max`, not from the target
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
        Playout { min: TARGET, max: MAX_TARGET, grow: GROW, max_adj: MAX_ADJ, headroom: HEADROOM, drain: false, playing: false, jitter: 0, boost: 0, clean: 0, low: usize::MAX, span: 0, adj: 0.0 }
    }
}

impl Playout {
    /// Worst recent packet lateness, in frames (from `Jitter`).
    pub fn set_jitter(&mut self, frames: usize) {
        self.jitter = frames;
    }

    /// Normal: 10 ms minimum, 125 ms ceiling, +10 ms per underrun.
    /// Music Mode: 150 ms minimum, 1 s ceiling, +30 ms per underrun, speed change ≤ 0.5%, and a
    /// shrinking target drains by that speed-up instead of skipping audio.
    pub fn set_music(&mut self, music: bool) {
        (self.min, self.max, self.grow, self.max_adj, self.headroom) = if music {
            (MUSIC_TARGET, MUSIC_MAX_TARGET, MUSIC_GROW, MUSIC_MAX_ADJ, MUSIC_HEADROOM)
        } else {
            (TARGET, MAX_TARGET, GROW, MAX_ADJ, HEADROOM)
        };
        self.drain = music;
    }

    /// After `set_music`: a Hi-Fi stream (PCM) waits for resends, so it keeps at least 300 ms.
    pub fn set_hifi(&mut self, hifi: bool) {
        if hifi {
            self.min = HIFI_TARGET;
        }
    }

    pub fn target(&self) -> usize {
        (self.min.max(self.jitter + MARGIN) + self.boost).min(self.max)
    }

    /// `need`: 48 kHz frames the next callback will take from the ring.
    pub fn plan(&mut self, fill: usize, need: usize) -> Plan {
        let target = self.target();
        let excess = fill.saturating_sub(need + target);
        let keep = if self.drain { self.max } else { target } + self.headroom;
        let discard = excess * (fill > need + keep) as usize;
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
            self.adj = ((self.low as f32 - target as f32) / TAU).clamp(-self.max_adj, self.max_adj);
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
        self.boost = (self.boost + self.grow).min(self.max);
        self.clean = 0;
    }
}

/// Worst packet lateness (arrival gap beyond the packet period) seen over the last 5–10 s,
/// in µs: how much cushion the link needs to ride out its bursts.
#[derive(Default)]
pub struct Jitter {
    cur: u32,
    prev: u32,
    elapsed: u32,
}

impl Jitter {
    /// `period_us`: the stream's packet period (10 ms, 20 ms in Music Mode).
    /// `window_us`: how long a stall is remembered (between 1× and 2× this).
    pub fn push(&mut self, gap_us: u32, period_us: u32, window_us: u32) -> u32 {
        // a pause this long is a peer restart or a stopped stream, not jitter; shorter ones are
        // stalls (Wi-Fi dropouts of ~0.7 s seen in the field), capped later by the target ceiling
        if gap_us < 2_000_000 {
            self.cur = self.cur.max(gap_us.saturating_sub(period_us));
            self.elapsed += gap_us;
            if self.elapsed >= window_us {
                (self.prev, self.cur, self.elapsed) = (self.cur, 0, 0);
            }
        }
        self.cur.max(self.prev)
    }
}

/// AIMD bitrate controller (MASTER.md §3.5): the sender's reaction to the peer's periodic
/// receive-side reports. `ceiling` tracks the user's Bitrate setting (see `ceiling()`).
pub struct RateControl {
    bitrate: i32,
    ceiling: i32,
}

impl RateControl {
    pub fn new(ceiling: i32) -> Self {
        let ceiling = ceiling.clamp(8_000, MUSIC_BITRATE);
        RateControl { bitrate: ceiling, ceiling }
    }

    /// A higher ceiling is ramped up to by later clean reports; a lower one applies at once.
    pub fn set_ceiling(&mut self, ceiling: i32) {
        self.ceiling = ceiling.clamp(8_000, MUSIC_BITRATE);
        self.bitrate = self.bitrate.min(self.ceiling);
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

/// When Hi-Fi gives way to Music Mode (Opus): fed the peer's 1 s reports while Hi-Fi is wanted.
/// More than 2% unrecovered loss or 2+ underruns over the last 10 s fall back; a minute of clean
/// reports (≤ 2% loss, no underruns) tries Hi-Fi again.
#[derive(Default)]
pub struct Fallback {
    recent: [[u64; 3]; 10], // (received + lost, lost, underruns) per report
    at: usize,
    clean: u32,
    pub fallen: bool,
}

impl Fallback {
    /// Returns whether Hi-Fi is fallen back now.
    pub fn on_report(&mut self, received: u64, lost: u64, underruns: u64) -> bool {
        let total = received + lost;
        if self.fallen {
            let clean = underruns == 0 && lost * 50 <= total;
            self.clean = if clean { self.clean + 1 } else { 0 };
            if self.clean >= 60 {
                *self = Fallback::default();
            }
            return self.fallen;
        }
        self.recent[self.at] = [total, lost, underruns];
        self.at = (self.at + 1) % self.recent.len();
        let [total, lost, underruns] = self.recent.iter().fold([0; 3], |a, r| [a[0] + r[0], a[1] + r[1], a[2] + r[2]]);
        self.fallen = lost * 50 > total || underruns >= 2;
        self.fallen
    }
}

/// CPU-driven Opus complexity controller (MASTER.md §3.5): an EMA of encode time per frame,
/// dropping complexity fast under load and only raising it back after a sustained quiet spell.
pub struct Complexity {
    level: u8,
    max: u8,
    ema_us: f32,
    hold: u32,
}

const COMPLEXITY_HOLD_FRAMES: u32 = 500; // ~5 s of 10 ms frames

impl Default for Complexity {
    fn default() -> Self {
        Complexity { level: 5, max: 5, ema_us: 0.0, hold: 0 }
    }
}

impl Complexity {
    /// Highest level to climb to: 5, or 10 in Music Mode. Returns the (clamped) current level.
    pub fn set_max(&mut self, max: u8) -> u8 {
        self.max = max;
        self.level = self.level.min(max);
        self.level
    }
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
                if self.level < self.max {
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

/// Streaming linear resampler for interleaved audio, state carried across calls. When lowering
/// the rate would fold audio into the audible band (e.g. 96 kHz → 48 kHz, or 48 kHz playback on
/// a 16 kHz headset), the input first goes through a Blackman-windowed sinc low-pass (~74 dB
/// stopband), computed only at the input frames the interpolation reads.
pub struct Resampler {
    pub step: f64, // input frames per output frame
    pos: f64,      // read position; 0 = `prev`, 1.. = current chunk
    prev: Vec<f32>,
    fir: Vec<f32>,      // anti-alias taps; empty at ≤ 48 kHz (no filtering, no delay)
    hist: Vec<f32>,     // the last fir.len() - 1 input frames, then the current chunk
    filtered: Vec<f32>, // the current chunk after the FIR
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Self {
        // An alias of f lands at `to` - f. Needed only if the source's top (`from`/2) folds below
        // 20 kHz (so not for 48 → 44.1 kHz). The transition is centred on `to`/2 and `to`/6 wide
        // (into 48 kHz: passband to 20 kHz, stopband from 28 kHz); 6 taps per transition width
        // of source rate: 73 at 96 → 48 kHz, 109 at 48 → 16 kHz.
        let taps = if to < from && (to as i64) - (from as i64) / 2 < 20_000 { (36 * from / to) as usize | 1 } else { 0 };
        let fc = to as f64 / 2.0 / from as f64;
        let m = taps.saturating_sub(1) as f64;
        let mut fir: Vec<f32> = (0..taps)
            .map(|i| {
                let (x, w) = (i as f64 - m / 2.0, std::f64::consts::TAU * i as f64 / m);
                let sinc = if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
                (sinc * (0.42 - 0.5 * w.cos() + 0.08 * (2.0 * w).cos())) as f32
            })
            .collect();
        let sum: f32 = fir.iter().sum();
        fir.iter_mut().for_each(|h| *h /= sum); // unity gain at DC
        let room = 16_384 * channels; // the capture callback's largest expected chunk
        Resampler {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: vec![0.0; channels],
            hist: Vec::with_capacity(if taps > 0 { room + taps * channels } else { 0 }),
            filtered: Vec::with_capacity(if taps > 0 { room } else { 0 }),
            fir,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.fir.is_empty() {
            return self.linear(input, out);
        }
        let ch = self.prev.len();
        let n = input.len() / ch;
        let (mut hist, mut y) = (std::mem::take(&mut self.hist), std::mem::take(&mut self.filtered));
        hist.resize((self.fir.len() - 1) * ch, 0.0); // zeros before the first chunk
        hist.extend_from_slice(&input[..n * ch]);
        y.clear();
        y.resize(n * ch, 0.0);
        // filter only the frames `linear` reads: each output's two neighbours, and the last frame
        let mut done = None;
        let mut fir_at = |j: usize, y: &mut Vec<f32>| {
            if done < Some(j) {
                for c in 0..ch {
                    y[j * ch + c] = hist[j * ch + c..].iter().step_by(ch).zip(&self.fir).map(|(x, h)| x * h).sum();
                }
                done = Some(j);
            }
        };
        let mut pos = self.pos;
        while pos < n as f64 {
            let i = pos as usize;
            if i > 0 {
                fir_at(i - 1, &mut y);
            }
            fir_at(i, &mut y);
            pos += self.step;
        }
        if n > 0 {
            fir_at(n - 1, &mut y);
        }
        hist.drain(..n * ch);
        self.linear(&y, out);
        (self.hist, self.filtered) = (hist, y);
    }

    fn linear(&mut self, input: &[f32], out: &mut Vec<f32>) {
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
        assert_eq!(rx.handle(&mut packets[10].clone(), &c, never), None); // late (or replayed) packet
        assert_eq!(c.fec_recovered.load(Relaxed), 1);
        assert_eq!(c.lost.load(Relaxed), 1, "a single loss counts as lost too");
        assert_eq!(c.received.load(Relaxed), 49);
        assert_eq!(pcm.len(), 50 * FRAME * 2);
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt();
        assert!(rms > 0.2, "rms {rms}");
    }

    #[test]
    fn live_music_mode_switch() {
        let key = [7u8; 32];
        let mut tx = Packetizer::new(1, 64_000, &key).unwrap();
        let tone = |n: usize, ch: usize| -> Vec<f32> { (0..n * ch).map(|i| 0.5 * (i as f32 * 0.13 / ch as f32).sin()).collect() };
        let mut packets: Vec<Vec<u8>> = (0..10).map(|_| tx.packet(&tone(FRAME, 1)).unwrap().to_vec()).collect();
        tx.set_mode(&mut Mode::new(2, true, false).unwrap()).unwrap();
        assert_eq!((tx.channels(), tx.frame()), (2, 2 * FRAME));
        packets.extend((0..10).map(|_| tx.packet(&tone(2 * FRAME, 2)).unwrap().to_vec()));
        // seq (the AEAD nonce) keeps counting across the encoder swap: no reuse
        let seqs: Vec<u32> = packets.iter().map(|p| u32::from_be_bytes([p[3], p[4], p[5], p[6]])).collect();
        assert_eq!(seqs, (0..20).collect::<Vec<u32>>());

        let (mut rx, c) = (Rx::new(&key), Counters::default());
        let mut sizes = Vec::new(); // stereo frames per `out` call
        for (i, p) in packets.iter().enumerate() {
            if i != 15 && i != 16 {
                let ch = rx.handle(&mut p.clone(), &c, &mut |s: &[f32]| sizes.push(s.len() / 2)).unwrap();
                assert_eq!(ch, if i < 10 { 1 } else { 2 });
            }
        }
        assert_eq!(rx.period_us(), 20_000);
        assert_eq!(c.lost.load(Relaxed), 2);
        assert_eq!(&sizes[..10], &[FRAME; 10], "10 ms mono decodes");
        // the two lost 20 ms packets are concealed (PLC) with 20 ms each, then packet 17 decodes
        assert_eq!(&sizes[10..], &[2 * FRAME; 10]);
        // a mono packet replayed after the switch to stereo is dropped, not played
        assert_eq!(rx.handle(&mut packets[3].clone(), &c, &mut |_: &[f32]| panic!("replay played")), None);
        assert_eq!(c.lost.load(Relaxed), 2);

        // and back to normal
        tx.set_mode(&mut Mode::new(1, false, false).unwrap()).unwrap();
        assert_eq!(tx.frame(), FRAME);
        let p = tx.packet(&tone(FRAME, 1)).unwrap();
        assert_eq!(u32::from_be_bytes([p[3], p[4], p[5], p[6]]), 20);
    }

    #[test]
    fn replay_and_seq_limits() {
        let key = [7u8; 32];
        let mut tx = Packetizer::new(1, 64_000, &key).unwrap();
        let mut packets: Vec<Vec<u8>> = (0..5).map(|_| tx.packet(&[0.0; FRAME]).unwrap().to_vec()).collect();
        let (mut rx, c) = (Rx::new(&key), Counters::default());
        for p in &packets {
            rx.handle(&mut p.clone(), &c, &mut |_: &[f32]| {}).unwrap();
        }
        let never = &mut |_: &[f32]| panic!("replay played");
        for i in [0, 2, 4] {
            assert_eq!(rx.handle(&mut packets[i].clone(), &c, never), None, "replayed packet {i}");
        }
        assert_eq!(rx.expected, Some(5), "replays don't move expected");
        // a big forward jump resyncs without concealment
        tx.seq = 5000;
        packets.push(tx.packet(&[0.0; FRAME]).unwrap().to_vec());
        let mut calls = 0;
        rx.handle(&mut packets[5].clone(), &c, &mut |_: &[f32]| calls += 1).unwrap();
        assert_eq!((calls, rx.expected), (1, Some(5001)));
        assert_eq!(rx.handle(&mut packets[4].clone(), &c, never), None);
        // seq (the nonce) never wraps: the last one is u32::MAX - 1, then packet() refuses
        tx.seq = u32::MAX - 1;
        let p = tx.packet(&[0.0; FRAME]).unwrap();
        assert_eq!(u32::from_be_bytes([p[3], p[4], p[5], p[6]]), u32::MAX - 1);
        assert!(tx.packet(&[0.0; FRAME]).is_err());
        assert!(tx.packet(&[0.0; FRAME]).is_err(), "stays refused");
    }

    /// Hi-Fi packets 0..n, each one constant `level(k)`, so what plays tells which packet it was.
    fn hifi_stream(n: u32) -> ([u8; 32], Vec<Vec<u8>>) {
        let key = [7u8; 32];
        let mut tx = Packetizer::new(1, 64_000, &key).unwrap();
        tx.set_mode(&mut Mode::new(1, true, true).unwrap()).unwrap();
        (key, (0..n).map(|k| tx.packet(&[level(k); PCM_FRAME * 2]).unwrap().to_vec()).collect())
    }

    fn level(k: u32) -> f32 {
        (k + 1) as f32 / 1024.0 // exact in 24 bits
    }

    /// Which packet a 5 ms chunk was (by its last sample); None = given up (silence).
    fn id(s: &[f32]) -> Option<u32> {
        let v = s[s.len() - 1];
        (v != 0.0).then(|| (v * 1024.0).round() as u32 - 1)
    }

    /// Feeds one packet: (accepted, the chunks played).
    fn feed(rx: &mut Rx, c: &Counters, p: &[u8]) -> (bool, Vec<Option<u32>>) {
        let mut played = vec![];
        let ok = rx.handle(&mut p.to_vec(), c, &mut |s: &[f32]| {
            assert_eq!(s.len(), PCM_FRAME * 2);
            played.push(id(s));
        });
        (ok.is_some(), played)
    }

    #[test]
    fn hifi_pcm_round_trip() {
        let key = [7u8; 32];
        let mut tx = Packetizer::new(1, 64_000, &key).unwrap();
        tx.packet(&[0.0; FRAME]).unwrap();
        tx.set_mode(&mut Mode::new(1, true, true).unwrap()).unwrap();
        assert_eq!((tx.pcm(), tx.channels(), tx.frame()), (true, 2, PCM_FRAME), "Hi-Fi: stereo, 5 ms");
        let lsb = 1.0 / 8_388_608.0;
        let mut pcm: Vec<f32> = (0..PCM_FRAME * 2).map(|i| (i as f32 * 0.37).sin() * 0.9).collect();
        pcm[..6].copy_from_slice(&[1.0, -1.5, 0.4 * lsb, 0.6 * lsb, -0.6 * lsb, f32::NAN]);
        let p = tx.packet(&pcm).unwrap().to_vec();
        assert_eq!(p.len(), HEADER + 1 + 1440 + TAG);
        assert!(p.len() <= MAX_PACKET);
        assert_eq!(seq_of(&p), 1, "seq (the nonce) continues across the switch");
        let (kind, _, body) = open(&cipher(&key), &mut p.clone()).map(|(k, s, b)| (k, s, b.to_vec())).unwrap();
        assert_eq!((kind, body.len()), (0x82, 1440));
        assert!(!matches!(kind, 1 | 2), "0.2.x's open() accepts only 1 | 2 there: an old receiver drops it as junk");

        let (mut rx, c, mut out) = (Rx::new(&key), Counters::default(), vec![]);
        assert_eq!(rx.handle(&mut p.clone(), &c, &mut |s: &[f32]| out.extend_from_slice(s)), Some(2));
        assert_eq!(out.len(), PCM_FRAME * 2);
        assert_eq!(&out[..6], &[8_388_607.0 * lsb, -1.0, 0.0, lsb, -lsb, 0.0], "clamped, rounded to 24 bits; NaN is silence");
        for (o, i) in out.iter().zip(&pcm).skip(6) {
            assert_eq!(*o, (i * 8_388_608.0).round() * lsb, "lossless to 24 bits");
        }
        assert_eq!((rx.pcm(), rx.period_us(), c.received.load(Relaxed)), (true, 5_000, 1));
        assert_eq!(rx.handle(&mut p.clone(), &c, &mut |_: &[f32]| panic!("replay played")), None);

        // CPU: pack + encrypt, then decrypt + unpack, per 5 ms packet
        let t = std::time::Instant::now();
        for _ in 0..1000 {
            let mut p = tx.packet(&pcm).unwrap().to_vec();
            rx.handle(&mut p, &c, &mut |_: &[f32]| {});
        }
        let us = t.elapsed().as_micros() as f64 / 1000.0;
        eprintln!("Hi-Fi: {us:.1} µs per 5 ms packet, both ends");
        assert!(us < 5000.0, "{us} µs: can't keep up with 5 ms packets"); // ~5 µs in release
    }

    #[test]
    fn hifi_reorder_window() {
        let (key, p) = hifi_stream(260);
        let (mut rx, c) = (Rx::new(&key), Counters::default());
        assert_eq!(feed(&mut rx, &c, &p[0]), (true, vec![Some(0)]), "in order: played at once");
        assert_eq!(feed(&mut rx, &c, &p[2]), (true, vec![]), "out of order: held");
        assert_eq!(feed(&mut rx, &c, &p[1]), (true, vec![Some(1), Some(2)]), "the gap filled: both play");
        assert_eq!(feed(&mut rx, &c, &p[1]), (false, vec![]), "played: duplicate refused");
        assert_eq!(feed(&mut rx, &c, &p[4]), (true, vec![]));
        assert_eq!(feed(&mut rx, &c, &p[4]), (false, vec![]), "held: duplicate refused");
        assert!(rx.nack(Instant::now()).is_some(), "3 is asked for");
        assert_eq!(feed(&mut rx, &c, &p[3]), (true, vec![Some(3), Some(4)]), "the resend fills the gap");
        assert_eq!(c.fec_recovered.load(Relaxed), 1, "counted as recovered");
        assert_eq!(feed(&mut rx, &c, &p[3]), (false, vec![]), "a resend is taken once");
        assert_eq!(feed(&mut rx, &c, &p[0]), (false, vec![]), "replay refused");
        assert_eq!((c.received.load(Relaxed), c.lost.load(Relaxed)), (5, 0));
        assert_eq!(feed(&mut rx, &c, &p[6]), (true, vec![]));
        // 1 s (200 packets) further on: 5 can't wait any longer, 7..=50 were never seen
        let (ok, played) = feed(&mut rx, &c, &p[250]);
        assert!(ok);
        assert_eq!(played, [None, Some(6)], "5 given up as silence, 6 played");
        assert_eq!(c.lost.load(Relaxed), 1 + 44, "5, and 7..=50 skipped");
        assert_eq!(feed(&mut rx, &c, &p[50]), (false, vec![]), "too old");
        assert_eq!(feed(&mut rx, &c, &p[52]), (true, vec![]), "in the window: held");
    }

    #[test]
    fn hifi_nack_and_resend() {
        let (key, p) = hifi_stream(12);
        let (mut rx, c) = (Rx::new(&key), Counters::default());
        assert!(rx.nack(Instant::now()).is_none(), "not Hi-Fi yet: nothing to ask");
        for i in [0, 3, 5] {
            feed(&mut rx, &c, &p[i]);
        }
        let t = Instant::now();
        let nack = rx.nack(t).unwrap().to_vec();
        assert!(is_nack(&nack) && !is_nack(&p[0]));
        assert!(rx.nack(t + Duration::from_millis(10)).is_none(), "not again before RENACK");
        let mut nacks = Nacks::new(&key);
        assert!(Nacks::new(&[8; 32]).open(&mut nack.clone()).is_none(), "wrong key");
        let mut bad = nack.clone();
        bad[HEADER] ^= 1;
        assert!(nacks.open(&mut bad).is_none(), "tampered");
        let seqs: Vec<u32> = nacks.open(&mut nack.clone()).unwrap().collect();
        assert_eq!(seqs, [1, 2, 4], "two ranges: 1..=2 and 4");
        assert!(nacks.open(&mut nack.clone()).is_none(), "a replayed NACK is refused");
        assert_eq!(feed(&mut rx, &c, &nack), (false, vec![]), "a NACK is never audio");

        // the sender finds the packets as sent: same seq, same ciphertext
        let mut sent = Resend::default();
        p.iter().for_each(|q| sent.keep(q));
        for s in &seqs {
            assert_eq!(sent.get(*s), Some(&p[*s as usize][..]));
        }
        assert_eq!(sent.get(12), None, "never sent");
        let (_, later) = hifi_stream(SLOTS as u32 + 2);
        sent.keep(&later[SLOTS + 1]);
        assert_eq!(sent.get(1), None, "overwritten after SLOTS packets");

        // 1 and 2 come back; 4 is asked for again, once RENACK has passed
        feed(&mut rx, &c, &p[1]);
        feed(&mut rx, &c, &p[2]);
        let again = rx.nack(t + RENACK).unwrap().to_vec();
        assert_eq!(nacks.open(&mut again.clone()).unwrap().collect::<Vec<_>>(), [4]);
        // a ring that never runs low never gives up; one that does gives 4 up
        let mut played = vec![];
        rx.release(GIVE_UP, &c, &mut |s: &[f32]| played.push(id(s)));
        assert!(played.is_empty());
        rx.release(0, &c, &mut |s: &[f32]| played.push(id(s)));
        assert_eq!(played, [None, Some(5)]);
        assert!(rx.nack(t + RENACK * 10).is_none(), "nothing missing any more");
    }

    #[test]
    fn hifi_gives_up_with_fades() {
        let (key, p) = hifi_stream(3);
        let (mut rx, c) = (Rx::new(&key), Counters::default());
        feed(&mut rx, &c, &p[0]);
        feed(&mut rx, &c, &p[2]);
        let mut out: Vec<Vec<f32>> = vec![];
        rx.release(GIVE_UP, &c, &mut |s: &[f32]| out.push(s.to_vec()));
        assert!(out.is_empty(), "still time for a resend");
        rx.release(GIVE_UP - 1, &c, &mut |s: &[f32]| out.push(s.to_vec()));
        assert_eq!(out.len(), 2, "1 as silence, then 2");
        assert_eq!(c.lost.load(Relaxed), 1, "counted lost");
        let (gap, next) = (&out[0], &out[1]);
        assert_eq!(gap[0], level(0), "fades out from the last sample played ...");
        assert!(gap[2 * FADE / 2] > 0.0 && gap[2 * FADE / 2] < level(0));
        assert!(gap[2 * FADE..].iter().all(|s| *s == 0.0), "... to silence within 1 ms");
        assert_eq!(next[0], 0.0, "the next packet fades in ...");
        assert!(next.chunks(2).take(FADE).zip(next.chunks(2).skip(1)).all(|(a, b)| a[0] <= b[0]));
        assert!(next[2 * FADE..].iter().all(|s| *s == level(2)), "... over 1 ms");
    }

    #[test]
    fn hifi_and_opus_share_one_seq_space() {
        let key = [7u8; 32];
        let mut tx = Packetizer::new(1, 64_000, &key).unwrap();
        let mut p: Vec<Vec<u8>> = (0..5).map(|_| tx.packet(&[0.1; FRAME]).unwrap().to_vec()).collect();
        tx.set_mode(&mut Mode::new(1, true, true).unwrap()).unwrap();
        p.extend((5..10).map(|_| tx.packet(&[0.1; PCM_FRAME * 2]).unwrap().to_vec()));
        tx.set_mode(&mut Mode::new(2, true, false).unwrap()).unwrap(); // fallback: Music Mode
        assert_eq!((tx.pcm(), tx.channels(), tx.frame()), (false, 2, 2 * FRAME));
        p.extend((10..15).map(|_| tx.packet(&[0.1; 4 * FRAME]).unwrap().to_vec()));
        let (mut rx, c, mut sizes) = (Rx::new(&key), Counters::default(), vec![]);
        for (i, q) in p.iter().enumerate() {
            if i != 7 {
                assert!(rx.handle(&mut q.clone(), &c, &mut |s: &[f32]| sizes.push(s.len() / 2)).is_some(), "{i}");
            }
            assert_eq!(rx.pcm(), (5..10).contains(&i), "{i}");
        }
        // 7 was given up (silence) when Opus took over at 10
        assert_eq!(sizes, [[FRAME; 5].as_slice(), &[PCM_FRAME; 5], &[2 * FRAME; 5]].concat());
        assert_eq!((c.lost.load(Relaxed), c.received.load(Relaxed)), (1, 14));
        let never = &mut |_: &[f32]| panic!("replay played");
        for i in [2, 6, 7, 12] {
            assert_eq!(rx.handle(&mut p[i].clone(), &c, never), None, "{i}: replayed or too late");
        }
    }

    #[test]
    fn hifi_fallback() {
        let mut f = Fallback::default();
        for _ in 0..30 {
            assert!(!f.on_report(200, 0, 0));
        }
        for _ in 0..20 {
            assert!(!f.on_report(196, 4, 0), "2% unrecovered loss is still OK");
        }
        assert!(!f.on_report(200, 0, 1), "one underrun is OK ...");
        for _ in 0..8 {
            assert!(!f.on_report(200, 0, 0));
        }
        assert!(f.on_report(200, 0, 1), "... a second within 10 s falls back");
        for _ in 0..30 {
            assert!(f.on_report(50, 0, 0));
        }
        assert!(f.on_report(50, 0, 1), "an underrun restarts the clean minute");
        for _ in 0..59 {
            assert!(f.on_report(50, 1, 0), "≤ 2% loss is clean");
        }
        assert!(!f.on_report(50, 0, 0), "a clean minute: Hi-Fi again");
        assert!(!f.on_report(200, 0, 1), "with a fresh 10 s window");
        let mut f = Fallback::default();
        for _ in 0..9 {
            f.on_report(200, 0, 0);
        }
        assert!(f.on_report(150, 50, 0), "> 2% lost over the last 10 s");
    }

    #[test]
    fn music_mode_drains_instead_of_skipping() {
        // the stall memory expires: the target falls to its minimum with the old target's worth
        // buffered (Music Mode: ~0.7 s → 150 ms after a Wi-Fi dropout; normal: ~120 → 10 ms, its
        // ceiling is 125 ms)
        let need = 480;
        for (music, high, skips) in [(true, RATE as usize * 700 / 1000, false), (false, RATE as usize * 120 / 1000, true)] {
            let mut p = Playout::default();
            p.set_music(music);
            p.set_jitter(high);
            let full = need + p.target(); // exactly the old target buffered: playing, nothing to drop
            assert!(matches!(p.plan(full, need), Plan::Play { discard: 0, .. }), "music {music}");
            p.set_jitter(0);
            let r = (0..200).map(|_| p.plan(full, need)).find(|r| matches!(r, Plan::Play { discard, .. } if *discard > 0));
            assert_eq!(r.is_some(), skips, "music {music}: {r:?}");
            if music {
                // ...it plays (slightly) fast instead, up to the pitch-safe 0.5%
                let Plan::Play { ratio, .. } = p.plan(full, need) else { panic!() };
                assert!(ratio > 1.0 && ratio <= 1.0 + MUSIC_MAX_ADJ as f64 + 1e-9, "{ratio}");
            }
        }
    }

    #[test]
    fn music_cushion() {
        let mut p = Playout::default();
        assert_eq!(p.target(), TARGET);
        p.set_music(true);
        assert_eq!(p.target(), RATE as usize * 150 / 1000);
        assert_eq!(p.plan(480 + MUSIC_TARGET - 1, 480), Plan::Silence, "prebuffers 150 ms");
        // a 150 ms stall is covered in Music Mode (ceiling 1 s), capped at 125 ms normally
        p.set_jitter(RATE as usize * 150 / 1000);
        assert_eq!(p.target(), RATE as usize * 155 / 1000);
        // a ~0.7 s Wi-Fi dropout (seen in the field) is covered too, up to the 1 s ceiling
        p.set_jitter(RATE as usize * 700 / 1000);
        assert_eq!(p.target(), RATE as usize * 705 / 1000);
        p.set_jitter(RATE as usize * 2);
        assert_eq!(p.target(), MUSIC_MAX_TARGET);
        p.set_jitter(RATE as usize * 150 / 1000);
        p.set_music(false);
        assert_eq!(p.target(), MAX_TARGET);
        p.set_jitter(0);
        p.set_music(true);
        p.underrun();
        assert_eq!(p.target(), MUSIC_TARGET + MUSIC_GROW, "a surprise stall adds 30 ms in Music Mode");
        // Music Mode remembers a stall for at least 2 min of smooth packets (periodic Wi-Fi stalls
        // came every 60–90 s in the field; a 30 s memory let each one hit a shrunken buffer)
        let mut j = Jitter::default();
        j.push(150_000, 20_000, MUSIC_JITTER_WINDOW_US);
        for _ in 0..6000 {
            j.push(20_000, 20_000, MUSIC_JITTER_WINDOW_US); // 120 s
        }
        assert_eq!(j.push(20_000, 20_000, MUSIC_JITTER_WINDOW_US), 130_000);
        // a ~0.7 s Wi-Fi dropout is learned too (it sizes the target, up to the 1 s ceiling)
        assert_eq!(Jitter::default().push(700_000, 20_000, MUSIC_JITTER_WINDOW_US), 680_000);
        // cushion far below target: Music Mode slows by at most 0.5% (pitch-safe)
        let mut m = Playout::default();
        m.set_music(true);
        assert!(matches!(m.plan(480 + MUSIC_TARGET, 480), Plan::Play { .. }), "starts once 150 ms is buffered");
        let r = (0..200).map(|_| m.plan(480 + MUSIC_TARGET / 2, 480)).last().unwrap();
        assert!(matches!(r, Plan::Play { ratio, .. } if (0.995..1.0).contains(&ratio)), "{r:?}");
        p.set_music(false);
        assert_eq!(p.target(), TARGET + MUSIC_GROW, "the stall boost carries over and fades as usual");
    }

    #[test]
    fn jitter_20ms_period() {
        let mut j = Jitter::default();
        for _ in 0..500 {
            assert_eq!(j.push(20_000, 20_000, JITTER_WINDOW_US), 0, "evenly spaced 20 ms packets are not late");
        }
        assert_eq!(j.push(25_000, 20_000, JITTER_WINDOW_US), 5_000);
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
        assert_eq!(j.push(10_000, 10_000, JITTER_WINDOW_US), 0); // on time
        assert_eq!(j.push(28_000, 10_000, JITTER_WINDOW_US), 18_000); // 18 ms late
        assert_eq!(j.push(3_000_000, 10_000, JITTER_WINDOW_US), 18_000); // restart pause ignored
        for _ in 0..1000 {
            j.push(10_000, 10_000, JITTER_WINDOW_US); // 10 s of smooth packets ages the spike out
        }
        assert_eq!(j.push(10_000, 10_000, JITTER_WINDOW_US), 0);
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

        // Music Mode: the ceiling rises to 160k and clean reports ramp up in +8k steps
        let mut r = RateControl::new(ceiling(64_000, false));
        r.set_ceiling(ceiling(64_000, true));
        assert_eq!(r.on_report(100, 0, 0).0, 72_000);
        assert_eq!(r.on_report(100, 0, 0).0, 80_000);
        for _ in 0..20 {
            r.on_report(100, 0, 0);
        }
        assert_eq!(r.on_report(100, 0, 0).0, 160_000);
        r.set_ceiling(ceiling(64_000, false));
        assert_eq!(r.on_report(100, 0, 0).0, 64_000, "back to normal clamps down at once");
        assert_eq!(ceiling(200_000, false), 96_000);
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
        // Music Mode raises the max to 10, same climb rules; leaving it clamps back to 5
        c.set_max(10);
        for _ in 0..COMPLEXITY_HOLD_FRAMES * 20 {
            c.on_encode(50.0);
        }
        assert_eq!(c.level, 10);
        for _ in 0..3 {
            c.on_encode(20_000.0);
        }
        assert!(c.level < 10, "backs off under load in Music Mode too");
        assert_eq!(c.set_max(5), 5);
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

    #[test]
    fn resample_anti_alias() {
        // level (dB re the input) of a 250 ms tone at `hz`, resampled from `from` to `to` in 10 ms chunks
        let level_to = |from: u32, to: u32, hz: f64| {
            let mut r = Resampler::new(from, to, 2);
            let tone = |i: usize| 0.5 * (std::f64::consts::TAU * hz * i as f64 / from as f64).sin() as f32;
            let input: Vec<f32> = (0..from as usize / 4).flat_map(|i| [tone(i), tone(i)]).collect();
            let mut out = Vec::new();
            for chunk in input.chunks(2 * from as usize / 100) {
                r.process(chunk, &mut out);
            }
            assert!((out.len() as f64 / 2.0 - to as f64 / 4.0).abs() <= 1.0, "{from}: {} frames", out.len() / 2);
            let left: Vec<f32> = out.iter().step_by(2).skip(480).copied().collect(); // past the filter's warm-up
            let rms = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt();
            20.0 * (rms / (0.5 / 2f32.sqrt())).log10()
        };
        let level = |from, hz| level_to(from, RATE, hz);
        for from in [88_200, 96_000, 176_400, 192_000] {
            let (alias, pass) = (level(from, 30_000.0), level(from, 1_000.0));
            assert!(alias <= -60.0, "{from}: 30 kHz folds to {alias} dB");
            assert!(pass.abs() <= 0.5, "{from}: 1 kHz at {pass} dB");
        }
        assert!(level(48_000, 1_000.0).abs() < 1e-3, "48 kHz passes untouched");
        // playback on a 16 kHz headset: 10 kHz would fold to 6 kHz
        let (alias, pass) = (level_to(RATE, 16_000, 10_000.0), level_to(RATE, 16_000, 1_000.0));
        assert!(alias <= -60.0 && pass.abs() <= 0.5, "48 → 16 kHz: alias {alias} dB, 1 kHz {pass} dB");
        assert!(Resampler::new(48_000, RATE, 2).fir.is_empty() && Resampler::new(RATE, 44_100, 2).fir.is_empty(), "no filter, no delay at ≤ 48 kHz");
    }
}
