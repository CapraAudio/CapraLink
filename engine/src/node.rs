//! One node per process: config, mDNS discovery, PIN pairing (SPAKE2), the Noise control
//! channel and the single active `Link` (MASTER.md §3.3).

use crate::dsp::RateControl;
use crate::vdev::Virtual;
use crate::{Keys, Link, Settings, Stats};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const SERVICE: &str = "_capralink._udp.local.";
const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const DIAL_TIMEOUT: Duration = Duration::from_secs(1); // per advertised address
const LINK_TIMEOUT: Duration = Duration::from_secs(10); // peer may be opening audio devices
const REPORT: Duration = Duration::from_secs(1); // also doubles as the session keepalive
const DEAD: Duration = Duration::from_secs(15);
const MAX_FAILURES: u32 = 5;

#[derive(Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    pub pin: String,
    pub devices: Vec<Device>,
    pub stats: Option<Stats>,
    pub settings: Settings,
    pub error: Option<String>,
    /// Why the virtual devices couldn't be created (Linux), if they couldn't.
    pub virtual_error: Option<String>,
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
}

/// Another computer's settings and its own device lists, for remote configuration.
#[derive(Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    pub settings: Settings,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Config {
    device_id: String,
    name: String,
    #[serde(flatten)]
    settings: Settings,
    #[serde(default)]
    peers: Vec<Peer>,
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
    Report { received: u64, lost: u64, underruns: u64, jitter_ms: f32 },
    /// Instead of `Link`: a one-request remote-configuration session (MASTER.md §3.6 M6b).
    Manage,
    #[serde(rename = "get_config")]
    GetConfig,
    Config(RemoteConfig),
    #[serde(rename = "set_settings")]
    SetSettings { settings: Settings },
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
}

struct St {
    cfg: Config,
    pin: String,
    failures: u32,
    found: HashMap<String, Found>,
    session: Option<Session>,
    error: Option<String>,
    vdev: Virtual,
}

struct Inner {
    dir: PathBuf,
    port: u16,
    mdns: Option<ServiceDaemon>,
    st: Mutex<St>,
}

#[derive(Clone)]
pub struct Node(Arc<Inner>);

