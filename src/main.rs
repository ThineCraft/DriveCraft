use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use drivecraft::auth::{self, Authenticator};
use drivecraft::catalog::{Catalog, Connection, Mode};
use drivecraft::engine::{Engine, Options, Report};
use drivecraft::graph::GraphClient;
use drivecraft::home::{self, ConnectRequest, Home};
use drivecraft::graph::NetStats;
use drivecraft::runlog::{self, RunLog};
use drivecraft::sysinfo::{self, human_bytes};
use std::sync::Arc;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "drivecraft",
    version,
    about = "Synchronize SharePoint document libraries and OneDrive for work.",
    long_about = "DriveCraft synchronizes SharePoint document libraries and OneDrive for work.\n\n\
                  Early prototype. Start with Archive mode (download only) and --dry-run."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    /// Download only. Never changes anything on the server.
    Archive,
    /// Two-way synchronization.
    Sync,
    /// Upload a disk source into the library. Never changes the source.
    Ingest,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Archive => Mode::Archive,
            ModeArg::Sync => Mode::Sync,
            ModeArg::Ingest => Mode::Ingest,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Tell DriveCraft which app registration to sign in with.
    Setup {
        /// Application (client) ID of the DriveCraft app registration.
        #[arg(long)]
        client_id: String,
        /// Tenant ID or domain. Leave out to allow any work or school account.
        #[arg(long, default_value = auth::DEFAULT_TENANT)]
        tenant: String,
    },
    /// Sign in with a work or school account.
    Login {
        /// Ask only for permission to read. DriveCraft then cannot change the server.
        #[arg(long)]
        read_only: bool,
        /// Sign in by typing a code into a browser, instead of opening one here.
        #[arg(long)]
        device_code: bool,
    },
    /// Forget the saved sign-in.
    Logout,
    /// Show who is signed in.
    Whoami,
    /// Connect a document library to a local folder.
    Connect {
        /// The library's address copied from the browser, or the word "onedrive".
        address: String,
        /// The local folder. Its name is yours to choose.
        folder: PathBuf,
        #[arg(long, value_enum, default_value = "archive")]
        mode: ModeArg,
        /// A short name for this connection. Defaults to the folder's name.
        #[arg(long)]
        name: Option<String>,
        /// Sync mode only: allow a folder that already has files in it. Files that
        /// are not in the library will be uploaded.
        #[arg(long)]
        merge: bool,
    },
    /// List connected libraries.
    List,
    /// Synchronize one connection, or all of them.
    Sync {
        name: Option<String>,
        /// Show what would happen without changing anything here or on the server.
        #[arg(long)]
        dry_run: bool,
        /// Allow this run to delete many items on the server.
        #[arg(long)]
        confirm_deletes: bool,
        /// Ingest mode: upload a new version of source files that changed.
        #[arg(long)]
        update: bool,
        /// Keep running, synchronizing again every this many seconds.
        #[arg(long)]
        every: Option<u64>,
        /// Print only warnings and the summary. The log file still records everything.
        #[arg(long)]
        quiet: bool,
        /// How many files to transfer at once (1 to 16).
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// On a first scan, ignore any seed in the library and scan it in full.
        #[arg(long)]
        no_seed: bool,
    },
    /// Publish a seed: a snapshot of the library's file list, stored in the library,
    /// so that other computers can skip the first full scan.
    Seed { name: String },
    /// Show who may open what in a library: the library's own permissions, and each
    /// folder or file that has permissions of its own. Read-only; nothing is changed.
    Permissions {
        name: String,
        /// Read the permissions from the server again first.
        #[arg(long)]
        refresh: bool,
        /// Show only folders and files whose path, or whose people, groups or links,
        /// mention all of these words.
        #[arg(long)]
        find: Option<String>,
        /// Ask the server a few read-only questions and say what it answered: whether
        /// it marks restricted items, lets permissions be listed, and shows group
        /// memberships. For trying DriveCraft against a new library.
        #[arg(long)]
        probe: bool,
    },
    /// Show where DriveCraft's log files are.
    Logs,
    /// Choose which network connection DriveCraft sends through.
    Network {
        /// Send from this local address, for example 192.168.1.9. Traffic then
        /// leaves through that connection and its gateway.
        #[arg(long)]
        bind: Option<std::net::IpAddr>,
        /// Go back to letting the operating system choose.
        #[arg(long)]
        clear: bool,
    },
    /// Show what the catalog knows about a connection.
    Status { name: Option<String> },
    /// Show a library's folder in File Explorer, with a status icon beside each item.
    /// Windows shows the icons while the DriveCraft desktop app is running.
    Explorer {
        name: String,
        /// Take the folder out of File Explorer again.
        #[arg(long)]
        off: bool,
        /// The name to show in File Explorer. Defaults to the connection's name.
        #[arg(long)]
        label: Option<String>,
        /// An icon file (.ico) to show beside it.
        #[arg(long)]
        icon: Option<PathBuf>,
        #[arg(long, hide = true)]
        hold: Option<u64>,
    },
    /// List what needs your attention: files that could not be synchronized, copies
    /// that were kept, deletions waiting for you.
    Problems {
        name: Option<String>,
        /// Clear the notices that stay until dismissed (kept copies, long paths).
        #[arg(long)]
        dismiss_notices: bool,
    },
    /// Change a connection between archive and sync.
    Mode {
        name: String,
        #[arg(value_enum)]
        mode: ModeArg,
    },
    /// Stop synchronizing a library. Local files are left where they are.
    Disconnect { name: String },
    /// Show a file's library details (its columns), or change them.
    Details {
        file: PathBuf,
        /// Set a column, as NAME=VALUE. NAME is the column's own name, shown in
        /// brackets by this command. An empty value clears it. May be repeated.
        #[arg(long)]
        set: Vec<String>,
        /// Also show columns DriveCraft does not normally show.
        #[arg(long)]
        all: bool,
    },
    /// List the files that are checked out in a library, and by whom.
    Checkouts { name: Option<String> },
    /// Check a file out so that others cannot change it.
    Checkout { file: PathBuf },
    /// Check a file back in.
    Checkin {
        file: PathBuf,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Check this machine's setup.
    Doctor,
    /// Show what was deleted from a library and is still in the site's recycle bin,
    /// or put items back.
    RecycleBin {
        name: String,
        /// Put the item with this identifier back where it was. May be repeated.
        #[arg(long)]
        restore: Vec<String>,
        /// Show everything in the site's recycle bin, not only this library's.
        #[arg(long)]
        whole_site: bool,
        /// Show only items whose name or folder contains every one of these words.
        #[arg(long)]
        find: Option<String>,
    },

}

