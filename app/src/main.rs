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
    /// When each connection last finished a run. A run's own writes to the folder
    /// must not be taken for the user's changes.
    finished: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// The folders DriveCraft is looking after in File Explorer. Windows shows their
    /// status icons only while these are held.
    links: std::sync::Mutex<std::collections::HashMap<PathBuf, drivecraft::explorer::Link>>,
    /// A Sync link from the browser that the interface has not dealt with yet.
    sync_link: std::sync::Mutex<Option<String>>,
    /// A file whose library details File Explorer asked to see.
    details_for: std::sync::Mutex<Option<String>>,
}

const DETAILS_ARG: &str = "--details";

/// How many ordinary lines of a run the window is sent each second.
const LINES_SHOWN_A_SECOND: u32 = 12;

/// Arguments File Explorer's right-click menu starts DriveCraft with.
const CHECK_OUT_ARG: &str = "--check-out";
const CHECK_IN_ARG: &str = "--check-in";

/// The argument Windows starts DriveCraft with at sign-in: straight to the tray.
const HIDDEN_ARG: &str = "--hidden";

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
    /// How many recorded problems, by how serious they are.
    problems: drivecraft::problems::Tally,
    /// How the folder is listed in File Explorer, when it is.
    explorer: Option<drivecraft::explorer::Shown>,
    /// How many files are checked out in the library, and how many by this user.
    checked_out: drivecraft::checkouts::Count,
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
    /// Whether DriveCraft starts when the user signs in to the computer.
    autostart: bool,
    /// The connection DriveCraft is sending through, and the ones it could use.
    link: drivecraft::net::ActiveLink,
    links: Vec<drivecraft::net::Link>,
    /// Who answers the browser's Sync button.
    sync_button: drivecraft::takeover::Status,
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
struct Progressing {
    name: String,
    #[serde(flatten)]
    moving: drivecraft::engine::Moving,
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

fn conn_view(
    cat: &drivecraft::catalog::Catalog,
    c: &Connection,
    running: &Option<String>,
    shown: &[drivecraft::explorer::Shown],
) -> anyhow::Result<ConnView> {
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
        problems: cat.problem_tally(c.id)?,
        explorer: shown.iter().find(|s| same_folder(&s.folder, &c.local_path)).cloned(),
        checked_out: cat.checkout_count(c.id)?,
    })
}

fn same_folder(a: &std::path::Path, b: &std::path::Path) -> bool {
    let tidy = |p: &std::path::Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    tidy(a) == tidy(b)
}

