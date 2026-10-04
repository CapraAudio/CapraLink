//! Service mode (MASTER.md §3.6): the headless daemon, its token-authenticated loopback RPC
//! (one request/response per TCP connection) and the "run at login" agent.
//!
//! RPC wire (newline-delimited JSON; the token never crosses the socket, and the daemon proves
//! it knows the token before the client sends anything that matters):
//!   client → `{"nonce": nc}`
//!   daemon → `{"nonce": ns, "proof": HMAC(token, "daemon" nc ns)}`  (client checks it)
//!   client → `{"proof": HMAC(token, "client" ns nc), "cmd": .., "args": ..}`  (daemon checks it)
//!   daemon → `{"ok": ..}` or `{"err": ..}`, then closes.
//! Nonces are 64 hex chars; the HMACs are HMAC-SHA256 over the strings, hex-encoded.

use crate::node::{config_dir_or_default, hex, random, write_private};
use crate::{AudioDevice, Check, MicCheck, Node, NodeState, PttKey, RemoteConfig, Settings};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN_FILE: &str = "rpc.token";
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(30); // pair/connect wait on the other computer
const MAX_REQUEST: u64 = 64 * 1024;
const MAX_HANDLERS: usize = 16;
// the whole handshake + request, so a trickling client can't hold a handler slot (short under test)
const AUTH_DEADLINE: Duration = Duration::from_millis(if cfg!(test) { 1500 } else { 5000 });

#[derive(Serialize, Deserialize)]
pub struct Devices {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

/// Headless engine: the node on `port` plus the RPC server on `port + 1`. Runs until the
/// `shutdown` command or a termination signal.
pub fn daemon(config_dir: Option<PathBuf>, port: u16) -> Result<()> {
    crate::log::init(config_dir.clone(), "daemon");
    let node = Node::start(config_dir, port, true)?;
    let rpc = node.port().checked_add(1).ok_or_else(|| anyhow!("port 65535 leaves no room for the engine port above it"));
    if let Err(e) = rpc.and_then(|p| serve_rpc(node.clone(), p)) {
        node.shutdown();
        return Err(e);
    }
    let n = node.clone();
    // ponytail: best effort (a detached Windows process may have no console to hook); without it a
    // kill skips the virtual-device cleanup, which the next start redoes anyway.
    let _ = ctrlc::set_handler(move || {
        n.shutdown();
        std::process::exit(0);
    });
    loop {
        std::thread::park();
    }
}

/// Serves RPC on 127.0.0.1:`port` from a background thread, creating the token file if needed.
pub fn serve_rpc(node: Node, port: u16) -> Result<()> {
    let token = token(node.dir())?;
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| format!("engine port {port} is in use"))?;
    let live = Arc::new(()); // one clone per running handler
    std::thread::Builder::new().name("capralink-rpc".into()).spawn(move || {
        for s in l.incoming().flatten() {
            if Arc::strong_count(&live) > MAX_HANDLERS {
                continue; // dropped
            }
            let (node, token, live) = (node.clone(), token.clone(), live.clone());
            let _ = std::thread::Builder::new().name("capralink-rpc-conn".into()).spawn(move || {
                handle(&node, &token, s);
                drop(live);
            });
        }
    })?;
    Ok(())
}

fn token(dir: &Path) -> Result<String> {
    match std::fs::read_to_string(dir.join(TOKEN_FILE)) {
        Ok(t) if t.trim().len() == 64 => Ok(t.trim().to_string()),
        _ => {
            let t = hex(&random::<32>());
            write_private(dir, TOKEN_FILE, t.as_bytes()).context("save engine token")?;
            Ok(t)
        }
    }
}

#[derive(Deserialize)]
struct Hello {
    nonce: String,
    #[serde(default)]
    proof: String,
}

#[derive(Deserialize)]
struct Req {
    #[serde(default)]
    proof: String,
    cmd: String,
    #[serde(default)]
    args: Value,
}

