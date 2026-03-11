use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, Url, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_store::StoreExt;
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};

use std::sync::atomic::AtomicBool;

type StopTx = watch::Sender<bool>;
type SocketHandle = Arc<Mutex<Option<StopTx>>>;
type UnreadCount = Arc<AtomicU32>;
type WindowFocused = Arc<AtomicBool>;

const TRAY_ICON: &[u8] = include_bytes!("../icons/32x32.png");

// ── Dock badge (macOS only) ───────────────────────────────────────────────────

fn set_dock_badge(app: &tauri::AppHandle, count: u32) {
    if let Some(window) = app.get_webview_window("main") {
        let label: Option<String> = if count > 0 { Some(count.to_string()) } else { None };
        let _ = window.set_badge_label(label);
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn save_workspace(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let full_url = if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("https://{}", url)
    };

    let store = app.store("settings.json").map_err(|e| e.to_string())?;
    store.set("workspaceUrl", full_url.clone());
    store.save().map_err(|e| e.to_string())?;

    let window = app
        .get_webview_window("main")
        .ok_or("Window not found")?;
    window
        .navigate(full_url.parse::<Url>().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
async fn set_agent_info(
    app: tauri::AppHandle,
    socket_handle: tauri::State<'_, SocketHandle>,
    user_id: String,
    api_url: String,
) -> Result<(), String> {
    if let Ok(store) = app.store("settings.json") {
        store.set("agentUserId", user_id.clone());
        store.set("agentApiUrl", api_url.clone());
        let _ = store.save();
    }

    connect_socket(app, socket_handle.inner().clone(), user_id, api_url);
    Ok(())
}

#[tauri::command]
async fn clear_agent_info(
    app: tauri::AppHandle,
    socket_handle: tauri::State<'_, SocketHandle>,
    unread: tauri::State<'_, UnreadCount>,
) -> Result<(), String> {
    if let Ok(store) = app.store("settings.json") {
        store.delete("agentUserId");
        store.delete("agentApiUrl");
        let _ = store.save();
    }

    let mut guard = socket_handle.lock().await;
    if let Some(stop_tx) = guard.take() {
        let _ = stop_tx.send(true);
    }
    unread.store(0, Ordering::SeqCst);
    set_dock_badge(&app, 0);
    Ok(())
}

// ── WebSocket connect helper ──────────────────────────────────────────────────

fn connect_socket(app: tauri::AppHandle, handle: SocketHandle, user_id: String, api_url: String) {
    tauri::async_runtime::spawn(async move {
        // Stop existing connection
        {
            let mut g = handle.lock().await;
            if let Some(old_tx) = g.take() {
                let _ = old_tx.send(true);
            }
        }

        let (stop_tx, mut stop_rx) = watch::channel(false);
        {
            let mut g = handle.lock().await;
            *g = Some(stop_tx);
        }

        // Convert http(s) URL to ws(s) URL
        let ws_url = api_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        let ws_url = format!("{}/desktop-ws", ws_url.trim_end_matches('/'));

        loop {
            if *stop_rx.borrow() { break; }

            match run_ws(&app, &ws_url, &user_id, &mut stop_rx).await {
                Ok(()) => {}
                Err(e) => eprintln!("[ZChat Desktop] WS error: {}", e),
            }

            if *stop_rx.borrow() { break; }

            eprintln!("[ZChat Desktop] Reconnecting in 5s...");
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                _ = stop_rx.changed() => { break; }
            }
        }
    });
}

// ── WebSocket session ─────────────────────────────────────────────────────────

async fn run_ws(
    app: &tauri::AppHandle,
    ws_url: &str,
    user_id: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let (ws_stream, _) = connect_async(ws_url).await.map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws_stream.split();

    let init = serde_json::json!({ "type": "agentConnected", "userId": user_id });
    write
        .send(Message::Text(init.to_string()))
        .await
        .map_err(|e| e.to_string())?;

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => handle_message(app, &text),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            _ = stop_rx.changed() => break,
        }
    }

    Ok(())
}

// ── Message handler ───────────────────────────────────────────────────────────