/// Bring File Explorer in line with the connections: hold a link to every folder
/// listed there, let go of the rest, and offer the right-click menu inside them.
fn tend_explorer(app: &AppHandle) {
    let shown = drivecraft::explorer::shown();
    let conns = Home::locate().and_then(|h| h.catalog()).and_then(|c| c.connections()).unwrap_or_default();
    let folders: Vec<PathBuf> = conns
        .iter()
        .map(|c| c.local_path.clone())
        .filter(|f| f.is_dir() && shown.iter().any(|s| same_folder(&s.folder, f)))
        .collect();
    let shared = app.state::<Shared>();
    let mut links = shared.links.lock().unwrap();
    links.retain(|f, _| folders.contains(f));
    for f in &folders {
        if !links.contains_key(f) {
            match drivecraft::explorer::connect(f) {
                Ok(link) => {
                    links.insert(f.clone(), link);
                }
                Err(e) => eprintln!("could not look after {} in File Explorer: {e:#}", f.display()),
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = drivecraft::explorer::set_menu(&exe, &folders);
    }
}

/// List a library's folder in File Explorer with status icons, or take it out again.
#[tauri::command]
async fn set_explorer(
    app: AppHandle,
    name: String,
    enabled: bool,
    label: Option<String>,
    icon: Option<String>,
) -> Result<drivecraft::explorer::Marked, String> {
    let shared = app.state::<Shared>();
    // Not while a run is moving files about.
    let _turn = shared.gate.lock().await;
    // Windows will not change a folder's listing while it is being looked after.
    shared.links.lock().unwrap().clear();
    let done = local(move || async move {
        use drivecraft::explorer;
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        if !enabled {
            return explorer::turn_off(&c);
        }
        let now = explorer::shown_as(&c.local_path);
        let label = label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
        let label = label.or(now.as_ref().map(|s| s.name.clone())).unwrap_or(c.name.clone());
        let icon = match icon.as_deref() {
            // DriveCraft's own icon, asked for by name.
            Some("default") => explorer::default_icon(),
            _ => icon.or(now.map(|s| s.icon)).unwrap_or_else(explorer::default_icon),
        };
        explorer::turn_on(&cat, &c, &label, &icon)
    })
    .await;
    tend_explorer(&app);
    done
}

/// Keep an icon the interface has drawn, so File Explorer can show it. `key` names
/// it (the picture and its colour); the same key is the same file.
#[tauri::command]
async fn save_icon(key: String, data: Vec<u8>) -> Result<String, String> {
    local(move || async move {
        let plain = !key.is_empty() && key.len() <= 80 && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        // An icon file starts 0 0 1 0 and is small; nothing else is written here.
        if !plain || data.len() < 22 || data.len() > 2_000_000 || data[..4] != [0, 0, 1, 0] {
            bail!("that is not an icon DriveCraft made");
        }
        let dir = Home::locate()?.dir.join("icons");
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{key}.ico"));
        std::fs::write(&file, data)?;
        Ok(file.display().to_string())
    })
    .await
}

#[derive(Serialize, Clone)]
struct Notice {
    text: String,
    good: bool,
}

/// Tell the user something that happened while the window may be out of sight.
fn tell(app: &AppHandle, title: &str, text: &str, good: bool) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(text).show();
    tray_says(app, &format!("DriveCraft: {title}: {text}"));
    let _ = app.emit("notice", Notice { text: format!("{title}: {text}"), good });
}

#[derive(Serialize)]
struct ColumnView {
    name: String,
    title: String,
    kind: drivecraft::graph::ColumnKind,
    editable: bool,
    required: bool,
    choices: Vec<String>,
    multi: bool,
    date_only: bool,
    description: String,
    value: serde_json::Value,
}

#[derive(Serialize)]
struct DetailsView {
    /// The file on this computer, as it was asked for.
    file: String,
    connection: String,
    /// Where the file is inside the library.
    path: String,
    modified_by: String,
    created_by: String,
    columns: Vec<ColumnView>,
}

async fn details_of(home: &Home, file: &std::path::Path) -> anyhow::Result<DetailsView> {
    use drivecraft::details;
    let (conn, id, rel) = home::locate_file(home, file)?;
    let graph = home.graph()?;
    let columns = graph.library_columns(&conn.drive_id).await?;
    let fields = graph.item_fields(&conn.drive_id, &id).await?;
    Ok(DetailsView {
        file: file.display().to_string(),
        connection: conn.name,
        path: rel,
        modified_by: fields.modified_by,
        created_by: fields.created_by,
        columns: columns
            .into_iter()
            .filter(details::worth_showing)
            .map(|c| ColumnView {
                value: fields.values.get(&c.name).cloned().unwrap_or(serde_json::Value::Null),
                editable: details::editable(&c),
                name: c.name,
                title: c.title,
                kind: c.kind,
                required: c.required,
                choices: c.choices,
                multi: c.multi,
                date_only: c.date_only,
                description: c.description,
            })
            .collect(),
    })
}

/// A file's library details: its values in the columns the library defines.
#[tauri::command]
async fn file_details(file: String) -> Result<DetailsView, String> {
    local(move || async move { details_of(&Home::locate()?, std::path::Path::new(&file)).await }).await
}

/// Change some of a file's column values. `changes` maps column names to what was
/// typed; an empty value clears the column. Returns the details as they are now.
#[tauri::command]
async fn save_details(file: String, changes: std::collections::HashMap<String, String>) -> Result<DetailsView, String> {
    local(move || async move {
        use drivecraft::details;
        let home = Home::locate()?;
        let file = PathBuf::from(file);
        let (conn, id, rel) = home::locate_file(&home, &file)?;
        let auth = home.authenticator()?;
        let tokens = auth.current().await.ok_or_else(|| anyhow!("You are not signed in."))?;
        if !tokens.can_write() {
            bail!("Changing details changes the library, which needs write access. Sign in again with write access.");
        }
        let graph = drivecraft::graph::GraphClient::new(auth);
        let columns = graph.library_columns(&conn.drive_id).await?;
        let mut pairs = Vec::new();
        for (name, typed) in &changes {
            let col = columns
                .iter()
                .find(|c| &c.name == name)
                .ok_or_else(|| anyhow!("this library has no column named '{name}'"))?;
            pairs.push((col, details::to_server(col, typed).map_err(|e| anyhow!(e))?));
        }
        if !pairs.is_empty() {
            let n = pairs.len();
            graph.set_item_fields(&conn.drive_id, &id, &details::changes(pairs)).await?;
            if let Ok(log) = RunLog::open(&home.dir) {
                log.line(&format!("changed {n} detail(s) of {rel} in {}", conn.name));
            }
        }
        details_of(&home, &file).await
    })
    .await
}

/// Let the user pick a file of a library, to see its details.
#[tauri::command]
async fn pick_library_file(name: String) -> Result<Option<String>, String> {
    let folder = local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        Ok(c.local_path)
    })
    .await?;
    let picked = rfd::AsyncFileDialog::new()
        .set_title("Choose a file to see its library details")
        .set_directory(&folder)
        .pick_file()
        .await;
    Ok(picked.map(|f| f.path().display().to_string()))
}

