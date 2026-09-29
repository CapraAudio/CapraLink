// CapraLink tray app: hosts the engine and a small settings window shown on demand.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use capralink_engine::{Node, NodeState, Settings};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder,
};

/// The node, created once at startup so pairing and incoming links work with the window closed.
/// Holds the startup error instead (e.g. port in use) so the window can show it.
struct App(Result<Node, String>);

impl App {
    fn node(&self) -> Result<&Node, String> {
        self.0.as_ref().map_err(Clone::clone)
    }

    fn run(&self, f: impl FnOnce(&Node) -> anyhow::Result<()>) -> Result<(), String> {
        f(self.node()?).map_err(|e| format!("{e:#}"))
    }
}

#[derive(Serialize)]
struct Devices {
    inputs: Vec<String>,
    outputs: Vec<String>,
}

#[tauri::command]
fn devices() -> Devices {
    Devices { inputs: capralink_engine::input_devices(), outputs: capralink_engine::output_devices() }
}

#[tauri::command]
fn state(app: State<App>) -> Result<NodeState, String> {
    Ok(app.node()?.state())
}

// Network actions block for up to a few seconds: `async` keeps them off the UI thread.
#[tauri::command(async)]
fn pair(app: State<App>, id: String, pin: String) -> Result<(), String> {
    app.run(|n| n.pair(&id, &pin))
}

#[tauri::command(async)]
fn connect(app: State<App>, id: String) -> Result<(), String> {
    app.run(|n| n.connect(&id))
}

#[tauri::command(async)]
fn disconnect(app: State<App>) -> Result<(), String> {
    app.run(|n| {
        n.disconnect();
        Ok(())
    })
}

#[tauri::command(async)]
fn forget(app: State<App>, id: String) -> Result<(), String> {
    app.run(|n| n.forget(&id))
}

#[tauri::command(async)]
fn set_settings(app: State<App>, settings: Settings) -> Result<(), String> {
    app.run(|n| n.set_settings(settings))
}

const WINDOW_LABEL: &str = "main";

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
        let _ = win.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("CapraLink")
        .inner_size(360.0, 660.0)
        .min_inner_size(360.0, 480.0)
        .build();
}

fn main() {
    tauri::Builder::default()
        .manage(App(Node::start(None, 47800, true).map_err(|e| format!("{e:#}"))))
        .invoke_handler(tauri::generate_handler![devices, state, pair, connect, disconnect, forget, set_settings])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open = MenuItem::with_id(app, "open", "Open CapraLink", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().unwrap())
                .icon_as_template(true)
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            show_window(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            RunEvent::Exit => {
                if let Ok(n) = app.state::<App>().node() {
                    n.shutdown();
                }
            }
            // closing the last window keeps the tray running; explicit Quit (code set) exits
            RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
            // launching the app again while it runs (Finder, `open`) brings the window back
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_window(app),
            _ => {}
        });
}