fn handle_message(app: &tauri::AppHandle, text: &str) {
    let Ok(data) = serde_json::from_str::<serde_json::Value>(text) else { return };

    match data["type"].as_str() {
        Some("newNotification") => {
            let notif = &data["data"]["notification"];
            let title = notif["title"].as_str().unwrap_or("New message").to_string();
            let body = notif["message"]
                .as_str()
                .or_else(|| notif["body"].as_str())
                .unwrap_or("")
                .to_string();
            let focused = app.state::<WindowFocused>().load(Ordering::SeqCst);
            let unread = app.state::<UnreadCount>();
            let count = unread.fetch_add(1, Ordering::SeqCst) + 1;
            set_dock_badge(app, count);
            if !focused {
                show_notification(app.clone(), title, body);
            }
        }
        Some("newConversation") => {
            let name = data["data"]["customer_name"]
                .as_str()
                .or_else(|| data["data"]["customerName"].as_str())
                .map(|n| format!("New conversation from {}", n))
                .unwrap_or_else(|| "New conversation started".to_string());
            let focused = app.state::<WindowFocused>().load(Ordering::SeqCst);
            let unread = app.state::<UnreadCount>();
            let count = unread.fetch_add(1, Ordering::SeqCst) + 1;
            set_dock_badge(app, count);
            if !focused {
                show_notification(app.clone(), "New Ticket".to_string(), name);
            }
        }
        _ => {}
    }
}

fn show_notification(app: tauri::AppHandle, title: String, body: String) {
    let _ = app.notification().builder().title(&title).body(&body).show();
}

// ── App ───────────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_notification::init())
        .manage(SocketHandle::default())
        .manage(UnreadCount::default())
        .manage(WindowFocused::default())
        .setup(|app| {
            use tauri_plugin_autostart::ManagerExt;
            let _ = app.autolaunch().enable();

            let store = app.store("settings.json")?;
            let initial_url = match store
                .get("workspaceUrl")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
            {
                Some(url) => WebviewUrl::External(url.parse()?),
                None => WebviewUrl::App("index.html".into()),
            };

            WebviewWindowBuilder::new(app, "main", initial_url)
                .title("ZChat")
                .inner_size(1280.0, 800.0)
                .min_inner_size(900.0, 600.0)
                .build()?;

            // Auto-reconnect socket if agent was previously logged in
            let saved_user_id = store
                .get("agentUserId")
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            let saved_api_url = store
                .get("agentApiUrl")
                .and_then(|v| v.as_str().map(|s| s.to_string()));

            if let (Some(uid), Some(url)) = (saved_user_id, saved_api_url) {
                let handle = app.app_handle().clone();
                let socket_handle = app.state::<SocketHandle>().inner().clone();
                connect_socket(handle, socket_handle, uid, url);
            }

            // Request notification permission on first launch
            let app2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let _ = app2
                    .notification()
                    .builder()
                    .title("ZChat")
                    .body("Agent desktop is ready")
                    .show();
            });

            // System tray
            let tray_icon = tauri::image::Image::from_bytes(TRAY_ICON)
                .expect("Failed to load tray icon");

            let show = MenuItem::with_id(app, "show", "Show ZChat", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::new()
                .icon(tray_icon)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app: &tauri::AppHandle, event| {
                    match event.id.as_ref() {
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray: &tauri::tray::TrayIcon, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                tauri::WindowEvent::Focused(focused) => {
                    let app = window.app_handle();
                    if let Some(focused_state) = app.try_state::<WindowFocused>() {
                        focused_state.store(*focused, Ordering::SeqCst);
                    }
                    if *focused {
                        if let Some(unread) = app.try_state::<UnreadCount>() {
                            unread.store(0, Ordering::SeqCst);
                            set_dock_badge(app, 0);
                        }
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            save_workspace,
            set_agent_info,
            clear_agent_info,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            match event {
                tauri::RunEvent::Reopen { has_visible_windows, .. } => {
                    if !has_visible_windows {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                }
                tauri::RunEvent::Resumed => {
                    if let Some(window) = app.get_webview_window("main") {
                        if !window.is_visible().unwrap_or(true) {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                }
                _ => {}
            }
        });
}
