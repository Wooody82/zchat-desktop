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
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;

use rust_socketio::{asynchronous::ClientBuilder, Payload};

type SocketClient = rust_socketio::asynchronous::Client;
type SocketHandle = Arc<Mutex<Option<SocketClient>>>;
type UnreadCount = Arc<AtomicU32>;

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
async fn check_for_updates(app: tauri::AppHandle) -> Result<String, String> {
    match app.updater().map_err(|e| e.to_string())?.check().await {
        Ok(Some(update)) => Ok(format!("Update available: v{}", update.version)),
        Ok(None) => Ok("You're on the latest version.".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    if let Some(update) = updater.check().await.map_err(|e| e.to_string())? {
        update.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
        app.restart();
    }
    Ok(())
}

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
    // Persist so we can reconnect on next app launch
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
    // Clear persisted agent info
    if let Ok(store) = app.store("settings.json") {
        store.delete("agentUserId");
        store.delete("agentApiUrl");
        let _ = store.save();
    }

    let mut guard = socket_handle.lock().await;
    if let Some(client) = guard.take() {
        let _ = client.disconnect().await;
    }
    unread.store(0, Ordering::SeqCst);
    set_dock_badge(&app, 0);
    Ok(())
}

// ── Socket connect helper (non-async, spawns task) ───────────────────────────

fn connect_socket(app: tauri::AppHandle, handle: SocketHandle, user_id: String, api_url: String) {
    tauri::async_runtime::spawn(async move {
        // Disconnect existing socket
        {
            let mut guard = handle.lock().await;
            if let Some(client) = guard.take() {
                let _ = client.disconnect().await;
            }
        }

        match build_socket(app.clone(), api_url, user_id).await {
            Ok(client) => {
                let mut g = handle.lock().await;
                *g = Some(client);
            }
            Err(e) => {
                eprintln!("[ZChat Desktop] Socket error: {}", e);
            }
        }
    });
}

// ── Notification helper ───────────────────────────────────────────────────────

fn show_notification(app: tauri::AppHandle, title: String, body: String) {
    let _ = app.notification().builder().title(&title).body(&body).show();
}

// ── Socket ────────────────────────────────────────────────────────────────────

async fn build_socket(
    app: tauri::AppHandle,
    api_url: String,
    user_id: String,
) -> Result<SocketClient, String> {
    let app_notif = app.clone();
    let app_convo = app.clone();
    let uid = user_id.clone();

    let socket = ClientBuilder::new(&api_url)
        .on("newNotification", move |payload: Payload, _| {
            let app = app_notif.clone();
            Box::pin(async move {
                let (title, body) = parse_notification_payload(&payload);
                let unread = app.state::<UnreadCount>();
                let count = unread.fetch_add(1, Ordering::SeqCst) + 1;
                set_dock_badge(&app, count);
                show_notification(app, title, body);
            })
        })
        .on("newConversation", move |payload: Payload, _| {
            let app = app_convo.clone();
            Box::pin(async move {
                let body = extract_customer_name(&payload);
                let unread = app.state::<UnreadCount>();
                let count = unread.fetch_add(1, Ordering::SeqCst) + 1;
                set_dock_badge(&app, count);
                show_notification(app, "New Ticket".to_string(), body);
            })
        })
        .connect()
        .await
        .map_err(|e| e.to_string())?;

    socket
        .emit("agentConnected", serde_json::json!({ "userId": uid }))
        .await
        .map_err(|e| e.to_string())?;

    Ok(socket)
}

fn parse_notification_payload(payload: &Payload) -> (String, String) {
    if let Payload::Text(vals) = payload {
        if let Some(data) = vals.first() {
            let notif = data.get("notification");
            let title = notif
                .and_then(|n| n.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("New message")
                .to_string();
            let body = notif
                .and_then(|n| n.get("message").or_else(|| n.get("body")))
                .and_then(|b| b.as_str())
                .unwrap_or("")
                .to_string();
            return (title, body);
        }
    }
    ("New message".to_string(), String::new())
}

fn extract_customer_name(payload: &Payload) -> String {
    if let Payload::Text(vals) = payload {
        if let Some(data) = vals.first() {
            if let Some(name) = data
                .get("customer_name")
                .or_else(|| data.get("customerName"))
                .and_then(|n| n.as_str())
            {
                return format!("New conversation from {}", name);
            }
        }
    }
    "New conversation started".to_string()
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(SocketHandle::default())
        .manage(UnreadCount::default())
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

            let show =
                MenuItem::with_id(app, "show", "Show ZChat", true, None::<&str>)?;
            let updates =
                MenuItem::with_id(app, "updates", "Check for Updates…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &updates, &quit])?;

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
                        "updates" => {
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let updater = match app.updater() {
                                    Ok(u) => u,
                                    Err(_) => return,
                                };
                                match updater.check().await {
                                    Ok(Some(update)) => {
                                        let _ = app.notification()
                                            .builder()
                                            .title("Update Available")
                                            .body(&format!("v{} is available. Downloading…", update.version))
                                            .show();
                                        if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
                                            app.restart();
                                        }
                                    }
                                    Ok(None) => {
                                        let _ = app.notification()
                                            .builder()
                                            .title("ZChat")
                                            .body("You're on the latest version.")
                                            .show();
                                    }
                                    Err(_) => {}
                                }
                            });
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
                // Clear badge when agent focuses the window
                tauri::WindowEvent::Focused(true) => {
                    let app = window.app_handle();
                    if let Some(unread) = app.try_state::<UnreadCount>() {
                        unread.store(0, Ordering::SeqCst);
                        set_dock_badge(app, 0);
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            save_workspace,
            set_agent_info,
            clear_agent_info,
            check_for_updates,
            install_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            match event {
                // Dock icon clicked with hidden window
                tauri::RunEvent::Reopen { has_visible_windows, .. } => {
                    if !has_visible_windows {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                }
                // App became active (notification click, app switcher, etc.)
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
