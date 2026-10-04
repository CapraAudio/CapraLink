//! One node per process: config, mDNS discovery, PIN pairing (SPAKE2), the Noise control
//! channel and the single active `Link` (MASTER.md §3.3).

use crate::dsp::{ceiling, Fallback, RateControl, MUSIC_TARGET, RATE, TARGET};
use crate::log::log;
use crate::ptt::{self, Ev, PttKey, PttMode, Talk};
use crate::vdev::Virtual;
use crate::{AudioDevice, Failure, Keys, Link, Settings, Stats, EVERYTHING, MAX_VOLUME, NO_DEVICE, VIRTUAL_INPUT, VIRTUAL_OUTPUT};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

const SERVICE: &str = "_capralink._udp.local.";
const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const DIAL_TIMEOUT: Duration = Duration::from_secs(1); // per advertised address
const LINK_TIMEOUT: Duration = Duration::from_secs(10); // peer may be opening audio devices
const REPORT: Duration = Duration::from_secs(1); // also doubles as the session keepalive
const DEAD: Duration = Duration::from_secs(15);
// pairing only while the user has the PIN showing; each wrong PIN locks it, doubling (short under test)
const PAIR_WINDOW: Duration = Duration::from_millis(if cfg!(test) { 3000 } else { 120_000 });
const LOCK_FIRST: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 60_000 });
const LOCK_MAX: Duration = Duration::from_millis(if cfg!(test) { 1000 } else { 3_600_000 });
// incoming connections before they authenticate: how many at once, per source IP, and for how long
const MAX_PENDING: usize = 8;
const MAX_PENDING_PER_IP: usize = 2;
const AUTH_DEADLINE: Duration = Duration::from_millis(if cfg!(test) { 2000 } else { 10_000 });
const NO_IPV6: &str = "IPv6 addresses aren't supported yet — use the computer's IPv4 address";
// auto-reconnect backoff (MASTER.md §3.9); short under test
const RETRY_FIRST: Duration = Duration::from_millis(if cfg!(test) { 100 } else { 2000 });
const RETRY_MAX: Duration = Duration::from_millis(if cfg!(test) { 400 } else { 30_000 });
const QUALITY_WINDOW: usize = 30; // seconds of receive reports that `Quality` covers
const QUALITY_LOG: Duration = Duration::from_secs(30);
// a long `Text` reply goes in pieces: even fully JSON-escaped (6x) one fits a 64 KB frame
const TEXT_CHUNK: usize = 8000;
// how long `ptt_capture` waits for a key (short under test)
const CAPTURE: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 10_000 });

#[derive(Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    pub pin: String,
    /// Seconds left in the pairing window (0 = closed; see `Node::open_pairing`).
    pub pairing_secs: u64,
    /// Seconds pairing stays locked after a wrong PIN (0 = not locked).
    pub pairing_locked_secs: u64,
    pub devices: Vec<Device>,
    pub stats: Option<Stats>,
    pub settings: Settings,
    /// The device whose audio settings `settings` holds: the connected one, else the last one.
    pub current: Option<String>,
    pub error: Option<String>,
    /// Why the virtual devices couldn't be created (Linux), if they couldn't.
    pub virtual_error: Option<String>,
    /// How well audio is arriving over the last ~30 s (None when not streaming).
    pub quality: Option<Quality>,
    /// Push-to-talk is on and this computer is talking.
    #[serde(default)]
    pub talking: bool,
    /// Why push-to-talk can't hear its key (e.g. no Input Monitoring permission on macOS).
    #[serde(default)]
    pub ptt_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Quality {
    /// "good", "fair" or "poor"
    pub grade: String,
    /// What's wrong, in plain words ("" when good).
    pub hint: String,
    pub loss_pct: f32,
    pub underruns: u64,
}

/// One setup check (see `Node::checks`).
#[derive(Serialize, Deserialize)]
pub struct Check {
    pub ok: bool,
    pub title: String,
    /// What to do about it, when not ok.
    pub fix: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub paired: bool,
    pub online: bool,
    pub connected: bool,
    /// Online, or offline but reachable at a remembered address (manual Connect still works).
    pub reachable: bool,
    /// Last address that worked for this peer (for "last seen at" display), if any.
    pub addr: Option<String>,
    /// The link to it dropped and this computer is trying to get it back.
    pub reconnecting: bool,
}

/// Another computer's settings and its own device lists, for remote configuration.
/// `inputs`/`outputs` are the listed names, all that 0.1.x reads (it would reject the whole reply
/// over entries it can't parse); `input_devices`/`output_devices` are the entries, absent from a
/// 0.1.x reply, whose settings hold names.
#[derive(Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    pub settings: Settings,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    #[serde(default)]
    pub input_devices: Vec<AudioDevice>,
    #[serde(default)]
    pub output_devices: Vec<AudioDevice>,
}

#[derive(Serialize, Deserialize)]
struct Config {
    device_id: String,
    name: String,
    #[serde(flatten)]
    settings: Settings,
    #[serde(default)]
    peers: Vec<Peer>,
    /// The device this node last connected to itself, for auto-reconnect (MASTER.md §3.9).
    #[serde(default)]
    last_peer: Option<String>,
    /// The device the working audio settings belong to (MASTER.md §3.10).
    #[serde(default)]
    current: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Peer {
    id: String,
    name: String,
    secret: String, // 32-byte hex
    /// Last "ip:port" that worked for reaching this peer's control port (manual-connect fallback
    /// when mDNS discovery can't see it).
    #[serde(default)]
    addr: Option<String>,
    /// This computer's audio settings for sessions with this peer (MASTER.md §3.10).
    #[serde(default)]
    audio: Option<Audio>,
}

/// The per-connection part of `Settings`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Audio {
    input: Option<String>,
    output: Option<String>,
    bitrate: i32,
    channels: u16,
    #[serde(default = "full_volume")]
    send_volume: u16,
    #[serde(default = "full_volume")]
    recv_volume: u16,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    ptt: PttMode,
    #[serde(default)]
    ptt_key: Option<PttKey>,
}

fn full_volume() -> u16 {
    100
}

impl Audio {
    fn of(s: &Settings) -> Audio {
        let s = s.clone();
        Audio { input: s.input, output: s.output, bitrate: s.bitrate, channels: s.channels, send_volume: s.send_volume, recv_volume: s.recv_volume, mute: s.mute, ptt: s.ptt, ptt_key: s.ptt_key }
    }

    fn apply(&self, s: Settings) -> Settings {
        let a = self.clone();
        Settings { input: a.input, output: a.output, bitrate: a.bitrate, channels: a.channels, send_volume: a.send_volume, recv_volume: a.recv_volume, mute: a.mute, ptt: a.ptt, ptt_key: a.ptt_key, ..s }
    }
}

/// A change between these settings needs the link restarted (new devices or encoder); volume,
/// mute, push-to-talk and Music Mode apply live.
fn restarts(a: &Settings, b: &Settings) -> bool {
    (&a.input, &a.output, a.bitrate, a.channels) != (&b.input, &b.output, b.bitrate, b.channels)
}

/// Settings that can't be saved.
fn check(s: &Settings) -> Result<()> {
    ensure!(matches!(s.channels, 1 | 2), "channels must be 1 or 2");
    ensure!(s.send_volume <= MAX_VOLUME && s.recv_volume <= MAX_VOLUME, "volume must be 0 to {MAX_VOLUME}%");
    ensure!(s.ptt == PttMode::Off || s.ptt_key.is_some(), "pick a push-to-talk button first");
    Ok(())
}

/// `cur` with the fields in `patch` (`Settings` fields as JSON) replaced.
fn merge(cur: &Settings, patch: &serde_json::Map<String, serde_json::Value>) -> Result<Settings> {
    let mut v = serde_json::to_value(cur)?;
    v.as_object_mut().expect("Settings is a struct").extend(patch.clone());
    Ok(serde_json::from_value(v)?)
}