fn pick(cat: &Catalog, name: Option<&str>) -> Result<Vec<Connection>> {
    match name {
        Some(n) => Ok(vec![cat
            .connection(n)?
            .ok_or_else(|| anyhow!("no connection named '{n}'. Run: drivecraft list"))?]),
        None => {
            let all = cat.connections()?;
            if all.is_empty() {
                bail!("no libraries are connected yet. Run: drivecraft connect <address> <folder>");
            }
            Ok(all)
        }
    }
}

fn report_lines(conn: &Connection, r: &Report, dry_run: bool) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut add = |n: u64, what: &str| {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    };
    add(r.downloaded, "downloaded");
    add(r.uploaded, "uploaded");
    add(r.seeded, "items taken from a seed");
    add(r.adopted, "already matching");
    add(r.unchanged, "unchanged");
    add(r.folders_created, "folders created here");
    add(r.remote_folders_created, "folders created on the server");
    add(r.renamed, "renamed here");
    add(r.moved_remote, "moved on the server");
    add(r.deleted_local, "removed here");
    add(r.deleted_remote, "deleted on the server");
    add(r.conflicts, "conflicts");
    add(r.skipped, "skipped");
    add(r.held_deletes, "deletions held back");
    add(r.no_access, "not open to you, left alone");
    add(r.failed, "FAILED");
    let summary = if parts.is_empty() {
        "nothing to do".to_string()
    } else {
        parts.join(", ")
    };
    let mut out = vec![format!(
        "{} [{}]: {}{}{}",
        conn.name,
        conn.mode.as_str(),
        if dry_run && !parts.is_empty() { "would be: " } else { "" },
        summary,
        if r.bytes_down + r.bytes_up > 0 {
            format!(" ({} down, {} up)", human_bytes(r.bytes_down), human_bytes(r.bytes_up))
        } else {
            String::new()
        }
    )];
    if r.held_deletes > 0 {
        out.push("  To delete those items on the server too, run again with --confirm-deletes.".into());
    }
    if r.long_paths > 0 {
        out.push(format!(
            "  {} paths are longer than this system or the server handles well. Run: drivecraft doctor",
            r.long_paths
        ));
    }
    out
}