impl Node {
    /// Loads (or creates) the config, listens on TCP `port` (0 = any free port), and
    /// advertises + browses via mDNS when `mdns` is set. `None` = OS config dir.
    pub fn start(config_dir: Option<PathBuf>, port: u16, mdns: bool) -> Result<Node> {
        let dir = config_dir_or_default(config_dir)?;
        let cfg = load(&dir)?;
        let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("port {port} is in use (is CapraLink already running?)"))?;
        let port = listener.local_addr()?.port();
        let (daemon, browse) = if mdns {
            let d = ServiceDaemon::new()?;
            // IPv4 only (the link is IPv4); with enable_addr_auto every remaining interface
            // address is advertised, and the dialer picks the one that answers.
            d.disable_interface(vec![IfKind::IPv6, IfKind::LoopbackV4])?;
            let props = [("id", cfg.device_id.as_str()), ("name", cfg.name.as_str()), ("v", "2")];
            let host = format!("{}.local.", cfg.device_id);
            d.register(ServiceInfo::new(SERVICE, &cfg.device_id, &host, "", port, &props[..])?.enable_addr_auto())?;
            let rx = d.browse(SERVICE)?;
            (Some(d), Some(rx))
        } else {
            (None, None)
        };
        let st = St { cfg, pin: new_pin(), failures: 0, found: HashMap::new(), session: None, error: None, vdev: Virtual::setup() };
        let node = Node(Arc::new(Inner { dir, port, mdns: daemon, st: Mutex::new(st) }));
        if let Some(rx) = browse {
            let n = node.clone();
            std::thread::Builder::new().name("capralink-mdns".into()).spawn(move || {
                while let Ok(ev) = rx.recv() {
                    n.on_mdns(ev);
                }
            })?;
        }
        let n = node.clone();
        std::thread::Builder::new().name("capralink-ctl".into()).spawn(move || n.accept(listener))?;
        Ok(node)
    }

    /// Clean exit: stops the session and tells the network we're gone.
    pub fn shutdown(&self) {
        self.disconnect();
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
                    name: st.found.get(&p.id).map_or(&p.name, |f| &f.name).clone(),
                    paired: true,
                    online,
                    connected: conn == Some(&p.id),
                    reachable: online || p.addr.is_some(),
                    addr: p.addr.clone(),
                }
            })
            .collect();
        for (id, f) in &st.found {
            if !st.cfg.peers.iter().any(|p| p.id == *id) {
                devices.push(Device { id: id.clone(), name: f.name.clone(), paired: false, online: true, connected: false, reachable: true, addr: None });
            }
        }
        devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        NodeState {
            name: st.cfg.name.clone(),
            pin: st.pin.clone(),
            devices,
            stats: st.session.as_ref().and_then(|s| s.link.as_ref()).map(Link::stats),
            settings: st.cfg.settings.clone(),
            error: st.error.clone(),
            virtual_error: st.vdev.error.clone(),
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
        let pin = pin.trim();
        ensure!(pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit()), "the PIN is 6 digits");
        let (my_id, my_name) = {
            let st = self.st();
            (st.cfg.device_id.clone(), st.cfg.name.clone())
        };
        let mut s = dial(addrs)?;
        let addr = s.peer_addr()?.to_string();
        send_msg(&mut s, &Msg::Pair { id: my_id.clone(), name: my_name, port: Some(self.0.port) })?;
        let Msg::Hello { id, name } = recv_msg(&mut s)? else { bail!("unexpected reply") };
        check_id(&id)?;
        let secret = pake(&mut s, pin, true, &my_id, &id).map_err(|_| anyhow!("pairing failed — check the PIN"))?;
        let mut st = self.st();
        add_peer(&mut st.cfg, &id, &name, &secret, Some(addr));
        save(&self.0.dir, &st.cfg)?;
        st.error = None;
        Ok(id)
    }

    /// Pairs directly by address, bypassing mDNS discovery (for when multicast is blocked on
    /// the LAN). `addr` is "ip", "ip:port" (IPv4/IPv6 literal or hostname) or blank port meaning
    /// the default. Returns the paired peer's name.
    pub fn pair_ip(&self, addr: &str, pin: &str) -> Result<String> {
        let addr = addr.trim();
        ensure!(!addr.is_empty(), "enter the other computer's IP address");
        let id = self.pair_addr(&resolve(addr)?, pin)?;
        let st = self.st();
        Ok(st.cfg.peers.iter().find(|p| p.id == id).map_or(id, |p| p.name.clone()))
    }

    pub fn connect(&self, id: &str) -> Result<()> {
        let (addrs, fullname) = self.addrs(id)?;
        let r = self.connect_to(id, &addrs);
        self.recheck(fullname, r)
    }

    /// Starts a session with paired device `id` at the first of `addrs` that answers,
    /// replacing any current one.
    pub fn connect_to(&self, id: &str, addrs: &[SocketAddr]) -> Result<()> {
        self.disconnect(); // one link at a time; also frees our UDP port
        let (my_id, secret, channels) = {
            let st = self.st();
            (st.cfg.device_id.clone(), secret(&st.cfg, id).ok_or_else(|| anyhow!("not paired with that device"))?, st.cfg.settings.channels)
        };
        let mut s = dial(addrs)?;
        let addr = s.peer_addr()?;
        send_msg(&mut s, &Msg::Session { id: my_id })?;
        let (ctl, keys) = handshake(&mut s, &secret, true).context("secure connection failed (try pairing again)")?;
        ctl.send(&Msg::Link { channels, port: self.0.port })?;
        s.set_read_timeout(Some(LINK_TIMEOUT))?;
        match ctl.recv()? {
            Some(Msg::Ok) => {}
            Some(Msg::Error { message }) => bail!("other computer: {message}"),
            _ => bail!("unexpected reply"),
        }
        self.activate(id, addr, &keys, ctl)?;
        let mut st = self.st();
        set_addr(&mut st.cfg, id, addr.to_string());
        save(&self.0.dir, &st.cfg)
    }

    pub fn disconnect(&self) {
        let old = self.st().session.take();
        if let Some(s) = old {
            s.ctl.close();
        }
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        if self.st().session.as_ref().is_some_and(|s| s.peer_id == id) {
            self.disconnect();
        }
        let mut st = self.st();
        st.cfg.peers.retain(|p| p.id != id);
        save(&self.0.dir, &st.cfg)
    }

    /// Saves the settings; a running link reconnects (fresh keys) to apply audio changes.
    /// A change to `service` installs/removes the login agent first.
    pub fn set_settings(&self, s: Settings) -> Result<()> {
        ensure!(matches!(s.channels, 1 | 2), "channels must be 1 or 2");
        let old = self.st().cfg.settings.clone();
        if old.service != s.service {
            crate::rpc::login_agent(s.service).context("background service")?;
        }
        let running = {
            let mut st = self.st();
            let audio_changed = Settings { service: s.service, remote_config: s.remote_config, ..old } != s;
            st.cfg.settings = s;
            save(&self.0.dir, &st.cfg)?;
            st.session.as_ref().filter(|_| audio_changed).map(|s| (s.peer_id.clone(), s.addr))
        };
        match running {
            Some((id, addr)) => self.connect_to(&id, &[addr]),
            None => Ok(()),
        }
    }

    /// Reads paired device `id`'s settings and device lists (it must allow remote configuration).
    pub fn remote_get(&self, id: &str) -> Result<RemoteConfig> {
        match self.manage(id, &Msg::GetConfig)? {
            Msg::Config(c) => Ok(c),
            _ => bail!("unexpected reply"),
        }
    }

    /// Changes paired device `id`'s settings, except its `service` and `remote_config`.
    pub fn remote_set(&self, id: &str, settings: Settings) -> Result<()> {
        match self.manage(id, &Msg::SetSettings { settings })? {
            Msg::Ok => Ok(()),
            _ => bail!("unexpected reply"),
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
        let mut s = dial(addrs)?;
        send_msg(&mut s, &Msg::Session { id: my_id })?;
        let (ctl, _) = handshake(&mut s, &secret, true).context("secure connection failed (try pairing again)")?;
        ctl.send(&Msg::Manage)?;
        ctl.send(req)?;
        s.set_read_timeout(Some(LINK_TIMEOUT + IO_TIMEOUT))?; // new audio settings may restart its link
        let reply = ctl.recv();
        ctl.close();
        match reply? {
            Some(Msg::Error { message }) => bail!("{message}"),
            Some(m) => Ok(m),
            None => bail!("unexpected reply"),
        }
    }

    /// Installs a new session: stops the old one, starts the Link, watches the control channel.
    fn activate(&self, id: &str, addr: SocketAddr, keys: &Keys, ctl: Arc<Ctl>) -> Result<()> {
        let mut st = self.st();
        if let Some(old) = st.session.take() {
            old.ctl.close(); // Link drop below frees the UDP port before the new bind
        }
        let link = match start_link(&st.cfg.settings, self.0.port, addr, keys) {
            Ok(l) => l,
            Err(e) => {
                drop(st);
                ctl.close();
                return Err(e);
            }
        };
        st.session = Some(Session { peer_id: id.to_string(), addr, link, ctl: ctl.clone() });
        st.error = None;
        drop(st);
        let n = self.clone();
        std::thread::Builder::new().name("capralink-session".into()).spawn(move || n.serve(ctl))?;
        Ok(())
    }

    /// Session control loop: exchanges 1 s reports (which double as keepalive), steers this
    /// node's own sender from the peer's reports, and tears the session down on stop / close /
    /// silence.
    fn serve(&self, ctl: Arc<Ctl>) {
        let _ = ctl.stream.set_read_timeout(Some(Duration::from_secs(1)));
        let ceiling = self.st().cfg.settings.bitrate;
        let mut rate = RateControl::new(ceiling);
        let mut last_counts = (0u64, 0u64, 0u64);
        let (mut last_rx, mut last_report) = (Instant::now(), Instant::now());
        let lost = loop {
            match ctl.recv() {
                Ok(Some(Msg::Stop)) => break false,
                Ok(Some(Msg::Report { received, lost, underruns, .. })) => {
                    last_rx = Instant::now();
                    let (bitrate, loss_perc) = rate.on_report(received, lost, underruns);
                    self.apply_rate(&ctl, bitrate, loss_perc);
                }
                Ok(_) => last_rx = Instant::now(),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if last_rx.elapsed() > DEAD {
                        break true;
                    }
                }
                Err(_) => break false,
            }
            if last_report.elapsed() >= REPORT {
                last_report = Instant::now();
                match self.link_delta(&ctl, &mut last_counts) {
                    Some((received, lost, underruns, jitter_ms)) => {
                        let _ = ctl.send(&Msg::Report { received, lost, underruns, jitter_ms });
                    }
                    None => {
                        let _ = ctl.send(&Msg::Ping); // no link (e.g. tests): keepalive only
                    }
                }
            }
        };
        ctl.close();
        let mut st = self.st();
        if st.session.as_ref().is_some_and(|s| Arc::ptr_eq(&s.ctl, &ctl)) {
            let s = st.session.take();
            if lost {
                let name = s.and_then(|s| st.cfg.peers.iter().find(|p| p.id == s.peer_id).map(|p| p.name.clone()));
                st.error = Some(format!("lost connection to {}", name.unwrap_or_default()));
            }
        }
    }

    /// This session's own receive-side counters, as deltas since `last` (updated in place).
    /// `None` if the session moved on (or has no Link, e.g. under test).
    fn link_delta(&self, ctl: &Arc<Ctl>, last: &mut (u64, u64, u64)) -> Option<(u64, u64, u64, f32)> {
        let st = self.st();
        let link = st.session.as_ref().filter(|s| Arc::ptr_eq(&s.ctl, ctl))?.link.as_ref()?;
        let (r, l, u, jitter_ms) = link.report_counters();
        let delta = (r.saturating_sub(last.0), l.saturating_sub(last.1), u.saturating_sub(last.2));
        *last = (r, l, u);
        Some((delta.0, delta.1, delta.2, jitter_ms))
    }

    /// Applies a new bitrate/loss% to this session's own sender, if it's still the current one.
    fn apply_rate(&self, ctl: &Arc<Ctl>, bitrate: i32, loss_perc: u8) {
        let st = self.st();
        if let Some(link) = st.session.as_ref().filter(|s| Arc::ptr_eq(&s.ctl, ctl)).and_then(|s| s.link.as_ref()) {
            link.set_rate(bitrate, loss_perc);
        }
    }

    // ponytail: handshakes run one at a time on the accept thread (which also serializes PIN
    // guesses); a stalled peer delays others by up to IO_TIMEOUT. Thread per connection if that bites.
    fn accept(&self, l: TcpListener) {
        for s in l.incoming().flatten() {
            let from = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
            if let Err(e) = setup(&s).map_err(Into::into).and_then(|_| self.incoming(s)) {
                self.st().error = Some(format!("incoming connection from {from}: {e:#}"));
            }
        }
    }

    fn incoming(&self, mut s: TcpStream) -> Result<()> {
        match recv_msg(&mut s)? {
            Msg::Pair { id, name, port } => self.on_pair(s, &id, &name, port),
            Msg::Session { id } => self.on_session(s, &id),
            _ => bail!("unexpected hello"),
        }
    }

    fn on_pair(&self, mut s: TcpStream, id: &str, name: &str, port: Option<u16>) -> Result<()> {
        check_id(id)?;
        let addr = match port {
            Some(p) => Some(SocketAddr::new(s.peer_addr()?.ip(), p).to_string()),
            None => None,
        };
        let (my_id, my_name, pin) = {
            let st = self.st();
            (st.cfg.device_id.clone(), st.cfg.name.clone(), st.pin.clone())
        };
        send_msg(&mut s, &Msg::Hello { id: my_id.clone(), name: my_name })?;
        let r = pake(&mut s, &pin, false, id, &my_id);
        let mut st = self.st();
        match r {
            Ok(secret) => {
                add_peer(&mut st.cfg, id, name, &secret, addr);
                (st.pin, st.failures, st.error) = (new_pin(), 0, None);
                save(&self.0.dir, &st.cfg)
            }
            Err(e) => {
                st.failures += 1;
                if st.failures >= MAX_FAILURES {
                    (st.pin, st.failures) = (new_pin(), 0);
                }
                Err(e.context("pairing failed (wrong PIN?)"))
            }
        }
    }

    fn on_session(&self, mut s: TcpStream, id: &str) -> Result<()> {
        let secret = secret(&self.st().cfg, id).ok_or_else(|| anyhow!("unknown device"))?;
        let (ctl, keys) = handshake(&mut s, &secret, false)?;
        let port = match ctl.recv()? {
            Some(Msg::Link { port, .. }) => port,
            Some(Msg::Manage) => return self.on_manage(&ctl),
            _ => bail!("expected link request"),
        };
        let addr = SocketAddr::new(s.peer_addr()?.ip(), port);
        if let Err(e) = self.activate(id, addr, &keys, ctl.clone()) {
            let _ = ctl.send(&Msg::Error { message: format!("{e:#}") });
            return Err(e);
        }
        let mut st = self.st();
        set_addr(&mut st.cfg, id, addr.to_string());
        save(&self.0.dir, &st.cfg)?;
        drop(st);
        ctl.send(&Msg::Ok)?;
        Ok(())
    }

    /// Answers one remote-configuration request; never touches the current session.
    fn on_manage(&self, ctl: &Ctl) -> Result<()> {
        let req = ctl.recv()?; // read before replying, so closing can't reset the reply away
        let (name, local) = {
            let st = self.st();
            (st.cfg.name.clone(), st.cfg.settings.clone())
        };
        let reply = match req {
            _ if !local.remote_config => Msg::Error { message: format!("remote configuration is off on {name}") },
            Some(Msg::GetConfig) => Msg::Config(RemoteConfig { name, settings: local, inputs: crate::input_devices(), outputs: crate::output_devices() }),
            // `service` and `remote_config` only change locally
            Some(Msg::SetSettings { settings }) => match self.set_settings(Settings { service: local.service, remote_config: local.remote_config, ..settings }) {
                Ok(()) => Msg::Ok,
                Err(e) => Msg::Error { message: format!("{name}: {e:#}") },
            },
            _ => Msg::Error { message: "unsupported request".into() },
        };
        let r = ctl.send(&reply);
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
                let found = Found { name: clean_name(name), addrs, fullname: info.get_fullname().to_string() };
                st.found.insert(id.to_string(), found);
            }
            ServiceEvent::ServiceRemoved(_, fullname) => st.found.retain(|_, f| f.fullname != fullname),
            _ => {}
        }
    }
}

