//! WrongIndex desktop (M4). A Tauri shell that displays daemon state and sends
//! authorized management requests through the shared Rust IPC client.
//!
//! Lifecycle (WI-406): closing the window hides it to the tray; "Quit Desktop"
//! exits only this application; the daemon and its watchers keep running.
//! "Stop Index Service" is a separate, confirmed action.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod bridge;
mod dialogs;

use std::time::Duration;

use bridge::{Bridge, CLIENT_NAME, ProjectAction, UiError};
use serde_json::{Value, json};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use wrongindex_ipc::{Client, DataRoot};

type Res = Result<Value, UiError>;

#[tauri::command]
async fn overview(bridge: State<'_, Bridge>) -> Res {
    Ok(bridge.overview().await)
}

#[tauri::command]
fn preview_project(bridge: State<'_, Bridge>, path: String) -> Res {
    bridge.preview(&path)
}

#[tauri::command]
async fn add_project(
    bridge: State<'_, Bridge>,
    checkout_root: String,
    index_dir: String,
    policy: String,
) -> Res {
    bridge
        .add_project(&checkout_root, &index_dir, &policy)
        .await
}

#[tauri::command]
async fn project_action(
    bridge: State<'_, Bridge>,
    project_id: String,
    action: String,
    arg: Option<String>,
) -> Res {
    let action = ProjectAction::parse(&action, arg.as_deref())?;
    bridge.project_action(&project_id, &action).await
}

#[tauri::command]
async fn project_detail(bridge: State<'_, Bridge>, project_id: String) -> Res {
    let status = bridge.project(&project_id).await?;
    let jobs = bridge.jobs(Some(&project_id), 25).await?;
    let ownership = bridge.ownership(&project_id).await?;
    Ok(json!({"status": status, "jobs": jobs["jobs"], "ownership": ownership}))
}

#[tauri::command]
async fn recent_jobs(bridge: State<'_, Bridge>) -> Res {
    bridge.jobs(None, 50).await
}

#[tauri::command]
async fn cancel_job(bridge: State<'_, Bridge>, job_id: String) -> Res {
    bridge.cancel_job(&job_id).await
}

#[tauri::command]
async fn open_folder(bridge: State<'_, Bridge>, project_id: String) -> Res {
    let root = bridge.checkout_root(&project_id).await?;
    // The path comes from the daemon's registration, never from the WebView.
    std::process::Command::new("explorer.exe")
        .arg(&root)
        .spawn()
        .map_err(|e| UiError::new("INTERNAL", e.to_string()))?;
    Ok(json!({"opened": root}))
}

/// Native folder picker (WI-402). `null` when the user cancels.
#[tauri::command]
async fn pick_folder(window: tauri::WebviewWindow) -> Res {
    let owner = owner_hwnd(&window);
    let picked =
        tauri::async_runtime::spawn_blocking(move || dialogs::pick(owner, dialogs::Pick::Folder))
            .await
            .map_err(|e| UiError::new("INTERNAL", e.to_string()))?
            .map_err(|e| UiError::new("INTERNAL", e))?;
    Ok(json!({"path": picked.map(|p| p.display().to_string())}))
}

#[tauri::command]
async fn search(bridge: State<'_, Bridge>, project_id: String, query: String) -> Res {
    bridge.search(&project_id, &query).await
}

/// Category sizes of the bundle that would be exported; nothing is written.
#[tauri::command]
async fn diagnostics_preview(bridge: State<'_, Bridge>, redact_paths: bool) -> Res {
    bridge.diagnostics(redact_paths, None).await
}

/// Ask where to save, then export. `{"saved": null}` when cancelled.
#[tauri::command]
async fn export_diagnostics(
    bridge: State<'_, Bridge>,
    window: tauri::WebviewWindow,
    redact_paths: bool,
) -> Res {
    let owner = owner_hwnd(&window);
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialogs::pick(
            owner,
            dialogs::Pick::SaveJson {
                suggested: "wrongindex-diagnostics.json".into(),
            },
        )
    })
    .await
    .map_err(|e| UiError::new("INTERNAL", e.to_string()))?
    .map_err(|e| UiError::new("INTERNAL", e))?;
    let Some(path) = picked else {
        return Ok(json!({"saved": null}));
    };
    let result = bridge.diagnostics(redact_paths, Some(&path)).await?;
    Ok(json!({"saved": path.display().to_string(), "result": result}))
}

