// CapraLink: `capralink --daemon` runs the headless engine; plain `capralink` is the tray app,
// a client of that engine (MASTER.md §3.6).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use capralink_engine::log::log;
use capralink_engine::{Check, Client, Devices, MicCheck, NodeState, RemoteConfig, Settings};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;

/// Connection to the engine daemon, (re)made on demand.
struct App {
    dir: Option<PathBuf>,
    port: u16,
    client: Mutex<Option<Client>>,
}

impl App {
    fn client(&self) -> Result<Client, String> {
        let mut c = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if c.is_none() {
            *c = Some(self.connect().map_err(|e| {
                log(&format!("can't reach the engine: {e:#}"));
                format!("Can't reach the CapraLink engine: {e:#}")
            })?);
        }
        Ok(c.clone().expect("set above"))
    }

    /// Connects to the running daemon, starting one if none answers.
    fn connect(&self) -> anyhow::Result<Client> {
        let rpc = self.port.checked_add(1).ok_or_else(|| anyhow::anyhow!("port 65535 leaves no room for the engine port above it"))?;
        let local = || Client::local(self.dir.clone(), rpc);
        if let Ok(c) = local() {
            return Ok(c);
        }
        spawn_daemon()?;
        let t = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            match local() {
                Ok(c) => return Ok(c),
                Err(e) if t.elapsed() > Duration::from_secs(5) => return Err(e),
                Err(_) => {}
            }
        }
    }

    fn run<T>(&self, f: impl FnOnce(&Client) -> anyhow::Result<T>) -> Result<T, String> {
        f(&self.client()?).map_err(|e| {
            if e.downcast_ref::<std::io::Error>().is_none() && e.to_string() != "unauthorized" {
                return format!("{e:#}");
            }
            *self.client.lock().unwrap_or_else(|e| e.into_inner()) = None; // reconnect next time
            log(&format!("lost the engine: {e:#}"));
            format!("Can't reach the CapraLink engine: {e:#}")
        })
    }
}

/// Starts `<self> --daemon` detached from this process and its terminal.
fn spawn_daemon() -> std::io::Result<()> {
    let mut c = Command::new(capralink_engine::daemon_exe()?);
    c.arg("--daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000 | 0x0000_0008); // CREATE_NO_WINDOW | DETACHED_PROCESS
    let mut child = c.spawn()?;
    std::thread::spawn(move || child.wait()); // reap it if it exits while the UI runs
    Ok(())
}

// Every command may wait for the engine to start: `async` keeps them off the UI thread.
#[tauri::command(async)]
fn devices(app: State<App>) -> Result<Devices, String> {
    app.run(Client::devices)
}

#[tauri::command(async)]
fn state(app: State<App>) -> Result<NodeState, String> {
    app.run(Client::state)
}

/// Sizes the window to its content height (the page measures itself), keeping the width.
#[tauri::command]
fn fit(window: tauri::WebviewWindow, height: f64) {
    let width = window.inner_size().ok().zip(window.scale_factor().ok()).map_or(360.0, |(s, f)| s.to_logical::<f64>(f).width);
    let _ = window.set_size(tauri::LogicalSize::new(width, height.clamp(300.0, 1000.0)));
}

/// Opens the latest release page in the browser (fixed URL: the page can't open anything else).
#[tauri::command]
fn open_releases() -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    capralink_engine::system_command(opener).arg(RELEASES).spawn().map(drop).map_err(|e| e.to_string())
}

const RELEASES: &str = "https://github.com/CapraAudio/CapraLink/releases/latest";

/// This build's version (the Cargo/Tauri version), shown in the window's corner.
#[tauri::command]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The update this install can apply in place, if any. Not for Linux deb/rpm (the updater only
/// replaces an AppImage), nor on macOS when the release's audio drivers (`drivers` in latest.json,
/// the git tree hash of drivers/macos, see build.rs) differ from this build's: the app-only
/// update would leave the old drivers installed.
async fn update_available(app: &AppHandle) -> Result<Option<tauri_plugin_updater::Update>, String> {
    if cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_none() {
        return Ok(None);
    }
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    Ok(update.filter(|u| !cfg!(target_os = "macos") || u.raw_json["drivers"].as_str() == Some(env!("DRIVERS_REV"))))
}

