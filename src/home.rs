//! Where DriveCraft keeps its settings, saved sign-in and catalog on this machine,
//! and the operations the command line and the desktop app both perform on them.

use crate::auth::{AppConfig, Authenticator};
use crate::catalog::{Catalog, Connection, Mode};
use crate::graph::{DriveInfo, GraphClient};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct Home {
    pub dir: PathBuf,
}

impl Home {
    pub fn locate() -> Result<Self> {
        let dir = match std::env::var_os("DRIVECRAFT_HOME") {
            Some(d) => PathBuf::from(d),
            None => directories::ProjectDirs::from("io", "ThineCraft", "DriveCraft")
                .context("could not find a place to keep DriveCraft's data")?
                .data_local_dir()
                .to_path_buf(),
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let home = Self { dir };
        crate::net::set_bind_address(home.bind_address());
        Ok(home)
    }

    fn network_path(&self) -> PathBuf {
        self.dir.join("network.json")
    }

    /// The local address DriveCraft sends from, if one has been chosen. The
    /// DRIVECRAFT_BIND_ADDRESS environment variable overrides the saved setting.
    pub fn bind_address(&self) -> Option<std::net::IpAddr> {
        if let Ok(v) = std::env::var("DRIVECRAFT_BIND_ADDRESS") {
            return v.trim().parse().ok();
        }
        let text = std::fs::read_to_string(self.network_path()).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v["bind_address"].as_str()?.parse().ok()
    }

    /// Choose, or clear, the local address DriveCraft sends from.
    pub fn save_bind_address(&self, addr: Option<std::net::IpAddr>) -> Result<()> {
        match addr {
            Some(ip) => std::fs::write(
                self.network_path(),
                serde_json::json!({ "bind_address": ip.to_string() }).to_string(),
            )?,
            None => {
                if self.network_path().exists() {
                    std::fs::remove_file(self.network_path())?;
                }
            }
        }
        crate::net::set_bind_address(addr);
        Ok(())
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// The app registration DriveCraft signs in with, if it has been set up.
    pub fn app(&self) -> Result<AppConfig> {
        let text = std::fs::read_to_string(self.config_path()).map_err(|_| {
            anyhow!(
                "DriveCraft has not been set up yet. It needs the application ID of its app registration.\n\
                 See https://drive.thinecraft.io/setup.html for how to create one."
            )
        })?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save_app(&self, client_id: &str, tenant: &str) -> Result<AppConfig> {
        let app = AppConfig {
            client_id: client_id.trim().to_string(),
            tenant: if tenant.trim().is_empty() {
                crate::auth::DEFAULT_TENANT.to_string()
            } else {
                tenant.trim().to_string()
            },
        };
        if app.client_id.is_empty() {
            bail!("the application ID is empty");
        }
        std::fs::write(self.config_path(), serde_json::to_string_pretty(&app)?)?;
        Ok(app)
    }

    pub fn authenticator(&self) -> Result<Authenticator> {
        Authenticator::new(self.app()?, self.dir.join("signin.bin"))
    }

    pub fn graph(&self) -> Result<GraphClient<Authenticator>> {
        Ok(GraphClient::new(self.authenticator()?))
    }

    /// Scratch space for files DriveCraft builds or unpacks, such as seeds.
    pub fn work_dir(&self) -> PathBuf {
        self.dir.join("work")
    }

    pub fn catalog(&self) -> Result<Catalog> {
        Catalog::open(&self.dir.join("catalog.db"))
    }

    /// Take the lock that keeps two synchronizations from running at once on this
    /// machine. It is released when the returned file is dropped.
    pub fn sync_lock(&self) -> Result<std::fs::File> {
        let lock = std::fs::File::create(self.dir.join("sync.lock"))?;
        if lock.try_lock().is_err() {
            bail!("another DriveCraft synchronization is already running on this machine");
        }
        Ok(lock)
    }
}

pub fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(p)?)
}

pub fn folder_has_files(folder: &Path) -> bool {
    folder.is_dir() && std::fs::read_dir(folder).map(|mut d| d.next().is_some()).unwrap_or(false)
}

/// What the user asked for when connecting a library.
pub struct ConnectRequest<'a> {
    pub address: &'a str,
    pub folder: &'a Path,
    pub mode: Mode,
    pub name: Option<&'a str>,
    /// Sync mode: accept a folder that already has files in it.
    pub merge: bool,
}

pub struct Connected {
    pub connection: Connection,
    pub drive: DriveInfo,
    pub folder_had_files: bool,
}

/// Check a connection request, find the library, and record the connection.
pub async fn connect_library(home: &Home, req: ConnectRequest<'_>) -> Result<Connected> {
    let folder = absolute(req.folder)?;
    let cat = home.catalog()?;
    let name = match req.name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => folder
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("give the connection a name"))?,
    };
    if cat.connection(&name)?.is_some() {
        bail!("a connection named '{name}' already exists; choose another name");
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
    let has_files = folder_has_files(&folder);
    match req.mode {
        Mode::Ingest if !folder.is_dir() => bail!("the source folder {} does not exist", folder.display()),
        Mode::Sync if has_files && !req.merge => bail!(
            "{} already has files in it. In Sync mode, files that are not in the library would be uploaded.\n\
             Use an empty folder, or allow merging if that is what you want.",
            folder.display()
        ),
        _ => {}
    }
    let drive = home.graph()?.resolve_library(req.address).await?;
    let connection = cat.add_connection(&name, &drive.id, &drive.web_url, &folder, req.mode)?;
    Ok(Connected {
        connection,
        drive,
        folder_had_files: has_files,
    })
}

/// Find which connection a local file belongs to, its identifier on the server, and
/// its path inside the library.
pub fn locate_file(home: &Home, file: &Path) -> Result<(Connection, String, String)> {
    let file = absolute(file)?;
    let cat = home.catalog()?;
    for c in cat.connections()? {
        if let Ok(rel) = file.strip_prefix(&c.local_path) {
            let rel: Vec<String> = rel
                .components()
                .map(|p| p.as_os_str().to_string_lossy().to_string())
                .collect();
            let rel = rel.join("/");
            let row = cat
                .local_by_path(c.id, &rel)?
                .ok_or_else(|| anyhow!("{rel} has not been synchronized yet, so the server does not know it"))?;
            return Ok((c, row.id, rel));
        }
    }
    bail!("{} is not inside a connected folder", file.display())
}