/// In tests there are no audio devices: the session runs without a Link.
fn start_link(s: &Settings, port: u16, peer: SocketAddr, keys: &Keys) -> Result<Option<Link>> {
    if cfg!(test) {
        return Ok(None);
    }
    Link::start(s, port, peer, keys).map(Some)
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
            // "CapraLink" is the fallback when no hostname could be read; retry so the real name shows up
            if cfg.name == "CapraLink" {
                cfg.name = hostname();
            }
            Ok(cfg)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let cfg = Config { device_id: hex(&random::<16>()), name: hostname(), settings: Settings::default(), peers: vec![] };
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
    cfg.peers.push(Peer { id: id.to_string(), name: clean_name(name), secret: hex(secret), addr });
}

/// Updates the remembered address of an already-paired peer, if it's still paired.
fn set_addr(cfg: &mut Config, id: &str, addr: String) {
    if let Some(p) = cfg.peers.iter_mut().find(|p| p.id == id) {
        p.addr = Some(addr);
    }
}

/// Parses a user-typed "ip" or "ip:port" (IPv4/IPv6 literal or hostname), defaulting to the
/// standard port when none is given.
fn resolve(addr: &str) -> Result<Vec<SocketAddr>> {
    use std::net::{IpAddr, ToSocketAddrs};
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(vec![sa]);
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, 47800)]);
    }
    let with_port = if addr.contains(':') { addr.to_string() } else { format!("{addr}:47800") };
    let addrs: Vec<SocketAddr> = with_port.to_socket_addrs().with_context(|| format!("can't resolve {addr}"))?.collect();
    ensure!(!addrs.is_empty(), "can't resolve {addr}");
    Ok(addrs)
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

