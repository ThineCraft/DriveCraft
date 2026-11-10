//! The DriveCraft desktop app: a window and a tray icon over the same core the
//! command line uses. The interface is HTML in `ui/`; this file is everything it can
//! ask the core to do.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{anyhow, bail};
use drivecraft::auth;
use drivecraft::catalog::{Connection, Mode};
use drivecraft::engine::{Engine, Options, Report};
use drivecraft::home::{self, ConnectRequest, Home};
use drivecraft::runlog::{self, RunLog};
use std::sync::Arc;
use drivecraft::sysinfo;
use serde::Serialize;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

struct Shared {
    /// One synchronization at a time.
    gate: tokio::sync::Mutex<()>,
    running: std::sync::Mutex<Option<String>>,
    auto_sync: AtomicBool,
}

/// Run core work on its own thread. The catalog is not shareable across threads, so
/// each operation gets a thread and a small runtime of its own.
fn local<T, F, Fut>(work: F) -> impl Future<Output = Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(work()))
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(out);
    });
    async move {
        rx.await
            .map_err(|_| "the operation stopped unexpectedly".to_string())?
    }
}

// ---- what the interface sees -------------------------------------------------

#[derive(Serialize)]
struct SetupView {
    client_id: String,
    tenant: String,
}

#[derive(Serialize)]
struct ConnView {
    name: String,
    mode: String,
    folder: String,
    folder_exists: bool,
    library_url: String,
    remote_files: u64,
    remote_folders: u64,
    remote_bytes: u64,
    local_items: u64,
    pending: u64,
    first_scan: &'static str,
    last_sync: Option<i64>,
    syncing: bool,
}

#[derive(Serialize)]
struct StateView {
    version: &'static str,
    data_dir: String,
    setup: Option<SetupView>,
    signed_in: bool,
    can_write: bool,
    connections: Vec<ConnView>,
    is_windows: bool,
    long_paths: Option<bool>,
    auto_sync: bool,
    /// The connection DriveCraft is sending through, and the ones it could use.
    link: drivecraft::net::ActiveLink,
    links: Vec<drivecraft::net::Link>,
}

#[derive(Serialize)]
struct Identity {
    name: String,
    account: String,
    can_write: bool,
}

#[derive(Serialize)]
struct LibraryView {
    name: String,
    url: String,
}

#[derive(Serialize, Clone)]
struct LogLine {
    name: String,
    line: String,
}

#[derive(Serialize, Clone)]
struct Finished {
    name: String,
    dry_run: bool,
    report: Option<Report>,
    error: Option<String>,
    throttled: u64,
    /// Timing, traffic and memory measurements for the run.
    details: Vec<String>,
}

fn conn_view(cat: &drivecraft::catalog::Catalog, c: &Connection, running: &Option<String>) -> anyhow::Result<ConnView> {
    let n = cat.counts(c.id)?;
    Ok(ConnView {
        name: c.name.clone(),
        mode: c.mode.as_str().to_string(),
        folder: c.local_path.display().to_string(),
        folder_exists: c.local_path.exists(),
        library_url: c.web_url.clone(),
        remote_files: n.remote_files,
        remote_folders: n.remote_folders,
        remote_bytes: n.remote_bytes,
        local_items: n.local_items,
        pending: n.pending,
        first_scan: if c.delta_link.is_some() {
            "complete"
        } else if c.cursor.is_some() {
            "interrupted"
        } else {
            "not started"
        },
        last_sync: c.last_sync,
        syncing: running.as_deref() == Some(c.name.as_str()),
    })
}

fn ui_settings_path(home: &Home) -> PathBuf {
    home.dir.join("app.json")
}

fn load_auto_sync() -> bool {
    Home::locate()
        .ok()
        .and_then(|h| std::fs::read_to_string(ui_settings_path(&h)).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["auto_sync"].as_bool())
        .unwrap_or(false)
}

// ---- commands ------------------------------------------------------------------