/// The file File Explorer asked about, handed over once.
#[tauri::command]
fn take_details_request(app: AppHandle) -> Option<String> {
    app.state::<Shared>().details_for.lock().unwrap().take()
}

/// Open a file's page in the library, in the browser.
#[tauri::command]
async fn open_in_library(file: String) -> Result<(), String> {
    local(move || async move {
        let home = Home::locate()?;
        let (conn, id, _) = home::locate_file(&home, std::path::Path::new(&file))?;
        let url = home.graph()?.item_fields(&conn.drive_id, &id).await?.web_url;
        // Only ever the library's own secure address, as the server reports it.
        if !url.starts_with("https://") {
            bail!("the server did not give a web address for this file");
        }
        open::that(&url)?;
        Ok(())
    })
    .await
}

/// File Explorer asked for a file's library details: bring the window up to show them.
fn details_asked(app: &AppHandle, file: String) {
    *app.state::<Shared>().details_for.lock().unwrap() = Some(file);
    show_window(app);
    let _ = app.emit("details", ());
}

fn details_arg(args: &[String]) -> Option<String> {
    let at = args.iter().position(|a| a == DETAILS_ARG)?;
    args.get(at + 1).cloned()
}

/// What File Explorer's right-click menu asked for, if these arguments are from it.
fn menu_request(args: &[String]) -> Option<(bool, PathBuf)> {
    let at = args.iter().position(|a| a == CHECK_OUT_ARG || a == CHECK_IN_ARG)?;
    Some((args[at] == CHECK_OUT_ARG, PathBuf::from(args.get(at + 1)?)))
}

#[derive(Serialize)]
struct BinPage {
    /// The library's own path on the server; items carry the folder they came from.
    library_path: String,
    items: Vec<drivecraft::graph::Binned>,
    /// Where the next page starts; `None` when this was the last.
    next: Option<String>,
}

/// One page of the recycle bin of a library's site, newest deletions first. The
/// interface asks for the first page, then for each `next` it is handed, so it can
/// show and search what has arrived while the rest is still coming.
#[tauri::command]
async fn bin_page(name: String, library_path: Option<String>, next: Option<String>) -> Result<BinPage, String> {
    local(move || async move {
        let home = Home::locate()?;
        let cat = home.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        let graph = home.graph()?;
        let site = match (&next, library_path) {
            // A later page: the link already says which site.
            (Some(_), Some(library_path)) => drivecraft::graph::SiteRef { key: String::new(), library_path },
            _ => graph.site_of(&c.drive_id).await?,
        };
        let (items, next) = graph.recycle_bin_page(&site, next.as_deref()).await?;
        Ok(BinPage { library_path: site.library_path, items, next })
    })
    .await
}

/// Put items from the recycle bin back where they were deleted from.
#[tauri::command]
async fn restore_bin(name: String, ids: Vec<String>) -> Result<(), String> {
    local(move || async move {
        let home = Home::locate()?;
        let cat = home.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        let auth = home.authenticator()?;
        let tokens = auth.current().await.ok_or_else(|| anyhow!("You are not signed in."))?;
        if !tokens.can_write() {
            bail!("Restoring changes the library, which needs write access. Sign in again with write access.");
        }
        let graph = drivecraft::graph::GraphClient::new(auth);
        let site = graph.site_of(&c.drive_id).await?;
        // A few at a time, so one request is never too large for the server.
        for some in ids.chunks(20) {
            graph.restore_binned(&site, some).await?;
        }
        if let Ok(log) = RunLog::open(&home.dir) {
            log.line(&format!("restored {} item(s) from the recycle bin of {}", ids.len(), c.name));
        }
        Ok(())
    })
    .await
}