/// Control messages. Pair/Hello/Session travel in clear; the rest inside Noise.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Msg {
    /// `port` = the sender's own listening (control/audio) port, so the responder can remember
    /// how to reach it later without mDNS; absent (older peer) means don't record it.
    Pair { id: String, name: String, #[serde(default)] port: Option<u16> },
    Hello { id: String, name: String },
    Session { id: String },
    Link { channels: u16, port: u16 },
    Ok,
    Error { message: String },
    Stop,
    Ping,
    /// Deltas (since this node's previous report) of its own Link's receive-side counters,
    /// sent every second so the peer's sender can steer bitrate/FEC (MASTER.md §3.5).
    /// For the delay readout (all absent from old peers, which ignore them): `ts` = the sender's
    /// clock (ms), `echo` = the peer's last `ts` and how long (ms) it waited here (round trip =
    /// now − ts − waited), `send_ms`/`recv_ms` = the sender's own sending/receiving part of the delay.
    Report {
        received: u64,
        lost: u64,
        underruns: u64,
        jitter_ms: f32,
        #[serde(default)]
        ts: Option<u64>,
        #[serde(default)]
        echo: Option<(u64, u64)>,
        #[serde(default)]
        send_ms: Option<f32>,
        #[serde(default)]
        recv_ms: Option<f32>,
    },
    /// Instead of `Link`: a one-request remote-configuration session (MASTER.md §3.6 M6b).
    Manage,
    #[serde(rename = "get_config")]
    GetConfig,
    Config(RemoteConfig),
    /// `settings` as JSON, merged onto the current ones: fields an older peer doesn't know
    /// (e.g. volume) keep their value.
    #[serde(rename = "set_settings")]
    SetSettings { settings: serde_json::Map<String, serde_json::Value>, #[serde(default)] name: Option<String> },
    /// This side's own Music Mode setting, sent after the session starts and on every change
    /// (MASTER.md §3.7). Old peers never send it (= off) and ignore it. `name` = the sender's own
    /// device name (at session start and on a rename; absent from old peers). `hifi` = the
    /// sender's own Hi-Fi setting, `hifi_ok` = it can do Hi-Fi (both absent from 0.2.x = can't).
    Mode {
        music: bool,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        hifi: bool,
        #[serde(default)]
        hifi_ok: bool,
    },
    /// Manage request: the diagnostics text (`Node::diagnostics`), answered with `Text`.
    Diagnostics { redact: bool },
    /// A long text, in pieces: `more` on all but the last (see `send_text`).
    Text { text: String, #[serde(default)] more: bool },
}

struct Found {
    name: String,
    addrs: Vec<SocketAddr>, // best first (see `rank`)
    fullname: String,
}

struct Session {
    peer_id: String,
    addr: SocketAddr, // peer's control/UDP address
    link: Option<Link>,
    ctl: Arc<Ctl>,
    peer_music: bool, // the peer's last `Mode`
    peer_hifi: (bool, bool), // ... its (hifi, hifi_ok)
    hifi_fallback: bool, // this side's Hi-Fi sending fell back to Opus (see `Fallback`)
    mine: bool,       // this node dialed it (only the initiator auto-reconnects)
    recent: VecDeque<[u64; 3]>, // per-second (received, lost, underruns) of our Link, newest last
    rtt_ms: Option<f32>, // control-channel round trip, from `Report` echoes
    peer_ms: (Option<f32>, Option<f32>), // the peer's (sending, receiving) part of the delay
}

/// Push-to-talk state.
#[derive(Default)]
struct Ptt {
    listener: Option<ptt::Listener>, // runs while a key is configured or being captured
    talk: Talk,
    of: (PttMode, Option<String>), // the mode and key id `talk` follows
    capture: Option<mpsc::Sender<Result<PttKey, String>>>, // a `ptt_capture` waiting for a key
    error: Option<String>,
}

/// How a session's control loop ended.
enum End {
    Stop, // the peer chose to end it
    Dead, // keepalive timeout
    Lost, // TCP error / failed send
    Device(String), // our capture/playback device went away: ended on purpose, not retried
}

struct St {
    cfg: Config,
    pin: String,
    failures: u32, // consecutive wrong PINs
    pairing_until: Option<Instant>,
    locked_until: Option<Instant>,
    found: HashMap<String, Found>,
    session: Option<Session>,
    error: Option<String>,
    vdev: Virtual,
    /// Bumped to cancel the retry loop (and any own dial that started before it); a loop only
    /// runs while this still equals the value it started with.
    retry: u64,
    retrying: Option<String>, // peer the retry loop is after
    ptt: Ptt,
}

struct Inner {
    dir: PathBuf,
    port: u16,
    mdns: Option<ServiceDaemon>,
    st: Mutex<St>,
    wake: Condvar, // wakes a sleeping retry loop when `retry` changes
    pending: Mutex<Vec<IpAddr>>, // sources of unauthenticated incoming connections
    pairing: Mutex<()>,          // held by the one PIN attempt allowed at a time
    dialing: Mutex<()>,          // held by this node's one session dial at a time
    starting: Mutex<()>,         // held while a session's audio starts (one at a time, outside `st`)
    #[cfg(test)]
    hook: Mutex<Option<Hook>>,
}

/// A test's pause point name and what to run there (see `Node::pause`).
#[cfg(test)]
type Hook = (&'static str, Box<dyn FnOnce() + Send>);

#[derive(Clone)]
pub struct Node(Arc<Inner>);

impl Node {
    /// Loads (or creates) the config, listens on TCP `port` (0 = any free port), and
    /// advertises + browses via mDNS when `mdns` is set. `None` = OS config dir.
    pub fn start(config_dir: Option<PathBuf>, port: u16, mdns: bool) -> Result<Node> {
        crate::pin_audio_host();
        let dir = config_dir_or_default(config_dir)?;
        crate::log::init(Some(dir.clone()), "capralinkd");
        let cfg = load(&dir)?;
        let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("port {port} is in use (is CapraLink already running?)"))?;
        let port = listener.local_addr()?.port();
        let (daemon, browse) = if mdns {
            let d = ServiceDaemon::new()?;
            // IPv4 only (the link is IPv4); with enable_addr_auto every remaining interface
            // address is advertised, and the dialer picks the one that answers.
            d.disable_interface(vec![IfKind::IPv6, IfKind::LoopbackV4])?;
            advertise(&d, &cfg.device_id, &cfg.name, port)?;
            let rx = d.browse(SERVICE)?;
            (Some(d), Some(rx))
        } else {
            (None, None)
        };
        let st = St { cfg, pin: new_pin(), failures: 0, pairing_until: None, locked_until: None, found: HashMap::new(), session: None, error: None, vdev: Virtual::setup(), retry: 0, retrying: None, ptt: Ptt::default() };
        let node = Node(Arc::new(Inner { dir, port, mdns: daemon, st: Mutex::new(st), wake: Condvar::new(), pending: Mutex::default(), pairing: Mutex::default(), dialing: Mutex::default(), starting: Mutex::default(), #[cfg(test)] hook: Mutex::default() }));
        if let Some(rx) = browse {
            let n = node.clone();
            std::thread::Builder::new().name("capralink-mdns".into()).spawn(move || {
                while let Ok(ev) = rx.recv() {
                    n.on_mdns(ev);
                }
            })?;
        }
        log(&format!("CapraLink {} started on {} {}, port {port}", crate::version(), std::env::consts::OS, std::env::consts::ARCH));
        let n = node.clone();
        std::thread::Builder::new().name("capralink-ctl".into()).spawn(move || n.accept(listener))?;
        {
            let mut st = node.st();
            if let Some(id) = st.cfg.last_peer.clone().filter(|_| st.cfg.settings.auto_reconnect) {
                node.start_retry(&mut st, id);
            }
            node.sync_ptt(&mut st);
        }
        Ok(node)
    }

    /// Clean exit: drops the session and tells the network we're gone. No `stop` and `last_peer`
    /// is kept, so the session comes back after a restart (ours or, if it dialed, the peer's).
    pub fn shutdown(&self) {
        let old = {
            let mut st = self.st();
            self.cancel_retry(&mut st);
            st.ptt.listener = None;
            st.session.take()
        };
        if let Some(s) = old {
            let _ = s.ctl.stream.shutdown(Shutdown::Both);
        }
        self.st().vdev.unload();
        if let Some(d) = &self.0.mdns {
            let id = self.st().cfg.device_id.clone();
            if let Ok(rx) = d.unregister(&format!("{id}.{SERVICE}")) {
                let _ = rx.recv_timeout(Duration::from_secs(1));
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.0.port
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.0.dir
    }

    fn st(&self) -> MutexGuard<'_, St> {
        self.0.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Test hook: runs the closure set for pause point `at`, once.
    #[cfg(test)]
    fn pause(&self, at: &str) {
        let f = self.0.hook.lock().unwrap().take_if(|(p, _)| *p == at);
        if let Some((_, f)) = f {
            f();
        }
    }

    /// (sending, receiving) VU levels of the active link, for smooth meters.
    pub fn levels(&self) -> Option<(f32, f32)> {
        self.st().session.as_ref().and_then(|s| s.link.as_ref()).map(Link::levels)
    }

    pub fn state(&self) -> NodeState {
        let st = self.st();
        let conn = st.session.as_ref().map(|s| s.peer_id.as_str());
        let mut devices: Vec<Device> = st
            .cfg
            .peers
            .iter()
            .map(|p| {
                let online = st.found.contains_key(&p.id) || conn == Some(&p.id);
                Device {
                    id: p.id.clone(),
                    name: p.name.clone(), // not the mDNS one (see `on_mdns`)
                    paired: true,
                    online,
                    connected: conn == Some(&p.id),
                    reachable: online || p.addr.is_some(),
                    addr: p.addr.clone(),
                    reconnecting: st.retrying.as_deref() == Some(&p.id),
                }
            })
            .collect();
        for (id, f) in &st.found {
            if !st.cfg.peers.iter().any(|p| p.id == *id) {
                devices.push(Device { id: id.clone(), name: f.name.clone(), paired: false, online: true, connected: false, reachable: true, addr: None, reconnecting: false });
            }
        }
        devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        let stats = st.session.as_ref().and_then(|s| Some((s, s.link.as_ref()?))).map(|(s, link)| {
            let ((send, recv), mut x) = (link.latency(), link.stats());
            // an older peer doesn't say its sending part: a typical capture buffer plus a frame
            let peer_send = s.peer_ms.0.unwrap_or(if x.music { 30.0 } else { 20.0 });
            x.delay_in_ms = recv.map(|r| one_way(peer_send, s.rtt_ms, r));
            x.delay_out_ms = send.zip(s.peer_ms.1).map(|(m, r)| one_way(m, s.rtt_ms, r));
            x.hifi_fallback = s.hifi_fallback && hifi(&st);
            x
        });
        NodeState {
            name: st.cfg.name.clone(),
            pin: st.pin.clone(),
            pairing_secs: secs_left(st.pairing_until),
            pairing_locked_secs: secs_left(st.locked_until),
            devices,
            quality: st.session.as_ref().zip(stats.as_ref()).map(|(s, x)| quality(s, x)),
            stats,
            settings: st.cfg.settings.clone(),
            current: st.cfg.current.clone(),
            error: st.error.clone(),
            virtual_error: st.vdev.error.clone(),
            talking: st.ptt.talk.on(),
            ptt_error: st.ptt.error.clone(),
        }
    }

    pub fn settings(&self) -> Settings {
        self.st().cfg.settings.clone()
    }

    /// Setup checks, run on demand: engine, virtual devices, Windows network/speaker, peers seen.
    pub fn checks(&self) -> Vec<Check> {
        let check = |ok: bool, title: String, fix: Option<String>| Check { ok, title, fix: fix.filter(|_| !ok) };
        let has = |list: Vec<AudioDevice>, n: &str| list.iter().any(|d| d.id == n);
        let mut v = vec![check(true, format!("CapraLink is running (port {})", self.0.port), None)];
        if cfg!(windows) {
            let ok = has(crate::output_devices(), VIRTUAL_INPUT);
            let title = if ok { "CapraLink Input is ready (VB-Cable)" } else { "CapraLink Input is missing: VB-Cable isn't installed" };
            v.push(check(ok, title.into(), Some("Install VB-Cable (free) from vb-audio.com/Cable for a CapraLink microphone".into())));
        } else {
            let ok = has(crate::input_devices(), VIRTUAL_OUTPUT) && has(crate::output_devices(), VIRTUAL_INPUT);
            let title = if ok { "CapraLink Input and Output are ready" } else { "CapraLink Input and Output are missing" };
            let fix = if cfg!(target_os = "macos") { "Reinstall CapraLink to add its audio devices".into() } else { self.st().vdev.error.clone().unwrap_or_else(|| "Restart CapraLink to add its audio devices".into()) };
            v.push(check(ok, title.into(), Some(fix)));
        }
        #[cfg(windows)]
        {
            let mut c = crate::system_command("powershell");
            c.args(["-NoProfile", "-Command", "(Get-NetConnectionProfile).NetworkCategory"]).stdin(std::process::Stdio::null());
            std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
            let public = c.output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Public"));
            let title = if public { "This network is set to Public" } else { "This network is Private" };
            v.push(check(!public, title.into(), Some("Set this network to Private in Settings → Network & internet, or Windows Firewall blocks other computers".into())));
            let cable = crate::default_output_name().is_some_and(|n| n.contains("CABLE Input"));
            let title = if cable { "VB-Cable is the default speaker" } else { "The default speaker isn't VB-Cable" };
            v.push(check(!cable, title.into(), Some("Windows sends all sound into VB-Cable: set your speakers or headset as the default output again in Sound settings".into())));
        }
        let st = self.st();
        for (label, saved, list) in [("Send from", &st.cfg.settings.input, crate::input_devices()), ("Play to", &st.cfg.settings.output, crate::output_devices())] {
            if let Some(d) = saved.as_deref().filter(|d| *d != crate::NO_DEVICE && crate::pick(&list, d).is_none()) {
                v.push(check(false, format!("{label} device \"{}\" isn't connected", crate::missing_name(d)), Some("Plug it in, then use Refresh devices… in the list, or pick another device".into())));
            }
        }
        for p in &st.cfg.peers {
            let seen = st.found.contains_key(&p.id) || st.session.as_ref().is_some_and(|s| s.peer_id == p.id);
            // not announced (yet, or multicast is blocked) but reachable by its saved address is normal
            let title = match (&p.addr, seen) {
                (_, true) => format!("{} is on the network", p.name),
                (Some(a), false) => format!("{} hasn't announced itself yet; CapraLink will use its saved address {a}", p.name),
                (None, false) => format!("{} hasn't been seen on the network", p.name),
            };
            v.push(check(seen || p.addr.is_some(), title, Some("If it's on, check it's on the same network, or use Edit address with its IP. On Windows, set the network to Private.".into())));
        }
        v
    }

    /// A support report: version, system, settings, devices, connection, setup checks and the
    /// recent log. `redact` swaps names and IPv4 addresses for placeholders (see `redactions`).
    pub fn diagnostics(&self, redact: bool) -> String {
        let (ins, outs, checks) = (crate::input_devices(), crate::output_devices(), self.checks());
        let mut t = String::new();
        let st = self.st();
        let _ = writeln!(t, "CapraLink {} diagnostics, {}", crate::version(), crate::log::now());
        let _ = writeln!(t, "System: {} ({})", crate::os_version(), std::env::consts::ARCH);
        if !redact {
            let _ = writeln!(t, "This computer: {}", st.cfg.name);
        }
        let s = &st.cfg.settings;
        let dev = |d: &Option<String>, list: &[AudioDevice]| match d.as_deref() {
            None => "System default".to_string(),
            Some(crate::NO_DEVICE) => "None (off)".to_string(),
            Some(d) => crate::pick(list, d).map_or_else(|| format!("{} (not connected)", crate::missing_name(d)), |i| list[i].name.clone()),
        };
        let on = |b: bool| if b { "on" } else { "off" };
        let _ = writeln!(t, "\n== Settings ==\nSend from: {}\nPlay to: {}", dev(&s.input, &ins), dev(&s.output, &outs));
        let _ = writeln!(t, "Channels: {}\nBitrate: {} kbps", if s.channels == 2 { "Stereo" } else { "Mono" }, s.bitrate / 1000);
        let _ = writeln!(t, "Music Mode: {}{}\nRun in background: {}\nRemote configuration: {}\nReconnect automatically: {}", on(s.music_mode), if s.hifi { " (Hi-Fi)" } else { "" }, on(s.service), on(s.remote_config), on(s.auto_reconnect));
        let key = s.ptt_key.as_ref().map_or(String::new(), |k| format!(" ({})", k.label));
        let _ = writeln!(t, "Send volume: {}%{}\nReceive volume: {}%\nPush-to-talk: {:?}{key}", s.send_volume, if s.mute { ", muted" } else { "" }, s.recv_volume, s.ptt);
        if let Some(e) = &st.ptt.error {
            let _ = writeln!(t, "Push-to-talk problem: {e}");
        }
        let _ = writeln!(t, "\n== Audio devices ==\nSend from: {}\nPlay to: {}", names(&ins).join(" | "), names(&outs).join(" | "));
        let _ = writeln!(t, "\n== Paired devices ==");
        let conn = st.session.as_ref().map(|s| s.peer_id.as_str());
        for p in &st.cfg.peers {
            let online = if st.found.contains_key(&p.id) || conn == Some(&p.id) { "online" } else { "offline" };
            let _ = writeln!(t, "{} [{}] last address {}, {online}{}", p.name, short(&p.id), p.addr.as_deref().unwrap_or("none"), if conn == Some(&p.id) { ", connected" } else { "" });
        }
        let _ = writeln!(t, "\n== Connection ==");
        match st.session.as_ref().and_then(|s| Some((s, s.link.as_ref()?.peek()))) {
            Some((s, x)) => drop(writeln!(t, "{x:?}\n{:?}", quality(s, &x))),
            None => t.push_str("Not streaming\n"),
        }
        let _ = writeln!(t, "\n== Setup checks ==");
        for c in &checks {
            let _ = writeln!(t, "[{}] {}{}", if c.ok { "ok" } else { "!!" }, c.title, c.fix.as_ref().map_or(String::new(), |f| format!(" — {f}")));
        }
        let _ = writeln!(t, "\n== Log ==\n{}", log_tail(&self.0.dir, 2000));
        if !redact {
            return t;
        }
        let devices: Vec<String> = st.cfg.peers.iter().map(|p| p.name.clone()).chain(st.found.values().map(|f| f.name.clone())).collect();
        let saved = st.cfg.peers.iter().filter_map(|p| p.audio.as_ref()).flat_map(|a| [a.input.clone(), a.output.clone()]);
        // names and ids (an id can hold a serial number), each its own "Audio device N"
        let listed = ins.into_iter().chain(outs).flat_map(|d| [d.name, d.id]);
        let audio: Vec<String> = listed.chain([st.cfg.settings.input.clone(), st.cfg.settings.output.clone()].into_iter().chain(saved).flatten()).collect();
        redact_text(&t, &redactions(&st.cfg.name, &devices, &audio))
    }

    /// Paired device `id`'s diagnostics (it must allow remote configuration).
    pub fn remote_diagnostics(&self, id: &str, redact: bool) -> Result<String> {
        let reply = self.manage(id, &Msg::Diagnostics { redact }).map_err(|e| match e.to_string().contains("unsupported request") {
            true => anyhow!("it runs an older version of CapraLink: update it to include its log"),
            false => e,
        });
        match reply? {
            Msg::Text { text, .. } => Ok(text),
            _ => bail!("unexpected reply"),
        }
    }

    /// Addresses to try for a device: mDNS-discovered ones first, then its remembered address
    /// (deduped), so a manual pairing/connect still works once mDNS has found it too. If mDNS
    /// hasn't found it, falls back to the remembered address alone. A failed attempt asks mDNS
    /// to re-check the discovered entry, if any.
    fn addrs(&self, id: &str) -> Result<(Vec<SocketAddr>, String)> {
        let st = self.st();
        let stored = st.cfg.peers.iter().find(|p| p.id == id).and_then(|p| p.addr.as_deref()).and_then(|a| a.parse().ok());
        match st.found.get(id) {
            Some(f) => {
                let mut addrs = f.addrs.clone();
                if let Some(a) = stored {
                    if !addrs.contains(&a) {
                        addrs.push(a);
                    }
                }
                Ok((addrs, f.fullname.clone()))
            }
            None => match stored {
                Some(a) => Ok((vec![a], String::new())),
                None => Err(anyhow!("that device is offline")),
            },
        }
    }

    fn recheck<T>(&self, fullname: String, r: Result<T>) -> Result<T> {
        if r.is_err() {
            if let Some(d) = &self.0.mdns {
                let _ = d.verify(fullname, IO_TIMEOUT);
            }
        }
        r
    }

    pub fn pair(&self, id: &str, pin: &str) -> Result<()> {
        let (addrs, fullname) = self.addrs(id)?;
        let r = self.pair_addr(&addrs, pin).map(drop);
        self.recheck(fullname, r)
    }

    /// Pairs with the node at the first of `addrs` that answers, using its PIN; returns the peer's device id.
    pub fn pair_addr(&self, addrs: &[SocketAddr], pin: &str) -> Result<String> {
        let r = self.pair_at(addrs, pin);
        if let Err(e) = &r {
            log(&format!("pairing failed: {e:#}"));
        }
        r
    }

    fn pair_at(&self, addrs: &[SocketAddr], pin: &str) -> Result<String> {
        let pin = pin.trim();
        ensure!(pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit()), "the PIN is 6 digits");
        let (my_id, my_name) = {
            let st = self.st();
            (st.cfg.device_id.clone(), st.cfg.name.clone())
        };
        let mut s = dial(addrs)?;
        let _deadline = deadline(&s, IO_TIMEOUT)?;
        let addr = s.peer_addr()?.to_string();
        send_msg(&mut s, &Msg::Pair { id: my_id.clone(), name: my_name, port: Some(self.0.port) })?;
        let (id, name) = match recv_msg(&mut s)? {
            Msg::Hello { id, name } => (id, name),
            Msg::Error { message } => bail!("{message}"), // pairing isn't open / is locked there
            _ => bail!("unexpected reply"),
        };
        check_id(&id)?;
        let (secret, _) = pake(&mut s, pin, true, &my_id, &id).map_err(|_| anyhow!("pairing failed — check the PIN"))?;
        let mut st = self.st();
        add_peer(&mut st.cfg, &id, &name, &secret, Some(addr));
        save(&self.0.dir, &st.cfg)?;
        st.error = None;
        log(&format!("paired with {name} [{}]", short(&id)));
        Ok(id)
    }

    /// Pairs directly by address, bypassing mDNS discovery (for when multicast is blocked on
    /// the LAN). `addr` is "ip", "ip:port" (IPv4 literal or hostname) or blank port meaning
    /// the default. Returns the paired peer's name.
    pub fn pair_ip(&self, addr: &str, pin: &str) -> Result<String> {
        let id = self.pair_addr(&resolve(addr)?, pin)?;
        let st = self.st();
        Ok(st.cfg.peers.iter().find(|p| p.id == id).map_or(id, |p| p.name.clone()))
    }

    /// Lets other computers pair with this one using its PIN for the next `PAIR_WINDOW`.
    pub fn open_pairing(&self) -> Result<()> {
        let mut st = self.st();
        let locked = secs_left(st.locked_until);
        ensure!(locked == 0, "pairing is locked for {locked} more seconds after a wrong PIN");
        st.pairing_until = Some(Instant::now() + PAIR_WINDOW);
        log("pairing window opened");
        Ok(())
    }

    pub fn connect(&self, id: &str) -> Result<()> {
        let (addrs, fullname) = self.addrs(id)?;
        let r = self.connect_to(id, &addrs);
        self.recheck(fullname, r)
    }

    /// Starts a session with paired device `id` at the first of `addrs` that answers,
    /// replacing any current one (and any reconnect attempts).
    pub fn connect_to(&self, id: &str, addrs: &[SocketAddr]) -> Result<()> {
        let gen = self.cancel_retry(&mut self.st());
        self.end_session(); // one link at a time; also frees our UDP port
        self.dial_link(id, addrs, gen)
    }

    /// Dials a session; `gen` = the `St::retry` value it belongs to (a later cancel voids it).
    /// One at a time, so a cancelled dial finishes (or gives up) before a newer one starts.
    fn dial_link(&self, id: &str, addrs: &[SocketAddr], gen: u64) -> Result<()> {
        let _one = self.0.dialing.lock().unwrap_or_else(|e| e.into_inner());
        let (my_id, secret, channels) = {
            let st = self.st();
            (st.cfg.device_id.clone(), secret(&st.cfg, id).ok_or_else(|| anyhow!("not paired with that device"))?, st.cfg.settings.channels)
        };
        let (ctl, keys) = open(addrs, &my_id, &secret)?;
        let (s, addr) = (&ctl.stream, ctl.stream.peer_addr()?);
        #[cfg(test)]
        self.pause("before link");
        // cancelled meanwhile: don't ask the peer to start (it would replace its current session)
        if self.st().retry != gen {
            ctl.close();
            bail!("cancelled");
        }
        ctl.send(&Msg::Link { channels, port: self.0.port })?;
        s.set_read_timeout(Some(LINK_TIMEOUT))?;
        match ctl.recv()? {
            Some(Msg::Ok) => {}
            Some(Msg::Error { message }) => bail!("other computer: {message}"),
            _ => bail!("unexpected reply"),
        }
        self.activate(id, &secret, addr, &keys, ctl.clone(), (gen, true))?;
        self.send_mode(&ctl);
        Ok(())
    }

    /// Ends the session by choice: no auto-reconnect, here or (via `stop`) on the peer.
    pub fn disconnect(&self) {
        let old = {
            let mut st = self.st();
            self.cancel_retry(&mut st);
            if st.cfg.last_peer.take().is_some() {
                let _ = save(&self.0.dir, &st.cfg);
            }
            st.session.take()
        };
        if let Some(s) = old {
            s.ctl.close();
        }
    }

    fn end_session(&self) {
        let old = self.st().session.take();
        if let Some(s) = old {
            s.ctl.close();
        }
    }

    /// Stops the retry loop (if any); returns the new generation.
    fn cancel_retry(&self, st: &mut St) -> u64 {
        st.retry += 1;
        st.retrying = None;
        self.0.wake.notify_all();
        st.retry
    }

    fn start_retry(&self, st: &mut St, id: String) {
        let gen = self.cancel_retry(st);
        st.retrying = Some(id.clone());
        let n = self.clone();
        let _ = std::thread::Builder::new().name("capralink-retry".into()).spawn(move || n.retry_loop(gen, &id));
    }

    /// Redials `id` with backoff until connected or cancelled (MASTER.md §3.9). Each attempt
    /// uses the current `addrs`; failures only show as `Device::reconnecting`.
    fn retry_loop(&self, gen: u64, id: &str) {
        let mut wait = RETRY_FIRST;
        loop {
            {
                let st = self.st();
                let (mut st, _) = self.0.wake.wait_timeout_while(st, wait, |st| st.retry == gen).unwrap_or_else(|e| e.into_inner());
                if st.retry != gen {
                    return;
                }
                if st.session.is_some() || !st.cfg.settings.auto_reconnect {
                    st.retrying = None;
                    return;
                }
            }
            let r = self.addrs(id).and_then(|(addrs, fullname)| {
                let r = self.dial_link(id, &addrs, gen);
                self.recheck(fullname, r)
            });
            if r.is_ok() {
                return;
            }
            wait = (wait * 2).min(RETRY_MAX);
        }
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        let ours = {
            let st = self.st();
            st.session.as_ref().is_some_and(|s| s.peer_id == id) || st.cfg.last_peer.as_deref() == Some(id)
        };
        if ours {
            self.disconnect();
        }
        let mut st = self.st();
        st.cfg.peers.retain(|p| p.id != id);
        if st.cfg.current.as_deref() == Some(id) {
            st.cfg.current = None;
        }
        save(&self.0.dir, &st.cfg)
    }

    /// Saves the settings; a running link reconnects (fresh keys) to apply device, bitrate and
    /// channel changes. A change to `service` installs/removes the login agent first. Music Mode,
    /// volume, mute and push-to-talk apply live (the peer is told about Music Mode), no reconnect.
    pub fn set_settings(&self, s: Settings) -> Result<()> {
        self.save_settings(|_| Ok(s.clone()), None)
    }

    /// `set_settings` for only the fields in `patch` (`Settings` fields as JSON), merged onto the
    /// current settings under the lock that writes, so a change made meanwhile to another field
    /// (e.g. by the other computer) isn't undone.
    pub fn patch_settings(&self, patch: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        self.save_settings(|cur| merge(cur, patch), None)
    }

    /// `set_settings`, or with `from`, paired device `from`'s remote save: refused if remote
    /// configuration is off, `service` and `remote_config` stay as they are, and if `from` isn't
    /// the current connection only its saved audio changes. Decided under the lock that writes,
    /// so a local change made meanwhile isn't undone.
    /// `new` makes the settings to save from the current ones (for a remote save from a peer
    /// that isn't the current connection: with that peer's saved audio).
    fn save_settings(&self, new: impl Fn(&Settings) -> Result<Settings>, from: Option<&str>) -> Result<()> {
        let service = {
            let cur = &self.st().cfg.settings;
            (cur.service, new(cur)?.service)
        };
        if from.is_none() && service.0 != service.1 {
            crate::rpc::login_agent(service.1).context("background service")?;
        }
        let (running, notify) = {
            let mut st = self.st();
            let old = st.cfg.settings.clone();
            let other = from.filter(|id| st.cfg.current.as_deref() != Some(*id));
            let saved = other.and_then(|id| st.cfg.peers.iter().find(|p| p.id == id)?.audio.clone());
            let mut s = new(&saved.map_or_else(|| old.clone(), |a| a.apply(old.clone())))?;
            check(&s)?;
            if from.is_some() {
                ensure!(old.remote_config, "remote configuration is off");
                (s.service, s.remote_config) = (old.service, old.remote_config);
            }
            if let Some(id) = other {
                if let Some(p) = st.cfg.peers.iter_mut().find(|p| p.id == id) {
                    p.audio = Some(Audio::of(&s));
                }
                s = Audio::of(&old).apply(s);
            }
            let mode = (s.music_mode, s.hifi);
            let audio_changed = restarts(&old, &s);
            if !s.auto_reconnect && st.retrying.is_some() {
                self.cancel_retry(&mut st);
            }
            let audio = Audio::of(&s);
            let cur = st.cfg.current.clone();
            if let Some(p) = st.cfg.peers.iter_mut().find(|p| Some(&p.id) == cur.as_ref()) {
                p.audio = Some(audio);
            }
            st.cfg.settings = s;
            save(&self.0.dir, &st.cfg)?;
            self.sync_ptt(&mut st);
            apply_live(&st);
            let sess = st.session.as_ref();
            (sess.filter(|_| audio_changed).map(|s| (s.peer_id.clone(), s.addr)), sess.filter(|_| !audio_changed && (old.music_mode, old.hifi) != mode).map(|s| (s.ctl.clone(), mode)))
        };
        if let Some((ctl, (music, hifi))) = notify {
            let _ = ctl.send(&Msg::Mode { music, name: None, hifi, hifi_ok: true });
        }
        match running {
            Some((id, addr)) => self.connect_to(&id, &[addr]),
            None => Ok(()),
        }
    }

    /// Reads paired device `id`'s settings and device lists (it must allow remote configuration).
    /// Refreshes the stored peer name if it has changed on the other end.
    pub fn remote_get(&self, id: &str) -> Result<RemoteConfig> {
        let c = match self.manage(id, &Msg::GetConfig)? {
            Msg::Config(c) => c,
            _ => bail!("unexpected reply"),
        };
        let mut st = self.st();
        if let Some(p) = st.cfg.peers.iter_mut().find(|p| p.id == id) {
            if p.name != c.name {
                p.name = c.name.clone();
                save(&self.0.dir, &st.cfg)?;
            }
        }
        Ok(c)
    }

    /// Changes paired device `id`'s settings, except its `service` and `remote_config`; `name`
    /// renames it too, when given (MASTER.md §3.6 device rename).
    pub fn remote_set(&self, id: &str, settings: Settings, name: Option<String>) -> Result<()> {
        let serde_json::Value::Object(settings) = serde_json::to_value(settings)? else { bail!("settings aren't an object") };
        match self.manage(id, &Msg::SetSettings { settings, name })? {
            Msg::Ok => Ok(()),
            _ => bail!("unexpected reply"),
        }
    }

    /// Renames this computer; re-advertises immediately, and tells the connected peer (paired
    /// peers take the name from the session or `remote_get`; MASTER.md §3.6 device rename).
    pub fn set_name(&self, name: &str) -> Result<()> {
        let name = clean_name(name.trim());
        ensure!(!name.is_empty(), "the name can't be empty");
        let mut st = self.st();
        st.cfg.name = name.clone();
        save(&self.0.dir, &st.cfg)?;
        let (id, ctl) = (st.cfg.device_id.clone(), st.session.as_ref().map(|s| s.ctl.clone()));
        drop(st);
        if let Some(ctl) = ctl {
            self.send_mode(&ctl); // the connected peer takes the new name from here, not from mDNS
        }
        if let Some(d) = &self.0.mdns {
            advertise(d, &id, &name, self.0.port)?;
        }
        Ok(())
    }

    /// Sets the address to reach paired device `id` at ("ip", "ip:port" or hostname, as in
    /// `pair_ip`). No PIN: the session handshake still checks the pairing secret.
    pub fn set_peer_addr(&self, id: &str, addr: &str) -> Result<()> {
        let a = resolve(addr)?[0].to_string();
        let mut st = self.st();
        let p = st.cfg.peers.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow!("not paired with that device"))?;
        p.addr = Some(a);
        save(&self.0.dir, &st.cfg)
    }

    /// Waits (up to 10 s) for the next key or button press and returns it, for the push-to-talk
    /// "Set button" (the window then saves it with `patch_settings`).
    pub fn ptt_capture(&self) -> Result<PttKey> {
        let (tx, rx) = mpsc::channel();
        {
            let mut st = self.st();
            ensure!(st.ptt.capture.is_none(), "already waiting for a button");
            st.ptt.capture = Some(tx);
            self.sync_ptt(&mut st);
            if st.ptt.listener.is_none() {
                st.ptt.capture = None;
                bail!("{}", st.ptt.error.clone().unwrap_or_default());
            }
        }
        let r = rx.recv_timeout(CAPTURE);
        let mut st = self.st();
        st.ptt.capture = None;
        self.sync_ptt(&mut st);
        match r {
            Ok(r) => r.map_err(|e| anyhow!("{e}")),
            Err(_) => Err(match &st.ptt.error {
                Some(e) => anyhow!("{e}"),
                None => anyhow!("no key or button was pressed"),
            }),
        }
    }

    /// Runs the key listener while push-to-talk has a key (or one is being captured), and
    /// starts the talk state over when the mode or key changes.
    fn sync_ptt(&self, st: &mut St) {
        let s = &st.cfg.settings;
        let of = (s.ptt, s.ptt_key.as_ref().map(|k| k.id.clone()));
        if st.ptt.of != of {
            st.ptt.talk = Talk::new(of.0);
            st.ptt.of = of;
        }
        let want = (st.ptt.of.0 != PttMode::Off && st.ptt.of.1.is_some()) || st.ptt.capture.is_some();
        if !want {
            (st.ptt.listener, st.ptt.error) = (None, None);
        } else if st.ptt.listener.is_none() {
            let (tx, rx) = mpsc::channel();
            let n = self.clone();
            let r = ptt::listen(tx).and_then(|l| {
                std::thread::Builder::new().name("capralink-ptt".into()).spawn(move || n.ptt_loop(rx))?;
                Ok(l)
            });
            match r {
                Ok(l) => st.ptt.listener = Some(l),
                Err(e) => st.ptt.error = Some(format!("Push-to-talk can't start: {e}")),
            }
        }
    }

    /// Follows the listener's keys until it stops: a capture takes the next press; the
    /// push-to-talk key drives `Talk`, whose changes reach the link and play the chirp.
    fn ptt_loop(&self, rx: mpsc::Receiver<Ev>) {
        loop {
            let wait = self.st().ptt.talk.deadline().map(|d| d.saturating_duration_since(Instant::now()));
            let ev = match wait {
                Some(w) => rx.recv_timeout(w),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            let mut st = self.st();
            let was = st.ptt.talk.on();
            let gone = matches!(ev, Err(RecvTimeoutError::Disconnected));
            let lost = gone || matches!(ev, Ok(Ev::Lost | Ev::Status(Some(_))));
            match ev {
                Ok(Ev::Status(e)) => {
                    if let Some((e, c)) = e.clone().zip(st.ptt.capture.take()) {
                        let _ = c.send(Err(e));
                    }
                    st.ptt.error = e;
                }
                Ok(Ev::Lost) | Err(RecvTimeoutError::Disconnected) => {}
                Ok(Ev::Key(k, true)) if st.ptt.capture.is_some() => {
                    let _ = st.ptt.capture.take().map(|c| c.send(Ok(k)));
                }
                Ok(Ev::Key(k, down)) if st.ptt.of.1.as_deref() == Some(k.id.as_str()) => match down {
                    true => st.ptt.talk.press(),
                    false => st.ptt.talk.release(Instant::now()),
                },
                Ok(Ev::Key(..)) | Err(RecvTimeoutError::Timeout) => {}
            }
            if lost {
                st.ptt.talk.reset(); // fail closed: a key release may never arrive
            }
            st.ptt.talk.tick(Instant::now());
            if st.ptt.talk.on() != was {
                log(if was { "push-to-talk: stopped talking" } else { "push-to-talk: talking" });
                // input lost: mute at once, even if that cuts the stop chirp
                if was && chirp(&st, false) && !lost {
                    // the stop chirp reaches the user through the link: mute once it has been sent
                    let n = self.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs_f32(ptt::CHIRP_SECS + 0.25));
                        apply_live(&n.st());
                    });
                } else {
                    apply_live(&st);
                    if !was {
                        chirp(&st, true);
                    }
                }
            }
            if gone {
                return;
            }
        }
    }

    /// One request over its own management session (a separate connection; any audio session
    /// with that device is left alone).
    fn manage(&self, id: &str, req: &Msg) -> Result<Msg> {
        let (addrs, fullname) = self.addrs(id)?;
        let r = self.manage_at(id, &addrs, req);
        self.recheck(fullname, r)
    }

    fn manage_at(&self, id: &str, addrs: &[SocketAddr], req: &Msg) -> Result<Msg> {
        let (my_id, secret) = {
            let st = self.st();
            (st.cfg.device_id.clone(), secret(&st.cfg, id).ok_or_else(|| anyhow!("not paired with that device"))?)
        };
        let (ctl, _) = open(addrs, &my_id, &secret)?;
        ctl.send(&Msg::Manage)?;
        ctl.send(req)?;
        ctl.stream.set_read_timeout(Some(LINK_TIMEOUT + IO_TIMEOUT))?; // new audio settings may restart its link
        let mut text = String::new();
        let reply = loop {
            match ctl.recv() {
                Ok(Some(Msg::Text { text: t, more: true })) => text.push_str(&t),
                Ok(Some(Msg::Text { text: t, .. })) => break Ok(Some(Msg::Text { text: text + &t, more: false })),
                r => break r,
            }
        };
        ctl.close();
        match reply? {
            Some(Msg::Error { message }) => bail!("{message}"),
            Some(m) => Ok(m),
            None => bail!("unexpected reply"),
        }
    }

    /// Installs a new session: stops the old one, starts the Link, watches the control channel,
    /// remembers the peer's address. Refused if retry generation `gen` (taken when the `Link`
    /// went out or came in) was cancelled meanwhile, e.g. by Disconnect. `mine` = this node dialed
    /// it; it becomes `last_peer`. An incoming session clears `last_peer`: the side that dialed
    /// owns reconnecting.
    fn activate(&self, id: &str, secret: &[u8; 32], addr: SocketAddr, keys: &Keys, ctl: Arc<Ctl>, (gen, mine): (u64, bool)) -> Result<()> {
        let _one = self.0.starting.lock().unwrap_or_else(|e| e.into_inner());
        // the checks run again after the audio has started: either may change while it does
        let refused = |st: &St, gen: u64| -> Option<&'static str> {
            // cancelled (Disconnect, another connect) since this start began: for an own dial,
            // since its `Link` went out (the peer's copy gets `stop`; own dials are serialized,
            // so no newer one of ours reached the peer before it), for the peer's, since its
            // `Link` came in (also while it waited for another start)
            if st.retry != gen {
                return Some("cancelled");
            }
            // forgotten (or paired again with a new key) since this session's handshake
            (!still_paired(&st.cfg, id, secret)).then_some("pairing was removed or changed")
        };
        let mut settings = {
            let mut st = self.st();
            match refused(&st, gen) {
                Some("cancelled") => {
                    drop(st);
                    ctl.close();
                    bail!("cancelled");
                }
                Some(why) => bail!("{why}"), // the caller reports it to the peer, then the connection drops
                None => {}
            }
            if let Some(old) = st.session.take() {
                old.ctl.close(); // its Link drops here, freeing the UDP port before the new bind
            }
            use_peer(&mut st.cfg, id);
            st.cfg.settings.clone()
        };
        // Opening devices can wait on the OS (e.g. macOS asking for microphone permission), so never
        // under the state lock: the window, Quit and everything else keep working meanwhile.
        let (mut st, link) = loop {
            #[cfg(test)]
            self.pause("start audio");
            let link = start_link(&settings, self.0.port, addr, keys);
            let st = self.st();
            match (link, refused(&st, gen)) {
                (Ok(l), None) => {
                    if !restarts(&st.cfg.settings, &settings) {
                        break (st, l); // volume, mute, push-to-talk: `apply_live` below
                    }
                    // the audio settings changed while it started: start again with the new ones
                    settings = st.cfg.settings.clone();
                    drop(st);
                    drop(l); // frees the UDP port before the new bind
                }
                (Ok(_), Some(why)) => {
                    drop(st);
                    ctl.close();
                    bail!("{why}");
                }
                (Err(e), _) => {
                    log(&format!("can't start audio with {}: {e:#}", peer_name(&st.cfg, id)));
                    drop(st);
                    ctl.close();
                    return Err(e);
                }
            }
        };
        self.cancel_retry(&mut st);
        st.session = Some(Session { peer_id: id.to_string(), addr, link, ctl: ctl.clone(), peer_music: false, peer_hifi: (false, false), hifi_fallback: false, mine, recent: VecDeque::new(), rtt_ms: None, peer_ms: (None, None) });
        log(&format!("session started with {} at {addr}, {}", peer_name(&st.cfg, id), if mine { "dialed from here" } else { "dialed by the other computer" }));
        st.error = None;
        set_addr(&mut st.cfg, id, addr.to_string());
        st.cfg.last_peer = mine.then(|| id.to_string());
        if let Err(e) = save(&self.0.dir, &st.cfg) {
            st.error = Some(format!("{e:#}"));
        }
        self.sync_ptt(&mut st); // this peer's push-to-talk settings
        apply_live(&st);
        drop(st);
        let (n, id) = (self.clone(), id.to_string());
        std::thread::Builder::new().name("capralink-session".into()).spawn(move || n.serve(ctl, &id))?;
        Ok(())
    }

    /// Session control loop: exchanges 1 s reports (which double as keepalive), steers this
    /// node's own sender from the peer's reports, and tears the session down on stop / close /
    /// silence.
    fn serve(&self, ctl: Arc<Ctl>, peer: &str) {
        let _ = ctl.stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut rate = RateControl::new(ceiling(self.st().cfg.settings.bitrate, false));
        let mut fallback = Fallback::default();
        let mut last_counts = (0u64, 0u64, 0u64);
        let (mut last_rx, mut last_report, mut last_log) = (Instant::now(), Instant::now(), Instant::now());
        let mut rebuilding = false; // a reconnect for `Failure::Rebuild` is under way
        let clock = Instant::now(); // `Report::ts`
        let ms = || clock.elapsed().as_millis() as u64;
        let mut their_ts: Option<(u64, Instant)> = None; // the peer's last `ts`, to echo
        let end = loop {
            match ctl.recv() {
                Ok(Some(Msg::Stop)) => break End::Stop,
                Ok(Some(Msg::Report { received, lost, underruns, ts, echo, send_ms, recv_ms, .. })) => {
                    last_rx = Instant::now();
                    their_ts = ts.map(|t| (t, last_rx)).or(their_ts);
                    let ceil = {
                        let mut st = self.st();
                        // Hi-Fi wanted: the peer's report says whether this side's sending keeps up
                        let want = hifi(&st);
                        if !want {
                            fallback = Fallback::default();
                        }
                        let fallen = want && fallback.on_report(received, lost, underruns);
                        if let Some(s) = st.session.as_mut().filter(|s| Arc::ptr_eq(&s.ctl, &ctl)) {
                            s.rtt_ms = echo.map(|(t, waited)| ms().saturating_sub(t).saturating_sub(waited) as f32).or(s.rtt_ms);
                            s.peer_ms = (send_ms, recv_ms);
                            if std::mem::replace(&mut s.hifi_fallback, fallen) != fallen {
                                if want {
                                    log(if fallen { "Hi-Fi: the network can't keep up, falling back to Music Mode" } else { "Hi-Fi: trying again after a clean minute" });
                                }
                                apply_live(&st);
                            }
                        }
                        ceiling(st.cfg.settings.bitrate, music(&st))
                    };
                    rate.set_ceiling(ceil);
                    let (bitrate, loss_perc) = rate.on_report(received, lost, underruns);
                    self.apply_rate(&ctl, bitrate, loss_perc);
                }
                Ok(Some(Msg::Mode { music, name, hifi, hifi_ok })) => {
                    last_rx = Instant::now();
                    let mut st = self.st();
                    if let Some(s) = st.session.as_mut().filter(|s| Arc::ptr_eq(&s.ctl, &ctl)) {
                        (s.peer_music, s.peer_hifi) = (music, (hifi, hifi_ok));
                    }
                    apply_live(&st);
                    let name = name.map(|n| clean_name(n.trim())).filter(|n| !n.is_empty());
                    if let Some((p, n)) = st.cfg.peers.iter_mut().find(|p| p.id == peer).zip(name).filter(|(p, n)| p.name != *n) {
                        p.name = n;
                        let _ = save(&self.0.dir, &st.cfg);
                    }
                }
                Ok(_) => last_rx = Instant::now(),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if last_rx.elapsed() > DEAD {
                        break End::Dead;
                    }
                }
                Err(_) => break End::Lost,
            }
            if last_report.elapsed() >= REPORT {
                last_report = Instant::now();
                let failure = self.st().session.as_ref().filter(|s| Arc::ptr_eq(&s.ctl, &ctl)).and_then(|s| Some((s.peer_id.clone(), s.link.as_ref()?.failure()?)));
                let failure = match failure {
                    // the stream must be rebuilt: reconnect (fresh keys, so packet numbers can restart);
                    // the new session replaces this one, and this loop then ends quietly
                    Some((id, Failure::Rebuild(f))) => {
                        if !rebuilding {
                            rebuilding = true;
                            log(&format!("{f}: reconnecting to rebuild the audio"));
                            let n = self.clone();
                            std::thread::spawn(move || {
                                let gen = n.st().retry;
                                if let Err(e) = n.connect(&id) {
                                    let mut st = n.st();
                                    st.error = Some(format!("{f}, and reconnecting failed: {e:#}"));
                                    // a passing failure (e.g. the other computer busy): keep trying as usual,
                                    // unless something else happened meanwhile (Disconnect, another connect;
                                    // `connect` itself moved the generation on by one)
                                    if st.cfg.settings.auto_reconnect && st.session.is_none() && st.retry == gen + 1 {
                                        n.start_retry(&mut st, id);
                                    }
                                }
                            });
                        }
                        None
                    }
                    Some((_, Failure::End(f))) => Some(f),
                    None => None,
                };
                if let Some(f) = failure {
                    // the peer shouldn't keep redialing a link that can't play: `stop` with a FIN, then
                    // wait (≤ 2 s) for its close, so the full shutdown below can't reset `stop` away
                    ctl.close();
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_secs(2) && matches!(ctl.recv(), Ok(Some(_))) {}
                    break End::Device(f);
                }
                let m = match self.link_delta(&ctl, &mut last_counts) {
                    Some(((received, lost, underruns, jitter_ms), (send_ms, recv_ms))) => {
                        let echo = their_ts.take().map(|(t, at)| (t, at.elapsed().as_millis() as u64));
                        Msg::Report { received, lost, underruns, jitter_ms, ts: Some(ms()), echo, send_ms, recv_ms }
                    }
                    None => Msg::Ping, // no link (e.g. tests): keepalive only
                };
                if ctl.send(&m).is_err() {
                    break End::Lost;
                }
                if last_log.elapsed() >= QUALITY_LOG {
                    last_log = Instant::now();
                    self.log_quality(&ctl);
                }
            }
        };
        // no `stop`: after a loss the peer should treat it as a loss too (and may reconnect)
        let _ = ctl.stream.shutdown(Shutdown::Both);
        let mut st = self.st();
        let current = st.session.as_ref().is_some_and(|s| Arc::ptr_eq(&s.ctl, &ctl));
        let why = match &end {
            _ if !current => "ended on this computer",
            End::Stop => "stopped by the other computer",
            End::Dead => "no reply from the other computer for 15 s",
            End::Lost => "connection lost",
            End::Device(f) => f,
        };
        log(&format!("session with {} ended: {why}", peer_name(&st.cfg, peer)));
        if !current {
            return; // ended here (disconnect, replaced, shutdown): nothing to do
        }
        let Some(s) = st.session.take() else { return };
        match end {
            End::Stop | End::Device(_) => {
                if st.cfg.last_peer.take().is_some() {
                    let _ = save(&self.0.dir, &st.cfg);
                }
                if let End::Device(f) = end {
                    st.error = Some(format!("{f} — pick another device and connect again"));
                }
            }
            _ if s.mine && st.cfg.settings.auto_reconnect && st.cfg.last_peer.as_deref() == Some(&s.peer_id) => self.start_retry(&mut st, s.peer_id),
            End::Dead => st.error = Some(format!("lost connection to {}", peer_name(&st.cfg, &s.peer_id))),
            End::Lost => {}
        }
    }

    /// Tells the peer this side's Music Mode, Hi-Fi and name.
    fn send_mode(&self, ctl: &Ctl) {
        let m = {
            let st = self.st();
            Msg::Mode { music: st.cfg.settings.music_mode, name: Some(st.cfg.name.clone()), hifi: st.cfg.settings.hifi, hifi_ok: true }
        };
        let _ = ctl.send(&m);
    }

    /// This session's own receive-side counters, as deltas since `last` (updated in place), and
    /// its `Link::latency`. `None` if the session moved on (or has no Link, e.g. under test).
    /// Also keeps the last `QUALITY_WINDOW` deltas for `Quality`.
    #[allow(clippy::type_complexity)]
    fn link_delta(&self, ctl: &Arc<Ctl>, last: &mut (u64, u64, u64)) -> Option<((u64, u64, u64, f32), (Option<f32>, Option<f32>))> {
        let mut st = self.st();
        let s = st.session.as_mut().filter(|s| Arc::ptr_eq(&s.ctl, ctl))?;
        let latency = s.link.as_ref()?.latency();
        let (r, l, u, jitter_ms) = s.link.as_ref()?.report_counters();
        let delta = (r.saturating_sub(last.0), l.saturating_sub(last.1), u.saturating_sub(last.2));
        *last = (r, l, u);
        s.recent.push_back([delta.0, delta.1, delta.2]);
        if s.recent.len() > QUALITY_WINDOW {
            s.recent.pop_front();
        }
        Some(((delta.0, delta.1, delta.2, jitter_ms), latency))
    }

    /// One log line on how this session's audio is doing.
    fn log_quality(&self, ctl: &Arc<Ctl>) {
        let st = self.st();
        let Some((s, link)) = st.session.as_ref().filter(|s| Arc::ptr_eq(&s.ctl, ctl)).and_then(|s| Some((s, s.link.as_ref()?))) else { return };
        let (x, jitter_ms) = (link.peek(), link.report_counters().3);
        let q = quality(s, &x);
        log(&format!(
            "quality {}: loss {:.1}%, underruns {}, buffer {:.0}/{:.0} ms, jitter {jitter_ms:.0} ms, {} kbps, complexity {}, slowest capture callback {} µs, sends dropped {}",
            q.grade, q.loss_pct, q.underruns, x.buffer_ms, x.target_ms, x.bitrate / 1000, x.complexity, x.callback_max_us, x.send_dropped
        ));
    }

    /// Applies a new bitrate/loss% to this session's own sender, if it's still the current one.
    fn apply_rate(&self, ctl: &Arc<Ctl>, bitrate: i32, loss_perc: u8) {
        let st = self.st();
        if let Some(link) = st.session.as_ref().filter(|s| Arc::ptr_eq(&s.ctl, ctl)).and_then(|s| s.link.as_ref()) {
            link.set_rate(bitrate, loss_perc);
        }
    }

    /// One thread per connection. Until it authenticates (hello + handshake / PIN check) it holds
    /// one of `MAX_PENDING` slots, and a watchdog closes it at `AUTH_DEADLINE` so trickled bytes
    /// can't hold a slot; dropping `authed` stops the watchdog and frees the slot.
    fn accept(&self, l: TcpListener) {
        for s in l.incoming().flatten() {
            let Ok(ip) = s.peer_addr().map(|a| a.ip()) else { continue };
            {
                let mut p = self.pending();
                if p.len() >= MAX_PENDING || p.iter().filter(|&&a| a == ip).count() >= MAX_PENDING_PER_IP {
                    continue; // dropped
                }
                p.push(ip);
            }
            let Ok(stop) = deadline(&s, AUTH_DEADLINE) else {
                self.release(ip);
                continue;
            };
            let authed = Slot { node: self.clone(), ip, _stop: stop };
            let n = self.clone();
            let _ = std::thread::Builder::new().name("capralink-conn".into()).spawn(move || {
                if let Err(e) = setup(&s).map_err(Into::into).and_then(|_| n.incoming(s, authed)) {
                    n.st().error = Some(format!("incoming connection from {ip}: {e:#}"));
                }
            });
        }
    }

    fn pending(&self) -> MutexGuard<'_, Vec<IpAddr>> {
        self.0.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn release(&self, ip: IpAddr) {
        let mut p = self.pending();
        if let Some(i) = p.iter().position(|&a| a == ip) {
            p.swap_remove(i);
        }
    }

    fn incoming(&self, mut s: TcpStream, authed: Slot) -> Result<()> {
        match recv_msg(&mut s)? {
            Msg::Pair { id, name, port } => self.on_pair(s, &id, &name, port, authed),
            Msg::Session { id } => self.on_session(s, &id, authed),
            _ => bail!("unexpected hello"),
        }
    }

    /// Answers a PIN attempt only while the pairing window is open and not locked, one at a time.
    /// A wrong PIN rotates the PIN and locks pairing (`LOCK_FIRST`, doubling, up to `LOCK_MAX`).
    fn on_pair(&self, mut s: TcpStream, id: &str, name: &str, port: Option<u16>, authed: Slot) -> Result<()> {
        check_id(id)?;
        let name = &clean_name(name); // also keeps the log one line per entry
        let addr = match port {
            Some(p) => Some(SocketAddr::new(s.peer_addr()?.ip(), p).to_string()),
            None => None,
        };
        let one_at_a_time = self.0.pairing.try_lock(); // held until this attempt is counted
        let busy = matches!(one_at_a_time, Err(TryLockError::WouldBlock));
        let (my_id, my_name, pin, refused) = {
            let st = self.st();
            let (name, locked) = (&st.cfg.name, secs_left(st.locked_until));
            let refused = if busy {
                Some(format!("pairing on {name} is busy, try again in a moment"))
            } else if locked > 0 {
                Some(format!("pairing on {name} is locked for {locked} s after a wrong PIN"))
            } else if secs_left(st.pairing_until) == 0 {
                Some(format!("pairing isn't open on {name}: press Show next to its PIN"))
            } else {
                None
            };
            (st.cfg.device_id.clone(), name.clone(), st.pin.clone(), refused)
        };
        if let Some(message) = refused {
            log(&format!("pairing attempt from {name} refused: {message}"));
            drop((one_at_a_time, authed)); // free before the peer reads the refusal and retries
            send_msg(&mut s, &Msg::Error { message: message.clone() })?;
            bail!("{message}");
        }
        send_msg(&mut s, &Msg::Hello { id: my_id.clone(), name: my_name })?;
        let r = pake(&mut s, &pin, false, id, &my_id);
        let mut st = self.st();
        match r {
            Ok((secret, reply)) => {
                log(&format!("paired with {name} [{}] (it entered this computer's PIN)", short(id)));
                add_peer(&mut st.cfg, id, name, &secret, addr);
                (st.pin, st.failures, st.error, st.pairing_until, st.locked_until) = (new_pin(), 0, None, None, None);
                save(&self.0.dir, &st.cfg)?;
                // done here before the peer can finish: its next connection finds the pairing
                // stored, pairing free and this connection's slot released
                drop((st, one_at_a_time, authed));
                send(&mut s, &reply)
            }
            Err(e) => {
                st.failures += 1;
                let lock = LOCK_FIRST.saturating_mul(1 << (st.failures - 1).min(16)).min(LOCK_MAX);
                (st.pin, st.locked_until) = (new_pin(), Some(Instant::now() + lock));
                log(&format!("wrong PIN from {name}: pairing locked for {} s", lock.as_secs_f32().ceil()));
                Err(e.context("pairing failed (wrong PIN?)"))
            }
        }
    }

    fn on_session(&self, mut s: TcpStream, id: &str, authed: Slot) -> Result<()> {
        let secret = secret(&self.st().cfg, id).ok_or_else(|| anyhow!("unknown device"))?;
        let (ctl, keys) = handshake(&mut s, &secret, false)?;
        // the connection deadline (`authed` still alive) also covers the first request, so an
        // authenticated peer can't hold it open by trickling bytes
        let port = match ctl.recv()? {
            Some(Msg::Link { port, .. }) => port,
            Some(Msg::Manage) => return self.on_manage(&ctl, id, &secret, authed),
            Some(Msg::Stop) => return Ok(()), // its dial was cancelled after the handshake
            _ => bail!("expected link request"),
        };
        let gen = self.st().retry; // a Disconnect from now on cancels it, also while it waits to start
        drop(authed); // the usual per-read timeouts from here on
        let addr = SocketAddr::new(s.peer_addr()?.ip(), port);
        if let Err(e) = self.activate(id, &secret, addr, &keys, ctl.clone(), (gen, false)) {
            let _ = ctl.send(&Msg::Error { message: format!("{e:#}") });
            return Err(e);
        }
        ctl.send(&Msg::Ok)?;
        self.send_mode(&ctl); // after Ok: an old initiator expects Ok first
        Ok(())
    }

    /// Answers one remote-configuration request from peer `id`. Its audio settings are this
    /// computer's settings for sessions with `id` (MASTER.md §3.10); the current session restarts
    /// only if it is with `id`.
    fn on_manage(&self, ctl: &Ctl, id: &str, secret: &[u8; 32], authed: Slot) -> Result<()> {
        let req = ctl.recv()?; // read before replying, so closing can't reset the reply away
        drop(authed);
        if !still_paired(&self.st().cfg, id, secret) {
            let r = ctl.send(&Msg::Error { message: "pairing was removed or changed".into() });
            ctl.close();
            return r;
        }
        let (name, local, theirs) = {
            let st = self.st();
            let theirs = st.cfg.peers.iter().find(|p| p.id == id).and_then(|p| p.audio.clone()).filter(|_| st.cfg.current.as_deref() != Some(id));
            (st.cfg.name.clone(), st.cfg.settings.clone(), theirs)
        };
        let peer = peer_name(&self.st().cfg, id);
        let remote_config = local.remote_config;
        let seen = match theirs { Some(a) => a.apply(local), None => local }; // what `GetConfig` shows `id`
        let reply = match req {
            _ if !remote_config => Msg::Error { message: format!("remote configuration is off on {name}") },
            Some(Msg::Diagnostics { redact }) => {
                log(&format!("sending diagnostics to {peer}"));
                Msg::Text { text: self.diagnostics(redact), more: false }
            }
            Some(Msg::GetConfig) => {
                let (ins, outs) = (crate::input_devices(), crate::output_devices());
                Msg::Config(RemoteConfig { name, settings: seen, inputs: names(&ins), outputs: names(&outs), input_devices: ins, output_devices: outs })
            }
            // `service` and `remote_config` only change locally
            Some(Msg::SetSettings { settings, name: new_name }) => {
                log(&format!("{peer} changed this computer's settings{}", if new_name.is_some() { " and name" } else { "" }));
                // validated before anything is renamed or stored for a peer that isn't connected
                let r = merge(&seen, &settings)
                    .and_then(|s| check(&s))
                    .and_then(|()| match new_name {
                        // checked again right before renaming: it may have been turned off since the request arrived
                        Some(_) if !self.st().cfg.settings.remote_config => Err(anyhow!("remote configuration is off")),
                        Some(n) => self.set_name(&n),
                        None => Ok(()),
                    })
                    .and_then(|()| {
                    #[cfg(test)]
                    self.pause("remote save");
                    self.save_settings(|cur| merge(cur, &settings), Some(id))
                });
                match r {
                    Ok(()) => Msg::Ok,
                    Err(e) => Msg::Error { message: format!("{name}: {e:#}") },
                }
            }
            _ => Msg::Error { message: "unsupported request".into() },
        };
        let r = match reply {
            Msg::Text { text, .. } => send_text(ctl, &text),
            m => ctl.send(&m),
        };
        ctl.close();
        r
    }

    fn on_mdns(&self, ev: ServiceEvent) {
        let mut st = self.st();
        match ev {
            ServiceEvent::ServiceResolved(info) => {
                let (Some(id), Some(name)) = (info.get_property_val_str("id"), info.get_property_val_str("name")) else { return };
                let addrs = order(info.get_addresses_v4().into_iter().map(|ip| SocketAddr::from((ip, info.get_port()))).collect());
                if addrs.is_empty() {
                    return;
                }
                if info.get_property_val_str("v") != Some("2") || id == st.cfg.device_id || check_id(id).is_err() {
                    return;
                }
                // anyone can announce any id and name: a paired device's name only changes through
                // its encrypted channel (`Mode`, `remote_get`), never from here
                let paired = st.cfg.peers.iter().any(|p| p.id == id);
                let found = Found { name: clean_name(name), addrs, fullname: info.get_fullname().to_string() };
                if st.found.insert(id.to_string(), found).is_none() && paired {
                    log(&format!("{} [{}] appeared on the network", peer_name(&st.cfg, id), short(id)));
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                let st = &mut *st;
                st.found.retain(|id, f| {
                    let gone = f.fullname == fullname;
                    if gone && st.cfg.peers.iter().any(|p| p.id == *id) {
                        log(&format!("{} [{}] left the network", peer_name(&st.cfg, id), short(id)));
                    }
                    !gone
                });
            }
            _ => {}
        }
    }
}

/// Effective Music Mode: on when either side has it on (MASTER.md §3.7).
fn music(st: &St) -> bool {
    st.cfg.settings.music_mode || st.session.as_ref().is_some_and(|s| s.peer_music)
}

/// Hi-Fi wanted for this link: in Music Mode, either side has it on and the peer can do it.
fn hifi(st: &St) -> bool {
    let peer = st.session.as_ref().map_or((false, false), |s| s.peer_hifi);
    music(st) && (st.cfg.settings.hifi || peer.0) && peer.1
}

/// Pushes the effective Music Mode and Hi-Fi, the volumes, mute and push-to-talk to the running Link.
fn apply_live(st: &St) {
    if let Some((sess, link)) = st.session.as_ref().and_then(|s| Some((s, s.link.as_ref()?))) {
        let s = &st.cfg.settings;
        link.set_mode(music(st), hifi(st) && !sess.hifi_fallback);
        link.set_volume(s.send_volume, s.recv_volume, s.mute || (s.ptt != PttMode::Off && !st.ptt.talk.on()));
    }
}

/// Plays the push-to-talk chirp: in the session's playback, else on the default output device.
/// True if that default device is CapraLink's own output, i.e. the chirp is heard through the link.
fn chirp(st: &St, start: bool) -> bool {
    if st.session.as_ref().and_then(|s| s.link.as_ref()).is_some_and(|l| l.chirp(start)) || cfg!(test) {
        return false;
    }
    let wave: fn(f32) -> f32 = if start { |t| ptt::chirp(true, t) } else { |t| ptt::chirp(false, t) };
    let _ = std::thread::Builder::new().name("capralink-chirp".into()).spawn(move || {
        if let Err(e) = crate::play(&None, ptt::CHIRP_SECS, wave) {
            log(&format!("push-to-talk sound: {e:#}"));
        }
    });
    crate::default_output_is_virtual()
}

/// One-way delay estimate, ms: the sender's part (capture + frame), half the round trip
/// (0 if unknown), the receiver's part (playout buffer + playback).
fn one_way(send_ms: f32, rtt_ms: Option<f32>, recv_ms: f32) -> u32 {
    (send_ms + rtt_ms.unwrap_or(0.0) / 2.0 + recv_ms).round().max(0.0) as u32
}

/// A session's audio quality over its last `QUALITY_WINDOW` reports, from its live stats.
fn quality(s: &Session, x: &Stats) -> Quality {
    let [r, l, u] = s.recent.iter().fold([0; 3], |a, d| [a[0] + d[0], a[1] + d[1], a[2] + d[2]]);
    let normal_ms = if x.music { MUSIC_TARGET } else { TARGET } as f32 * 1000.0 / RATE as f32;
    grade(r, l, u, x.target_ms - normal_ms)
}

/// Grades received/lost packets and underruns; `added_ms` = playout buffer above normal.
fn grade(received: u64, lost: u64, underruns: u64, added_ms: f32) -> Quality {
    let loss_pct = if received + lost == 0 { 0.0 } else { lost as f32 * 100.0 / (received + lost) as f32 };
    let grade = if loss_pct < 1.0 && underruns == 0 {
        "good"
    } else if loss_pct < 5.0 && underruns <= 2 {
        "fair"
    } else {
        "poor"
    };
    let hint = match grade {
        "good" => String::new(),
        _ if loss_pct >= 1.0 => format!("Wi-Fi is dropping packets ({loss_pct:.0}% in the last 30 s) — Ethernet or 5 GHz Wi-Fi helps"),
        _ => format!("Network delay spikes — CapraLink added {:.0} ms of buffer to cover them", added_ms.max(0.0)),
    };
    Quality { grade: grade.into(), hint, loss_pct, underruns }
}

/// A peer's name for messages and the log, else its id prefix.
fn peer_name(cfg: &Config, id: &str) -> String {
    cfg.peers.iter().find(|p| p.id == id).map_or_else(|| format!("[{}]", short(id)), |p| p.name.clone())
}

/// The id prefix the log and diagnostics show.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Sends a long text in pieces that fit a frame (`more` on all but the last).
fn send_text(ctl: &Ctl, text: &str) -> Result<()> {
    let mut rest = text;
    loop {
        let mut i = rest.len().min(TEXT_CHUNK);
        while !rest.is_char_boundary(i) {
            i -= 1;
        }
        ctl.send(&Msg::Text { text: rest[..i].into(), more: i < rest.len() })?;
        rest = &rest[i..];
        if rest.is_empty() {
            return Ok(());
        }
    }
}

/// The last `n` lines of the log (the rotated file, then the current one).
fn log_tail(dir: &Path, n: usize) -> String {
    let all: String = [crate::log::OLD, crate::log::FILE].iter().filter_map(|f| std::fs::read(dir.join(f)).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()).collect();
    let lines: Vec<&str> = all.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn names(list: &[AudioDevice]) -> Vec<String> {
    list.iter().map(|d| d.name.clone()).collect()
}

/// Placeholders for diagnostics: this computer, other computers ("Device A"...) and audio devices
/// ("Audio device 1"...), keeping CapraLink's own fixed names. Longest first, so a name inside
/// another ("Mic" in "USB Mic") can't split it.
// ponytail: plain substring match, so a very short name (say "A") also hides that text in log
// lines; match on word boundaries if that ever makes reports unreadable.
fn redactions(me: &str, devices: &[String], audio: &[String]) -> Vec<(String, String)> {
    let fixed = [VIRTUAL_INPUT, VIRTUAL_OUTPUT, EVERYTHING, NO_DEVICE, "System default"];
    let mut map = vec![(me.to_string(), "This computer".to_string())];
    let mut add = |names: &[String], label: &dyn Fn(usize) -> String| {
        let mut i = 0;
        for n in names {
            if !n.is_empty() && !fixed.contains(&n.as_str()) && !map.iter().any(|(k, _)| k == n) {
                map.push((n.clone(), label(i)));
                i += 1;
            }
        }
    };
    add(devices, &|i| if i < 26 { format!("Device {}", (b'A' + i as u8) as char) } else { format!("Device {}", i + 1) });
    add(audio, &|i| format!("Audio device {}", i + 1));
    map.retain(|(k, _)| !k.is_empty());
    map.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    map
}

/// Applies `redactions` in one pass, and replaces each IPv4 address with `<ip-N>` (the same
/// address always gets the same N).
fn redact_text(text: &str, map: &[(String, String)]) -> String {
    let (mut out, mut ips, mut rest, mut prev) = (String::new(), Vec::<&str>::new(), text, ' ');
    'next: while let Some(c) = rest.chars().next() {
        for (from, to) in map {
            if rest.starts_with(from.as_str()) {
                out.push_str(to);
                rest = &rest[from.len()..];
                prev = ' ';
                continue 'next;
            }
        }
        if c.is_ascii_digit() && !(prev.is_ascii_digit() || prev == '.') {
            let run = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
            let ip = rest[..run].trim_end_matches('.');
            if ip.parse::<Ipv4Addr>().is_ok() {
                let n = ips.iter().position(|a| *a == ip).unwrap_or_else(|| {
                    ips.push(ip);
                    ips.len() - 1
                });
                let _ = write!(out, "<ip-{}>", n + 1);
                (rest, prev) = (&rest[ip.len()..], '>');
                continue;
            }
        }
        out.push(c);
        (rest, prev) = (&rest[c.len_utf8()..], c);
    }
    out
}

/// In tests there are no audio devices: the session runs without a Link.
fn start_link(s: &Settings, port: u16, peer: SocketAddr, keys: &Keys) -> Result<Option<Link>> {
    if cfg!(test) {
        return Ok(None);
    }
    Link::start(s, port, peer, keys).map(Some)
}

/// (Re-)registers this node's mDNS service with the given name; mdns-sd re-announces in place,
/// no unregister needed (used at startup and by `set_name`).
fn advertise(d: &ServiceDaemon, id: &str, name: &str, port: u16) -> Result<()> {
    let props = [("id", id), ("name", name), ("v", "2")];
    let host = format!("{id}.local.");
    Ok(d.register(ServiceInfo::new(SERVICE, id, &host, "", port, &props[..])?.enable_addr_auto())?)
}

// ---------- config ----------

/// `None` = the OS config dir's CapraLink folder.
pub(crate) fn config_dir_or_default(dir: Option<PathBuf>) -> Result<PathBuf> {
    match dir {
        Some(d) => Ok(d),
        None => Ok(dirs::config_dir().ok_or_else(|| anyhow!("no config directory"))?.join("CapraLink")),
    }
}

fn load(dir: &Path) -> Result<Config> {
    let path = dir.join("config.json");
    match std::fs::read(&path) {
        Ok(b) => {
            let mut cfg: Config = serde_json::from_slice(&b).with_context(|| format!("invalid config file {}", path.display()))?;
            cfg.name = clean_name(&cfg.name); // saved before names were limited in bytes too
            // "CapraLink" is the fallback when no hostname could be read; retry so the real name shows up
            if cfg.name == "CapraLink" {
                cfg.name = hostname();
            }
            Ok(cfg)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let cfg = Config { device_id: hex(&random::<16>()), name: hostname(), settings: Settings::default(), peers: vec![], last_peer: None, current: None };
            save(dir, &cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save(dir: &Path, cfg: &Config) -> Result<()> {
    write_private(dir, "config.json", &serde_json::to_vec_pretty(cfg)?).context("save config")
}

/// Atomic write (temp file + rename); owner-only on Unix since it holds secrets.
pub(crate) fn write_private(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.tmp"));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

fn add_peer(cfg: &mut Config, id: &str, name: &str, secret: &[u8; 32], addr: Option<String>) {
    cfg.peers.retain(|p| p.id != id);
    cfg.peers.push(Peer { id: id.to_string(), name: clean_name(name), secret: hex(secret), addr, audio: None });
}

/// Makes `id` the current connection: its saved audio becomes the working settings, or, the
/// first time, the working settings become its saved audio.
fn use_peer(cfg: &mut Config, id: &str) {
    cfg.current = Some(id.to_string());
    if let Some(p) = cfg.peers.iter_mut().find(|p| p.id == id) {
        match &p.audio {
            Some(a) => cfg.settings = a.apply(cfg.settings.clone()),
            None => p.audio = Some(Audio::of(&cfg.settings)),
        }
    }
}

/// Updates the remembered address of an already-paired peer, if it's still paired.
fn set_addr(cfg: &mut Config, id: &str, addr: String) {
    if let Some(p) = cfg.peers.iter_mut().find(|p| p.id == id) {
        p.addr = Some(addr);
    }
}

/// Parses a user-typed "ip" or "ip:port" (IPv4 literal or hostname; the sockets are IPv4-only),
/// defaulting to the standard port when none is given.
fn resolve(addr: &str) -> Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    let addr = addr.trim();
    ensure!(!addr.is_empty(), "enter the other computer's IP address");
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        ensure!(sa.is_ipv4(), NO_IPV6);
        return Ok(vec![sa]);
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        ensure!(ip.is_ipv4(), NO_IPV6);
        return Ok(vec![SocketAddr::new(ip, 47800)]);
    }
    ensure!(!addr.starts_with('['), NO_IPV6);
    let with_port = if addr.contains(':') { addr.to_string() } else { format!("{addr}:47800") };
    let addrs: Vec<SocketAddr> = with_port.to_socket_addrs().with_context(|| format!("can't resolve {addr}"))?.filter(SocketAddr::is_ipv4).collect();
    ensure!(!addrs.is_empty(), "{addr} has no IPv4 address");
    Ok(addrs)
}

/// Whole seconds until `t`, rounded up (0 = passed, or none).
fn secs_left(t: Option<Instant>) -> u64 {
    t.map_or(0, |t| t.saturating_duration_since(Instant::now()).as_millis().div_ceil(1000) as u64)
}

/// `id` is still paired with the key a session's handshake used (not forgotten or re-paired since).
fn still_paired(cfg: &Config, id: &str, key: &[u8; 32]) -> bool {
    secret(cfg, id).is_some_and(|s| s == *key)
}

fn secret(cfg: &Config, id: &str) -> Option<[u8; 32]> {
    let s = &cfg.peers.iter().find(|p| p.id == id)?.secret;
    let mut out = [0u8; 32];
    (s.len() == 64).then_some(())?;
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn hostname() -> String {
    // Windows sets COMPUTERNAME; Linux has the kernel's name even without a `hostname` binary (SteamOS)
    let n = std::env::var("COMPUTERNAME").ok().or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok()).or_else(|| {
        let o = crate::system_command("hostname").output().ok()?;
        Some(String::from_utf8_lossy(&o.stdout).into_owned())
    });
    let n = n.unwrap_or_default();
    let n = n.trim().trim_end_matches(".local");
    if n.is_empty() { "CapraLink".into() } else { clean_name(n) }
}

// ---------- crypto + wire ----------

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random number generator");
    b
}

fn new_pin() -> String {
    format!("{:06}", u32::from_le_bytes(random()) % 1_000_000)
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn check_id(id: &str) -> Result<()> {
    ensure!(id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "bad device id");
    Ok(())
}

/// At most 64 characters and 250 bytes: mDNS refuses a TXT entry (`name=` + the name) over 255
/// bytes, and the engine can't start without advertising its name.
fn clean_name(n: &str) -> String {
    let mut out = String::new();
    for c in n.chars().filter(|c| !c.is_control()).take(64) {
        if out.len() + c.len_utf8() > 250 {
            break;
        }
        out.push(c);
    }
    out
}

fn hkdf(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 32];
    Hkdf::<Sha256>::new(None, ikm).expand(info, &mut k).expect("32 bytes is a valid HKDF length");
    k
}

fn confirm(k: &[u8], role: &[u8], init_id: &str, resp_id: &str) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(k).expect("HMAC takes any key length");
    for part in [b"confirm", role, init_id.as_bytes(), resp_id.as_bytes()] {
        m.update(part);
    }
    m
}

/// SPAKE2 on the responder's PIN + key confirmation; returns the pairing secret and, for the
/// responder, its confirmation, which it sends last (once it has stored the pairing).
/// The responder only confirms after checking the initiator's MAC, so a guesser learns nothing.
fn pake(s: &mut TcpStream, pin: &str, initiator: bool, init_id: &str, resp_id: &str) -> Result<([u8; 32], Vec<u8>)> {
    let (st, msg) = Spake2::<Ed25519Group>::start_symmetric(&Password::new(pin.as_bytes()), &Identity::new(b"capralink-pair-v1"));
    send(s, &msg)?;
    let k = st.finish(&recv(s)?).map_err(|_| anyhow!("bad SPAKE2 message"))?;
    let (i, r) = (confirm(&k, b"initiator", init_id, resp_id), confirm(&k, b"responder", init_id, resp_id));
    let reply = if initiator {
        send(s, &i.finalize().into_bytes())?;
        r.verify_slice(&recv(s)?).map_err(|_| anyhow!("key confirmation failed"))?;
        Vec::new()
    } else {
        i.verify_slice(&recv(s)?).map_err(|_| anyhow!("key confirmation failed"))?;
        r.finalize().into_bytes().to_vec()
    };
    Ok((hkdf(&k, b"capralink pairing secret"), reply))
}

/// Noise NNpsk0 over the open connection. Audio keys come from the raw split keys (which
/// include the ephemeral DH, so recorded audio stays safe if the pairing secret leaks later),
/// salted with the handshake hash.
fn handshake(s: &mut TcpStream, secret: &[u8; 32], initiator: bool) -> Result<(Arc<Ctl>, Keys)> {
    let b = snow::Builder::new(NOISE.parse()?).psk(0, secret)?;
    let mut hs = if initiator { b.build_initiator()? } else { b.build_responder()? };
    let mut buf = [0u8; 256];
    if initiator {
        let n = hs.write_message(&[], &mut buf)?;
        send(s, &buf[..n])?;
        hs.read_message(&recv(s)?, &mut buf)?;
    } else {
        hs.read_message(&recv(s)?, &mut buf)?;
        let n = hs.write_message(&[], &mut buf)?;
        send(s, &buf[..n])?;
    }
    let hh = hs.get_handshake_hash().to_vec();
    let (i2r, r2i) = hs.dangerously_get_raw_split();
    let key = |ikm: &[u8]| {
        let mut k = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&hh), ikm).expand(b"capralink audio v2", &mut k).expect("32 bytes is a valid HKDF length");
        k
    };
    let (i2r, r2i) = (key(&i2r), key(&r2i));
    let keys = if initiator { Keys { send: i2r, recv: r2i } } else { Keys { send: r2i, recv: i2r } };
    Ok((Arc::new(Ctl { stream: s.try_clone()?, noise: Mutex::new(hs.into_transport_mode()?), inbox: Mutex::default() }), keys))
}

/// A connection's pending slot (see `Node::accept`), freed as soon as its own thread drops it: a
/// reply sent after that can't race the peer's next connection into a full slot list.
struct Slot {
    node: Node,
    ip: IpAddr,
    _stop: mpsc::Sender<()>, // dropping it stops the watchdog
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.node.release(self.ip);
    }
}