fn handle(node: &Node, token: &str, mut s: TcpStream) {
    let _ = s.set_read_timeout(Some(IO_TIMEOUT));
    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
    let (reply, quit) = match authenticate(token, &s) {
        Err(e) => {
            crate::log::log(&format!("refused a local connection: {e:#}"));
            (Err(e), false)
        }
        Ok(r) => {
            let reply = dispatch(node, &r.cmd, r.args);
            // pairing logs its own outcome (and an argument error could quote the PIN)
            if let Some(e) = reply.as_ref().err().filter(|_| !r.cmd.starts_with("pair")) {
                crate::log::log(&format!("{}: {e:#}", r.cmd));
            }
            (reply, r.cmd == "shutdown")
        }
    };
    let body = match reply {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "err": format!("{e:#}") }),
    };
    let _ = s.write_all(body.to_string().as_bytes());
    drop(s);
    if quit {
        node.shutdown();
        std::process::exit(0);
    }
}

/// The daemon's side of the handshake (see the module docs): proves itself, then returns the
/// request if the client's proof checks out.
fn authenticate(token: &str, mut s: &TcpStream) -> Result<Req> {
    let mut r = BufReader::new(Deadline(s, Instant::now() + AUTH_DEADLINE).take(MAX_REQUEST));
    let theirs: Hello = line(&mut r)?;
    ensure!(theirs.nonce.len() == 64, "bad nonce");
    let mine = hex(&random::<32>());
    writeln!(s, "{}", json!({ "nonce": mine, "proof": proof(token, "daemon", &theirs.nonce, &mine) }))?;
    let req: Req = line(&mut r)?;
    ensure!(same(req.proof.as_bytes(), proof(token, "client", &mine, &theirs.nonce).as_bytes()), "unauthorized");
    Ok(req)
}

/// Reads that time out at an absolute deadline, however the bytes arrive.
struct Deadline<'a>(&'a TcpStream, Instant);

impl Read for Deadline<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let left = self.1.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.0.set_read_timeout(Some(left))?;
        self.0.read(b)
    }
}

fn proof(token: &str, role: &str, first: &str, second: &str) -> String {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length");
    for part in [role, first, second] {
        m.update(part.as_bytes());
    }
    hex(&m.finalize().into_bytes())
}

/// One JSON line.
fn line<T: DeserializeOwned>(r: &mut impl BufRead) -> Result<T> {
    let mut l = String::new();
    r.read_line(&mut l)?;
    Ok(serde_json::from_str(&l)?)
}

/// Constant-time equality.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn dispatch(node: &Node, cmd: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Args {
        id: String,
        pin: String,
        addr: String,
        settings: Option<Settings>,
        patch: Option<serde_json::Map<String, Value>>,
        name: Option<String>,
        redact: bool,
        peer: Option<String>,
    }
    let a: Args = if args.is_null() { Args::default() } else { serde_json::from_value(args)? };
    let done = |r: Result<()>| r.map(|()| Value::Null);
    match cmd {
        "state" => Ok(serde_json::to_value(node.state())?),
        "levels" => Ok(serde_json::to_value(node.levels())?),
        "devices" => Ok(serde_json::to_value(Devices { inputs: crate::input_devices(), outputs: crate::output_devices() })?),
        "pair" => done(node.pair(&a.id, &a.pin)),
        "pair_ip" => node.pair_ip(&a.addr, &a.pin).map(Value::from),
        "connect" => done(node.connect(&a.id)),
        "open_pairing" => done(node.open_pairing()),
        "disconnect" => {
            node.disconnect();
            Ok(Value::Null)
        }
        "forget" => done(node.forget(&a.id)),
        "set_settings" => done(node.set_settings(a.settings.ok_or_else(|| anyhow!("missing settings"))?)),
        "patch_settings" => done(node.patch_settings(&a.patch.ok_or_else(|| anyhow!("missing patch"))?)),
        "set_peer_addr" => done(node.set_peer_addr(&a.id, &a.addr)),
        "set_name" => done(node.set_name(&a.name.ok_or_else(|| anyhow!("missing name"))?)),
        "remote_get" => Ok(serde_json::to_value(node.remote_get(&a.id)?)?),
        "remote_set" => done(node.remote_set(&a.id, a.settings.ok_or_else(|| anyhow!("missing settings"))?, a.name)),
        "checks" => Ok(serde_json::to_value(node.checks())?),
        "test_tone" => done(crate::test_tone(&node.settings().output)),
        "mic_check" => Ok(serde_json::to_value(crate::mic_check(&node.settings().input)?)?),
        "ptt_capture" => Ok(serde_json::to_value(node.ptt_capture()?)?),
        "diagnostics" => {
            let mut text = node.diagnostics(a.redact);
            if let Some(peer) = a.peer {
                text += "\n\n==== The other computer ====\n";
                match node.remote_diagnostics(&peer, a.redact) {
                    Ok(t) => text += &t,
                    Err(e) => text += &format!("Couldn't get its diagnostics: {e:#}\n"),
                }
            }
            Ok(Value::from(text))
        }
        "shutdown" => Ok(Value::Null), // `handle` exits after replying
        _ => bail!("unknown command {cmd}"),
    }
}