#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    notes: String,
}

/// The in-app update on offer, or None (up to date, or this install can't update itself).
#[tauri::command]
async fn update_check(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    Ok(update_available(&app).await?.map(|u| UpdateInfo { version: u.version, notes: u.body.unwrap_or_default() }))
}

/// Downloads and verifies the update, stops the engine (it must restart on the new version, and
/// Windows can't replace a running exe), installs it and relaunches. Only returns on failure.
#[tauri::command]
async fn update_install(app: AppHandle) -> Result<(), String> {
    let update = update_available(&app).await?.ok_or("No update available")?;
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| format!("Can't download the update: {e}"))?;
    let a = app.state::<App>();
    let client = a.client.lock().unwrap_or_else(|e| e.into_inner()).take();
    // the engine may be running without this window having connected to it
    if let Some(c) = client.or_else(|| Client::local(a.dir.clone(), a.port.checked_add(1)?).ok()) {
        let _ = c.shutdown();
        std::thread::sleep(Duration::from_secs(1)); // let it exit
    }
    update.install(bytes).map_err(|e| format!("Can't install the update: {e}"))?;
    app.restart() // Windows: install() already exited, the installer relaunches
}

#[tauri::command(async)]
fn levels(app: State<App>) -> Result<Option<(f32, f32)>, String> {
    app.run(Client::levels)
}

#[tauri::command(async)]
fn pair(app: State<App>, id: String, pin: String) -> Result<(), String> {
    app.run(|c| c.pair(&id, &pin))
}

#[tauri::command(async)]
fn pair_ip(app: State<App>, addr: String, pin: String) -> Result<String, String> {
    app.run(|c| c.pair_ip(&addr, &pin))
}

#[tauri::command(async)]
fn connect(app: State<App>, id: String) -> Result<(), String> {
    app.run(|c| c.connect(&id))
}

#[tauri::command(async)]
fn open_pairing(app: State<App>) -> Result<(), String> {
    app.run(Client::open_pairing)
}

#[tauri::command(async)]
fn disconnect(app: State<App>) -> Result<(), String> {
    app.run(Client::disconnect)
}

#[tauri::command(async)]
fn forget(app: State<App>, id: String) -> Result<(), String> {
    app.run(|c| c.forget(&id))
}

#[tauri::command(async)]
fn patch_settings(app: State<App>, patch: serde_json::Value) -> Result<(), String> {
    app.run(|c| c.patch_settings(patch))
}

#[tauri::command(async)]
fn set_peer_addr(app: State<App>, id: String, addr: String) -> Result<(), String> {
    app.run(|c| c.set_peer_addr(&id, &addr))
}

#[tauri::command(async)]
fn set_name(app: State<App>, name: String) -> Result<(), String> {
    app.run(|c| c.set_name(&name))
}

#[tauri::command(async)]
fn remote_get(app: State<App>, id: String) -> Result<RemoteConfig, String> {
    app.run(|c| c.remote_get(&id))
}

#[tauri::command(async)]
fn remote_set(app: State<App>, id: String, settings: Settings, name: Option<String>) -> Result<(), String> {
    app.run(|c| c.remote_set(&id, &settings, name.as_deref()))
}

#[tauri::command(async)]
fn checks(app: State<App>) -> Result<Vec<Check>, String> {
    app.run(Client::checks)
}

#[tauri::command(async)]
fn test_tone(app: State<App>) -> Result<(), String> {
    app.run(Client::test_tone)
}

#[tauri::command(async)]
fn mic_check(app: State<App>) -> Result<MicCheck, String> {
    app.run(Client::mic_check)
}