/// The files checked out in a library, as last reported by the server.
#[tauri::command]
async fn list_checkouts(name: String, ask_server: bool) -> Result<Vec<drivecraft::checkouts::Listed>, String> {
    local(move || async move {
        let home = Home::locate()?;
        let cat = home.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        if ask_server {
            drivecraft::checkouts::refresh(&cat, &home.graph()?, &c).await?;
        }
        cat.checkouts(c.id)
    })
    .await
}

/// Who may open what in a library, as recorded; with `refresh`, read from the server
/// again first. Read-only: nothing on the server is changed.
#[derive(serde::Serialize)]
struct PermissionsView {
    #[serde(flatten)]
    view: drivecraft::permissions::View,
    /// Items whose permissions the server would not show, in the read just made.
    refused: u64,
}

#[tauri::command]
async fn library_permissions(app: AppHandle, name: String, refresh: bool) -> Result<PermissionsView, String> {
    let shared = app.state::<Shared>();
    // A read from the server writes to the catalog; it waits its turn with synchronizing.
    let _turn = if refresh { Some(shared.gate.lock().await) } else { None };
    local(move || async move {
        let home = Home::locate()?;
        let cat = home.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        let mut refused = 0;
        if refresh {
            let graph = home.graph()?;
            let read = drivecraft::permissions::read_all(&cat, &graph, &c, |_, _| {}).await?;
            refused = read.refused;
            if let Ok(log) = RunLog::open(&home.dir) {
                log.line(&format!(
                    "read the permissions of {} items in {} ({} refused, full lists: {})",
                    read.items, c.name, read.refused, read.full
                ));
            }
        }
        Ok(PermissionsView { view: drivecraft::permissions::view(&cat, &c)?, refused })
    })
    .await
}

/// Check a file of a library out or in, from the app's own list.
#[tauri::command]
async fn check_file(app: AppHandle, name: String, path: String, out: bool, comment: String) -> Result<(), String> {
    let folder = local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        Ok(c.local_path)
    })
    .await?;
    check(app, out, drivecraft::paths::join_rel(&folder, &path), comment).await;
    Ok(())
}

/// Check a file out or back in, and say how it went.
async fn check(app: AppHandle, out: bool, file: PathBuf, comment: String) {
    let done = local(move || async move {
        let home = Home::locate()?;
        let (conn, id, rel) = home::locate_file(&home, &file)?;
        let graph = home.graph()?;
        if out {
            graph.check_out(&conn.drive_id, &id).await?;
        } else {
            graph.check_in(&conn.drive_id, &id, &comment).await?;
        }
        // Show it at once; the next synchronization confirms it with the server.
        let cat = home.catalog()?;
        let before = cat.checkouts(conn.id)?;
        let me = if out { Some(graph.me().await.map(|(n, _)| n).unwrap_or_default()) } else { None };
        cat.note_checkout(conn.id, &id, me.as_deref(), drivecraft::problems::unix_now())?;
        let _ = drivecraft::checkouts::show_in_explorer(&cat, &conn, &before);
        if let Ok(log) = RunLog::open(&home.dir) {
            log.line(&format!("{} {rel} in {}", if out { "checked out" } else { "checked in" }, conn.name));
        }
        Ok(rel)
    })
    .await;
    match done {
        Ok(rel) => tell(&app, if out { "Checked out" } else { "Checked in" }, &rel, true),
        Err(e) => tell(&app, if out { "Could not check out" } else { "Could not check in" }, &e, false),
    }
}

/// Where DriveCraft writes down who had the Sync button before it.
fn sync_button_backup(home: &Home) -> PathBuf {
    home.dir.join("sync-button-before.json")
}