fn clean_name(n: &str) -> String {
    n.chars().filter(|c| !c.is_control()).take(64).collect()
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

/// SPAKE2 on the responder's PIN + key confirmation; returns the pairing secret.
/// The responder only confirms after checking the initiator's MAC, so a guesser learns nothing.
fn pake(s: &mut TcpStream, pin: &str, initiator: bool, init_id: &str, resp_id: &str) -> Result<[u8; 32]> {
    let (st, msg) = Spake2::<Ed25519Group>::start_symmetric(&Password::new(pin.as_bytes()), &Identity::new(b"capralink-pair-v1"));
    send(s, &msg)?;
    let k = st.finish(&recv(s)?).map_err(|_| anyhow!("bad SPAKE2 message"))?;
    let (i, r) = (confirm(&k, b"initiator", init_id, resp_id), confirm(&k, b"responder", init_id, resp_id));
    if initiator {
        send(s, &i.finalize().into_bytes())?;
        r.verify_slice(&recv(s)?).map_err(|_| anyhow!("key confirmation failed"))?;
    } else {
        i.verify_slice(&recv(s)?).map_err(|_| anyhow!("key confirmation failed"))?;
        send(s, &r.finalize().into_bytes())?;
    }
    Ok(hkdf(&k, b"capralink pairing secret"))
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
    Ok((Arc::new(Ctl { stream: s.try_clone()?, noise: Mutex::new(hs.into_transport_mode()?) }), keys))
}

/// The encrypted control channel of a session.
struct Ctl {
    stream: TcpStream,
    noise: Mutex<snow::TransportState>,
}

impl Ctl {
    fn send(&self, m: &Msg) -> Result<()> {
        let pt = serde_json::to_vec(m)?;
        let mut ct = vec![0u8; pt.len() + 16];
        let mut noise = self.noise.lock().unwrap_or_else(|e| e.into_inner()); // held across the write to keep nonce order
        let n = noise.write_message(&pt, &mut ct)?;
        send(&mut &self.stream, &ct[..n])
    }

    /// `Ok(None)` = authentic but unknown message (newer peer).
    fn recv(&self) -> io::Result<Option<Msg>> {
        let ct = recv(&mut &self.stream)?;
        let mut pt = vec![0u8; ct.len()];
        let n = self.noise.lock().unwrap_or_else(|e| e.into_inner()).read_message(&ct, &mut pt).map_err(io::Error::other)?;
        Ok(serde_json::from_slice(&pt[..n]).ok())
    }

    fn close(&self) {
        let _ = self.send(&Msg::Stop);
        let _ = self.stream.shutdown(Shutdown::Both);
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

// ponytail: a read timeout landing mid-frame desyncs the stream and ends the session; frames
// are tiny so it hasn't been seen. Buffer partial frames if it ever is.
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
        let wrong = format!("{:06}", (pin.parse::<u32>().unwrap() + 1) % 1_000_000);
        assert!(a.pair_addr(&[addr(&b)], &wrong).is_err());
        assert!(peers(&adir).is_empty() && peers(&bdir).is_empty());
        assert_eq!(b.state().pin, pin, "one failure keeps the PIN");
        for _ in 0..4 {
            assert!(a.pair_addr(&[addr(&b)], &wrong).is_err());
        }
        assert_ne!(b.state().pin, pin, "5 failures rotate the PIN");

        let pin = b.state().pin;
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
        a.remote_set(&bid, want).unwrap();
        let s = load(&bdir).unwrap().settings;
        assert_eq!((s.bitrate, s.channels, s.service, s.remote_config), (32_000, 2, false, true));
        wait(both); // b restarts its link for the new audio settings
        let _ = std::fs::remove_dir_all(adir);
        let _ = std::fs::remove_dir_all(bdir);
    }

    #[test]
    fn old_config_and_hello_without_new_fields_parse() {
        let cfg: Config = serde_json::from_str(
            r#"{"device_id":"a","name":"b","input":null,"output":null,"bitrate":48000,"channels":1,
                "peers":[{"id":"1","name":"c","secret":"00"}]}"#,
        )
        .unwrap();
        assert!(cfg.peers[0].addr.is_none());
        assert!(!cfg.settings.remote_config);

        let m: Msg = serde_json::from_str(r#"{"type":"pair","id":"a","name":"b"}"#).unwrap();
        assert!(matches!(m, Msg::Pair { port: None, .. }));
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
}