/// RPC client for the local daemon. Transport failures come back as `io::Error`s (downcastable).
#[derive(Clone)]
pub struct Client {
    port: u16,
    token: String,
}

impl Client {
    /// Reads the daemon's token from the config dir and checks that it answers on `port`.
    pub fn local(config_dir: Option<PathBuf>, port: u16) -> Result<Client> {
        let dir = config_dir_or_default(config_dir)?;
        let token = std::fs::read_to_string(dir.join(TOKEN_FILE))?.trim().to_string();
        let c = Client { port, token };
        c.state()?;
        Ok(c)
    }

    fn call<T: DeserializeOwned>(&self, cmd: &str, args: Value) -> Result<T> {
        let s = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), Duration::from_secs(1))?;
        s.set_read_timeout(Some(CALL_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;
        let mine = hex(&random::<32>());
        writeln!(&s, "{}", json!({ "nonce": mine }))?;
        let mut r = BufReader::new(&s);
        let theirs: Hello = line(&mut r).context("bad reply from the engine")?;
        // whatever answers here must know the token before it gets anything
        if !same(theirs.proof.as_bytes(), proof(&self.token, "daemon", &mine, &theirs.nonce).as_bytes()) {
            bail!("unauthorized");
        }
        writeln!(&s, "{}", json!({ "proof": proof(&self.token, "client", &theirs.nonce, &mine), "cmd": cmd, "args": args }))?;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)?;
        #[derive(Deserialize)]
        struct Resp {
            ok: Option<Value>,
            err: Option<String>,
        }
        let r: Resp = serde_json::from_slice(&buf).context("bad reply from the engine")?;
        if let Some(e) = r.err {
            bail!("{e}");
        }
        Ok(serde_json::from_value(r.ok.unwrap_or(Value::Null))?)
    }

    pub fn state(&self) -> Result<NodeState> {
        self.call("state", Value::Null)
    }

    /// (sending, receiving) VU levels, or None when not streaming.
    pub fn levels(&self) -> Result<Option<(f32, f32)>> {
        self.call("levels", Value::Null)
    }

    pub fn devices(&self) -> Result<Devices> {
        self.call("devices", Value::Null)
    }

    pub fn pair(&self, id: &str, pin: &str) -> Result<()> {
        self.call("pair", json!({ "id": id, "pin": pin }))
    }

    pub fn pair_ip(&self, addr: &str, pin: &str) -> Result<String> {
        self.call("pair_ip", json!({ "addr": addr, "pin": pin }))
    }

    pub fn connect(&self, id: &str) -> Result<()> {
        self.call("connect", json!({ "id": id }))
    }

    /// Opens this computer's pairing window (see `Node::open_pairing`).
    pub fn open_pairing(&self) -> Result<()> {
        self.call("open_pairing", Value::Null)
    }

    pub fn disconnect(&self) -> Result<()> {
        self.call("disconnect", Value::Null)
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        self.call("forget", json!({ "id": id }))
    }

    pub fn set_settings(&self, s: &Settings) -> Result<()> {
        self.call("set_settings", json!({ "settings": s }))
    }

    /// Changes only the given `Settings` fields (a JSON object), keeping every other one as the
    /// engine has it.
    pub fn patch_settings(&self, patch: Value) -> Result<()> {
        self.call("patch_settings", json!({ "patch": patch }))
    }

    pub fn set_peer_addr(&self, id: &str, addr: &str) -> Result<()> {
        self.call("set_peer_addr", json!({ "id": id, "addr": addr }))
    }

    pub fn set_name(&self, name: &str) -> Result<()> {
        self.call("set_name", json!({ "name": name }))
    }

    pub fn remote_get(&self, id: &str) -> Result<RemoteConfig> {
        self.call("remote_get", json!({ "id": id }))
    }

    /// `name` renames the target too, when given.
    pub fn remote_set(&self, id: &str, s: &Settings, name: Option<&str>) -> Result<()> {
        self.call("remote_set", json!({ "id": id, "settings": s, "name": name }))
    }

    pub fn checks(&self) -> Result<Vec<Check>> {
        self.call("checks", Value::Null)
    }

    /// Plays a test tone on the Play to device.
    pub fn test_tone(&self) -> Result<()> {
        self.call("test_tone", Value::Null)
    }

    /// Records 3 s from the Send from device (well inside `CALL_TIMEOUT`).
    pub fn mic_check(&self) -> Result<MicCheck> {
        self.call("mic_check", Value::Null)
    }

    /// Waits (up to 10 s, well inside `CALL_TIMEOUT`) for the next key or button press, for
    /// the push-to-talk "Set button".
    pub fn ptt_capture(&self) -> Result<PttKey> {
        self.call("ptt_capture", Value::Null)
    }

    /// This computer's diagnostics, plus paired device `peer`'s when given.
    pub fn diagnostics(&self, redact: bool, peer: Option<&str>) -> Result<String> {
        self.call("diagnostics", json!({ "redact": redact, "peer": peer }))
    }

    pub fn shutdown(&self) -> Result<()> {
        self.call("shutdown", Value::Null)
    }
}