fn net_since(now: NetStats, before: NetStats) -> NetStats {
    NetStats {
        requests: now.requests - before.requests,
        retries: now.retries - before.retries,
        throttled: now.throttled - before.throttled,
        throttle_wait_ms: now.throttle_wait_ms - before.throttle_wait_ms,
        pace: now.pace,
        pace_cuts: now.pace_cuts - before.pace_cuts,
    }
}

async fn sync_once(
    cat: &Catalog,
    graph: &GraphClient<Authenticator>,
    log: &Arc<RunLog>,
    conns: &[Connection],
    opts: &Options,
    quiet: bool,
    can_write: bool,
) -> Result<bool> {
    let mut ok = true;
    let say = |line: &str| {
        log.line(line);
        println!("{line}");
    };
    for conn in conns {
        let conn = cat.connection_by_id(conn.id)?;
        if conn.mode != Mode::Archive && !can_write && !opts.dry_run {
            say(&format!(
                "{}: skipped. You signed in read-only, which cannot {}. Run: drivecraft login",
                conn.name,
                if conn.mode == Mode::Sync { "synchronize both ways" } else { "upload" }
            ));
            ok = false;
            continue;
        }
        log.line(&format!(
            "==== {} [{}]{} {} <-> {}",
            conn.name,
            conn.mode.as_str(),
            if opts.dry_run { " (preview)" } else { "" },
            conn.local_path.display(),
            conn.web_url
        ));
        let before = graph.stats();
        let engine_log = log.clone();
        let engine = Engine::new(cat, graph, conn.clone(), opts.clone()).with_log(move |line| {
            engine_log.line(line);
            let important = line.starts_with("warning:")
                || line.starts_with("failed:")
                || line.starts_with("progress:")
                || line.starts_with("scanned ");
            if !quiet || important {
                println!("  {line}");
            }
        });
        match engine.run().await {
            Ok(report) => {
                for line in report_lines(&conn, &report, opts.dry_run) {
                    say(&line);
                }
                for line in runlog::run_details(&report, &net_since(graph.stats(), before)) {
                    log.line(&line);
                    if !quiet {
                        println!("  {line}");
                    }
                }
                ok &= report.failed == 0;
                if !opts.dry_run {
                    if let Err(e) = drivecraft::checkouts::refresh(cat, graph, &conn).await {
                        log.line(&format!("could not list checked-out files: {e:#}"));
                    }
                }
            }
            Err(e) => {
                say(&format!("{}: stopped: {e:#}", conn.name));
                ok = false;
            }
        }
    }
    Ok(ok)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
}