/// Saves the diagnostics (this computer's, plus paired device `peer`'s) where the user picks
/// in a Save dialog; returns the path, or None if cancelled. The page never names the path.
#[tauri::command(async)]
fn export_diagnostics(window: tauri::WebviewWindow, app: State<App>, redact: bool, peer: Option<String>) -> Result<Option<String>, String> {
    let text = app.run(|c| c.diagnostics(redact, peer.as_deref()))?;
    let name = format!("CapraLink-diagnostics-{}.txt", &capralink_engine::log::now()[..10]);
    let Some(path) = window.dialog().file().set_parent(&window).set_file_name(name).add_filter("Text", &["txt"]).blocking_save_file() else { return Ok(None) };
    let path = path.into_path().map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("Can't save {}: {e}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

/// Tray Quit: the engine goes too unless it is meant to run in the background. Never waits more
/// than 2 s on the engine, so Quit works even if the engine is stuck.
fn quit(app: &AppHandle) {
    // never waits on the lock either: it is held while the engine is (re)started
    let client = app.state::<App>().client.try_lock().ok().and_then(|c| c.clone());
    if let Some(c) = client {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if c.state().is_ok_and(|s| !s.settings.service) {
                let _ = c.shutdown();
            }
            let _ = done.send(());
        });
        let _ = finished.recv_timeout(Duration::from_secs(2));
    }
    app.exit(0);
}

/// Tray "Music Mode": flips the daemon's setting (read fresh, so nothing else is reset), then
/// shows what the daemon actually has. Off the main thread: the daemon may need starting.
fn toggle_music(app: &AppHandle, item: CheckMenuItem<tauri::Wry>) {
    let app = app.clone();
    std::thread::spawn(move || {
        let a = app.state::<App>();
        let _ = a.run(|c| {
            let on = c.state()?.settings.music_mode;
            c.patch_settings(serde_json::json!({ "music_mode": !on }))
        });
        if let Ok(st) = a.run(Client::state) {
            let _ = item.set_checked(st.settings.music_mode);
        }
    });
}

/// Keeps the tray check in step with changes made elsewhere (window, remote config).
/// Only asks an engine this UI is already connected to; never starts one.
fn sync_music(app: AppHandle, item: CheckMenuItem<tauri::Wry>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        // the open window already polls state; a second reader would reset its gap meters
        if app.get_webview_window(WINDOW_LABEL).is_some() {
            continue;
        }
        // never waits on the lock either: it is held while the engine is (re)started
    let client = app.state::<App>().client.try_lock().ok().and_then(|c| c.clone());
        if let Some(st) = client.and_then(|c| c.state().ok()) {
            if item.is_checked().is_ok_and(|on| on != st.settings.music_mode) {
                let _ = item.set_checked(st.settings.music_mode);
            }
        }
    });
}

/// `--status`, `--connect NAME_OR_ID`, `--disconnect`, `--music on|off`. Needs the engine already
/// running (it never starts one: a script shouldn't leave a daemon behind). Returns what to print.
fn cli(args: &[String], dir: Option<PathBuf>, port: u16) -> anyhow::Result<Option<String>> {
    let rpc = port.checked_add(1).ok_or_else(|| anyhow::anyhow!("port 65535 leaves no room for the engine port above it"))?;
    let c = Client::local(dir, rpc).map_err(|e| anyhow::anyhow!("the CapraLink engine isn't running ({e}); open CapraLink first"))?;
    let st = c.state()?;
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--status"] => {
            let peers: Vec<_> = st.devices.iter().filter(|d| d.paired).collect();
            let connected = peers.iter().find(|d| d.connected).map(|d| serde_json::json!({ "id": d.id, "name": d.name }));
            let devices: Vec<_> = peers.iter().map(|d| serde_json::json!({ "id": d.id, "name": d.name, "online": d.online, "connected": d.connected })).collect();
            // the peer's own Music Mode isn't part of NodeState, so `music_mode` is this computer's setting
            let quality = st.quality.as_ref().map(|q| q.grade.as_str());
            Ok(Some(serde_json::json!({ "connected": connected, "devices": devices, "music_mode": st.settings.music_mode, "quality": quality, "error": st.error }).to_string()))
        }
        ["--connect", who] => {
            let d = st.devices.iter().find(|d| d.paired && (d.id == who || d.name.eq_ignore_ascii_case(who)));
            c.connect(&d.ok_or_else(|| anyhow::anyhow!("no paired device named {who}"))?.id).map(|_| None)
        }
        ["--disconnect"] => c.disconnect().map(|_| None),
        ["--music", v] => c.patch_settings(serde_json::json!({ "music_mode": parse_on_off(v)? })).map(|_| None),
        _ => anyhow::bail!("usage: capralink --status | --connect NAME_OR_ID | --disconnect | --music on|off"),
    }
}