/// The encrypted control channel of a session.
struct Ctl {
    stream: TcpStream,
    noise: Mutex<snow::TransportState>,
    inbox: Mutex<Vec<u8>>, // bytes read but not yet a whole frame (a read timeout keeps them)
}

impl Ctl {
    fn send(&self, m: &Msg) -> Result<()> {
        let pt = serde_json::to_vec(m)?;
        let mut ct = vec![0u8; pt.len() + 16];
        let mut noise = self.noise.lock().unwrap_or_else(|e| e.into_inner()); // held across the write to keep nonce order
        let n = noise.write_message(&pt, &mut ct)?;
        send(&mut &self.stream, &ct[..n])
    }

    /// `Ok(None)` = authentic but unknown message (newer peer). A timeout mid-frame loses nothing:
    /// the next call carries on with the same frame.
    fn recv(&self) -> io::Result<Option<Msg>> {
        let ct = {
            let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let len = inbox.get(..2).map_or(usize::MAX, |l| 2 + u16::from_be_bytes([l[0], l[1]]) as usize);
                if inbox.len() >= len {
                    break inbox.drain(..len).skip(2).collect::<Vec<u8>>();
                }
                let mut b = [0u8; 4096];
                match (&self.stream).read(&mut b) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => inbox.extend_from_slice(&b[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        };
        let mut pt = vec![0u8; ct.len()];
        let n = self.noise.lock().unwrap_or_else(|e| e.into_inner()).read_message(&ct, &mut pt).map_err(io::Error::other)?;
        Ok(serde_json::from_slice(&pt[..n]).ok())
    }

    fn close(&self) {
        let _ = self.send(&Msg::Stop);
        // FIN, not RST: on Windows, shutting down the read side with data still unread resets the
        // connection, and the peer can lose the `stop` (it would then redial). Our `serve` closes the rest.
        let _ = self.stream.shutdown(Shutdown::Write);
    }
}

/// Preference for advertised addresses: home/office LAN ranges first, virtual/odd ones later.
fn rank(ip: Ipv4Addr) -> u8 {
    match ip.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        [172, b, ..] if (16..32).contains(&b) => 2,
        _ if ip.is_loopback() || ip.is_link_local() => 4,
        _ => 3,
    }
}

/// Sorts by `rank`, dropping loopback/link-local unless nothing else is left.
fn order(mut addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let r = |a: &SocketAddr| match a.ip() {
        std::net::IpAddr::V4(ip) => rank(ip),
        _ => 4,
    };
    addrs.sort_by_key(|a| (r(a), *a));
    if addrs.iter().any(|a| r(a) < 4) {
        addrs.retain(|a| r(a) < 4);
    }
    addrs
}

/// Connects to the first address that answers.
fn dial(addrs: &[SocketAddr]) -> Result<TcpStream> {
    let mut err = anyhow!("no address for that device");
    for a in addrs {
        match TcpStream::connect_timeout(a, DIAL_TIMEOUT) {
            Ok(s) => {
                setup(&s)?;
                return Ok(s);
            }
            Err(e) => err = anyhow!(e).context(format!("can't reach {a}")),
        }
    }
    Err(err)
}

/// Opens a session's control channel with a paired device at the first of `addrs` that completes
/// the handshake: an address that answers but isn't that device (a stale or forged mDNS entry)
/// can't block the next one.
fn open(addrs: &[SocketAddr], my_id: &str, secret: &[u8; 32]) -> Result<(Arc<Ctl>, Keys)> {
    let mut err = anyhow!("no address for that device");
    for a in addrs {
        let r = dial(&[*a]).and_then(|mut s| {
            let _deadline = deadline(&s, IO_TIMEOUT)?;
            send_msg(&mut s, &Msg::Session { id: my_id.into() })?;
            handshake(&mut s, secret, true).context("secure connection failed (try pairing again)")
        });
        match r {
            Ok(c) => return Ok(c),
            Err(e) => err = e,
        }
    }
    Err(err)
}

/// Closes `s` after `d` unless the returned guard is dropped first: per-read timeouts alone
/// let a peer trickling bytes hold a handshake open for ever.
fn deadline(s: &TcpStream, d: Duration) -> io::Result<mpsc::Sender<()>> {
    let (stop, rx) = mpsc::channel::<()>();
    let s = s.try_clone()?;
    std::thread::Builder::new().name("capralink-deadline".into()).spawn(move || {
        if rx.recv_timeout(d) == Err(RecvTimeoutError::Timeout) {
            let _ = s.shutdown(Shutdown::Both);
        }
    })?;
    Ok(stop)
}

fn setup(s: &TcpStream) -> io::Result<()> {
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))
}