async fn run(cli: Cli) -> Result<bool> {
    let home = Home::locate()?;
    match cli.cmd {
        Cmd::Setup { client_id, tenant } => {
            home.save_app(&client_id, &tenant)?;
            println!("Saved. Next: drivecraft login --read-only");
        }
        Cmd::Login { read_only, device_code } => {
            let auth = home.authenticator()?;
            let scopes = if read_only {
                auth::scopes_read_only()
            } else {
                auth::scopes_read_write()
            };
            let say = |s: &str| println!("{s}");
            let tokens = if device_code {
                auth.login_device_code(&scopes, say).await?
            } else {
                auth.login_browser(&scopes, say).await?
            };
            let graph = GraphClient::new(auth);
            let who = graph.me().await.map(|(n, u)| format!("{n} <{u}>")).unwrap_or_default();
            println!(
                "Signed in{}. Access: {}.",
                if who.is_empty() { String::new() } else { format!(" as {who}") },
                if tokens.can_write() { "read and write" } else { "read only" }
            );
        }
        Cmd::Logout => {
            home.authenticator()?.logout()?;
            println!("Signed out on this machine.");
        }
        Cmd::Whoami => {
            let auth = home.authenticator()?;
            let Some(t) = auth.current().await else {
                println!("Not signed in. Run: drivecraft login");
                return Ok(false);
            };
            let can_write = t.can_write();
            let graph = GraphClient::new(auth);
            let (name, upn) = graph.me().await?;
            println!("{name} <{upn}>  access: {}", if can_write { "read and write" } else { "read only" });
        }
        Cmd::Connect { address, folder, mode, name, merge } => {
            let mode: Mode = mode.into();
            let done = home::connect_library(
                &home,
                ConnectRequest {
                    address: &address,
                    folder: &folder,
                    mode,
                    name: name.as_deref(),
                    merge,
                },
            )
            .await?;
            let (conn, drive, has_files) = (done.connection, done.drive, done.folder_had_files);
            let folder = conn.local_path.clone();
            println!("Connected '{}' [{}]", conn.name, conn.mode.as_str());
            println!("  library: {}  ({})", drive.name, drive.web_url);
            println!("  folder:  {}", folder.display());
            if mode == Mode::Archive && has_files {
                println!("  The folder already has files. Identical ones are kept; different ones are kept as '(local copy)'.");
            }
            println!("Next: drivecraft sync {} --dry-run", conn.name);
        }
        Cmd::List => {
            let cat = home.catalog()?;
            let all = cat.connections()?;
            if all.is_empty() {
                println!("No libraries are connected.");
            }
            for c in all {
                println!("{}  [{}]  {}  <->  {}", c.name, c.mode.as_str(), c.local_path.display(), c.web_url);
            }
        }
        Cmd::Sync { name, dry_run, confirm_deletes, update, every, quiet, parallel, no_seed } => {
            let cat = home.catalog()?;
            let conns = pick(&cat, name.as_deref())?;
            let _lock = home.sync_lock()?;
            let auth = home.authenticator()?;
            let can_write = auth
                .current()
                .await
                .ok_or_else(|| anyhow!("not signed in. Run: drivecraft login"))?
                .can_write();
            let log = Arc::new(RunLog::open(&home.dir)?);
            let link = drivecraft::net::active_link().describe();
            log.line(&format!("sending through {link}"));
            println!("Sending through {link}");
            let mut graph = GraphClient::new(auth);
            let hook_log = log.clone();
            graph.set_event_hook(move |line| {
                hook_log.line(line);
                println!("  {line}");
            });
            let opts = Options {
                dry_run,
                confirm_deletes,
                ingest_update: update,
                concurrency: parallel.clamp(1, 16),
                seed_dir: (!no_seed).then(|| home.work_dir()),
                ..Default::default()
            };
            let mut ok = sync_once(&cat, &graph, &log, &conns, &opts, quiet, can_write).await?;
            println!("Log: {}", log.path.display());
            if let Some(secs) = every {
                let secs = secs.max(30);
                println!("Watching. Synchronizing every {secs} seconds; press Ctrl+C to stop.");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                        _ = tokio::signal::ctrl_c() => break,
                    }
                    ok = sync_once(&cat, &graph, &log, &conns, &opts, true, can_write).await?;
                }
            }
            return Ok(ok);
        }
        Cmd::Network { bind, clear } => {
            if clear {
                home.save_bind_address(None)?;
            } else if let Some(ip) = bind {
                // Fail now, with a clear message, if this computer has no such address.
                std::net::TcpListener::bind((ip, 0))
                    .map_err(|e| anyhow!("this computer has no connection with the address {ip}: {e}"))?;
                home.save_bind_address(Some(ip))?;
            }
            let active = drivecraft::net::active_link();
            println!("Sending through {}", active.describe());
            println!("Connections on this computer:");
            for l in drivecraft::net::links() {
                let mark = if Some(l.address) == active.address { "*" } else { " " };
                println!("  {mark} {}  {}", l.address, l.name);
            }
            println!("Pick one with 'drivecraft network --bind ADDRESS', or 'drivecraft network --clear' to let the system choose.");
        }
        Cmd::Seed { name } => {
            let cat = home.catalog()?;
            let conn = cat
                .connection(&name)?
                .ok_or_else(|| anyhow!("no connection named '{name}'. Run: drivecraft list"))?;
            let auth = home.authenticator()?;
            let can_write = auth
                .current()
                .await
                .ok_or_else(|| anyhow!("not signed in. Run: drivecraft login"))?
                .can_write();
            if !can_write {
                bail!("publishing a seed stores a file in the library, which needs write access. Run: drivecraft login");
            }
            let graph = GraphClient::new(auth);
            let done = drivecraft::seed::publish(&cat, &graph, &conn, &home.work_dir()).await?;
            println!(
                "Published a seed for '{}': {} items, {}. Stored in the library's {} folder.",
                conn.name,
                done.items,
                human_bytes(done.bytes),
                drivecraft::seed::FOLDER
            );
            if done.permissions {
                println!(
                    "The seed lists who may open each folder or file with permissions of its own. Anyone who can read \
                     the seed can see those names and permissions. Each computer removes what its own user may not open."
                );
            }
            if let Some(note) = &done.note {
                println!("{note}");
            }
            if done.withheld > 0 {
                println!(
                    "Left out {} folder(s) or file(s) with permissions of their own, and everything inside them. \
                     Each computer asks the library whether its own user may open them.",
                    done.withheld
                );
            }
            if done.replaced > 0 {
                println!("Replaced {} older seed(s).", done.replaced);
            }
            println!("Other computers that connect to this library will start from it instead of scanning everything.");
        }
        Cmd::Permissions { name, refresh, find, probe } => {
            let cat = home.catalog()?;
            let conn = cat
                .connection(&name)?
                .ok_or_else(|| anyhow!("no connection named '{name}'. Run: drivecraft list"))?;
            if probe {
                let auth = home.authenticator()?;
                auth.current().await.ok_or_else(|| anyhow!("not signed in. Run: drivecraft login"))?;
                let graph = GraphClient::new(auth);
                // An item this computer was refused, if there is one, shows what refusal looks like.
                let refused = cat.withheld_due(conn.id, i64::MAX)?.into_iter().next();
                println!("Asking the library '{}' a few read-only questions about permissions...", conn.name);
                for line in graph.probe_permissions(&conn.drive_id, refused.as_deref()).await {
                    println!("  {line}");
                }
                println!("Nothing was changed.");
                return Ok(true);
            }
            if refresh || cat.permissions_read(conn.id)?.0.is_none() {
                let auth = home.authenticator()?;
                auth.current().await.ok_or_else(|| anyhow!("not signed in. Run: drivecraft login"))?;
                let graph = GraphClient::new(auth);
                println!("Reading permissions from the library...");
                let read = drivecraft::permissions::read_all(&cat, &graph, &conn, |done, total| {
                    if total > 50 {
                        println!("  {done} of {total} items");
                    }
                })
                .await?;
                println!(
                    "Read the permissions of {} items{}.",
                    read.items,
                    if read.refused > 0 { format!("; the server would not show {}", read.refused) } else { String::new() }
                );
            }
            let view = drivecraft::permissions::view(&cat, &conn)?;
            let shown = match find.as_deref() {
                Some(words) => drivecraft::permissions::find(&view, words),
                None => view.entries.clone(),
            };
            for line in drivecraft::permissions::lines(&view, &shown) {
                println!("{line}");
            }
            if let Some(at) = view.read_at {
                let restricted = view.entries.iter().filter(|e| !e.path.is_empty()).count();
                println!(
                    "{restricted} folder(s) or file(s) with permissions of their own{}. Read {}.",
                    if find.is_some() { format!(", {} shown", shown.iter().filter(|e| !e.path.is_empty()).count()) } else { String::new() },
                    runlog::human_duration((drivecraft::problems::unix_now() - at).max(0) as f64) + " ago"
                );
            }
        }
        Cmd::Logs => {
            let dir = runlog::logs_dir(&home.dir);
            println!("Log folder: {}", dir.display());
            let mut files: Vec<_> = std::fs::read_dir(&dir)
                .map(|d| d.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.sort();
            match files.last() {
                Some(latest) => println!("Latest:     {}", latest.display()),
                None => println!("No runs have been logged yet."),
            }
        }
        Cmd::Status { name } => {
            let cat = home.catalog()?;
            for c in pick(&cat, name.as_deref())? {
                let n = cat.counts(c.id)?;
                println!("{} [{}]", c.name, c.mode.as_str());
                println!("  library: {}", c.web_url);
                println!("  folder:  {}", c.local_path.display());
                println!(
                    "  on the server: {} files in {} folders, {}",
                    n.remote_files,
                    n.remote_folders,
                    human_bytes(n.remote_bytes)
                );
                println!("  recorded here: {} items; {} waiting to be reconciled", n.local_items, n.pending);
                let t = cat.problem_tally(c.id)?;
                if t.total() > 0 {
                    println!(
                        "  problems: {} need attention, {} notices. Run: drivecraft problems {}",
                        t.bad + t.warn,
                        t.notice,
                        c.name
                    );
                }
                println!(
                    "  first scan: {}",
                    if c.delta_link.is_some() {
                        "complete"
                    } else if c.cursor.is_some() {
                        "interrupted; it will resume"
                    } else {
                        "not started"
                    }
                );
            }
        }
        Cmd::Explorer { name, off, label, icon, hold } => {
            let cat = home.catalog()?;
            let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
            if off {
                let m = drivecraft::explorer::turn_off(&c)?;
                println!("'{}' is no longer shown in File Explorer ({} items put back to ordinary files).", c.name, m.done);
            } else {
                let icon = match icon {
                    Some(p) => std::path::absolute(&p)?.display().to_string(),
                    None => drivecraft::explorer::default_icon(),
                };
                let m = drivecraft::explorer::turn_on(&cat, &c, label.as_deref().unwrap_or(&c.name), &icon)?;
                println!("'{}' is shown in File Explorer. {} items marked as synchronized, {} could not be marked.", c.name, m.done, m.failed);
                if let Some(secs) = hold {
                    let _link = drivecraft::explorer::connect(&c.local_path)?;
                    println!("connected for {secs}s");
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                }
            }
        }
        Cmd::Problems { name, dismiss_notices } => {
            let cat = home.catalog()?;
            for c in pick(&cat, name.as_deref())? {
                cat.prune_problems(&c)?;
                if dismiss_notices {
                    let n = cat.dismiss_lasting(c.id)?;
                    println!("{}: dismissed {n} notices", c.name);
                }
                let list = cat.problems(c.id)?;
                if list.is_empty() {
                    println!("{}: no problems", c.name);
                    continue;
                }
                println!("{} [{}]: {} to look at", c.name, c.mode.as_str(), list.len());
                let mut heading = None;
                for p in list {
                    if heading != Some(p.kind) {
                        heading = Some(p.kind);
                        println!("  {}", p.kind.title());
                    }
                    if p.path.is_empty() {
                        println!("    {}", p.message);
                    } else {
                        println!("    {}\n      {}", p.path, p.message);
                    }
                }
            }
        }
        Cmd::Mode { name, mode } => {
            let cat = home.catalog()?;
            let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
            let mode: Mode = mode.into();
            if c.mode == Mode::Ingest || mode == Mode::Ingest {
                bail!("an ingest connection cannot be converted; disconnect and connect again");
            }
            cat.set_mode(c.id, mode)?;
            println!("'{}' is now in {} mode.", c.name, mode.as_str());
            if mode == Mode::Sync {
                println!("Changes made in the local folder will now be sent to the server. Try: drivecraft sync {} --dry-run", c.name);
            }
        }
        Cmd::Disconnect { name } => {
            let cat = home.catalog()?;
            let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'"))?;
            drivecraft::explorer::turn_off(&c)?;
            cat.remove_connection(c.id)?;
            println!("Disconnected '{}'. The files in {} were left as they are.", c.name, c.local_path.display());
        }
        Cmd::Details { file, set, all } => {
            let (conn, id, rel) = home::locate_file(&home, &file)?;
            let graph = home.graph()?;
            let columns = graph.library_columns(&conn.drive_id).await?;
            if !set.is_empty() {
                let mut changes = serde_json::Map::new();
                for pair in &set {
                    let (name, value) = pair.split_once('=').ok_or_else(|| anyhow!("write --set as NAME=VALUE"))?;
                    let col = columns
                        .iter()
                        .find(|c| c.name.eq_ignore_ascii_case(name.trim()))
                        .ok_or_else(|| anyhow!("this library has no column named '{}'", name.trim()))?;
                    changes.insert(col.name.clone(), drivecraft::details::to_server(col, value.trim()).map_err(|e| anyhow!(e))?);
                }
                graph.set_item_fields(&conn.drive_id, &id, &serde_json::Value::Object(changes)).await?;
                println!("Saved.");
            }
            let fields = graph.item_fields(&conn.drive_id, &id).await?;
            println!("{rel}  ({})", conn.name);
            for c in columns.iter().filter(|c| all || drivecraft::details::worth_showing(c)) {
                let value = drivecraft::details::as_text(fields.values.get(&c.name));
                println!("  {} [{}]{}: {}", c.title, c.name, if c.read_only { " (read only)" } else { "" }, value);
            }
        }
        Cmd::Checkouts { name } => {
            let cat = home.catalog()?;
            let graph = home.graph()?;
            for c in pick(&cat, name.as_deref())? {
                if let Err(e) = drivecraft::checkouts::refresh(&cat, &graph, &c).await {
                    println!("{}: could not ask the server ({e:#}); showing what was last known", c.name);
                }
                let list = cat.checkouts(c.id)?;
                if list.is_empty() {
                    println!("{}: nothing is checked out", c.name);
                    continue;
                }
                println!("{}: {} checked out", c.name, list.len());
                for f in list {
                    let who = if f.mine { "you".to_string() } else if f.by.is_empty() { "someone".to_string() } else { f.by };
                    println!("  {}  (by {who})", f.path.unwrap_or_else(|| "a file that is not on this computer".into()));
                }
            }
        }
        Cmd::Checkout { file } => {
            let (conn, id, rel) = home::locate_file(&home, &file)?;
            let graph = home.graph()?;
            graph.check_out(&conn.drive_id, &id).await?;
            let cat = home.catalog()?;
            let before = cat.checkouts(conn.id)?;
            let me = graph.me().await.map(|(n, _)| n).unwrap_or_default();
            cat.note_checkout(conn.id, &id, Some(&me), drivecraft::problems::unix_now())?;
            let _ = drivecraft::checkouts::show_in_explorer(&cat, &conn, &before);
            println!("Checked out {rel}");
        }
        Cmd::Checkin { file, comment } => {
            let (conn, id, rel) = home::locate_file(&home, &file)?;
            home.graph()?.check_in(&conn.drive_id, &id, &comment).await?;
            let cat = home.catalog()?;
            let before = cat.checkouts(conn.id)?;
            cat.note_checkout(conn.id, &id, None, drivecraft::problems::unix_now())?;
            let _ = drivecraft::checkouts::show_in_explorer(&cat, &conn, &before);
            println!("Checked in {rel}");
        }
        Cmd::RecycleBin { name, restore, whole_site, find } => {
            let cat = home.catalog()?;
            let c = cat.connection(&name)?.ok_or_else(|| anyhow!("no connection named '{name}'. Run: drivecraft list"))?;
            let graph = home.graph()?;
            let site = graph.site_of(&c.drive_id).await?;
            if !restore.is_empty() {
                graph.restore_binned(&site, &restore).await?;
                println!("Restored {} item(s). They return to this computer on the next synchronization.", restore.len());
                return Ok(true);
            }
            let all = graph.recycle_bin(&site, 100_000).await?;
            let words: Vec<String> = find.unwrap_or_default().split_whitespace().map(str::to_lowercase).collect();
            let list: Vec<_> = all
                .iter()
                .filter(|b| whole_site || b.is_from(&site.library_path))
                .filter(|b| {
                    let hay = format!("{} {} {}", b.name, b.from, b.deleted_by).to_lowercase();
                    words.iter().all(|w| hay.contains(w))
                })
                .collect();
            if list.is_empty() {
                println!("{}: the recycle bin has nothing from this library", c.name);
            }
            for b in list {
                println!("{}  {}  {}/{}  ({}, deleted by {})", b.id, b.deleted_at, b.from, b.name, human_bytes(b.size), b.deleted_by);
            }
        }
        Cmd::Doctor => {
            println!("DriveCraft {}", env!("CARGO_PKG_VERSION"));
            println!("Data folder: {}", home.dir.display());
            match home.app() {
                Ok(app) => println!("App registration: {} (tenant: {})", app.client_id, app.tenant),
                Err(_) => println!("App registration: not set up. Run: drivecraft setup --client-id <id>"),
            }
            println!("Network: sending through {}", drivecraft::net::active_link().describe());
            if let Ok(auth) = home.authenticator() {
                match auth.current().await {
                    Some(t) => println!("Sign-in: saved ({})", if t.can_write() { "read and write" } else { "read only" }),
                    None => println!("Sign-in: none. Run: drivecraft login"),
                }
            }
            match sysinfo::windows_long_paths_enabled() {
                Some(true) => println!("Windows long path support: on"),
                Some(false) => println!(
                    "Windows long path support: OFF. Paths over 260 characters will sync, but many programs will not open them.\n  \
                     This is a Windows setting for this computer's administrator to decide; DriveCraft does not change it."
                ),
                None if cfg!(windows) => println!("Windows long path support: could not be read"),
                None => {}
            }
            if let Ok(cat) = home.catalog() {
                for c in cat.connections()? {
                    let exists = c.local_path.exists();
                    println!(
                        "Connection '{}' [{}]: {}{}",
                        c.name,
                        c.mode.as_str(),
                        c.local_path.display(),
                        if exists { "" } else { "  (folder not found)" }
                    );
                }
            }
        }
    }
    Ok(true)
}