#[tauri::command]
async fn get_state(app: AppHandle) -> Result<StateView, String> {
    let shared = app.state::<Shared>();
    let running = shared.running.lock().unwrap().clone();
    let auto_sync = shared.auto_sync.load(Ordering::Relaxed);
    local(move || async move {
        let home = Home::locate()?;
        let setup = home.app().ok().map(|a| SetupView {
            client_id: a.client_id,
            tenant: a.tenant,
        });
        let tokens = match home.authenticator() {
            Ok(a) => a.current().await,
            Err(_) => None,
        };
        let cat = home.catalog()?;
        let mut connections = Vec::new();
        for c in cat.connections()? {
            connections.push(conn_view(&cat, &c, &running)?);
        }
        Ok(StateView {
            version: env!("CARGO_PKG_VERSION"),
            data_dir: home.dir.display().to_string(),
            setup,
            signed_in: tokens.is_some(),
            can_write: tokens.map(|t| t.can_write()).unwrap_or(false),
            connections,
            is_windows: cfg!(windows),
            long_paths: sysinfo::windows_long_paths_enabled(),
            auto_sync,
            link: drivecraft::net::active_link(),
            links: drivecraft::net::links(),
        })
    })
    .await
}

#[tauri::command]
async fn save_setup(client_id: String, tenant: String) -> Result<(), String> {
    local(move || async move {
        let home = Home::locate()?;
        home.save_app(&client_id, &tenant)?;
        // A different app registration cannot use the old sign-in.
        home.authenticator()?.logout()?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn whoami() -> Result<Option<Identity>, String> {
    local(|| async {
        let home = Home::locate()?;
        let Ok(auth) = home.authenticator() else { return Ok(None) };
        let Some(tokens) = auth.current().await else { return Ok(None) };
        let can_write = tokens.can_write();
        let graph = drivecraft::graph::GraphClient::new(auth);
        let (name, account) = graph.me().await?;
        Ok(Some(Identity { name, account, can_write }))
    })
    .await
}

/// Opens the system browser to sign in. Returns once the browser sign-in completes.
#[tauri::command]
async fn login(read_only: bool) -> Result<Identity, String> {
    local(move || async move {
        let home = Home::locate()?;
        let auth = home.authenticator()?;
        let scopes = if read_only {
            auth::scopes_read_only()
        } else {
            auth::scopes_read_write()
        };
        let tokens = auth.login_browser(&scopes, |_| {}).await?;
        let can_write = tokens.can_write();
        let graph = drivecraft::graph::GraphClient::new(auth);
        let (name, account) = graph.me().await.unwrap_or_default();
        Ok(Identity { name, account, can_write })
    })
    .await
}

#[tauri::command]
async fn logout() -> Result<(), String> {
    local(|| async { Home::locate()?.authenticator()?.logout() }).await
}

#[tauri::command]
async fn pick_folder(title: String) -> Result<Option<String>, String> {
    let picked = rfd::AsyncFileDialog::new().set_title(title).pick_folder().await;
    Ok(picked.map(|f| f.path().display().to_string()))
}

/// Look an address up without connecting it, so the user can see what it is.
#[tauri::command]
async fn check_library(address: String) -> Result<LibraryView, String> {
    local(move || async move {
        let d = Home::locate()?.graph()?.resolve_library(&address).await?;
        Ok(LibraryView { name: d.name, url: d.web_url })
    })
    .await
}

#[tauri::command]
async fn connect(address: String, folder: String, mode: String, name: String, merge: bool) -> Result<String, String> {
    local(move || async move {
        let home = Home::locate()?;
        let mode = Mode::parse(&mode).ok_or_else(|| anyhow!("unknown mode"))?;
        if folder.trim().is_empty() {
            bail!("choose a local folder");
        }
        let done = home::connect_library(
            &home,
            ConnectRequest {
                address: &address,
                folder: std::path::Path::new(folder.trim()),
                mode,
                name: Some(&name),
                merge,
            },
        )
        .await?;
        Ok(done.connection.name)
    })
    .await
}

#[tauri::command]
async fn set_mode(name: String, mode: String) -> Result<(), String> {
    local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        let mode = Mode::parse(&mode).ok_or_else(|| anyhow!("unknown mode"))?;
        if c.mode == Mode::Ingest || mode == Mode::Ingest {
            bail!("an ingest connection cannot be converted; disconnect it and connect again");
        }
        cat.set_mode(c.id, mode)?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn disconnect(name: String) -> Result<(), String> {
    local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        cat.remove_connection(c.id)?;
        Ok(())
    })
    .await
}

/// Open a connection's local folder in the file manager, or its library in the browser.
#[tauri::command]
async fn open_connection(name: String, target: String) -> Result<(), String> {
    local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        match target.as_str() {
            "folder" => {
                if !c.local_path.exists() {
                    bail!("{} does not exist yet; synchronize first", c.local_path.display());
                }
                open::that(&c.local_path)?;
            }
            "library" => {
                // Only ever a SharePoint address recorded when the library was connected.
                if !c.web_url.starts_with("https://") {
                    bail!("the recorded library address is not a web address");
                }
                open::that(&c.web_url)?;
            }
            _ => bail!("unknown target"),
        }
        Ok(())
    })
    .await
}