/// Take over the browser's Sync button, or give it back exactly as it was.
#[tauri::command]
async fn set_sync_button(enabled: bool) -> Result<drivecraft::takeover::Status, String> {
    local(move || async move {
        let home = Home::locate()?;
        let backup = sync_button_backup(&home);
        if enabled {
            drivecraft::takeover::take(&std::env::current_exe()?, &backup)?;
        } else {
            drivecraft::takeover::give_back(&backup)?;
        }
        if let Ok(log) = RunLog::open(&home.dir) {
            log.line(if enabled { "took over the browser's Sync button" } else { "gave the browser's Sync button back" });
        }
        Ok(drivecraft::takeover::status(&backup))
    })
    .await
}

#[derive(Serialize)]
struct SyncLinkView {
    /// The link itself, to pass on to another program if the user prefers.
    url: String,
    site_title: String,
    library_title: String,
    /// The library's address, ready for the connect form.
    address: String,
    suggested_name: String,
    suggested_folder: String,
    /// The connection that already has this library, if any.
    existing: Option<String>,
    /// The browser was signed in as someone else: their account.
    other_account: Option<String>,
    /// Sync was pressed inside this folder; DriveCraft connects whole libraries.
    folder: Option<String>,
    /// The program that had the Sync button before DriveCraft.
    other: Option<String>,
}

/// The Sync link waiting to be dealt with, worked out into what the connect form
/// needs. Asking takes it: it is handed over once.
#[tauri::command]
async fn take_sync_link(app: AppHandle) -> Result<Option<SyncLinkView>, String> {
    let Some(url) = app.state::<Shared>().sync_link.lock().unwrap().take() else {
        return Ok(None);
    };
    local(move || async move {
        let link = drivecraft::synclink::parse(&url).map_err(|e| anyhow!(e))?;
        let home = Home::locate()?;
        let graph = home.graph()?;
        let library = graph.library_of_link(&link).await?;
        let cat = home.catalog()?;
        let existing = cat.connections()?.into_iter().find(|c| c.drive_id == library.id).map(|c| c.name);
        let me = graph.me().await.map(|(_, account)| account).unwrap_or_default();
        let other_account = (!link.account.is_empty() && !me.is_empty() && !link.account.eq_ignore_ascii_case(&me))
            .then(|| link.account.clone());
        let name = link.suggested_name();
        let folder = std::env::var_os("USERPROFILE")
            .or(std::env::var_os("HOME"))
            .map(|h| PathBuf::from(h).join("DriveCraft").join(&name).display().to_string())
            .unwrap_or_default();
        Ok(Some(SyncLinkView {
            other: drivecraft::takeover::status(&sync_button_backup(&home)).other,
            url,
            site_title: link.site_title,
            library_title: if link.library_title.is_empty() { library.name } else { link.library_title },
            address: library.web_url,
            suggested_name: name,
            suggested_folder: folder,
            existing,
            other_account,
            folder: link.folder,
        }))
    })
    .await
}

/// Hand a Sync link to the program that had the Sync button before DriveCraft.
#[tauri::command]
async fn pass_on_sync_link(url: String) -> Result<(), String> {
    local(move || async move { drivecraft::takeover::pass_on(&sync_button_backup(&Home::locate()?), &url) }).await
}

/// A Sync link among a program's arguments, if the browser started it with one.
fn sync_link_arg(args: &[String]) -> Option<String> {
    args.iter().skip(1).find(|a| drivecraft::synclink::is_sync_link(a)).cloned()
}

/// The browser's Sync button was pressed: bring the window up and let it ask.
fn sync_pressed(app: &AppHandle, url: String) {
    *app.state::<Shared>().sync_link.lock().unwrap() = Some(url);
    show_window(app);
    let _ = app.emit("sync-link", ());
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
    let autostart = {
        use tauri_plugin_autostart::ManagerExt;
        app.autolaunch().is_enabled().unwrap_or(false)
    };
    local(move || async move {
        let shown = drivecraft::explorer::shown();
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
            connections.push(conn_view(&cat, &c, &running, &shown)?);
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
            autostart,
            link: drivecraft::net::active_link(),
            links: drivecraft::net::links(),
            sync_button: drivecraft::takeover::status(&sync_button_backup(&home)),
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
async fn disconnect(app: AppHandle, name: String) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let _turn = shared.gate.lock().await;
    shared.links.lock().unwrap().clear();
    let done = local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        // The folder goes back to being an ordinary one in File Explorer.
        drivecraft::explorer::turn_off(&c)?;
        cat.remove_connection(c.id)?;
        Ok(())
    })
    .await;
    tend_explorer(&app);
    done
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

/// What needs the user's attention in a library, the most serious first.
#[tauri::command]
async fn list_problems(name: String) -> Result<Vec<drivecraft::problems::Stored>, String> {
    local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        cat.prune_problems(&c)?;
        cat.problems(c.id)
    })
    .await
}

