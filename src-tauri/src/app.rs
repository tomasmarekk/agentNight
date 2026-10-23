//! Tauri shell: tray icon, the small window and the commands the UI calls.

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::window::Color;
use tauri::{
    AppHandle, Emitter, LogicalPosition, Manager, RunEvent, State, WebviewUrl,
    WebviewWindowBuilder, WindowEvent, Wry,
};

use crate::config::Config;
use crate::monitor::{Cmd, Monitor, Snapshot};
use crate::policy::Phase;

const WINDOW: &str = "main";
const WIDTH: f64 = 340.0;
const HEIGHT: f64 = 460.0;

#[cfg(target_os = "macos")]
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray-template.png");
#[cfg(target_os = "macos")]
const TRAY_ARMED: &[u8] = include_bytes!("../icons/tray-template-armed.png");
#[cfg(not(target_os = "macos"))]
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray.png");
#[cfg(not(target_os = "macos"))]
const TRAY_ARMED: &[u8] = include_bytes!("../icons/tray-armed.png");

struct AppState {
    tx: Sender<Cmd>,
    snapshot: Mutex<Snapshot>,
    /// Where the window was when it was last hidden.
    position: Mutex<Option<LogicalPosition<f64>>>,
    tray: Mutex<Option<Tray>>,
}

struct Tray {
    icon: TrayIcon,
    arm: CheckMenuItem<Wry>,
    /// (armed, can_arm, active, phase) last shown, to skip redundant updates.
    shown: Option<(bool, bool, usize, Phase)>,
}

#[tauri::command]
fn get_state(state: State<AppState>) -> Snapshot {
    state.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn set_armed(armed: bool, state: State<AppState>) {
    let _ = state.tx.send(Cmd::Arm(armed));
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || hide(&handle));
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

pub fn run() {
    let cfg = Config::from_env();
    let (tx, rx) = mpsc::channel();
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .manage(AppState {
            tx,
            snapshot: Mutex::new(Snapshot {
                dry_run: cfg.dry_run,
                ..Snapshot::default()
            }),
            position: Mutex::new(None),
            tray: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_armed,
            hide_window,
            quit_app
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            build_tray(app.handle())?;
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("monitor".into())
                .spawn(move || {
                    Monitor::new(cfg).run(rx, |snapshot| publish(&handle, snapshot));
                })?;
            if !start_hidden {
                show(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start agentNight")
        .run(|_, event| {
            // Closing the window keeps the app running in the tray; only Quit exits.
            if let RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                api.prevent_exit();
            }
        });
}

fn publish(app: &AppHandle, snapshot: &Snapshot) {
    let state = app.state::<AppState>();
    let previous = std::mem::replace(&mut *state.snapshot.lock().unwrap(), snapshot.clone());
    let _ = app.emit("state", snapshot);
    update_tray(app, snapshot);

    let countdown_started =
        snapshot.phase == Phase::Countdown && previous.phase != Phase::Countdown;
    let new_notice = snapshot.notice.is_some() && snapshot.notice != previous.notice;
    if countdown_started || (new_notice && !snapshot.armed && previous.armed) {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || show(&handle));
    }
}

/// Shows the window, creating it if needed. The window is destroyed when hidden so the
/// webview does not use memory while agentNight sits in the tray.
fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let state = app.state::<AppState>();
    let position = *state.position.lock().unwrap();
    let builder = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App("index.html".into()))
        .title("agentNight")
        .inner_size(WIDTH, HEIGHT)
        .resizable(false)
        .maximizable(false)
        .decorations(false)
        .shadow(true)
        .background_color(Color(17, 17, 19, 255))
        .focused(true);
    let builder = match position {
        Some(p) => builder.position(p.x, p.y),
        None => builder.center(),
    };
    match builder.build() {
        Ok(window) => {
            let tx = state.tx.clone();
            let handle = app.clone();
            window.on_window_event(move |event| match event {
                WindowEvent::CloseRequested { .. } => remember_position(&handle),
                WindowEvent::Destroyed => {
                    let _ = tx.send(Cmd::Visible(false));
                }
                _ => {}
            });
            let _ = state.tx.send(Cmd::Visible(true));
        }
        Err(e) => eprintln!("agentNight: cannot open window: {e}"),
    }
}

fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW) {
        remember_position(app);
        let _ = w.destroy();
    }
}

fn toggle(app: &AppHandle) {
    match app.get_webview_window(WINDOW) {
        Some(w) if w.is_focused().unwrap_or(false) => hide(app),
        _ => show(app),
    }
}

fn remember_position(app: &AppHandle) {
    let Some(w) = app.get_webview_window(WINDOW) else {
        return;
    };
    if let (Ok(pos), Ok(scale)) = (w.outer_position(), w.scale_factor()) {
        *app.state::<AppState>().position.lock().unwrap() = Some(pos.to_logical(scale));
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open agentNight", true, None::<&str>)?;
    let arm = CheckMenuItem::with_id(
        app,
        "arm",
        "Shut down when done",
        false,
        false,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit agentNight", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &arm, &PredefinedMenuItem::separator(app)?, &quit],
    )?;
    let icon = TrayIconBuilder::with_id("tray")
        .icon(Image::from_bytes(TRAY_IDLE)?)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("agentNight")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show(app),
            "arm" => {
                let state = app.state::<AppState>();
                let armed = state.snapshot.lock().unwrap().armed;
                let _ = state.tx.send(Cmd::Arm(!armed));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle(tray.app_handle());
            }
        })
        .build(app)?;
    *app.state::<AppState>().tray.lock().unwrap() = Some(Tray {
        icon,
        arm,
        shown: None,
    });
    Ok(())
}

fn update_tray(app: &AppHandle, s: &Snapshot) {
    let state = app.state::<AppState>();
    let mut guard = state.tray.lock().unwrap();
    let Some(tray) = guard.as_mut() else { return };
    let key = (s.armed, s.can_arm, s.active, s.phase);
    // The check mark toggles itself on click, so always resync it.
    let _ = tray.arm.set_checked(s.armed);
    if tray.shown == Some(key) {
        return;
    }
    let armed_changed = tray.shown.is_none_or(|k| k.0 != s.armed);
    tray.shown = Some(key);

    let _ = tray.arm.set_enabled(s.can_arm);
    if armed_changed {
        let bytes = if s.armed { TRAY_ARMED } else { TRAY_IDLE };
        if let Ok(img) = Image::from_bytes(bytes) {
            let _ = tray.icon.set_icon(Some(img));
            let _ = tray.icon.set_icon_as_template(cfg!(target_os = "macos"));
        }
    }
    let agents = match s.active {
        0 => "no agents working".to_string(),
        1 => "1 agent working".to_string(),
        n => format!("{n} agents working"),
    };
    let tip = match s.phase {
        Phase::Off => format!("agentNight · {agents}"),
        Phase::Watching => format!("agentNight · shutdown armed · {agents}"),
        Phase::Settling => "agentNight · all done, shutting down soon".into(),
        Phase::Offline => "agentNight · all done, waiting for network".into(),
        Phase::Countdown => "agentNight · shutting down".into(),
    };
    let _ = tray.icon.set_tooltip(Some(tip));
}