/// Send through the connection with this address, or let the system choose (empty).
#[tauri::command]
async fn set_network(address: String) -> Result<(), String> {
    local(move || async move {
        let home = Home::locate()?;
        let addr = address.trim();
        if addr.is_empty() {
            return home.save_bind_address(None);
        }
        let ip: std::net::IpAddr = addr.parse().map_err(|_| anyhow!("'{addr}' is not a network address"))?;
        if !drivecraft::net::links().iter().any(|l| l.address == ip) {
            bail!("this computer has no connection with the address {ip}");
        }
        home.save_bind_address(Some(ip))
    })
    .await
}

/// Store a snapshot of the library's file list in the library, for other computers.
#[tauri::command]
async fn publish_seed(app: AppHandle, name: String) -> Result<drivecraft::seed::Published, String> {
    let shared = app.state::<Shared>();
    let _turn = shared.gate.lock().await;
    local(move || async move {
        let home = Home::locate()?;
        let cat = home.catalog()?;
        let conn = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        let auth = home.authenticator()?;
        let tokens = auth.current().await.ok_or_else(|| anyhow!("You are not signed in."))?;
        if !tokens.can_write() {
            bail!("Publishing a seed stores a file in the library, which needs write access. Sign in again with write access.");
        }
        let graph = drivecraft::graph::GraphClient::new(auth);
        let done = drivecraft::seed::publish(&cat, &graph, &conn, &home.work_dir()).await?;
        if let Ok(log) = RunLog::open(&home.dir) {
            log.line(&format!("published a seed for {}: {} items, {} bytes", conn.name, done.items, done.bytes));
        }
        Ok(done)
    })
    .await
}