/// Forget one notice, or (with no kind) every notice of the library. Only the ones
/// that stay until dismissed can be forgotten; the rest clear when they are fixed.
#[tauri::command]
async fn dismiss_problems(name: String, kind: Option<String>, path: Option<String>) -> Result<(), String> {
    local(move || async move {
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        match kind {
            Some(kind) => {
                let kind = drivecraft::problems::Kind::parse(&kind).ok_or_else(|| anyhow!("unknown kind of problem"))?;
                cat.dismiss_problem(c.id, kind, path.as_deref().unwrap_or_default())?;
            }
            None => {
                cat.dismiss_lasting(c.id)?;
            }
        }
        Ok(())
    })
    .await
}

/// Show the item a problem is about in the file manager.
#[tauri::command]
async fn show_problem(name: String, path: String) -> Result<(), String> {
    local(move || async move {
        use std::path::Component;
        let cat = Home::locate()?.catalog()?;
        let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
        // Only ever somewhere inside the library's own folder.
        let inside = !path.is_empty()
            && std::path::Path::new(&path).components().all(|p| matches!(p, Component::Normal(_)));
        let mut target = if inside {
            drivecraft::paths::join_rel(&c.local_path, &path)
        } else {
            c.local_path.clone()
        };
        // The item itself may be gone; show the nearest folder that is still there.
        while !target.exists() {
            match target.parent() {
                Some(p) if p.starts_with(&c.local_path) => target = p.to_path_buf(),
                _ => bail!("{} was not found", c.local_path.display()),
            }
        }
        if cfg!(windows) && target != c.local_path {
            std::process::Command::new("explorer").arg("/select,").arg(&target).spawn()?;
        } else {
            open::that(&target)?;
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
            log.line(&format!(
                "published a seed for {}: {} items, {} bytes; {} parts with permissions of their own left out",
                conn.name, done.items, done.bytes, done.withheld
            ));
        }
        Ok(done)
    })
    .await
}

/// Open DriveCraft's web site in the browser. The address is fixed here.
#[tauri::command]
fn open_site() -> Result<(), String> {
    open::that("https://drive.thinecraft.io").map_err(|e| e.to_string())
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

/// Start DriveCraft, in the tray, whenever the user signs in to this computer.
#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let auto = app.autolaunch();
    if enabled { auto.enable() } else { auto.disable() }.map_err(|e| e.to_string())
}

fn tray_says(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(text));
    }
}

async fn run_sync(app: AppHandle, name: String, opts: Options) -> Finished {
    let shared = app.state::<Shared>();
    let _turn = shared.gate.lock().await;
    *shared.running.lock().unwrap() = Some(name.clone());
    tray_says(&app, &format!("DriveCraft: synchronizing {name}"));
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
        let conn_id = conn.id;
        let cat = &cat;
        let work = async move {
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
            // The log file gets every line. The window gets what a person can follow:
            // a large run names a file a hundred times a second, and showing each one
            // slows the run and freezes the window. What matters is always shown.
            let shown = Arc::new(std::sync::Mutex::new((std::time::Instant::now(), 0u32, 0u64)));
            move |line: &str| {
                log.line(line);
                let send = |text: String| {
                    let _ = emitter.emit("sync-log", LogLine { name: name.clone(), line: text });
                };
                let matters = ["stage:", "warning:", "failed:", "progress:", "network:", "pacing:", "read ", "scanned "]
                    .iter()
                    .any(|p| line.starts_with(p));
                let mut s = shown.lock().unwrap();
                if s.0.elapsed() >= std::time::Duration::from_secs(1) {
                    if s.2 > 0 {
                        send(format!("\u{2026} and {} more lines, in the log file", s.2));
                    }
                    *s = (std::time::Instant::now(), 0, 0);
                }
                if matters || s.1 < LINES_SHOWN_A_SECOND {
                    s.1 += 1;
                    send(line.to_string());
                } else {
                    s.2 += 1;
                }
            }
        };
        graph.set_event_hook(show.clone());
        let tell = {
            let (emitter, name) = (emitter.clone(), conn_name.clone());
            move |m: &drivecraft::engine::Moving| {
                let _ = emitter.emit("sync-progress", Progressing { name: name.clone(), moving: m.clone() });
            }
        };
        let outcome = Engine::new(cat, &graph, conn.clone(), opts)
            .with_log(show.clone())
            .with_progress(tell)
            .run()
            .await;
        if outcome.is_ok() && !dry_run {
            if let Err(e) = drivecraft::checkouts::refresh(cat, &graph, &conn).await {
                log.line(&format!("could not list checked-out files: {e:#}"));
            }
        }
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
        };
        let outcome = work.await;
        // A run that could not even start (not signed in, say) is a problem to show.
        if let (Err(e), false) = (&outcome, dry_run) {
            let _ = cat.record_stopped(conn_id, &format!("{e:#}"), drivecraft::problems::unix_now());
        }
        outcome
    })
    .await;

    shared.finished.lock().unwrap().insert(name.clone(), std::time::Instant::now());
    *shared.running.lock().unwrap() = None;
    tray_says(
        &app,
        &match &outcome {
            Ok((r, _, _)) if r.failed == 0 => format!("DriveCraft: {name} is up to date"),
            Ok((r, _, _)) => format!("DriveCraft: {name} finished with {} problems", r.failed),
            Err(_) => format!("DriveCraft: {name} could not synchronize"),
        },
    );
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

