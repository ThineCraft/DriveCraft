use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use drivecraft::auth::{self, AppConfig, Authenticator};
use drivecraft::catalog::{Catalog, Connection, Mode};
use drivecraft::engine::{Engine, Options, Report};
use drivecraft::graph::GraphClient;
use drivecraft::sysinfo::{self, human_bytes};
use std::path::{Path, PathBuf};

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
        /// Print only warnings and the summary.
        #[arg(long)]
        quiet: bool,
    },
    /// Show what the catalog knows about a connection.
    Status { name: Option<String> },
    /// Change a connection between archive and sync.
    Mode {
        name: String,
        #[arg(value_enum)]
        mode: ModeArg,
    },
    /// Stop synchronizing a library. Local files are left where they are.
    Disconnect { name: String },
    /// Check a file out so that others cannot change it.
    Checkout { file: PathBuf },
    /// Check a file back in.
    Checkin {
        file: PathBuf,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Check this machine's setup.
    Doctor {
        /// Switch on Windows long path support (needs an administrator prompt).
        #[arg(long)]
        enable_long_paths: bool,
    },
}

struct Home {
    dir: PathBuf,
}

impl Home {
    fn locate() -> Result<Self> {
        let dir = match std::env::var_os("DRIVECRAFT_HOME") {
            Some(d) => PathBuf::from(d),
            None => directories::ProjectDirs::from("io", "ThineCraft", "DriveCraft")
                .context("could not find a place to keep DriveCraft's data")?
                .data_local_dir()
                .to_path_buf(),
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    fn app(&self) -> Result<AppConfig> {
        let text = std::fs::read_to_string(self.config_path()).map_err(|_| {
            anyhow!(
                "DriveCraft has not been set up yet. Run:\n  drivecraft setup --client-id <application id>\n\
                 See getting-started.html for how to create the app registration."
            )
        })?;
        Ok(serde_json::from_str(&text)?)
    }

    fn authenticator(&self) -> Result<Authenticator> {
        Authenticator::new(self.app()?, self.dir.join("signin.bin"))
    }

    fn catalog(&self) -> Result<Catalog> {
        Catalog::open(&self.dir.join("catalog.db"))
    }
}

fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(p)?)
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

fn print_report(conn: &Connection, r: &Report, dry_run: bool) {
    let verb = if dry_run { "would be" } else { "" };
    let mut parts: Vec<String> = Vec::new();
    let mut add = |n: u64, what: &str| {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    };
    add(r.downloaded, "downloaded");
    add(r.uploaded, "uploaded");
    add(r.adopted, "already matching");
    add(r.folders_created, "folders created here");
    add(r.remote_folders_created, "folders created on the server");
    add(r.renamed, "renamed here");
    add(r.moved_remote, "moved on the server");
    add(r.deleted_local, "removed here");
    add(r.deleted_remote, "deleted on the server");
    add(r.conflicts, "conflicts");
    add(r.skipped, "skipped");
    add(r.held_deletes, "deletions held back");
    add(r.failed, "FAILED");
    let summary = if parts.is_empty() {
        "nothing to do".to_string()
    } else {
        parts.join(", ")
    };
    println!(
        "{} [{}]: {}{}{}",
        conn.name,
        conn.mode.as_str(),
        if dry_run && !parts.is_empty() { format!("{verb}: ") } else { String::new() },
        summary,
        if r.bytes_down + r.bytes_up > 0 {
            format!(" ({} down, {} up)", human_bytes(r.bytes_down), human_bytes(r.bytes_up))
        } else {
            String::new()
        }
    );
    if r.long_paths > 0 {
        println!(
            "  {} paths are longer than this system or the server handles well. Run: drivecraft doctor",
            r.long_paths
        );
    }
}

async fn sync_once(
    home: &Home,
    cat: &Catalog,
    graph: &GraphClient<Authenticator>,
    conns: &[Connection],
    opts: &Options,
    quiet: bool,
    can_write: bool,
) -> Result<bool> {
    let mut ok = true;
    for conn in conns {
        let conn = cat.connection_by_id(conn.id)?;
        if conn.mode != Mode::Archive && !can_write && !opts.dry_run {
            println!(
                "{}: skipped. You signed in read-only, which cannot {}. Run: drivecraft login",
                conn.name,
                if conn.mode == Mode::Sync { "synchronize both ways" } else { "upload" }
            );
            ok = false;
            continue;
        }
        let engine = Engine::new(cat, graph, conn.clone(), opts.clone()).with_log(move |line| {
            if !quiet || line.starts_with("warning:") || line.starts_with("failed:") {
                println!("  {line}");
            }
        });
        match engine.run().await {
            Ok(report) => {
                print_report(&conn, &report, opts.dry_run);
                ok &= report.failed == 0;
            }
            Err(e) => {
                println!("{}: stopped: {e:#}", conn.name);
                ok = false;
            }
        }
    }
    if graph.times_throttled() > 0 {
        println!(
            "The server asked DriveCraft to slow down {} time(s); it waited as asked.",
            graph.times_throttled()
        );
    }
    let _ = home;
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
            let app = AppConfig {
                client_id: client_id.trim().to_string(),
                tenant: tenant.trim().to_string(),
            };
            if app.client_id.is_empty() {
                bail!("the application ID is empty");
            }
            std::fs::write(home.config_path(), serde_json::to_string_pretty(&app)?)?;
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
            let folder = absolute(&folder)?;
            let cat = home.catalog()?;
            let name = match name {
                Some(n) => n,
                None => folder
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .ok_or_else(|| anyhow!("give the connection a name with --name"))?,
            };
            if cat.connection(&name)?.is_some() {
                bail!("a connection named '{name}' already exists; choose another with --name");
            }
            for other in cat.connections()? {
                if folder.starts_with(&other.local_path) || other.local_path.starts_with(&folder) {
                    bail!(
                        "{} overlaps the folder of the connection '{}'",
                        folder.display(),
                        other.name
                    );
                }
            }
            let has_files = folder.is_dir()
                && std::fs::read_dir(&folder).map(|mut d| d.next().is_some()).unwrap_or(false);
            match mode {
                Mode::Ingest if !folder.is_dir() => bail!("the source folder {} does not exist", folder.display()),
                Mode::Sync if has_files && !merge => bail!(
                    "{} already has files in it. In Sync mode, files that are not in the library would be uploaded.\n\
                     Use an empty folder, or add --merge if that is what you want.",
                    folder.display()
                ),
                _ => {}
            }
            let auth = home.authenticator()?;
            let graph = GraphClient::new(auth);
            let drive = graph.resolve_library(&address).await?;
            let conn = cat.add_connection(&name, &drive.id, &drive.web_url, &folder, mode)?;
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
        Cmd::Sync { name, dry_run, confirm_deletes, update, every, quiet } => {
            let cat = home.catalog()?;
            let conns = pick(&cat, name.as_deref())?;
            let lock = std::fs::File::create(home.dir.join("sync.lock"))?;
            if lock.try_lock().is_err() {
                bail!("another DriveCraft sync is already running on this machine");
            }
            let auth = home.authenticator()?;
            let can_write = auth
                .current()
                .await
                .ok_or_else(|| anyhow!("not signed in. Run: drivecraft login"))?
                .can_write();
            let graph = GraphClient::new(auth);
            let opts = Options {
                dry_run,
                confirm_deletes,
                ingest_update: update,
                ..Default::default()
            };
            let mut ok = sync_once(&home, &cat, &graph, &conns, &opts, quiet, can_write).await?;
            if let Some(secs) = every {
                let secs = secs.max(30);
                println!("Watching. Synchronizing every {secs} seconds; press Ctrl+C to stop.");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                        _ = tokio::signal::ctrl_c() => break,
                    }
                    ok = sync_once(&home, &cat, &graph, &conns, &opts, true, can_write).await?;
                }
            }
            return Ok(ok);
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
            cat.remove_connection(c.id)?;
            println!("Disconnected '{}'. The files in {} were left as they are.", c.name, c.local_path.display());
        }
        Cmd::Checkout { file } => {
            let (graph, conn, id, rel) = locate_file(&home, &file)?;
            graph.check_out(&conn.drive_id, &id).await?;
            println!("Checked out {rel}");
        }
        Cmd::Checkin { file, comment } => {
            let (graph, conn, id, rel) = locate_file(&home, &file)?;
            graph.check_in(&conn.drive_id, &id, &comment).await?;
            println!("Checked in {rel}");
        }
        Cmd::Doctor { enable_long_paths } => {
            println!("DriveCraft {}", env!("CARGO_PKG_VERSION"));
            println!("Data folder: {}", home.dir.display());
            match home.app() {
                Ok(app) => println!("App registration: {} (tenant: {})", app.client_id, app.tenant),
                Err(_) => println!("App registration: not set up. Run: drivecraft setup --client-id <id>"),
            }
            if let Ok(auth) = home.authenticator() {
                match auth.current().await {
                    Some(t) => println!("Sign-in: saved ({})", if t.can_write() { "read and write" } else { "read only" }),
                    None => println!("Sign-in: none. Run: drivecraft login"),
                }
            }
            if enable_long_paths {
                match sysinfo::enable_windows_long_paths() {
                    Ok(()) => println!("Windows long path support: switched on. Programs pick it up after they restart."),
                    Err(e) => println!(
                        "Windows long path support could not be switched on: {e}\n  \
                         It is a machine-wide setting. Open a terminal as administrator and run this again,\n  \
                         or ask your IT department if it is managed by policy."
                    ),
                }
            }
            match sysinfo::windows_long_paths_enabled() {
                Some(true) => println!("Windows long path support: on"),
                Some(false) => println!(
                    "Windows long path support: OFF. Paths over 260 characters will sync, but many programs will not open them.\n  \
                     To switch it on: drivecraft doctor --enable-long-paths (from an administrator prompt)"
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

/// Find which connection a local file belongs to and its identifier on the server.
fn locate_file(home: &Home, file: &Path) -> Result<(GraphClient<Authenticator>, Connection, String, String)> {
    let file = absolute(file)?;
    let cat = home.catalog()?;
    for c in cat.connections()? {
        if let Ok(rel) = file.strip_prefix(&c.local_path) {
            let rel: Vec<String> = rel.components().map(|p| p.as_os_str().to_string_lossy().to_string()).collect();
            let rel = rel.join("/");
            let row = cat
                .local_by_path(c.id, &rel)?
                .ok_or_else(|| anyhow!("{rel} has not been synchronized yet, so the server does not know it"))?;
            let graph = GraphClient::new(home.authenticator()?);
            return Ok((graph, c, row.id, rel));
        }
    }
    bail!("{} is not inside a connected folder", file.display())
}