fn parse_on_off(v: &str) -> anyhow::Result<bool> {
    match v {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => anyhow::bail!("--music takes on or off, not {v}"),
    }
}

const WINDOW_LABEL: &str = "main";

// macOS menu bar: black silhouette that the system recolours; elsewhere: white for dark panels
#[cfg(target_os = "macos")]
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray-template.png");
#[cfg(not(target_os = "macos"))]
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
        let _ = win.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("CapraLink")
        .inner_size(360.0, 700.0)
        .min_inner_size(360.0, 300.0)
        .build();
}

fn main() {
    // Test overrides, honoured by both the daemon and the UI.
    let dir: Option<PathBuf> = std::env::var_os("CAPRALINK_CONFIG_DIR").map(Into::into);
    let port = std::env::var("CAPRALINK_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(47800);

    // Before any Tauri/GTK/webview init, so the engine runs without a display.
    if std::env::args().any(|a| a == "--daemon") {
        if let Err(e) = capralink_engine::daemon(dir, port) {
            log(&format!("engine stopped: {e:#}"));
            std::process::exit(1);
        }
        return;
    }

    // Scriptable client (Steam Deck plugin, shell): talks to a running engine, never opens the UI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--status" | "--connect" | "--disconnect" | "--music")) {
        match cli(&args, dir, port) {
            Ok(Some(out)) => println!("{out}"),
            Ok(None) => {}
            Err(e) => {
                eprintln!("capralink: {e:#}");
                std::process::exit(1);
            }
        }
        return;
    }

    capralink_engine::log::init(dir.clone(), "ui");
    let app = App { dir, port, client: Mutex::new(None) };
    tauri::Builder::default()
        // a second launch (no tray on stock GNOME, Start menu on Windows) brings this window back
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_window(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app)
        .invoke_handler(tauri::generate_handler![devices, state, version, update_check, update_install, open_releases, levels, fit, pair, pair_ip, open_pairing, connect, disconnect, forget, patch_settings, set_peer_addr, set_name, remote_get, remote_set, checks, test_tone, mic_check, export_diagnostics])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open = MenuItem::with_id(app, "open", "Open CapraLink", true, None::<&str>)?;
            let music = CheckMenuItem::with_id(app, "music", "Music Mode", true, false, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &music, &quit_item])?;
            sync_music(app.handle().clone(), music.clone());

            TrayIconBuilder::new()
                .icon(tauri::image::Image::from_bytes(TRAY_ICON)?)
                .icon_as_template(cfg!(target_os = "macos"))
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => show_window(app),
                    "music" => toggle_music(app, music.clone()),
                    "quit" => quit(app),
                    _ => {}
                })
                // double-click opens the window where the platform reports it (Windows); macOS
                // opens the menu on the first click and Linux trays send no clicks at all
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;

            show_window(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match (app, event) {
            // closing the last window keeps the tray running; explicit Quit (code set) exits
            (_, RunEvent::ExitRequested { api, code: None, .. }) => api.prevent_exit(),
            // launching the app again while it runs (Finder, `open`) brings the window back
            #[cfg(target_os = "macos")]
            (app, RunEvent::Reopen { .. }) => show_window(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn music_arg() {
        assert!(super::parse_on_off("on").unwrap());
        assert!(!super::parse_on_off("off").unwrap());
        assert!(super::parse_on_off("yes").is_err());
    }
}