/// Synchronize every library. In the background (`unattended`) an ingest is left
/// alone: bringing a folder into a library is something the user starts.
async fn sync_all(app: AppHandle, unattended: bool) {
    let names = local(move || async move {
        let cat = Home::locate()?.catalog()?;
        Ok(cat
            .connections()?
            .into_iter()
            .filter(|c| !(unattended && c.mode == Mode::Ingest))
            .map(|c| c.name)
            .collect::<Vec<_>>())
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
    sync_all(app, false).await;
    Ok(())
}

/// Watch the folders of two-way connections, and synchronize one shortly after
/// something in it changes. Runs only while "keep in sync" is on.
fn watch_folders(app: AppHandle) {
    use notify::Watcher;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    /// A change is acted on once the folder has been quiet for this long, so that a
    /// file still being saved or a batch still being copied is sent once, whole.
    const QUIET: Duration = Duration::from_secs(5);
    /// Changes seen this soon after a run are the run's own downloads.
    const OWN: Duration = Duration::from_secs(4);

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut watcher) = notify::recommended_watcher(tx) else { return };
        let mut watched: HashMap<PathBuf, String> = HashMap::new();
        let mut changed: HashMap<String, Instant> = HashMap::new();
        let mut looked: Option<Instant> = None;
        loop {
            // Connections come and go; look again every so often.
            if looked.is_none_or(|t| t.elapsed() > Duration::from_secs(20)) {
                looked = Some(Instant::now());
                let want: HashMap<PathBuf, String> = Home::locate()
                    .and_then(|h| h.catalog())
                    .and_then(|c| c.connections())
                    .map(|l| {
                        l.into_iter()
                            .filter(|c| c.mode == Mode::Sync && c.local_path.is_dir())
                            .map(|c| (c.local_path, c.name))
                            .collect()
                    })
                    .unwrap_or_default();
                for gone in watched.keys().filter(|p| !want.contains_key(*p)) {
                    let _ = watcher.unwatch(gone);
                }
                for (folder, name) in &want {
                    if !watched.contains_key(folder)
                        && watcher.watch(folder, notify::RecursiveMode::Recursive).is_err()
                    {
                        eprintln!("could not watch {} for {name}", folder.display());
                    }
                }
                watched = want;
            }
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    if matches!(event.kind, notify::EventKind::Access(_)) {
                        continue;
                    }
                    let shared = app.state::<Shared>();
                    let running = shared.running.lock().unwrap().clone();
                    for path in &event.paths {
                        let ignored = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_none_or(drivecraft::paths::is_ignored_local_name);
                        if ignored {
                            continue;
                        }
                        let Some(name) = watched.iter().find(|(f, _)| path.starts_with(f)).map(|(_, n)| n.clone())
                        else {
                            continue;
                        };
                        let own = running.as_deref() == Some(name.as_str())
                            || shared.finished.lock().unwrap().get(&name).is_some_and(|t| t.elapsed() < OWN);
                        if !own {
                            changed.insert(name, Instant::now());
                        }
                    }
                }
                Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if !app.state::<Shared>().auto_sync.load(Ordering::Relaxed) {
                changed.clear();
                continue;
            }
            let ready: Vec<String> = changed
                .iter()
                .filter(|(_, at)| at.elapsed() >= QUIET)
                .map(|(n, _)| n.clone())
                .collect();
            for name in ready {
                changed.remove(&name);
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let opts = Options {
                        seed_dir: Home::locate().ok().map(|h| h.work_dir()),
                        ..Default::default()
                    };
                    run_sync(app, name, opts).await;
                });
            }
        }
    });
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