/// Open the folder that holds DriveCraft's log files.
#[tauri::command]
async fn open_logs() -> Result<(), String> {
    local(|| async {
        let dir = runlog::logs_dir(&Home::locate()?.dir);
        std::fs::create_dir_all(&dir)?;
        open::that(&dir)?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn set_auto_sync(app: AppHandle, enabled: bool) -> Result<(), String> {
    app.state::<Shared>().auto_sync.store(enabled, Ordering::Relaxed);
    local(move || async move {
        let home = Home::locate()?;
        std::fs::write(ui_settings_path(&home), serde_json::json!({ "auto_sync": enabled }).to_string())?;
        Ok(())
    })
    .await
}

async fn run_sync(app: AppHandle, name: String, opts: Options) -> Finished {
    let shared = app.state::<Shared>();
    let _turn = shared.gate.lock().await;
    *shared.running.lock().unwrap() = Some(name.clone());
    let _ = app.emit("sync-started", name.clone());

    let dry_run = opts.dry_run;
    let (emitter, conn_name) = (app.clone(), name.clone());
    let outcome = local(move || async move {
        let home = Home::locate()?;
        let _lock = home.sync_lock()?;
        let cat = home.catalog()?;
        let conn = cat
            .connection(&conn_name)?
            .ok_or_else(|| anyhow!("no connection named '{conn_name}'"))?;
        let auth = home.authenticator()?;
        let tokens = auth.current().await.ok_or_else(|| anyhow!("You are not signed in."))?;
        if conn.mode != Mode::Archive && !tokens.can_write() && !opts.dry_run {
            bail!(
                "You are signed in read-only, which cannot {}. Sign in again with write access.",
                if conn.mode == Mode::Sync { "synchronize both ways" } else { "upload" }
            );
        }
        let log = Arc::new(RunLog::open(&home.dir)?);
        log.line(&format!("sending through {}", drivecraft::net::active_link().describe()));
        log.line(&format!(
            "==== {} [{}]{} {} <-> {} (desktop app)",
            conn.name,
            conn.mode.as_str(),
            if opts.dry_run { " (preview)" } else { "" },
            conn.local_path.display(),
            conn.web_url
        ));
        let mut graph = drivecraft::graph::GraphClient::new(auth);
        let show = {
            let (log, emitter, name) = (log.clone(), emitter.clone(), conn_name.clone());
            move |line: &str| {
                log.line(line);
                let _ = emitter.emit(
                    "sync-log",
                    LogLine {
                        name: name.clone(),
                        line: line.to_string(),
                    },
                );
            }
        };
        graph.set_event_hook(show.clone());
        let outcome = Engine::new(&cat, &graph, conn, opts).with_log(show.clone()).run().await;
        let report = match outcome {
            Ok(r) => r,
            Err(e) => {
                log.line(&format!("stopped: {e:#}"));
                return Err(e);
            }
        };
        let details = runlog::run_details(&report, &graph.stats());
        log.line(&format!(
            "finished: {} downloaded, {} uploaded, {} removed here, {} deleted on the server, {} conflicts, {} failed",
            report.downloaded, report.uploaded, report.deleted_local, report.deleted_remote, report.conflicts, report.failed
        ));
        for line in &details {
            log.line(line);
        }
        Ok((report, graph.times_throttled(), details))
    })
    .await;

    *shared.running.lock().unwrap() = None;
    let finished = match outcome {
        Ok((report, throttled, details)) => Finished {
            name,
            dry_run,
            report: Some(report),
            error: None,
            throttled,
            details,
        },
        Err(e) => Finished {
            name,
            dry_run,
            report: None,
            error: Some(e),
            throttled: 0,
            details: Vec::new(),
        },
    };
    let _ = app.emit("sync-finished", finished.clone());
    finished
}

#[tauri::command]
async fn sync(app: AppHandle, name: String, dry_run: bool, confirm_deletes: bool, update: bool) -> Result<Finished, String> {
    let opts = Options {
        dry_run,
        confirm_deletes,
        ingest_update: update,
        seed_dir: Home::locate().ok().map(|h| h.work_dir()),
        ..Default::default()
    };
    Ok(run_sync(app, name, opts).await)
}

async fn sync_all(app: AppHandle) {
    let names = local(|| async {
        let cat = Home::locate()?.catalog()?;
        Ok(cat.connections()?.into_iter().map(|c| c.name).collect::<Vec<_>>())
    })
    .await
    .unwrap_or_default();
    for name in names {
        let opts = Options {
            seed_dir: Home::locate().ok().map(|h| h.work_dir()),
            ..Default::default()
        };
        run_sync(app.clone(), name, opts).await;
    }
}

#[tauri::command]
async fn sync_everything(app: AppHandle) -> Result<(), String> {
    sync_all(app).await;
    Ok(())
}

/// Put the window away. DriveCraft keeps running from its tray icon.
#[tauri::command]
fn hide_to_tray(window: tauri::WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .manage(Shared {
            gate: tokio::sync::Mutex::new(()),
            running: std::sync::Mutex::new(None),
            auto_sync: AtomicBool::new(load_auto_sync()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_setup,
            whoami,
            login,
            logout,
            pick_folder,
            check_library,
            connect,
            set_mode,
            disconnect,
            open_connection,
            open_logs,
            set_network,
            publish_seed,
            hide_to_tray,
            set_auto_sync,
            sync,
            sync_everything
        ])
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open DriveCraft", true, None::<&str>)?;
            let sync_now = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit DriveCraft", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &sync_now, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("DriveCraft")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_window(app),
                    "sync" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move { sync_all(app).await });
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
                        show_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // Keep libraries in sync in the background when the user has asked for it.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    if handle.state::<Shared>().auto_sync.load(Ordering::Relaxed) {
                        sync_all(handle.clone()).await;
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps DriveCraft running in the tray.
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Minimized some other way (the taskbar, a keyboard shortcut): that
                // goes to the tray as well, so there is one place to find DriveCraft.
                tauri::WindowEvent::Resized(_) => {
                    if window.is_minimized().unwrap_or(false) {
                        let _ = window.unminimize();
                        let _ = window.hide();
                    }
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("DriveCraft could not start");
}