fn owner_hwnd(window: &tauri::WebviewWindow) -> isize {
    #[cfg(windows)]
    {
        window.hwnd().map(|h| h.0 as isize).unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        0
    }
}

#[tauri::command]
fn autostart_status() -> Res {
    autostart::status().map_err(|e| UiError::new("INTERNAL", e))
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Res {
    autostart::set_enabled(enabled).map_err(|e| UiError::new("INTERNAL", e))
}

#[tauri::command]
async fn start_service(bridge: State<'_, Bridge>) -> Res {
    bridge.start_service().await
}

#[tauri::command]
async fn stop_service(bridge: State<'_, Bridge>) -> Res {
    bridge.stop_service().await
}

/// Follow the daemon event stream and forward events to the WebView.
///
/// `wi://connection` reports connectivity changes; `wi://resync` tells the UI
/// to fetch a fresh snapshot (first connect, new daemon instance, or lost
/// history); `wi://event` carries individual events. The UI never treats the
/// stream as the source of truth: it re-reads state from the daemon.
async fn pump_events(app: AppHandle, root: DataRoot) {
    let mut position: Option<(String, u64)> = None;
    let mut connected: Option<bool> = None;
    loop {
        if let Ok(client) = Client::connect(&root, CLIENT_NAME).await {
            let (since_instance, since_seq) = match &position {
                Some((i, s)) => (Some(i.clone()), Some(*s)),
                None => (None, None),
            };
            if let Ok((ack, mut stream)) = client
                .subscribe(since_seq, since_instance.as_deref(), None)
                .await
            {
                let instance = ack["instanceId"].as_str().unwrap_or_default().to_string();
                let same = position.as_ref().is_some_and(|(i, _)| *i == instance);
                if connected != Some(true) {
                    connected = Some(true);
                    let _ = app.emit("wi://connection", json!({"connected": true}));
                }
                if !same || ack["gap"] == true {
                    let _ = app.emit("wi://resync", json!({"instanceId": instance}));
                }
                position = Some((instance.clone(), ack["currentSeq"].as_u64().unwrap_or(0)));
                while let Ok(Some(ev)) = stream.next().await {
                    if ev["kind"] == "stream.gap" {
                        let _ = app.emit("wi://resync", json!({"instanceId": instance}));
                        continue;
                    }
                    if let (Some(p), Some(seq)) = (position.as_mut(), ev["seq"].as_u64()) {
                        p.1 = seq;
                    }
                    let _ = app.emit("wi://event", ev);
                }
            }
        }
        if connected != Some(false) {
            connected = Some(false);
            let _ = app.emit("wi://connection", json!({"connected": false}));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn main() {
    let root = match DataRoot::resolve(None) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wrongindex-desktop: data root: {e}");
            std::process::exit(2);
        }
    };
    // Sign-in start (autostart): launch the service if needed, no window.
    if std::env::args()
        .skip(1)
        .any(|a| a == autostart::START_SERVICE_ARG)
    {
        let bridge = Bridge::new(root);
        let code = match tauri::async_runtime::block_on(bridge.start_service()) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("wrongindex-desktop: {}: {}", e.code, e.message);
                1
            }
        };
        std::process::exit(code);
    }
    let pump_root = root.clone();
    let result = tauri::Builder::default()
        .manage(Bridge::new(root))
        .invoke_handler(tauri::generate_handler![
            overview,
            preview_project,
            add_project,
            project_action,
            project_detail,
            recent_jobs,
            cancel_job,
            open_folder,
            pick_folder,
            search,
            diagnostics_preview,
            export_diagnostics,
            autostart_status,
            set_autostart,
            start_service,
            stop_service,
        ])
        .on_window_event(|window, event| {
            // Closing the window keeps the desktop shell in the tray; the
            // service is never affected by the window's lifecycle.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .setup(move |app| {
            let open = MenuItem::with_id(app, "open", "Open WrongIndex", true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop", "Stop Index Service...", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Desktop", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &sep, &stop, &quit])?;
            let mut tray = TrayIconBuilder::with_id("wrongindex")
                .tooltip("WrongIndex")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_main(app),
                    // Stopping the service is confirmed in the window.
                    "stop" => {
                        show_main(app);
                        let _ = app.emit("wi://confirm-stop", json!({}));
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
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            tauri::async_runtime::spawn(pump_events(app.handle().clone(), pump_root.clone()));
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("wrongindex-desktop: {e}");
        std::process::exit(70);
    }
}