// ---------- login agent ----------

/// What to launch as the daemon: the AppImage itself on Linux (its mount path is temporary),
/// else this binary.
pub fn daemon_exe() -> io::Result<PathBuf> {
    match std::env::var_os("APPIMAGE") {
        Some(p) if !p.is_empty() => Ok(p.into()),
        _ => std::env::current_exe(),
    }
}

/// Installs (or removes) the login agent that runs `<exe> --daemon`. Takes effect at next
/// login; the running daemon is left alone.
pub(crate) fn login_agent(on: bool) -> Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    set_agent(on, &daemon_exe()?)
}

#[cfg(target_os = "macos")]
fn set_agent(on: bool, exe: &Path) -> Result<()> {
    let dir = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?.join("Library/LaunchAgents");
    let path = dir.join("com.capraaudio.capralink.plist");
    if on {
        std::fs::create_dir_all(&dir)?;
        return Ok(std::fs::write(&path, plist(exe))?);
    }
    // Not `launchctl bootout`: when launchd started this daemon that would kill it mid-request.
    // The job stays loaded until logout; it only restarts on a crash.
    remove(&path)
}

#[cfg(target_os = "linux")]
fn set_agent(on: bool, exe: &Path) -> Result<()> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow!("no config directory"))?.join("systemd/user");
    let path = dir.join("capralink.service");
    if on {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, unit(exe))?;
        return run("systemctl", &["--user", "enable", "capralink.service"]);
    }
    let _ = run("systemctl", &["--user", "disable", "capralink.service"]);
    remove(&path)
}

#[cfg(windows)]
fn set_agent(on: bool, exe: &Path) -> Result<()> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if on {
        return run("reg", &["add", KEY, "/v", "CapraLink", "/t", "REG_SZ", "/d", &run_value(exe), "/f"]);
    }
    let _ = run("reg", &["delete", KEY, "/v", "CapraLink", "/f"]); // fails if already absent
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn set_agent(_on: bool, _exe: &Path) -> Result<()> {
    bail!("not supported on this system")
}

#[allow(dead_code)] // each OS uses one of these helpers
fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[allow(dead_code)]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let mut c = crate::system_command(cmd);
    c.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
    let o = c.output().with_context(|| format!("run {cmd}"))?;
    ensure!(o.status.success(), "{cmd} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    Ok(())
}

#[allow(dead_code)]
fn plist(exe: &Path) -> String {
    let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.capraaudio.capralink</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
</dict>
</plist>
"#
    )
}

#[allow(dead_code)]
fn unit(exe: &Path) -> String {
    let exe = exe.display().to_string().replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    format!(
        "[Unit]\nDescription=CapraLink audio link\n\n[Service]\nExecStart=\"{exe}\" --daemon\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
    )
}

