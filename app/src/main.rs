// CapraLink tray app: hosts the engine and a small settings window shown on demand.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use capralink_engine::{Config, Link, Stats};
use serde::Serialize;
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder,
};

#[derive(Default)]
struct AppState {
    link: Mutex<Option<Link>>,
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
fn start(
    state: tauri::State<AppState>,
    peer: String,
    port: u16,
    input: Option<String>,
    output: Option<String>,
    bitrate: i32,
    channels: u16,
) -> Result<(), String> {
    let addr = capralink_engine::resolve_peer(&peer).map_err(|e| format!("{e:#}"))?;
    let cfg = Config { peer: addr, port, input, output, bitrate, channels };
    let mut guard = state.link.lock().unwrap();
    *guard = None; // drop any existing link first so its UDP port frees
    let link = Link::start(cfg).map_err(|e| e.to_string())?;
    *guard = Some(link);
    Ok(())
}

#[tauri::command]
fn stop(state: tauri::State<AppState>) {
    state.link.lock().unwrap().take();
}

#[tauri::command]
fn stats(state: tauri::State<AppState>) -> Option<Stats> {
    state.link.lock().unwrap().as_ref().map(|l| l.stats())
}

const WINDOW_LABEL: &str = "main";

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
        let _ = win.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("CapraLink")
        .inner_size(360.0, 600.0)
        .resizable(false)
        .build();
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![devices, start, stop, stats])
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
            // closing the last window keeps the tray running; explicit Quit (code set) exits
            RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
            // launching the app again while it runs (Finder, `open`) brings the window back
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_window(app),
            _ => {}
        });
}
