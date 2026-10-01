// CapraLink: `capralink --daemon` runs the headless engine; plain `capralink` is the tray app,
// a client of that engine (MASTER.md §3.6).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use capralink_engine::{Client, Devices, NodeState, RemoteConfig, Settings};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};

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
            *c = Some(self.connect().map_err(|e| format!("Can't reach the CapraLink engine: {e:#}"))?);
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
fn set_settings(app: State<App>, settings: Settings) -> Result<(), String> {
    app.run(|c| c.set_settings(&settings))
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

/// Tray Quit: the engine goes too unless it is meant to run in the background.
fn quit(app: &AppHandle) {
    let client = app.state::<App>().client.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(c) = client {
        if c.state().is_ok_and(|s| !s.settings.service) {
            let _ = c.shutdown();
        }
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
            let s = c.state()?.settings;
            c.set_settings(&Settings { music_mode: !s.music_mode, ..s })
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
        let client = app.state::<App>().client.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(st) = client.and_then(|c| c.state().ok()) {
            if item.is_checked().is_ok_and(|on| on != st.settings.music_mode) {
                let _ = item.set_checked(st.settings.music_mode);
            }
        }
    });
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
            eprintln!("capralink --daemon: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let app = App { dir, port, client: Mutex::new(None) };
    tauri::Builder::default()
        // a second launch (no tray on stock GNOME, Start menu on Windows) brings this window back
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_window(app)))
        .manage(app)
        .invoke_handler(tauri::generate_handler![devices, state, levels, fit, pair, pair_ip, open_pairing, connect, disconnect, forget, set_settings, set_peer_addr, set_name, remote_get, remote_set])
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