/// Frame: `u16 length | payload`.
fn send(s: &mut impl Write, b: &[u8]) -> Result<()> {
    let len = u16::try_from(b.len())?;
    s.write_all(&[&len.to_be_bytes()[..], b].concat())?;
    Ok(())
}

/// For the steps before a session (pairing, handshake), where any timeout ends the connection;
/// sessions read with `Ctl::recv`.
fn recv(s: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len)?;
    let mut b = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn send_msg(s: &mut TcpStream, m: &Msg) -> Result<()> {
    send(s, &serde_json::to_vec(m)?)
}

fn recv_msg(s: &mut TcpStream) -> Result<Msg> {
    Ok(serde_json::from_slice(&recv(s)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> (Node, PathBuf) {
        let dir = std::env::temp_dir().join(format!("capralink-test-{}", hex(&random::<8>())));
        (Node::start(Some(dir.clone()), 0, false).unwrap(), dir)
    }

    fn addr(n: &Node) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], n.port()))
    }

    fn peers(dir: &Path) -> Vec<Peer> {
        load(dir).unwrap().peers
    }

    fn wait(mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn pairing() {
        let ((a, adir), (b, bdir)) = (node(), node());
        let pin = b.state().pin;
        let err = a.pair_addr(&[addr(&b)], &pin).unwrap_err().to_string();
        assert!(err.contains("pairing isn't open on") && err.contains("press Show"), "{err}");
        assert_eq!(b.state().pin, pin, "a closed window costs no PIN");

        b.open_pairing().unwrap();
        assert!(b.state().pairing_secs > 0);
        let wrong = format!("{:06}", (pin.parse::<u32>().unwrap() + 1) % 1_000_000);
        assert!(a.pair_addr(&[addr(&b)], &wrong).is_err());
        assert!(peers(&adir).is_empty() && peers(&bdir).is_empty());
        let pin = b.state().pin;
        assert!(b.state().pairing_locked_secs > 0, "a wrong PIN locks pairing");
        assert!(b.open_pairing().unwrap_err().to_string().contains("locked"));
        let err = a.pair_addr(&[addr(&b)], &pin).unwrap_err().to_string();
        assert!(err.contains("is locked for"), "locked refuses even the right PIN: {err}");
        assert_eq!(b.state().pin, pin, "a refused attempt costs no PIN");
        wait(|| b.state().pairing_locked_secs == 0);

        b.open_pairing().unwrap();
        // pair by IP address (no mDNS involved either side)
        let bname = a.pair_ip(&format!("127.0.0.1:{}", b.port()), &pin).unwrap();
        wait(|| peers(&bdir).len() == 1); // the responder saves just after sending its confirmation
        let (pa, pb) = (peers(&adir), peers(&bdir));
        assert_eq!((pa.len(), pb.len()), (1, 1));
        let bid = pa[0].id.clone();
        assert_eq!(pa[0].name, bname);
        assert_eq!(pb[0].id, load(&adir).unwrap().device_id);
        assert_eq!(pa[0].secret, pb[0].secret);
        assert_eq!(pa[0].addr.as_deref(), Some(format!("127.0.0.1:{}", b.port())).as_deref());
        assert_eq!(pb[0].addr.as_deref(), Some(format!("127.0.0.1:{}", a.port())).as_deref(), "responder records the initiator's listening port");
        assert_ne!(b.state().pin, pin, "PIN rotates after pairing");
        let s = b.state();
        assert_eq!((s.pairing_secs, s.pairing_locked_secs), (0, 0), "success closes the window and resets the lock");

        // session: mDNS is off on both, so `connect` must fall back to the remembered address
        a.connect(&bid).unwrap();
        wait(|| b.state().devices.iter().any(|d| d.connected));
        assert!(a.state().devices.iter().any(|d| d.id == bid && d.connected && d.paired && d.reachable));
        a.disconnect();
        wait(|| !b.state().devices.iter().any(|d| d.connected));
        // after forgetting, the other side rejects the session
        b.forget(&pb[0].id).unwrap();
        assert!(a.connect_to(&bid, &[addr(&b)]).is_err());
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn remote_config() {
        let ((a, adir), (b, bdir)) = (node(), node());
        b.open_pairing().unwrap();
        a.pair_ip(&format!("127.0.0.1:{}", b.port()), &b.state().pin).unwrap();
        wait(|| peers(&bdir).len() == 1);
        let bid = peers(&adir)[0].id.clone();
        a.connect(&bid).unwrap();
        wait(|| b.state().devices.iter().any(|d| d.connected));
        let both = || a.state().devices.iter().any(|d| d.connected) && b.state().devices.iter().any(|d| d.connected);

        let err = a.remote_get(&bid).map(drop).unwrap_err().to_string();
        assert!(err.contains("remote configuration is off"), "{err}");
        b.set_settings(Settings { remote_config: true, ..b.state().settings }).unwrap();
        let rc = a.remote_get(&bid).unwrap();
        assert_eq!((rc.name, &rc.settings), (b.state().name, &b.state().settings));
        assert!(both(), "a manage session leaves the audio session alone");

        let want = Settings { bitrate: 32_000, channels: 2, service: true, remote_config: false, ..rc.settings };
        a.remote_set(&bid, want.clone(), None).unwrap();
        let s = load(&bdir).unwrap().settings;
        assert_eq!((s.bitrate, s.channels, s.service, s.remote_config), (32_000, 2, false, true));
        wait(both); // b restarts its link for the new audio settings

        // remote rename (service/remote_config in `want` are ignored, so b's remote_config is
        // still on): the target renames and tells the connected initiator over the session.
        a.remote_set(&bid, want.clone(), Some("Renamed B".into())).unwrap();
        assert_eq!(load(&bdir).unwrap().name, "Renamed B");
        wait(|| peers(&adir).iter().any(|p| p.id == bid && p.name == "Renamed B"));
        a.st().cfg.peers.iter_mut().find(|p| p.id == bid).unwrap().name = "Stale".into();
        let rc2 = a.remote_get(&bid).unwrap();
        assert_eq!(rc2.name, "Renamed B");
        assert_eq!(stored_name(&a, &bid).0, "Renamed B", "initiator's stored peer name updates after remote_get");

        let err = a.remote_set(&bid, want.clone(), Some("   ".into())).unwrap_err().to_string();
        assert!(err.contains("can't be empty"), "{err}");

        b.set_settings(Settings { remote_config: false, ..b.state().settings }).unwrap();
        assert!(a.remote_set(&bid, want, Some("Nope".into())).is_err(), "remote_config off refuses the rename too");
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn audio_settings_are_per_connection() {
        let ((a, adir), (b, bdir), (c, cdir)) = (node(), node(), node());
        b.open_pairing().unwrap();
        a.pair_ip(&format!("127.0.0.1:{}", b.port()), &b.state().pin).unwrap();
        c.open_pairing().unwrap();
        a.pair_ip(&format!("127.0.0.1:{}", c.port()), &c.state().pin).unwrap();
        wait(|| peers(&bdir).len() == 1 && peers(&cdir).len() == 1);
        let (bid, cid) = (load(&bdir).unwrap().device_id, load(&cdir).unwrap().device_id);
        let aid = load(&adir).unwrap().device_id;
        let set = |n: &Node, bitrate| n.set_settings(Settings { bitrate, ..n.state().settings }).unwrap();

        a.connect(&bid).unwrap();
        set(&a, 32_000); // saved for b
        a.connect(&cid).unwrap(); // c has nothing saved yet: it starts from the working settings
        assert_eq!(a.state().settings.bitrate, 32_000);
        set(&a, 48_000); // saved for c
        a.connect(&bid).unwrap();
        assert_eq!((a.state().settings.bitrate, a.state().current), (32_000, Some(bid.clone())));
        a.connect(&cid).unwrap();
        assert_eq!(a.state().settings.bitrate, 48_000);

        // b configures a while a talks to c: only a's settings for b change, the session stays
        a.set_settings(Settings { remote_config: true, ..a.state().settings }).unwrap();
        let ctl = a.st().session.as_ref().unwrap().ctl.clone();
        b.remote_set(&aid, Settings { bitrate: 16_000, ..b.state().settings }, None).unwrap();
        assert_eq!(a.state().settings.bitrate, 48_000);
        assert!(Arc::ptr_eq(&ctl, &a.st().session.as_ref().unwrap().ctl), "session with c not restarted");
        assert_eq!(b.remote_get(&aid).unwrap().settings.bitrate, 16_000);
        a.connect(&bid).unwrap();
        assert_eq!(a.state().settings.bitrate, 16_000);

        a.forget(&bid).unwrap();
        assert!(a.state().current.is_none());
        for d in [adir, bdir, cdir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn music_mode_is_link_wide_and_live() {
        let ((a, adir), (b, bdir)) = (node(), node());
        b.open_pairing().unwrap();
        a.pair_ip(&format!("127.0.0.1:{}", b.port()), &b.state().pin).unwrap();
        wait(|| peers(&bdir).len() == 1);
        let bid = peers(&adir)[0].id.clone();
        a.connect(&bid).unwrap();
        wait(|| b.state().devices.iter().any(|d| d.connected));
        let ctl = |n: &Node| n.st().session.as_ref().unwrap().ctl.clone();
        let peer_music = |n: &Node| n.st().session.as_ref().is_some_and(|s| s.peer_music);
        let (ca, cb) = (ctl(&a), ctl(&b));
        assert!(!peer_music(&a) && !peer_music(&b));

        b.set_settings(Settings { music_mode: true, ..b.state().settings }).unwrap();
        wait(|| peer_music(&a));
        assert!(music(&a.st()) && music(&b.st()), "either side on = both on");
        assert!(!peer_music(&b));
        a.set_settings(Settings { music_mode: true, ..a.state().settings }).unwrap();
        wait(|| peer_music(&b));
        b.set_settings(Settings { music_mode: false, ..b.state().settings }).unwrap();
        wait(|| !peer_music(&a));
        assert!(music(&b.st()), "a still has it on");
        assert!(Arc::ptr_eq(&ca, &ctl(&a)) && Arc::ptr_eq(&cb, &ctl(&b)), "same sessions: no reconnect");
        assert!(load(&adir).unwrap().settings.music_mode && !load(&bdir).unwrap().settings.music_mode);

        // Hi-Fi: either side wants it, both can (both told the other at session start), live
        let peer_hifi = |n: &Node| n.st().session.as_ref().map(|s| s.peer_hifi);
        assert_eq!((peer_hifi(&a), peer_hifi(&b)), (Some((false, true)), Some((false, true))));
        assert!(!hifi(&a.st()) && !hifi(&b.st()));
        b.patch_settings(serde_json::json!({ "hifi": true }).as_object().unwrap()).unwrap();
        wait(|| peer_hifi(&a) == Some((true, true)));
        assert!(hifi(&a.st()) && hifi(&b.st()), "either side on = both on");
        assert!(Arc::ptr_eq(&ca, &ctl(&a)) && Arc::ptr_eq(&cb, &ctl(&b)), "no reconnect");
        assert!(load(&bdir).unwrap().settings.hifi);
        a.set_settings(Settings { music_mode: false, ..a.state().settings }).unwrap();
        wait(|| !music(&b.st()));
        assert!(!hifi(&a.st()) && !hifi(&b.st()), "no effect without Music Mode");
        // the truth table, on a's side
        for (music_on, mine, wants, can, on) in [
            (true, true, false, true, true),
            (true, false, true, true, true),
            (true, false, false, true, false),
            (true, true, true, false, false), // an older peer can't
            (false, true, true, true, false),
        ] {
            let mut st = a.st();
            (st.cfg.settings.music_mode, st.cfg.settings.hifi) = (music_on, mine);
            st.session.as_mut().unwrap().peer_hifi = (wants, can);
            assert_eq!(hifi(&st), on, "music {music_on}, mine {mine}, peer wants {wants}, can {can}");
        }
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    /// Two nodes, paired, `a` connected to `b`; returns b's id.
    fn connected() -> ((Node, PathBuf), (Node, PathBuf), String) {
        let ((a, adir), (b, bdir)) = (node(), node());
        b.open_pairing().unwrap();
        a.pair_ip(&format!("127.0.0.1:{}", b.port()), &b.state().pin).unwrap();
        wait(|| peers(&bdir).len() == 1);
        let bid = peers(&adir)[0].id.clone();
        a.connect(&bid).unwrap();
        wait(|| is(&b, |d| d.connected));
        ((a, adir), (b, bdir), bid)
    }

    fn is(n: &Node, f: impl Fn(&Device) -> bool) -> bool {
        n.state().devices.iter().any(f)
    }

    /// The TCP connection drops with no `stop` (network loss, crash).
    fn lose(n: &Node) {
        let s = n.st().session.take();
        let _ = s.unwrap().ctl.stream.shutdown(Shutdown::Both);
    }

    /// Waits longer than a few backoff rounds, then checks nothing reconnected.
    fn stays_down(a: &Node, b: &Node) {
        std::thread::sleep(RETRY_MAX * 3);
        assert!(!is(a, |d| d.connected || d.reconnecting) && !is(b, |d| d.connected || d.reconnecting));
    }

    #[test]
    fn reconnects_after_loss_at_current_address() {
        let ((a, adir), (b, bdir), bid) = connected();
        assert_eq!(load(&adir).unwrap().last_peer.as_deref(), Some(bid.as_str()));
        assert_eq!(load(&bdir).unwrap().last_peer, None, "only the dialing side reconnects");

        // set_peer_addr: paired ids only, needs an address
        assert!(a.set_peer_addr("0".repeat(32).as_str(), "127.0.0.1:1").unwrap_err().to_string().contains("not paired"));
        assert!(a.set_peer_addr(&bid, "  ").unwrap_err().to_string().contains("IP address"));

        // b goes away (its stand-in refuses a from now on), then comes back on another port
        b.st().cfg.peers.clear();
        lose(&b);
        wait(|| is(&a, |d| d.id == bid && d.reconnecting && !d.connected));
        std::thread::sleep(RETRY_MAX * 2);
        assert!(is(&a, |d| d.reconnecting) && a.state().error.is_none(), "failed attempts don't set an error");
        let b2 = Node::start(Some(bdir.clone()), 0, false).unwrap();
        a.set_peer_addr(&bid, &format!(" localhost:{} ", b2.port())).unwrap();
        assert_eq!(peers(&adir)[0].addr, Some(format!("127.0.0.1:{}", b2.port())));
        wait(|| is(&a, |d| d.connected && !d.reconnecting) && is(&b2, |d| d.connected));
        assert!(!is(&b2, |d| d.reconnecting));
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn no_reconnect_when_ended_on_purpose() {
        let ((a, adir), (b, bdir), bid) = connected();
        a.disconnect();
        assert_eq!(load(&adir).unwrap().last_peer, None);
        stays_down(&a, &b);

        a.connect(&bid).unwrap();
        wait(|| is(&b, |d| d.connected));
        b.disconnect(); // the peer's `stop`
        wait(|| !is(&a, |d| d.connected));
        stays_down(&a, &b);
        assert_eq!(load(&adir).unwrap().last_peer, None);

        a.connect(&bid).unwrap();
        wait(|| is(&b, |d| d.connected));
        a.set_settings(Settings { auto_reconnect: false, ..a.state().settings }).unwrap();
        lose(&b);
        wait(|| !is(&a, |d| d.connected));
        stays_down(&a, &b);

        // the responder never redials, even with auto_reconnect on
        a.set_settings(Settings { auto_reconnect: true, ..a.state().settings }).unwrap();
        a.connect(&bid).unwrap();
        wait(|| is(&b, |d| d.connected));
        lose(&a);
        wait(|| !is(&b, |d| d.connected));
        stays_down(&a, &b);
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn reconnects_to_last_peer_at_start() {
        let ((a, adir), (b, bdir), _) = connected();
        a.shutdown();
        wait(|| !is(&b, |d| d.connected));
        assert!(load(&adir).unwrap().last_peer.is_some(), "shutdown keeps last_peer");
        let a2 = Node::start(Some(adir.clone()), 0, false).unwrap();
        wait(|| is(&a2, |d| d.connected) && is(&b, |d| d.connected));
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn remote_diagnostics() {
        let ((a, adir), (b, bdir)) = (node(), node());
        b.open_pairing().unwrap();
        a.pair_ip(&format!("127.0.0.1:{}", b.port()), &b.state().pin).unwrap();
        wait(|| peers(&bdir).len() == 1);
        let bid = peers(&adir)[0].id.clone();
        let checks = a.checks();
        assert!(checks[0].ok && checks[0].fix.is_none());
        let seen = checks.iter().find(|c| c.title.contains(&b.state().name)).unwrap();
        // no mDNS here, but pairing by IP saved b's address: not announced, still reachable
        assert!(seen.ok && seen.title.contains("hasn't announced itself yet") && seen.fix.is_none(), "{}", seen.title);

        let err = a.remote_diagnostics(&bid, true).unwrap_err().to_string();
        assert!(err.contains("remote configuration is off"), "{err}");
        b.set_settings(Settings { remote_config: true, ..b.state().settings }).unwrap();
        // a long log (multi-byte text) comes over in several pieces
        let log: String = (0..400).map(|i| format!("{i} {}\n", "é".repeat(60))).collect();
        std::fs::write(bdir.join(crate::log::OLD), format!("{log}last line from 10.1.2.3\n")).unwrap();
        let t = a.remote_diagnostics(&bid, true).unwrap();
        assert!(t.len() > 3 * TEXT_CHUNK && t.contains(&log) && t.contains("last line from <ip-"), "{}", &t[..500]);
        // (test nodes share the hostname, so log lines may say "This computer:" — only the header line counts)
        assert!(!t.lines().any(|l| l.starts_with("This computer:")) && !t.contains(&b.state().name), "redacted on b's side");
        assert!(a.remote_diagnostics(&bid, false).unwrap().contains(&format!("This computer: {}", b.state().name)));
        let mine = a.diagnostics(false);
        assert!(mine.contains("== Setup checks ==") && mine.contains(&format!("[{}]", &bid[..8])) && !mine.contains(&bid), "ids only as prefixes");
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn quality_grades() {
        let q = |r, l, u, added| {
            let q = grade(r, l, u, added);
            (q.grade, q.hint)
        };
        assert_eq!(q(0, 0, 0, 0.0), ("good".into(), String::new()));
        assert_eq!(q(3000, 20, 0, 5.0).0, "good");
        assert_eq!(q(3000, 60, 0, 0.0), ("fair".into(), "Wi-Fi is dropping packets (2% in the last 30 s) — Ethernet or 5 GHz Wi-Fi helps".into()));
        assert_eq!(q(3000, 0, 2, 30.4), ("fair".into(), "Network delay spikes — CapraLink added 30 ms of buffer to cover them".into()));
        assert_eq!(q(3000, 0, 3, 40.0).0, "poor");
        assert_eq!(q(1000, 100, 0, 0.0).0, "poor");
        assert_eq!(grade(900, 100, 1, 0.0).loss_pct, 10.0);
    }

    #[test]
    fn redaction() {
        let map = redactions(
            "Brian's Mac",
            &["Studio PC".into(), "Deck".into(), "Studio PC".into()],
            &["Mic".into(), "MacBook Pro Microphone".into(), VIRTUAL_INPUT.into(), VIRTUAL_OUTPUT.into(), EVERYTHING.into(), NO_DEVICE.into(), "Mic".into()],
        );
        let t = "Brian's Mac paired with Studio PC at 192.168.1.5:47800; Deck at 10.0.0.2, Studio PC again at 192.168.1.5.\n\
                 Send from MacBook Pro Microphone, Mic, CapraLink Input, CapraLink Output, Everything this PC plays, none. v0.1.0 1.2.3.4.5";
        assert_eq!(
            redact_text(t, &map),
            "This computer paired with Device A at <ip-1>:47800; Device B at <ip-2>, Device A again at <ip-1>.\n\
             Send from Audio device 2, Audio device 1, CapraLink Input, CapraLink Output, Everything this PC plays, none. v0.1.0 1.2.3.4.5"
        );
    }

    #[test]
    fn redaction_hides_device_ids() {
        let map = redactions("Me", &[], &["USB Mic".into(), "coreaudio:AppleUSBAudioEngine:Acme:SN12345:1".into(), VIRTUAL_INPUT.into()]);
        let t = redact_text("device not found: coreaudio:AppleUSBAudioEngine:Acme:SN12345:1 (USB Mic), CapraLink Input", &map);
        assert_eq!(t, "device not found: Audio device 2 (Audio device 1), CapraLink Input");
    }

    #[test]
    fn remote_config_wire_compat() {
        // a 0.1.x reply: names only, settings saved by name
        let old = r#"{"name":"Old PC","settings":{"input":"USB Mic"},"inputs":["USB Mic","CapraLink Output"],"outputs":["Speakers"]}"#;
        let c: RemoteConfig = serde_json::from_str(old).unwrap();
        assert_eq!((c.inputs.len(), c.input_devices.len(), c.settings.input.as_deref()), (2, 0, Some("USB Mic")));
        // this version's reply, read the way 0.1.x reads it (same fields, unknown ones ignored)
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Old {
            name: String,
            settings: Settings,
            inputs: Vec<String>,
            outputs: Vec<String>,
        }
        let mic = AudioDevice { id: "coreaudio:usb-1".into(), name: "USB Mic".into() };
        let new = RemoteConfig { name: "New".into(), settings: Settings::default(), inputs: vec![mic.name.clone()], outputs: vec![], input_devices: vec![mic.clone()], output_devices: vec![] };
        let json = serde_json::to_string(&new).unwrap();
        let o: Old = serde_json::from_str(&json).unwrap();
        assert_eq!(o.inputs, ["USB Mic"]);
        let back: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.input_devices, [mic]);
    }

    #[test]
    fn set_name_trims_cleans_and_rejects_empty() {
        let (a, adir) = node();
        assert!(a.set_name("   ").unwrap_err().to_string().contains("can't be empty"));
        a.set_name("  New Name  ").unwrap();
        assert_eq!(a.state().name, "New Name");
        assert_eq!(load(&adir).unwrap().name, "New Name");
        let _ = std::fs::remove_dir_all(adir);
    }

    #[test]
    fn old_config_and_hello_without_new_fields_parse() {
        let cfg: Config = serde_json::from_str(
            r#"{"device_id":"a","name":"b","input":null,"output":null,"bitrate":48000,"channels":1,
                "peers":[{"id":"1","name":"c","secret":"00"}]}"#,
        )
        .unwrap();
        assert!(cfg.peers[0].addr.is_none() && cfg.last_peer.is_none() && cfg.current.is_none() && cfg.peers[0].audio.is_none());
        assert!(!cfg.settings.remote_config && !cfg.settings.music_mode && cfg.settings.auto_reconnect);

        let m: Msg = serde_json::from_str(r#"{"type":"pair","id":"a","name":"b"}"#).unwrap();
        assert!(matches!(m, Msg::Pair { port: None, .. }));

        let m: Msg = serde_json::from_str(r#"{"type":"set_settings","settings":{"bitrate":48000,"channels":1}}"#).unwrap();
        assert!(matches!(m, Msg::SetSettings { name: None, .. }), "old SetSettings JSON without `name` parses");

        let m: Msg = serde_json::from_str(r#"{"type":"mode","music":true}"#).unwrap();
        assert!(matches!(m, Msg::Mode { music: true, name: None, hifi: false, hifi_ok: false }), "old Mode JSON without `name` or Hi-Fi parses: can't do Hi-Fi");
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "lowercase")]
        enum Old {
            Mode { music: bool },
        }
        let new = serde_json::to_vec(&Msg::Mode { music: true, name: Some("B".into()), hifi: true, hifi_ok: true }).unwrap();
        assert!(matches!(serde_json::from_slice(&new), Ok(Old::Mode { music: true })), "an old peer reads the new Mode");
        let m: Msg = serde_json::from_slice(&new).unwrap();
        assert!(matches!(m, Msg::Mode { music: true, hifi: true, hifi_ok: true, .. }));
        let st: Stats = serde_json::from_str(r#"{"sent":0,"received":0,"lost":0,"fec_recovered":0,"underruns":0,"buffer_ms":0,"target_ms":0,
            "in_peak":0,"out_peak":0,"tx_gap_ms":0,"rx_gap_ms":0,"bitrate":0,"complexity":0}"#).unwrap();
        assert!(!st.music, "old Stats JSON without `music` parses");
    }

    #[test]
    fn resolve_is_ipv4_only() {
        for a in ["::1", "[::1]:47800", "[fe80::1]"] {
            assert!(resolve(a).unwrap_err().to_string().contains("IPv6 addresses aren't supported"), "{a}");
        }
        assert_eq!(resolve(" 10.0.0.2 ").unwrap(), vec![SocketAddr::from(([10, 0, 0, 2], 47800))]);
        assert!(resolve("localhost:1").unwrap().iter().all(SocketAddr::is_ipv4));
    }

    #[test]
    fn unauthenticated_connections_are_capped_per_ip() {
        let (b, bdir) = node();
        let idle: Vec<_> = (0..MAX_PENDING_PER_IP).map(|_| TcpStream::connect(addr(&b)).unwrap()).collect();
        wait(|| b.pending().len() == MAX_PENDING_PER_IP);
        let mut extra = TcpStream::connect(addr(&b)).unwrap();
        extra.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(extra.read(&mut [0u8; 1]).unwrap(), 0, "over the limit: closed at once");
        drop(idle);
        wait(|| b.pending().is_empty());
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn address_order() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let got = order(vec![a("172.17.0.1:1"), a("8.8.8.8:1"), a("169.254.1.1:1"), a("10.0.0.2:1"), a("192.168.1.5:1"), a("127.0.0.1:1")]);
        assert_eq!(got, vec![a("192.168.1.5:1"), a("10.0.0.2:1"), a("172.17.0.1:1"), a("8.8.8.8:1")]);
        assert_eq!(order(vec![a("127.0.0.1:1"), a("169.254.1.1:1")]), vec![a("127.0.0.1:1"), a("169.254.1.1:1")]);
        assert!(dial(&[]).is_err());
    }

    #[test]
    fn session_keys_match() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        let secret = [9u8; 32];
        let resp = std::thread::spawn(move || {
            let next = || handshake(&mut l.accept().unwrap().0, &secret, false);
            (next().unwrap(), next().unwrap(), next().is_err())
        });
        let dial = |k: &[u8; 32]| handshake(&mut TcpStream::connect(at).unwrap(), k, true);
        let (ctl_a, ka) = dial(&secret).unwrap();
        let (_, ka2) = dial(&secret).unwrap();
        let wrong = dial(&[8u8; 32]);
        let ((ctl_b, kb), (_, kb2), bad_rejected) = resp.join().unwrap();
        assert_eq!(ka.send, kb.recv);
        assert_eq!(ka.recv, kb.send);
        assert_eq!((ka2.send, ka2.recv), (kb2.recv, kb2.send));
        assert_ne!(ka.send, ka.recv);
        assert!(ka.send != ka2.send && ka.recv != ka2.recv, "same pairing secret, new session: fresh keys");
        assert!(wrong.is_err() && bad_rejected, "wrong pairing secret must fail the handshake");
        ctl_a.send(&Msg::Ping).unwrap();
        assert!(matches!(ctl_b.recv().unwrap(), Some(Msg::Ping)));
    }

    /// Paired nodes without a session, plus each one's id.
    fn paired_nodes() -> ((Node, PathBuf), (Node, PathBuf), String, String) {
        let ((a, adir), (b, bdir)) = (node(), node());
        b.open_pairing().unwrap();
        a.pair_addr(&[addr(&b)], &b.state().pin).unwrap();
        wait(|| b.state().devices.iter().filter(|d| d.paired).count() == 1);
        let (aid, bid) = (a.st().cfg.device_id.clone(), b.st().cfg.device_id.clone());
        ((a, adir), (b, bdir), aid, bid)
    }

    #[test]
    fn invalid_remote_audio_does_not_change_name_or_saved_peer_settings() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        b.set_settings(Settings { remote_config: true, ..b.state().settings }).unwrap();
        let before = std::fs::read(bdir.join("config.json")).unwrap();
        // b has no current connection: the per-peer save path must validate channels too.
        assert!(b.state().current.is_none());
        for channels in [0, 3, u16::MAX] {
            let settings = Settings { channels, ..b.state().settings };
            let err = a.remote_set(&bid, settings, Some("Invalid rename".into())).unwrap_err().to_string();
            assert!(err.contains("channels must be 1 or 2"), "{err}");
            assert_eq!(std::fs::read(bdir.join("config.json")).unwrap(), before);
        }
        a.connect(&bid).unwrap();
        wait(|| is(&b, |d| d.connected));
        a.shutdown();
        b.shutdown();
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn long_names_still_fit_the_mdns_record() {
        let long = "😀".repeat(64); // 64 characters, 256 bytes
        let (a, adir) = node();
        a.set_name(&long).unwrap();
        let name = a.state().name;
        assert!(name.len() <= 250 && long.starts_with(&name), "{} bytes", name.len());
        // what `advertise` registers at every start; mdns-sd refuses a TXT entry over 255 bytes
        let id = "0".repeat(32);
        ServiceInfo::new(SERVICE, &id, &format!("{id}.local."), "", 47800, &[("id", id.as_str()), ("name", name.as_str()), ("v", "2")][..]).unwrap();
        // a name saved before the byte limit is shortened when the config loads
        {
            let mut st = a.st();
            st.cfg.name = long.clone();
            save(&adir, &st.cfg).unwrap();
        }
        assert_eq!(load(&adir).unwrap().name, name);
        a.shutdown();
        let _ = std::fs::remove_dir_all(adir);
    }


    /// Both ends of an authenticated control channel.
    fn control_pair() -> (Arc<Ctl>, Arc<Ctl>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        let responder = std::thread::spawn(move || handshake(&mut l.accept().unwrap().0, &[7; 32], false).unwrap().0);
        let initiator = handshake(&mut TcpStream::connect(at).unwrap(), &[7; 32], true).unwrap().0;
        (initiator, responder.join().unwrap())
    }

    #[test]
    fn stop_arrives_although_the_closer_had_unread_data() {
        let (x, y) = control_pair();
        y.stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        y.send(&Msg::Ping).unwrap(); // x never reads it, like a report in flight when Disconnect is pressed
        std::thread::sleep(Duration::from_millis(100));
        x.close();
        assert!(matches!(y.recv().unwrap(), Some(Msg::Stop)), "the peer must see a chosen end, not a lost connection");
    }
    fn stored_name(n: &Node, id: &str) -> (String, String) {
        let st = n.state();
        (n.st().cfg.peers.iter().find(|p| p.id == id).unwrap().name.clone(), st.devices.iter().find(|d| d.id == id).unwrap().name.clone())
    }

    #[test]
    fn only_the_paired_device_itself_can_rename_it() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        a.st().cfg.peers[0].name = "Stale".into();
        // anyone on the network can announce b's id with another name
        let info = ServiceInfo::new(SERVICE, &bid, &format!("{bid}.local."), "127.0.0.1", 1, &[("id", bid.as_str()), ("name", "Evil"), ("v", "2")][..]).unwrap();
        a.on_mdns(ServiceEvent::ServiceResolved(Box::new(info.as_resolved_service())));
        assert!(a.st().found.contains_key(&bid));
        assert_eq!(stored_name(&a, &bid), ("Stale".into(), "Stale".into()));
        // the session tells a b's real name, and a rename while connected
        a.connect_to(&bid, &[addr(&b)]).unwrap();
        let real = b.state().name;
        wait(|| stored_name(&a, &bid) == (real.clone(), real.clone()));
        b.set_name("Renamed B").unwrap();
        wait(|| stored_name(&a, &bid) == ("Renamed B".into(), "Renamed B".into()));
        assert_eq!(peers(&adir)[0].name, "Renamed B");
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn a_bad_first_address_does_not_block_the_next() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        b.set_settings(Settings { remote_config: true, ..b.state().settings }).unwrap();
        // answers TCP, but isn't CapraLink
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let bad = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            }
        });
        assert!(matches!(a.manage_at(&bid, &[bad, addr(&b)], &Msg::GetConfig).unwrap(), Msg::Config(_)));
        a.connect_to(&bid, &[bad, addr(&b)]).unwrap();
        wait(|| is(&b, |d| d.connected));
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn a_frame_split_by_read_timeouts_arrives_intact() {
        let (x, y) = control_pair();
        y.stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let pt = serde_json::to_vec(&Msg::Ping).unwrap();
        let mut ct = vec![0u8; pt.len() + 16];
        let n = x.noise.lock().unwrap().write_message(&pt, &mut ct).unwrap();
        let frame = [&(n as u16).to_be_bytes()[..], &ct[..n]].concat();
        // a stalled sender: half the length, then part of the payload, each followed by a timeout
        for part in [&frame[..1], &frame[1..5]] {
            (&x.stream).write_all(part).unwrap();
            let e = y.recv().map(drop).unwrap_err();
            assert!(matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{e}");
        }
        (&x.stream).write_all(&frame[5..]).unwrap();
        assert!(matches!(y.recv().unwrap(), Some(Msg::Ping)));
        x.send(&Msg::Stop).unwrap();
        assert!(matches!(y.recv().unwrap(), Some(Msg::Stop)), "the channel keeps working");
    }

    /// A session `a` authenticated with `b` but hasn't sent its first request yet (as a modified
    /// client could hold it).
    fn pending(a: &Node, b: &Node, aid: &str, bid: &str) -> Arc<Ctl> {
        let key = secret(&a.st().cfg, bid).unwrap();
        let mut s = TcpStream::connect(addr(b)).unwrap();
        send_msg(&mut s, &Msg::Session { id: aid.into() }).unwrap();
        handshake(&mut s, &key, true).unwrap().0
    }

    fn refused(ctl: &Ctl) -> bool {
        matches!(ctl.recv(), Ok(Some(Msg::Error { message })) if message.contains("pairing was removed or changed"))
    }

    #[test]
    fn forgotten_or_repaired_peer_cannot_use_a_pending_session() {
        let ((a, adir), (b, bdir), aid, bid) = paired_nodes();
        b.set_settings(Settings { remote_config: true, ..b.state().settings }).unwrap();
        let link = |ctl: &Ctl| ctl.send(&Msg::Link { channels: 1, port: a.port() }).unwrap();

        // forgotten after the handshake: no audio session, no remote configuration
        let (l, m) = (pending(&a, &b, &aid, &bid), pending(&a, &b, &aid, &bid));
        b.forget(&aid).unwrap();
        link(&l);
        assert!(refused(&l));
        m.send(&Msg::Manage).unwrap();
        let settings = serde_json::json!({ "music_mode": true }).as_object().unwrap().clone();
        m.send(&Msg::SetSettings { settings, name: Some("Taken over".into()) }).unwrap();
        assert!(refused(&m));
        assert!(b.pending().is_empty(), "their slots are free before the refusals arrive");
        assert!(!is(&b, |d| d.connected) && b.state().current.is_none() && b.state().name != "Taken over" && !b.state().settings.music_mode);

        // paired again (new key) after the handshake: the old key's session is refused too
        b.open_pairing().unwrap();
        a.pair_addr(&[addr(&b)], &b.state().pin).unwrap();
        assert!(is(&b, |d| d.paired) && b.pending().is_empty(), "stored, slot free, before pairing returns");
        let old = pending(&a, &b, &aid, &bid);
        b.open_pairing().unwrap();
        a.pair_addr(&[addr(&b)], &b.state().pin).unwrap();
        assert!(secret(&b.st().cfg, &aid) == secret(&a.st().cfg, &bid));
        link(&old);
        assert!(refused(&old));
        assert!(!is(&b, |d| d.connected));
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn remote_save_cannot_undo_turning_remote_configuration_off() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        b.set_settings(Settings { remote_config: true, ..b.state().settings }).unwrap();
        let before = b.state().settings;
        // b's user turns remote configuration off while a's save is on its way in
        let b2 = b.clone();
        *b.0.hook.lock().unwrap() = Some(("remote save", Box::new(move || b2.set_settings(Settings { remote_config: false, ..b2.state().settings }).unwrap())));
        assert!(a.remote_set(&bid, Settings { bitrate: 16_000, ..before.clone() }, None).is_err());
        assert_eq!(b.state().settings, Settings { remote_config: false, ..before.clone() });
        assert_eq!(load(&bdir).unwrap().settings, Settings { remote_config: false, ..before });
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn a_cancelled_dial_cannot_end_a_newer_session() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        // an older dial (say a reconnect attempt) stalls after its handshake...
        let (reached, at) = mpsc::channel();
        let (go, wait_go) = mpsc::channel::<()>();
        *a.0.hook.lock().unwrap() = Some(("before link", Box::new(move || {
            reached.send(()).unwrap();
            let _ = wait_go.recv_timeout(Duration::from_secs(2));
        })));
        let (a2, bid2, gen, at_b) = (a.clone(), bid.clone(), a.st().retry, addr(&b));
        let old = std::thread::spawn(move || a2.dial_link(&bid2, &[at_b], gen));
        at.recv().unwrap();
        // ...while the user connects (it may wait for the older dial to give up)
        let (a2, bid2) = (a.clone(), bid.clone());
        let newer = std::thread::spawn(move || a2.connect(&bid2));
        std::thread::sleep(Duration::from_millis(500));
        let _ = go.send(());
        assert!(old.join().unwrap().is_err(), "the older dial was cancelled");
        newer.join().unwrap().unwrap();
        let (ca, cb) = (a.st().session.as_ref().unwrap().ctl.clone(), b.st().session.as_ref().map(|s| s.ctl.clone()));
        std::thread::sleep(Duration::from_millis(500));
        assert!(a.st().session.as_ref().is_some_and(|s| Arc::ptr_eq(&s.ctl, &ca)), "the newer session survives here");
        assert!(cb.is_some_and(|cb| b.st().session.as_ref().is_some_and(|s| Arc::ptr_eq(&s.ctl, &cb))), "and on the other computer");
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn first_request_must_arrive_before_the_deadline() {
        let ((a, adir), (b, bdir), aid, bid) = paired_nodes();
        let ctl = pending(&a, &b, &aid, &bid);
        std::thread::sleep(AUTH_DEADLINE + Duration::from_millis(500));
        let _ = ctl.send(&Msg::Link { channels: 1, port: a.port() });
        assert!(!matches!(ctl.recv(), Ok(Some(Msg::Ok))), "a session held open past the deadline was accepted");
        assert!(!is(&b, |d| d.connected));
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn a_slow_audio_start_does_not_freeze_the_engine() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        // b's audio start hangs, like macOS waiting for the user to allow microphone access
        let (reached, at) = mpsc::channel();
        let (go, wait_go) = mpsc::channel::<()>();
        *b.0.hook.lock().unwrap() = Some(("start audio", Box::new(move || {
            reached.send(()).unwrap();
            let _ = wait_go.recv_timeout(Duration::from_secs(5));
        })));
        let a2 = a.clone();
        let dial = std::thread::spawn(move || a2.connect(&bid));
        at.recv().unwrap();
        // meanwhile b still answers and changes settings (the window, Quit, remote configuration)
        let (b2, (done, finished)) = (b.clone(), mpsc::channel());
        std::thread::spawn(move || {
            let _ = b2.state();
            b2.set_settings(Settings { music_mode: true, ..b2.state().settings }).unwrap();
            done.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_secs(1)).is_ok(), "the engine froze while audio was starting");
        go.send(()).unwrap();
        dial.join().unwrap().unwrap();
        wait(|| is(&b, |d| d.connected));
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn disconnect_cancels_an_incoming_session_while_its_audio_starts() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        let (reached, at) = mpsc::channel();
        let (go, wait_go) = mpsc::channel::<()>();
        *b.0.hook.lock().unwrap() = Some(("start audio", Box::new(move || {
            reached.send(()).unwrap();
            let _ = wait_go.recv_timeout(Duration::from_secs(5));
        })));
        let a2 = a.clone();
        let dial = std::thread::spawn(move || a2.connect(&bid));
        at.recv().unwrap();
        b.disconnect(); // b's user disconnects while b is still opening its devices
        go.send(()).unwrap();
        assert!(dial.join().unwrap().is_err());
        std::thread::sleep(Duration::from_millis(300));
        assert!(b.st().session.is_none() && a.st().session.is_none());
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn settings_changed_while_audio_starts_restart_it_with_the_new_ones() {
        let ((a, adir), (b, bdir), _aid, bid) = paired_nodes();
        use std::sync::atomic::{AtomicBool, Ordering};
        let restarted = Arc::new(AtomicBool::new(false));
        let (b2, r) = (b.clone(), restarted.clone());
        *b.0.hook.lock().unwrap() = Some(("start audio", Box::new(move || {
            // the first start: the user picks another bitrate meanwhile (no session yet to restart)
            b2.set_settings(Settings { bitrate: 16_000, ..b2.state().settings }).unwrap();
            *b2.0.hook.lock().unwrap() = Some(("start audio", Box::new(move || r.store(true, Ordering::SeqCst))));
        })));
        a.connect(&bid).unwrap();
        assert!(restarted.load(Ordering::SeqCst), "the audio started once more, with the new settings");
        assert!(b.st().session.is_some());
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }
    #[test]
    fn a_settings_patch_changes_only_its_own_fields() {
        let ((a, adir), (b, bdir), _aid, _bid) = paired_nodes();
        // another change lands first (e.g. the other computer picks a microphone)...
        b.set_settings(Settings { input: Some("Mic".into()), ..b.state().settings }).unwrap();
        // ...then this window's bitrate change: the microphone stays
        b.patch_settings(serde_json::json!({ "bitrate": 16_000 }).as_object().unwrap()).unwrap();
        let s = b.state().settings;
        assert_eq!((s.input.as_deref(), s.bitrate), (Some("Mic"), 16_000));
        assert!(b.patch_settings(serde_json::json!({ "channels": 3 }).as_object().unwrap()).is_err());
        assert_eq!(b.state().settings.channels, s.channels);
        drop(a);
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    fn patch(n: &Node, v: serde_json::Value) -> Result<()> {
        n.patch_settings(v.as_object().unwrap())
    }

    fn f13() -> PttKey {
        PttKey { id: "key:105".into(), label: "F13".into() }
    }

    #[test]
    fn volume_mute_and_ptt_settings_load_from_old_files_and_validate() {
        // 0.2.x config: no volume, mute or push-to-talk, here or in a peer's saved audio
        let cfg: Config = serde_json::from_str(
            r#"{"device_id":"a","name":"b","bitrate":48000,"channels":1,
                "peers":[{"id":"1","name":"c","secret":"00","audio":{"input":null,"output":null,"bitrate":32000,"channels":2}}]}"#,
        )
        .unwrap();
        let s = &cfg.settings;
        assert_eq!((s.send_volume, s.recv_volume, s.mute, s.ptt, &s.ptt_key), (100, 100, false, PttMode::Off, &None));
        let a = cfg.peers[0].audio.clone().unwrap();
        assert_eq!((a.send_volume, a.recv_volume, a.mute, a.ptt, a.bitrate), (100, 100, false, PttMode::Off, 32_000));
        let json = serde_json::to_value(Settings { ptt: PttMode::Toggle, ptt_key: Some(f13()), ..Settings::default() }).unwrap();
        assert_eq!((json["ptt"].as_str(), &json["ptt_key"]), (Some("toggle"), &serde_json::json!({ "id": "key:105", "label": "F13" })));

        let (n, dir) = node();
        for (bad, why) in [
            (serde_json::json!({ "send_volume": 151 }), "volume must be 0 to 150%"),
            (serde_json::json!({ "recv_volume": 200 }), "volume must be 0 to 150%"),
            (serde_json::json!({ "ptt": "hold" }), "pick a push-to-talk button first"),
            (serde_json::json!({ "ptt": "loud" }), "unknown variant"),
        ] {
            let e = patch(&n, bad).unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
        }
        assert_eq!(n.state().settings, Settings::default());
        patch(&n, serde_json::json!({ "send_volume": 150, "recv_volume": 0, "mute": true, "ptt": "hold", "ptt_key": f13() })).unwrap();
        let s = load(&dir).unwrap().settings;
        assert_eq!((s.send_volume, s.recv_volume, s.mute, s.ptt, s.ptt_key), (150, 0, true, PttMode::Hold, Some(f13())));
        assert!(n.st().ptt.listener.is_some(), "a key: listening");
        patch(&n, serde_json::json!({ "ptt": "off" })).unwrap();
        assert!(n.st().ptt.listener.is_none(), "off: not listening");
        n.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn volume_mute_and_ptt_apply_live_and_are_per_connection() {
        let ((a, adir), (b, bdir), bid) = connected();
        let ctl = |n: &Node| n.st().session.as_ref().unwrap().ctl.clone();
        let ca = ctl(&a);
        patch(&a, serde_json::json!({ "send_volume": 40, "recv_volume": 120, "mute": true, "ptt": "toggle", "ptt_key": f13() })).unwrap();
        assert!(Arc::ptr_eq(&ca, &ctl(&a)), "no reconnect");
        let saved = peers(&adir).into_iter().find(|p| p.id == bid).unwrap().audio.unwrap();
        assert_eq!((saved.send_volume, saved.recv_volume, saved.mute, saved.ptt), (40, 120, true, PttMode::Toggle), "saved for b");
        patch(&a, serde_json::json!({ "bitrate": 16_000 })).unwrap();
        assert!(!Arc::ptr_eq(&ca, &ctl(&a)), "a bitrate change still reconnects");

        // b configures a remotely, as 0.2.x does: whole settings without the new fields
        a.set_settings(Settings { remote_config: true, ..a.state().settings }).unwrap();
        let aid = a.st().cfg.device_id.clone();
        let old = serde_json::json!({ "input": null, "output": null, "bitrate": 24_000, "channels": 1, "service": false, "remote_config": false, "music_mode": false, "auto_reconnect": true });
        let m = b.manage(&aid, &Msg::SetSettings { settings: old.as_object().unwrap().clone(), name: None }).unwrap();
        assert!(matches!(m, Msg::Ok));
        let s = a.state().settings;
        assert_eq!((s.bitrate, s.send_volume, s.recv_volume, s.mute, s.ptt), (24_000, 40, 120, true, PttMode::Toggle), "fields it doesn't know are kept");
        let e = b.remote_set(&aid, Settings { send_volume: 151, ..s.clone() }, None).unwrap_err().to_string();
        assert!(e.contains("volume must be"), "{e}");
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn push_to_talk_follows_the_key_and_capture_takes_the_next_press() {
        let (n, dir) = node();
        let send = |down| n.st().ptt.listener.as_ref().unwrap().tx.send(Ev::Key(f13(), down)).unwrap();
        let talking = || n.state().talking;
        // capture: the next press, whatever push-to-talk is set to
        let n2 = n.clone();
        let got = std::thread::spawn(move || n2.ptt_capture());
        wait(|| n.st().ptt.capture.is_some());
        assert!(n.ptt_capture().unwrap_err().to_string().contains("already waiting"));
        n.st().ptt.listener.as_ref().unwrap().tx.send(Ev::Key(f13(), false)).unwrap(); // a release isn't a press
        send(true);
        assert_eq!(got.join().unwrap().unwrap(), f13());
        assert!(n.st().ptt.listener.is_none(), "stops listening once captured (no key set)");
        assert!(n.ptt_capture().unwrap_err().to_string().contains("no key or button was pressed"), "times out");

        patch(&n, serde_json::json!({ "ptt": "hold", "ptt_key": f13() })).unwrap();
        send(true);
        wait(talking);
        send(false);
        let t = Instant::now();
        wait(|| !talking());
        assert!(t.elapsed() >= ptt::TAIL - Duration::from_millis(20), "the release tail");
        let other = PttKey { id: "key:9".into(), label: "V".into() };
        n.st().ptt.listener.as_ref().unwrap().tx.send(Ev::Key(other, true)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!talking(), "another key does nothing");

        patch(&n, serde_json::json!({ "ptt": "toggle" })).unwrap();
        send(true);
        send(false);
        wait(talking);
        send(true);
        wait(|| !talking());
        send(false);
        // a listener problem shows, and clears
        n.st().ptt.listener.as_ref().unwrap().tx.send(Ev::Status(Some("no permission".into()))).unwrap();
        wait(|| n.state().ptt_error.as_deref() == Some("no permission"));
        n.st().ptt.listener.as_ref().unwrap().tx.send(Ev::Status(None)).unwrap();
        wait(|| n.state().ptt_error.is_none());
        n.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delay_estimate_and_report_wire_compat() {
        assert_eq!(one_way(20.0, Some(3.0), 31.4), 53, "capture + frame, half the round trip, buffer + playback");
        assert_eq!(one_way(20.0, None, 10.0), 30, "round trip unknown: LAN, counted as 0");
        // a 0.2.x report parses (no delay fields)...
        let m: Msg = serde_json::from_str(r#"{"type":"report","received":5,"lost":0,"underruns":0,"jitter_ms":1.5}"#).unwrap();
        assert!(matches!(m, Msg::Report { received: 5, ts: None, echo: None, send_ms: None, recv_ms: None, .. }));
        // ...and 0.2.x reads the new one (unknown fields ignored)
        #[derive(Deserialize)]
        #[allow(dead_code)]
        #[serde(tag = "type", rename_all = "lowercase")]
        enum Old {
            Report { received: u64, lost: u64, underruns: u64, jitter_ms: f32 },
        }
        let new = serde_json::to_vec(&Msg::Report { received: 5, lost: 1, underruns: 0, jitter_ms: 2.0, ts: Some(1000), echo: Some((900, 40)), send_ms: Some(20.0), recv_ms: Some(30.0) }).unwrap();
        assert!(matches!(serde_json::from_slice(&new), Ok(Old::Report { received: 5, lost: 1, .. })));
        let back: Msg = serde_json::from_slice(&new).unwrap();
        assert!(matches!(back, Msg::Report { echo: Some((900, 40)), recv_ms: Some(30.0), .. }));
        // so does the new NodeState/Stats JSON the window gets from an older engine
        let st: Stats = serde_json::from_str(r#"{"sent":0,"received":0,"lost":0,"fec_recovered":0,"underruns":0,"buffer_ms":0,"target_ms":0,
            "in_peak":0,"out_peak":0,"tx_gap_ms":0,"rx_gap_ms":0,"bitrate":0,"complexity":0}"#).unwrap();
        assert_eq!((st.delay_in_ms, st.delay_out_ms), (None, None));
    }
    #[test]
    fn ptt_input_loss_stops_talking() {
        let (n, dir) = node();
        patch(&n, serde_json::json!({"ptt":"hold", "ptt_key": f13()})).unwrap();
        let tx = n.st().ptt.listener.as_ref().unwrap().tx.clone();
        tx.send(Ev::Key(f13(), true)).unwrap();
        wait(|| n.state().talking);
        tx.send(Ev::Status(Some("input device lost".into()))).unwrap();
        wait(|| n.state().ptt_error.is_some());
        let after_error = n.state().talking; // at once: no release tail
        tx.send(Ev::Status(None)).unwrap();
        tx.send(Ev::Key(f13(), true)).unwrap();
        wait(|| n.state().talking);
        tx.send(Ev::Lost).unwrap();
        wait(|| !n.state().talking); // a device went away with the key held
        n.shutdown();
        let _ = std::fs::remove_dir_all(dir);
        assert!(!after_error, "Hold PTT stays transmitting after input failure");
    }

    #[test]
    fn disconnect_cancels_queued_incoming_link() {
        let ((a, adir), (b, bdir), aid, bid) = paired_nodes();
        // Simulates another start currently owning the serialized audio-start lock.
        let starting = b.0.starting.lock().unwrap();
        let ctl = pending(&a, &b, &aid, &bid);
        ctl.send(&Msg::Link { channels: 1, port: a.port() }).unwrap();
        wait(|| b.pending().is_empty()); // handler accepted Link and is waiting for starting
        b.disconnect();
        drop(starting);
        let accepted = matches!(ctl.recv(), Ok(Some(Msg::Ok)));
        let connected = b.state().devices.iter().any(|d| d.connected);
        b.disconnect();
        a.shutdown();
        b.shutdown();
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
        assert!(!accepted && !connected, "queued incoming session starts after Disconnect");
    }

    #[test]
    fn outgoing_handshake_has_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            setup(&s).unwrap();
            recv_msg(&mut s).unwrap(); // Session
            recv(&mut s).unwrap(); // initiator's Noise message
            s.write_all(&256u16.to_be_bytes()).unwrap();
            for _ in 0..7 {
                if s.write_all(&[0]).is_err() {
                    break; // cut off
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let start = Instant::now();
        assert!(open(&[addr], &"a".repeat(32), &[7;32]).is_err());
        let elapsed = start.elapsed();
        server.join().unwrap();
        assert!(elapsed < IO_TIMEOUT + Duration::from_millis(500), "untrusted trickling server held outgoing handshake for {elapsed:?}");
    }

}