/// The argument the uninstaller starts DriveCraft with, before removing it.
const LEAVE_ARG: &str = "--uninstall-cleanup";

/// Put back everything DriveCraft changed on this computer outside its own folders:
/// the browser's Sync button, the folders listed in File Explorer and their
/// right-click menu, and starting with Windows. Files, the catalog, the sign-in
/// and the logs are not touched. Each step is attempted whatever the others do.
fn leave_as_found() {
    let exe = std::env::current_exe().unwrap_or_default();
    if let Ok(home) = Home::locate() {
        let _ = drivecraft::takeover::give_back(&sync_button_backup(&home));
        if let Ok(conns) = home.catalog().and_then(|c| c.connections()) {
            for c in conns {
                let _ = drivecraft::explorer::turn_off(&c);
            }
        }
        if let Ok(log) = RunLog::open(&home.dir) {
            log.line("DriveCraft is being removed: gave back the Sync button and File Explorer");
        }
    }
    let _ = drivecraft::explorer::set_menu(&exe, &[]);
    let name = exe.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !name.is_empty() {
        drivecraft::takeover::remove_startup("DriveCraft", &name);
    }
}

fn main() {
    if std::env::args().any(|a| a == LEAVE_ARG) {
        leave_as_found();
        return;
    }
    tauri::Builder::default()
        // A second copy hands over to the one already running and exits.
        .plugin(tauri_plugin_single_instance::init(|app, args, _dir| {
            if let Some(url) = sync_link_arg(&args) {
                return sync_pressed(app, url);
            }
            if let Some(file) = details_arg(&args) {
                return details_asked(app, file);
            }
            match menu_request(&args) {
                // File Explorer's right-click menu: do it, without bringing the window up.
                Some((out, file)) => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(check(app, out, file, String::new()));
                }
                None => show_window(app),
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![HIDDEN_ARG]),
        ))
        .manage(Shared {
            gate: tokio::sync::Mutex::new(()),
            running: std::sync::Mutex::new(None),
            auto_sync: AtomicBool::new(load_auto_sync()),
            finished: Default::default(),
            links: Default::default(),
            sync_link: Default::default(),
            details_for: Default::default(),
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
            open_site,
            list_problems,
            dismiss_problems,
            show_problem,
            set_explorer,
            save_icon,
            list_checkouts,
            library_permissions,
            check_file,
            file_details,
            save_details,
            pick_library_file,
            take_details_request,
            open_in_library,
            set_sync_button,
            take_sync_link,
            pass_on_sync_link,
            bin_page,
            restore_bin,
            set_network,
            publish_seed,
            hide_to_tray,
            set_auto_sync,
            set_autostart,
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
                        tauri::async_runtime::spawn(async move { sync_all(app, false).await });
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
                        sync_all(handle.clone(), true).await;
                    }
                }
            });
            watch_folders(app.handle().clone());
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    tend_explorer(&handle);
                    // If DriveCraft has the Sync button, keep it pointing at this program.
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = drivecraft::takeover::refresh(&exe);
                    }
                });
            }
            // Started by Windows at sign-in, or by File Explorer's right-click menu:
            // stay in the tray. Otherwise show the window.
            let args: Vec<String> = std::env::args().collect();
            if let Some(url) = sync_link_arg(&args) {
                sync_pressed(app.handle(), url);
            } else if let Some(file) = details_arg(&args) {
                details_asked(app.handle(), file);
            } else if let Some((out, file)) = menu_request(&args) {
                tauri::async_runtime::spawn(check(app.handle().clone(), out, file, String::new()));
            } else if !args.iter().any(|a| a == HIDDEN_ARG) {
                show_window(app.handle());
            }
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