#[allow(dead_code)]
fn run_value(exe: &Path) -> String {
    format!("\"{}\" --daemon", exe.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_round_trip() {
        let dir = std::env::temp_dir().join(format!("capralink-rpc-{}", hex(&random::<8>())));
        let node = Node::start(Some(dir.clone()), 0, false).unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        serve_rpc(node.clone(), port).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(TOKEN_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let c = Client::local(Some(dir.clone()), port).unwrap();
        assert_eq!(c.state().unwrap().name, node.state().name);
        let bad = Client { port, token: "0".repeat(64) };
        assert_eq!(bad.state().map(drop).unwrap_err().to_string(), "unauthorized");
        assert_eq!(Client { port, token: String::new() }.state().map(drop).unwrap_err().to_string(), "unauthorized");
        // the daemon refuses a client that can't prove the token, too
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(&s, "{}", json!({ "nonce": "0".repeat(64) })).unwrap();
        let mut r = BufReader::new(&s);
        let h: Hello = line(&mut r).unwrap();
        let token = std::fs::read_to_string(dir.join(TOKEN_FILE)).unwrap();
        assert_eq!(h.proof, proof(token.trim(), "daemon", &"0".repeat(64), &h.nonce));
        writeln!(&s, "{}", json!({ "proof": "0".repeat(64), "cmd": "state" })).unwrap();
        assert_eq!(line::<Value>(&mut r).unwrap(), json!({ "err": "unauthorized" }));
        c.open_pairing().unwrap();
        assert!(c.state().unwrap().pairing_secs > 0);

        // `service` is a no-op agent install under test
        let s = Settings { input: Some("Mic".into()), bitrate: 32_000, channels: 2, service: true, ..Settings::default() };
        c.set_settings(&s).unwrap();
        assert_eq!(c.state().unwrap().settings, s);
        assert!(c.set_settings(&Settings { channels: 3, ..Settings::default() }).is_err());
        assert!(c.connect("nobody").is_err());
        assert!(c.set_peer_addr("nobody", "127.0.0.1").unwrap_err().to_string().contains("not paired"));
        c.disconnect().unwrap();
        assert!(c.checks().unwrap()[0].ok);
        let d = c.diagnostics(true, Some("nobody")).unwrap();
        assert!(!d.lines().any(|l| l.starts_with("This computer:")) && d.contains("Couldn't get its diagnostics"), "{d}"); // the name line is left out when hidden
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn trickling_clients_are_cut_off_at_the_deadline() {
        let dir = std::env::temp_dir().join(format!("capralink-rpc-{}", hex(&random::<8>())));
        let node = Node::start(Some(dir.clone()), 0, false).unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        serve_rpc(node, port).unwrap();
        let start = Instant::now();
        // every handler slot taken by a client sending a byte every 100 ms, well inside the per-read timeout
        let slow: Vec<TcpStream> = (0..MAX_HANDLERS)
            .map(|_| {
                let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                let w = s.try_clone().unwrap();
                std::thread::spawn(move || {
                    while start.elapsed() < AUTH_DEADLINE * 3 && (&w).write_all(b" ").is_ok() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
                s
            })
            .collect();
        for mut s in slow {
            // macOS refuses a timeout on a socket the engine already closed: that's the outcome we want
            let _ = s.set_read_timeout(Some(AUTH_DEADLINE * 3));
            // closed (a reset is fine: it was still sending), not left waiting
            if let Err(e) = s.read_to_end(&mut Vec::new()) {
                assert!(!matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{e}");
            }
        }
        assert!(start.elapsed() < AUTH_DEADLINE * 2, "{:?}", start.elapsed());
        // and the slots are free again
        std::thread::sleep(Duration::from_millis(100));
        Client::local(Some(dir.clone()), port).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_config_loads_without_service() {
        let s: Settings = serde_json::from_str(r#"{"input":null,"output":null,"bitrate":48000,"channels":1}"#).unwrap();
        assert!(!s.service);
        assert!(s.auto_reconnect, "auto-reconnect defaults to on");
    }

    #[test]
    fn agent_files() {
        let exe = Path::new("/Applications/Capra Link.app/Contents/MacOS/capralink");
        let p = plist(exe);
        assert!(p.contains("<string>/Applications/Capra Link.app/Contents/MacOS/capralink</string>") && p.contains("<string>--daemon</string>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n    <false/>"));
        let u = unit(Path::new("/home/deck/Apps/Capra\"Link%.AppImage"));
        assert!(u.contains(r#"ExecStart="/home/deck/Apps/Capra\"Link%%.AppImage" --daemon"#), "{u}");
        assert!(u.contains("Restart=on-failure") && u.contains("WantedBy=default.target"));
        assert_eq!(run_value(Path::new(r"C:\Program Files\CapraLink\capralink.exe")), r#""C:\Program Files\CapraLink\capralink.exe" --daemon"#);
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
    }
}
