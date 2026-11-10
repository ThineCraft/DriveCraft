//! The synchronization engine.
//!
//! One run does three things in order:
//!
//! 1. Pull the server's change feed into the catalog.
//! 2. Bring the local folder in line with the server (Sync and Archive).
//! 3. Send local changes to the server (Sync), or upload a disk source (Ingest).
//!
//! Archive never calls anything that changes the server. Ingest never writes to the
//! source folder. Both rules hold by construction: the code paths that write are
//! simply not reached in those modes.

use crate::catalog::{Catalog, Connection, LocalRow, Mode};
use crate::paths::{self, child_rel, depth, file_name_rel, join_rel, parent_rel};
use crate::quickxor::hash_file;
use crate::remote::{Downloaded, ItemKind, Remote, RemoteError, RemoteItem, UploadTarget};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct Options {
    /// Report what would happen without changing files here or on the server.
    pub dry_run: bool,
    /// Allow a run to delete more than `max_deletes` items on the server.
    pub confirm_deletes: bool,
    pub max_deletes: usize,
    pub concurrency: usize,
    /// Ingest: upload a new version when a source file changed after it was ingested.
    pub ingest_update: bool,
    /// Where to unpack a seed. When set, a connection that has never been scanned
    /// looks for a seed in the library before doing a full scan.
    pub seed_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: false,
            confirm_deletes: false,
            max_deletes: 20,
            concurrency: 4,
            ingest_update: false,
            seed_dir: None,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub scanned: u64,
    pub downloaded: u64,
    pub uploaded: u64,
    pub adopted: u64,
    pub unchanged: u64,
    pub folders_created: u64,
    pub remote_folders_created: u64,
    pub renamed: u64,
    pub moved_remote: u64,
    pub deleted_local: u64,
    pub deleted_remote: u64,
    pub conflicts: u64,
    pub skipped: u64,
    pub failed: u64,
    pub held_deletes: u64,
    /// Items taken from a seed instead of a full scan.
    pub seeded: u64,
    pub long_paths: u64,
    pub bytes_down: u64,
    pub bytes_up: u64,
    /// Seconds spent reading the server's change feed.
    pub scan_secs: f64,
    /// Seconds spent bringing this computer in line with the server.
    pub apply_secs: f64,
    /// Seconds spent sending local changes, or ingesting.
    pub push_secs: f64,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn changes(&self) -> u64 {
        self.downloaded
            + self.uploaded
            + self.folders_created
            + self.remote_folders_created
            + self.renamed
            + self.moved_remote
            + self.deleted_local
            + self.deleted_remote
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stat {
    size: u64,
    mtime_ns: i64,
    is_dir: bool,
}

fn stat(path: &Path) -> Option<Stat> {
    let m = std::fs::metadata(path).ok()?;
    let mtime_ns = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Some(Stat {
        size: m.len(),
        mtime_ns,
        is_dir: m.is_dir(),
    })
}

fn set_mtime(path: &Path, mtime_ns: i64) -> std::io::Result<()> {
    if mtime_ns <= 0 {
        return Ok(());
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns as u64);
    std::fs::File::options().write(true).open(path)?.set_modified(t)
}

fn same_content(local: &LocalRow, remote: &RemoteItem) -> bool {
    match (&local.hash, &remote.hash) {
        (Some(a), Some(b)) => a == b,
        _ => local.ctag.is_some() && local.ctag == remote.ctag,
    }
}

fn verifies(d: &Downloaded, item: &RemoteItem) -> bool {
    match &item.hash {
        Some(h) => *h == d.hash,
        None => d.size == item.size,
    }
}

struct Download {
    item: RemoteItem,
    rel: String,
}

/// Periodic "how far along" lines for long transfers: count, rate and time left.
struct Progress {
    verb: &'static str,
    total: usize,
    total_bytes: u64,
    started: std::time::Instant,
    last: std::time::Instant,
}

impl Progress {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(30);

    fn new(verb: &'static str, total: usize, total_bytes: u64) -> Self {
        let now = std::time::Instant::now();
        Self {
            verb,
            total,
            total_bytes,
            started: now,
            last: now,
        }
    }

    fn tick(&mut self, done: usize, bytes: u64) -> Option<String> {
        if self.last.elapsed() < Self::EVERY || done >= self.total {
            return None;
        }
        self.last = std::time::Instant::now();
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let rate = done as f64 / secs;
        let left = if rate > 0.0 {
            crate::runlog::human_duration((self.total - done) as f64 / rate)
        } else {
            "unknown".into()
        };
        Some(format!(
            "progress: {} {} of {} files ({} of {}), {:.1} files per second, {} per second, about {} left",
            self.verb,
            done,
            self.total,
            crate::sysinfo::human_bytes(bytes),
            crate::sysinfo::human_bytes(self.total_bytes),
            rate,
            crate::sysinfo::human_bytes((bytes as f64 / secs) as u64),
            left
        ))
    }
}

enum UploadKind {
    New { parent_id: String, name: String },
    Replace { item_id: String, etag: Option<String> },
}

struct Upload {
    rel: String,
    abs: PathBuf,
    st: Stat,
    kind: UploadKind,
}

pub struct Engine<'a, R: Remote> {
    cat: &'a Catalog,
    remote: &'a R,
    conn: Connection,
    opts: Options,
    report: Report,
    log: Box<dyn Fn(&str) + 'a>,
    root: PathBuf,
    server_prefix_len: usize,
}

impl<'a, R: Remote> Engine<'a, R> {
    pub fn new(cat: &'a Catalog, remote: &'a R, conn: Connection, opts: Options) -> Self {
        let root = conn.local_path.clone();
        let server_prefix_len = url::Url::parse(&conn.web_url)
            .map(|u| paths::percent_decode(u.path()).chars().count() + 1)
            .unwrap_or(0);
        Self {
            cat,
            remote,
            conn,
            opts,
            report: Report::default(),
            log: Box::new(|_| {}),
            root,
            server_prefix_len,
        }
    }

    /// Receive a line for every action taken (or, in a dry run, that would be taken).
    pub fn with_log(mut self, log: impl Fn(&str) + 'a) -> Self {
        self.log = Box::new(log);
        self
    }

    fn note(&self, what: impl AsRef<str>) {
        if self.opts.dry_run {
            (self.log)(&format!("would {}", what.as_ref()));
        } else {
            (self.log)(what.as_ref());
        }
    }

    fn warn(&mut self, what: impl Into<String>) {
        let w = what.into();
        (self.log)(&format!("warning: {w}"));
        self.report.warnings.push(w);
    }

    fn fail(&mut self, what: impl Into<String>) {
        self.report.failed += 1;
        let w = what.into();
        (self.log)(&format!("failed: {w}"));
        self.report.warnings.push(format!("failed: {w}"));
    }

    fn abs(&self, rel: &str) -> PathBuf {
        join_rel(&self.root, rel)
    }

    fn id(&self) -> i64 {
        self.conn.id
    }

    pub async fn run(mut self) -> Result<Report> {
        match self.conn.mode {
            Mode::Ingest => {
                if !self.root.is_dir() {
                    bail!("source folder {} was not found", self.root.display());
                }
            }
            Mode::Sync | Mode::Archive => {
                if !self.root.exists() {
                    let tracked = self.cat.counts(self.id())?.local_items;
                    if tracked > 0 {
                        bail!(
                            "the local folder {} is missing, but {} items were synchronized into it before. \
                             Nothing was changed. Reconnect the drive it lives on, or disconnect this library.",
                            self.root.display(),
                            tracked
                        );
                    }
                    if !self.opts.dry_run {
                        std::fs::create_dir_all(&self.root)
                            .with_context(|| format!("creating {}", self.root.display()))?;
                    }
                }
            }
        }

        let started = std::time::Instant::now();
        let seeded = self.try_seed().await;
        self.pull_remote(seeded).await?;
        self.report.scan_secs = started.elapsed().as_secs_f64();
        (self.log)(&format!(
            "read {} changes from the server in {:.1}s",
            self.report.scanned, self.report.scan_secs
        ));
        let started = std::time::Instant::now();
        match self.conn.mode {
            Mode::Archive => {
                self.apply_remote().await?;
                self.report.apply_secs = started.elapsed().as_secs_f64();
            }
            Mode::Sync => {
                self.apply_remote().await?;
                self.report.apply_secs = started.elapsed().as_secs_f64();
                let started = std::time::Instant::now();
                self.push_local().await?;
                self.report.push_secs = started.elapsed().as_secs_f64();
            }
            Mode::Ingest => {
                if !self.opts.dry_run {
                    self.cat.clear_all_dirty(self.id())?;
                }
                self.ingest().await?;
                self.report.push_secs = started.elapsed().as_secs_f64();
            }
        }
        if !self.opts.dry_run {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.cat.set_last_sync(self.id(), now)?;
        }
        Ok(self.report)
    }

    // ---- step 1: the server's change feed ------------------------------------

    /// On a first scan, look for a seed in the library. Any problem with a seed is
    /// only a reason to do the full scan instead.
    async fn try_seed(&mut self) -> bool {
        let Some(dir) = self.opts.seed_dir.clone() else { return false };
        let fresh = self.conn.delta_link.is_none() && self.conn.cursor.is_none() && !self.conn.resyncing;
        if !fresh || self.opts.dry_run {
            return false;
        }
        match crate::seed::fetch(self.cat, self.remote, &self.conn, &dir).await {
            Ok(Some(info)) => {
                self.report.seeded = info.items;
                (self.log)(&format!(
                    "started from a seed in the library: {} items, so the full scan is skipped",
                    info.items
                ));
                true
            }
            Ok(None) => false,
            Err(e) => {
                self.warn(format!("a seed was found but could not be used ({e:#}); doing a full scan"));
                false
            }
        }
    }

    async fn pull_remote(&mut self, seeded: bool) -> Result<()> {
        let mut first_after_seed = seeded;
        let mut resynced = false;
        let mut next_progress = 5000;
        let scan_started = std::time::Instant::now();
        loop {
            self.conn = self.cat.connection_by_id(self.id())?;
            let link = self.conn.cursor.clone().or(self.conn.delta_link.clone());
            let page = match self.remote.delta(&self.conn.drive_id, link.as_deref()).await {
                Ok(p) => p,
                Err(RemoteError::ResyncRequired) if !resynced => {
                    resynced = true;
                    self.warn("the server's change bookmark expired; rescanning the whole library");
                    self.cat.begin_resync(self.id())?;
                    continue;
                }
                // A seed's bookmark was made on another computer, possibly by another
                // person. If the server will not accept it here, scan in full instead.
                Err(e) if first_after_seed && !resynced => {
                    resynced = true;
                    self.warn(format!(
                        "the seed's bookmark was not accepted here ({e}); rescanning the whole library"
                    ));
                    self.cat.begin_resync(self.id())?;
                    continue;
                }
                Err(e) => return Err(e).context("reading changes from the server"),
            };
            first_after_seed = false;
            self.cat.apply_delta_page(
                &self.conn,
                &page.items,
                page.next_link.as_deref(),
                page.delta_link.as_deref(),
            )?;
            self.report.scanned += page.items.len() as u64;
            if self.report.scanned >= next_progress {
                (self.log)(&format!(
                    "scanned {} items on the server ({:.0} per second)...",
                    self.report.scanned,
                    self.report.scanned as f64 / scan_started.elapsed().as_secs_f64().max(0.001)
                ));
                next_progress += 5000;
            }
            if page.delta_link.is_some() {
                break;
            }
            if page.next_link.is_none() {
                bail!("the server's change feed ended without a bookmark");
            }
        }
        self.conn = self.cat.connection_by_id(self.id())?;
        Ok(())
    }

    // ---- step 2: bring the local folder in line with the server --------------

    async fn apply_remote(&mut self) -> Result<()> {
        let dirty = self.cat.dirty_items(self.id())?;
        let mut live: Vec<(String, RemoteItem)> = Vec::new();
        let mut dead: Vec<(Option<LocalRow>, RemoteItem)> = Vec::new();
        for it in dirty {
            if it.deleted {
                let l = self.cat.local_get(self.id(), &it.id)?;
                dead.push((l, it));
            } else if it.is_root {
                self.cat.clear_dirty(self.id(), &it.id)?;
            } else {
                match self.cat.remote_path(self.id(), &it)? {
                    Some(p) => live.push((p, it)),
                    None => {
                        // Inside something DriveCraft does not synchronize.
                        if !self.opts.dry_run {
                            self.cat.clear_dirty(self.id(), &it.id)?;
                        }
                    }
                }
            }
        }
        live.sort_by(|a, b| depth(&a.0).cmp(&depth(&b.0)).then_with(|| a.0.cmp(&b.0)));

        let mut downloads = Vec::new();
        for (path, it) in live {
            // The seed folder is DriveCraft's own; it is never synchronized as content.
            if crate::seed::is_seed_path(&path) {
                if !self.opts.dry_run {
                    self.cat.clear_dirty(self.id(), &it.id)?;
                }
                continue;
            }
            let r = match it.kind {
                ItemKind::Folder => self.apply_folder(&path, &it),
                ItemKind::File => self.apply_file(&path, &it, &mut downloads),
                ItemKind::Other => {
                    self.report.skipped += 1;
                    if !self.opts.dry_run {
                        self.cat.clear_dirty(self.id(), &it.id)?;
                    }
                    Ok(())
                }
            };
            if let Err(e) = r {
                self.fail(format!("{path}: {e:#}"));
            }
        }
        self.run_downloads(downloads).await?;

        dead.sort_by_key(|(l, _)| std::cmp::Reverse(l.as_ref().map(|l| depth(&l.rel_path)).unwrap_or(0)));
        for (l, it) in dead {
            if let Err(e) = self.apply_deleted(l, &it) {
                self.fail(format!("{}: {e:#}", it.name));
            }
        }
        Ok(())
    }

    /// Clear `rel` of anything recorded under a different item, so `self_id` can take
    /// it. A recorded occupant is parked under a temporary name; it is waiting for its
    /// own rename or deletion later in the same run.
    fn make_way(&mut self, rel: &str, self_id: &str) -> Result<()> {
        let Some(occ) = self.cat.local_by_path(self.id(), rel)? else {
            return Ok(());
        };
        if occ.id == self_id {
            return Ok(());
        }
        let abs = self.abs(rel);
        if std::fs::symlink_metadata(&abs).is_ok() {
            let parked = child_rel(parent_rel(rel), &paths::partial_name(&occ.id));
            std::fs::rename(&abs, self.abs(&parked))
                .with_context(|| format!("moving {rel} out of the way"))?;
            if occ.kind == ItemKind::Folder {
                self.cat.local_rename_prefix(self.id(), rel, &parked)?;
            }
            let mut moved = occ;
            moved.rel_path = parked;
            self.cat.local_upsert(self.id(), &moved)?;
        } else {
            self.cat.local_delete(self.id(), &occ.id)?;
        }
        Ok(())
    }

    /// Something DriveCraft did not put there is in the way: keep it under another name.
    fn keep_aside(&mut self, abs: &Path, rel: &str) -> Result<()> {
        let aside = paths::local_copy_path(abs);
        std::fs::rename(abs, &aside).with_context(|| format!("keeping a copy of {rel}"))?;
        self.report.conflicts += 1;
        self.warn(format!(
            "{rel} differs from the server's copy; your version was kept as {}",
            aside.file_name().unwrap_or_default().to_string_lossy()
        ));
        Ok(())
    }

    fn apply_folder(&mut self, path: &str, it: &RemoteItem) -> Result<()> {
        let abs = self.abs(path);
        let folder_row = |rel: &str| LocalRow {
            id: it.id.clone(),
            rel_path: rel.to_string(),
            kind: ItemKind::Folder,
            size: 0,
            mtime_ns: 0,
            ctag: None,
            hash: None,
        };
        match self.cat.local_get(self.id(), &it.id)? {
            Some(l) if l.rel_path == path => {
                if !self.opts.dry_run {
                    if !abs.is_dir() && self.conn.mode == Mode::Archive {
                        std::fs::create_dir_all(&abs)?;
                    }
                    self.cat.clear_dirty(self.id(), &it.id)?;
                }
            }
            Some(l) => {
                self.note(format!("rename folder {} -> {path}", l.rel_path));
                if self.opts.dry_run {
                    return Ok(());
                }
                let old_abs = self.abs(&l.rel_path);
                let case_only = l.rel_path.to_lowercase() == path.to_lowercase();
                if !case_only {
                    self.make_way(path, &it.id)?;
                    if let Ok(m) = std::fs::symlink_metadata(&abs) {
                        let empty_dir = m.is_dir()
                            && std::fs::read_dir(&abs).map(|mut d| d.next().is_none()).unwrap_or(false);
                        if empty_dir {
                            std::fs::remove_dir(&abs)?;
                        } else {
                            self.keep_aside(&abs, path)?;
                        }
                    }
                }
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if old_abs.is_dir() {
                    std::fs::rename(&old_abs, &abs)?;
                } else {
                    std::fs::create_dir_all(&abs)?;
                }
                self.cat.local_rename_prefix(self.id(), &l.rel_path, path)?;
                self.cat.local_upsert(self.id(), &folder_row(path))?;
                self.cat.clear_dirty(self.id(), &it.id)?;
                self.report.renamed += 1;
            }
            None => {
                if self.opts.dry_run {
                    if !abs.is_dir() {
                        self.note(format!("create folder {path}"));
                    }
                    return Ok(());
                }
                self.make_way(path, &it.id)?;
                if !abs.is_dir() {
                    if std::fs::symlink_metadata(&abs).is_ok() {
                        self.keep_aside(&abs, path)?;
                    }
                    std::fs::create_dir_all(&abs)?;
                    self.report.folders_created += 1;
                    self.note(format!("create folder {path}"));
                }
                self.cat.local_upsert(self.id(), &folder_row(path))?;
                self.cat.clear_dirty(self.id(), &it.id)?;
            }
        }
        Ok(())
    }

    fn queue_download(&mut self, downloads: &mut Vec<Download>, path: &str, it: &RemoteItem) {
        self.note(format!("download {path} ({} bytes)", it.size));
        if self.opts.dry_run {
            self.report.downloaded += 1;
            return;
        }
        if cfg!(windows) && self.abs(path).as_os_str().len() >= paths::WINDOWS_PATH_LIMIT {
            self.report.long_paths += 1;
            if self.report.long_paths <= 10 {
                self.warn(format!(
                    "long path: {path} exceeds {} characters on this machine; some programs will not open it",
                    paths::WINDOWS_PATH_LIMIT
                ));
            }
        }
        downloads.push(Download {
            item: it.clone(),
            rel: path.to_string(),
        });
    }

    fn apply_file(&mut self, path: &str, it: &RemoteItem, downloads: &mut Vec<Download>) -> Result<()> {
        let abs = self.abs(path);
        let mut local = self.cat.local_get(self.id(), &it.id)?;

        // Moved or renamed on the server.
        if let Some(cur) = &local {
            if cur.rel_path != path {
                self.note(format!("rename {} -> {path}", cur.rel_path));
                if self.opts.dry_run {
                    return Ok(());
                }
                let old_abs = self.abs(&cur.rel_path);
                if cur.rel_path.to_lowercase() != path.to_lowercase() {
                    self.make_way(path, &it.id)?;
                    if std::fs::symlink_metadata(&abs).is_ok() {
                        self.keep_aside(&abs, path)?;
                    }
                }
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if old_abs.is_file() {
                    std::fs::rename(&old_abs, &abs)?;
                    self.report.renamed += 1;
                }
                let mut moved = cur.clone();
                moved.rel_path = path.to_string();
                self.cat.local_upsert(self.id(), &moved)?;
                local = Some(moved);
            }
        }

        match local {
            Some(cur) => {
                let unchanged_on_server = same_content(&cur, it);
                match stat(&abs) {
                    None => {
                        // Missing here. In Sync, a file deleted here and untouched on the
                        // server is a local deletion, handled when local changes are sent.
                        if self.conn.mode == Mode::Sync && unchanged_on_server {
                            if !self.opts.dry_run {
                                self.cat.clear_dirty(self.id(), &it.id)?;
                            }
                        } else {
                            self.queue_download(downloads, path, it);
                        }
                    }
                    Some(st) => {
                        let changed_here = st.size != cur.size || st.mtime_ns != cur.mtime_ns;
                        if unchanged_on_server {
                            if !self.opts.dry_run {
                                self.cat.clear_dirty(self.id(), &it.id)?;
                            }
                        } else if !changed_here {
                            self.queue_download(downloads, path, it);
                        } else {
                            // Changed in both places.
                            let (_, h) = hash_file(&abs)?;
                            if it.hash.as_deref() == Some(h.as_str()) {
                                if !self.opts.dry_run {
                                    self.adopt(path, it, st)?;
                                }
                            } else {
                                self.note(format!("keep a local copy of {path} (changed here and on the server)"));
                                if self.opts.dry_run {
                                    self.report.conflicts += 1;
                                } else {
                                    self.keep_aside(&abs, path)?;
                                }
                                self.queue_download(downloads, path, it);
                            }
                        }
                    }
                }
            }
            None => {
                if !self.opts.dry_run {
                    self.make_way(path, &it.id)?;
                }
                match stat(&abs) {
                    None => self.queue_download(downloads, path, it),
                    Some(st) if !st.is_dir => {
                        // A file is already here that DriveCraft did not record.
                        let (size, h) = hash_file(&abs)?;
                        let same = match &it.hash {
                            Some(rh) => *rh == h,
                            None => size == it.size,
                        };
                        if same {
                            if !self.opts.dry_run {
                                self.adopt(path, it, st)?;
                            }
                            self.report.adopted += 1;
                        } else {
                            if self.opts.dry_run {
                                self.report.conflicts += 1;
                            } else {
                                self.keep_aside(&abs, path)?;
                            }
                            self.queue_download(downloads, path, it);
                        }
                    }
                    Some(_) => {
                        if !self.opts.dry_run {
                            self.keep_aside(&abs, path)?;
                        }
                        self.queue_download(downloads, path, it);
                    }
                }
            }
        }
        Ok(())
    }

    /// Record that the file on disk already matches the server's copy.
    fn adopt(&mut self, path: &str, it: &RemoteItem, st: Stat) -> Result<()> {
        self.cat.local_upsert(
            self.id(),
            &LocalRow {
                id: it.id.clone(),
                rel_path: path.to_string(),
                kind: ItemKind::File,
                size: st.size,
                mtime_ns: st.mtime_ns,
                ctag: it.ctag.clone(),
                hash: it.hash.clone(),
            },
        )?;
        self.cat.clear_dirty(self.id(), &it.id)?;
        Ok(())
    }

    async fn run_downloads(&mut self, downloads: Vec<Download>) -> Result<()> {
        if downloads.is_empty() {
            return Ok(());
        }
        let remote = self.remote;
        let drive = self.conn.drive_id.clone();
        let root = self.root.clone();
        let (drive_ref, root_ref) = (&drive, &root);
        let total = downloads.len();
        let total_bytes: u64 = downloads.iter().map(|d| d.item.size).sum();
        let mut done = 0usize;
        let mut progress = Progress::new("downloaded", total, total_bytes);
        let mut stream = futures_util::stream::iter(downloads.into_iter().map(|d| async move {
            let abs = join_rel(root_ref, &d.rel);
            let tmp = abs.with_file_name(paths::partial_name(&d.item.id));
            if let Some(parent) = abs.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let res = fetch_verified(remote, drive_ref, &d.item, &tmp).await;
            (d, abs, tmp, res)
        }))
        .buffer_unordered(self.opts.concurrency.max(1));

        while let Some((d, abs, tmp, res)) = stream.next().await {
            done += 1;
            match res {
                Ok(odd) => {
                    let finish = (|| -> Result<Stat> {
                        if let Some(m) = d.item.mtime_ns {
                            set_mtime(&tmp, m)?;
                        }
                        std::fs::rename(&tmp, &abs)?;
                        stat(&abs).context("the downloaded file disappeared")
                    })();
                    match finish {
                        Ok(st) => {
                            self.cat.local_upsert(
                                self.id(),
                                &LocalRow {
                                    id: d.item.id.clone(),
                                    rel_path: d.rel.clone(),
                                    kind: ItemKind::File,
                                    size: st.size,
                                    mtime_ns: st.mtime_ns,
                                    ctag: d.item.ctag.clone(),
                                    hash: d.item.hash.clone(),
                                },
                            )?;
                            self.cat.clear_dirty(self.id(), &d.item.id)?;
                            self.report.downloaded += 1;
                            self.report.bytes_down += st.size;
                            if let Some(odd) = odd {
                                self.warn(format!("{}: {odd}", d.rel));
                            }
                            if let Some(line) = progress.tick(done, self.report.bytes_down) {
                                (self.log)(&line);
                            }
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            self.fail(format!("{}: {e:#}", d.rel));
                        }
                    }
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.fail(format!("{}: {e}", d.rel));
                }
            }
        }
        Ok(())
    }

    fn remove_local_file(&mut self, l: &LocalRow) -> Result<()> {
        let abs = self.abs(&l.rel_path);
        let Some(st) = stat(&abs) else { return Ok(()) };
        if st.is_dir {
            return Ok(());
        }
        if st.size != l.size || st.mtime_ns != l.mtime_ns {
            self.warn(format!(
                "{} was deleted on the server but has been changed here; it was kept",
                l.rel_path
            ));
            return Ok(());
        }
        std::fs::remove_file(&abs)?;
        self.report.deleted_local += 1;
        Ok(())
    }

    fn apply_deleted(&mut self, local: Option<LocalRow>, it: &RemoteItem) -> Result<()> {
        let Some(l) = local else {
            if !self.opts.dry_run {
                self.cat.purge_remote(self.id(), &it.id)?;
            }
            return Ok(());
        };
        self.note(format!("remove {} (deleted on the server)", l.rel_path));
        if self.opts.dry_run {
            self.report.deleted_local += 1;
            return Ok(());
        }
        match l.kind {
            ItemKind::Folder => {
                for child in self.cat.local_under(self.id(), &l.rel_path)? {
                    if child.kind == ItemKind::Folder {
                        let _ = std::fs::remove_dir(self.abs(&child.rel_path));
                    } else {
                        self.remove_local_file(&child)?;
                    }
                    self.cat.local_delete(self.id(), &child.id)?;
                    self.cat.purge_remote(self.id(), &child.id)?;
                }
                let abs = self.abs(&l.rel_path);
                if abs.is_dir() {
                    if std::fs::remove_dir(&abs).is_ok() {
                        self.report.deleted_local += 1;
                    } else {
                        self.warn(format!(
                            "{} was deleted on the server but still holds files that are only here; it was kept",
                            l.rel_path
                        ));
                    }
                }
            }
            _ => self.remove_local_file(&l)?,
        }
        self.cat.local_delete(self.id(), &l.id)?;
        self.cat.purge_remote(self.id(), &it.id)?;
        Ok(())
    }

    // ---- step 3a: send local changes to the server (Sync) --------------------

    fn rel_of(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut parts = Vec::new();
        for c in rel.components() {
            parts.push(c.as_os_str().to_str()?.to_string());
        }
        Some(parts.join("/"))
    }

    fn server_path_too_long(&self, rel: &str) -> bool {
        self.server_prefix_len + rel.chars().count() > paths::SERVER_PATH_LIMIT
    }

    fn dir_remote_id(&self, rel: &str, root_id: &str) -> Result<Option<String>> {
        if rel.is_empty() {
            return Ok(Some(root_id.to_string()));
        }
        Ok(self
            .cat
            .local_by_path(self.id(), rel)?
            .filter(|l| l.kind == ItemKind::Folder)
            .map(|l| l.id))
    }

    async fn push_local(&mut self) -> Result<()> {
        let root_item = self
            .cat
            .remote_root(self.id())?
            .context("the library's root folder is not in the catalog yet")?;
        let root_id = root_item.id.clone();

        let mut seen: HashSet<String> = HashSet::new();
        let mut new_dirs: Vec<String> = Vec::new();
        let mut new_files: Vec<(String, Stat)> = Vec::new();
        let mut changed: Vec<(LocalRow, Stat)> = Vec::new();
        let mut any_entries = false;

        let mut walk = WalkDir::new(&self.root)
            .min_depth(1)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        loop {
            let entry = match walk.next() {
                None => break,
                Some(Err(e)) => {
                    self.warn(format!("could not read part of the local folder: {e}"));
                    continue;
                }
                Some(Ok(e)) => e,
            };
            any_entries = true;
            let Some(rel) = self.rel_of(entry.path()) else {
                self.warn(format!("skipped a name that is not valid text: {}", entry.path().display()));
                if entry.file_type().is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type();
            if let Some(l) = self.cat.local_by_path(self.id(), &rel)? {
                seen.insert(l.id.clone());
                if ft.is_file() && l.kind == ItemKind::File {
                    if let Some(st) = stat(entry.path()) {
                        if st.size != l.size || st.mtime_ns != l.mtime_ns {
                            changed.push((l, st));
                        }
                    }
                }
                continue;
            }
            if ft.is_symlink() {
                self.report.skipped += 1;
                self.warn(format!("{rel} is a symbolic link; links are only followed in Ingest mode"));
                continue;
            }
            if paths::is_ignored_local_name(&name) {
                if ft.is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            if let Some(why) = paths::invalid_server_name(&name) {
                self.report.skipped += 1;
                self.warn(format!("{rel} was not uploaded: the name {why}"));
                if ft.is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            if ft.is_dir() {
                new_dirs.push(rel);
            } else if ft.is_file() {
                if let Some(st) = stat(entry.path()) {
                    new_files.push((rel, st));
                }
            }
        }

        let mut missing: Vec<LocalRow> = self
            .cat
            .local_all(self.id())?
            .into_iter()
            .filter(|l| !seen.contains(&l.id))
            .collect();

        // A file that vanished and one that appeared with the same content is a move.
        let mut moves: Vec<(LocalRow, String, Stat)> = Vec::new();
        if !missing.is_empty() && !new_files.is_empty() {
            let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
            for (i, l) in missing.iter().enumerate() {
                if l.kind == ItemKind::File && l.hash.is_some() {
                    by_size.entry(l.size).or_default().push(i);
                }
            }
            let mut used: HashSet<usize> = HashSet::new();
            let mut remaining = Vec::new();
            for (rel, st) in new_files {
                let mut matched = None;
                if let Some(cands) = by_size.get(&st.size) {
                    if let Ok((_, h)) = hash_file(&self.abs(&rel)) {
                        matched = cands
                            .iter()
                            .copied()
                            .find(|i| !used.contains(i) && missing[*i].hash.as_deref() == Some(h.as_str()));
                    }
                }
                match matched {
                    Some(i) => {
                        used.insert(i);
                        moves.push((missing[i].clone(), rel, st));
                    }
                    None => remaining.push((rel, st)),
                }
            }
            new_files = remaining;
            let moved_ids: HashSet<String> = moves.iter().map(|m| m.0.id.clone()).collect();
            missing.retain(|l| !moved_ids.contains(&l.id));
        }

        // New folders, parents first.
        for rel in new_dirs {
            self.note(format!("create folder {rel} on the server"));
            if self.opts.dry_run {
                self.report.remote_folders_created += 1;
                continue;
            }
            let Some(parent_id) = self.dir_remote_id(parent_rel(&rel), &root_id)? else {
                continue;
            };
            match self
                .remote
                .create_folder(&self.conn.drive_id, &parent_id, file_name_rel(&rel))
                .await
            {
                Ok(item) => {
                    self.cat.upsert_remote_clean(&self.conn, &item)?;
                    self.cat.local_upsert(
                        self.id(),
                        &LocalRow {
                            id: item.id.clone(),
                            rel_path: rel.clone(),
                            kind: ItemKind::Folder,
                            size: 0,
                            mtime_ns: 0,
                            ctag: None,
                            hash: None,
                        },
                    )?;
                    self.report.remote_folders_created += 1;
                }
                Err(e) => self.fail(format!("{rel}: {e}")),
            }
        }

        // Moves and renames.
        for (l, new_rel, st) in moves {
            self.note(format!("move {} -> {new_rel} on the server", l.rel_path));
            if self.opts.dry_run {
                self.report.moved_remote += 1;
                continue;
            }
            let new_parent = if parent_rel(&l.rel_path) != parent_rel(&new_rel) {
                match self.dir_remote_id(parent_rel(&new_rel), &root_id)? {
                    Some(id) => Some(id),
                    None => continue,
                }
            } else {
                None
            };
            let new_name = file_name_rel(&new_rel);
            let rename = (file_name_rel(&l.rel_path) != new_name).then_some(new_name);
            match self
                .remote
                .move_item(&self.conn.drive_id, &l.id, new_parent.as_deref(), rename)
                .await
            {
                Ok(item) => {
                    self.cat.upsert_remote_clean(&self.conn, &item)?;
                    let mut row = l.clone();
                    row.rel_path = new_rel;
                    row.size = st.size;
                    row.mtime_ns = st.mtime_ns;
                    self.cat.local_upsert(self.id(), &row)?;
                    self.report.moved_remote += 1;
                }
                Err(e) => self.fail(format!("{}: {e}", l.rel_path)),
            }
        }

        // Uploads: new files and changed files.
        let mut uploads = Vec::new();
        for (rel, st) in new_files {
            if self.server_path_too_long(&rel) {
                self.report.skipped += 1;
                self.report.long_paths += 1;
                self.warn(format!(
                    "{rel} was not uploaded: its path on the server would exceed {} characters",
                    paths::SERVER_PATH_LIMIT
                ));
                continue;
            }
            self.note(format!("upload {rel} ({} bytes)", st.size));
            if self.opts.dry_run {
                self.report.uploaded += 1;
                continue;
            }
            let Some(parent_id) = self.dir_remote_id(parent_rel(&rel), &root_id)? else {
                continue;
            };
            uploads.push(Upload {
                abs: self.abs(&rel),
                kind: UploadKind::New {
                    parent_id,
                    name: file_name_rel(&rel).to_string(),
                },
                rel,
                st,
            });
        }
        for (l, st) in changed {
            let abs = self.abs(&l.rel_path);
            // Touched but identical: just remember the new timestamp.
            if st.size == l.size {
                if let (Ok((_, h)), Some(lh)) = (hash_file(&abs), &l.hash) {
                    if h == *lh {
                        if !self.opts.dry_run {
                            let mut row = l.clone();
                            row.mtime_ns = st.mtime_ns;
                            self.cat.local_upsert(self.id(), &row)?;
                        }
                        continue;
                    }
                }
            }
            self.note(format!("upload {} ({} bytes, changed here)", l.rel_path, st.size));
            if self.opts.dry_run {
                self.report.uploaded += 1;
                continue;
            }
            let etag = self.cat.remote_get(self.id(), &l.id)?.and_then(|r| r.etag);
            uploads.push(Upload {
                abs,
                kind: UploadKind::Replace {
                    item_id: l.id.clone(),
                    etag,
                },
                rel: l.rel_path.clone(),
                st,
            });
        }
        self.run_uploads(uploads).await?;

        // Deletions: only the topmost missing item of each removed tree.
        let missing_dirs: Vec<String> = missing
            .iter()
            .filter(|l| l.kind == ItemKind::Folder)
            .map(|l| format!("{}/", l.rel_path))
            .collect();
        let top: Vec<LocalRow> = missing
            .iter()
            .filter(|l| !missing_dirs.iter().any(|d| l.rel_path.starts_with(d.as_str())))
            .cloned()
            .collect();
        if top.is_empty() {
            return Ok(());
        }
        let folder_emptied = !any_entries;
        if !self.opts.confirm_deletes && (top.len() > self.opts.max_deletes || folder_emptied) {
            self.report.held_deletes = top.len() as u64;
            let why = if folder_emptied {
                "the local folder is empty".to_string()
            } else {
                format!("more than {}", self.opts.max_deletes)
            };
            self.warn(format!(
                "{} items are missing here ({why}). They were NOT deleted on the server; \
                 that needs your confirmation.",
                top.len()
            ));
            return Ok(());
        }
        for l in top {
            self.note(format!("delete {} on the server (removed here)", l.rel_path));
            if self.opts.dry_run {
                self.report.deleted_remote += 1;
                continue;
            }
            let remote_row = self.cat.remote_get(self.id(), &l.id)?;
            // Folder tags change whenever a child changes, so only files are guarded.
            let etag = match l.kind {
                ItemKind::File => remote_row.as_ref().and_then(|r| r.etag.clone()),
                _ => None,
            };
            match self.remote.delete(&self.conn.drive_id, &l.id, etag.as_deref()).await {
                Ok(()) | Err(RemoteError::NotFound(_)) => {
                    for child in self.cat.local_under(self.id(), &l.rel_path)? {
                        self.cat.local_delete(self.id(), &child.id)?;
                        self.cat.purge_remote(self.id(), &child.id)?;
                    }
                    self.cat.local_delete(self.id(), &l.id)?;
                    self.cat.purge_remote(self.id(), &l.id)?;
                    self.report.deleted_remote += 1;
                }
                Err(RemoteError::Conflict(_)) => {
                    // Changed on the server since we last saw it: the change wins and
                    // the file comes back on the next run.
                    self.report.conflicts += 1;
                    self.cat.local_delete(self.id(), &l.id)?;
                    self.cat.mark_dirty(self.id(), &l.id)?;
                    self.warn(format!(
                        "{} was removed here but changed on the server; it will be restored",
                        l.rel_path
                    ));
                }
                Err(e) => self.fail(format!("{}: {e}", l.rel_path)),
            }
        }
        Ok(())
    }

    async fn run_uploads(&mut self, uploads: Vec<Upload>) -> Result<()> {
        if uploads.is_empty() {
            return Ok(());
        }
        let remote = self.remote;
        let drive = self.conn.drive_id.clone();
        let drive_ref = &drive;
        let total = uploads.len();
        let total_bytes: u64 = uploads.iter().map(|u| u.st.size).sum();
        let mut done = 0usize;
        let mut progress = Progress::new("uploaded", total, total_bytes);
        let mut stream = futures_util::stream::iter(uploads.into_iter().map(|u| async move {
            let target = match &u.kind {
                UploadKind::New { parent_id, name } => UploadTarget::New { parent_id, name },
                UploadKind::Replace { item_id, etag } => UploadTarget::Replace {
                    item_id,
                    etag: etag.as_deref(),
                },
            };
            let res = remote.upload(drive_ref, target, &u.abs, Some(u.st.mtime_ns)).await;
            (u, res)
        }))
        .buffer_unordered(self.opts.concurrency.max(1));

        while let Some((u, res)) = stream.next().await {
            done += 1;
            match res {
                Ok(item) => {
                    self.cat.upsert_remote_clean(&self.conn, &item)?;
                    self.cat.local_upsert(
                        self.id(),
                        &LocalRow {
                            id: item.id.clone(),
                            rel_path: u.rel.clone(),
                            kind: ItemKind::File,
                            size: u.st.size,
                            mtime_ns: u.st.mtime_ns,
                            ctag: item.ctag.clone(),
                            hash: item.hash.clone(),
                        },
                    )?;
                    self.report.uploaded += 1;
                    self.report.bytes_up += u.st.size;
                    if let Some(line) = progress.tick(done, self.report.bytes_up) {
                        (self.log)(&line);
                    }
                }
                Err(RemoteError::Conflict(_)) => {
                    self.report.conflicts += 1;
                    self.warn(format!(
                        "{} was not uploaded: the server's copy changed as well. Nothing was overwritten.",
                        u.rel
                    ));
                }
                Err(e) => self.fail(format!("{}: {e}", u.rel)),
            }
        }
        Ok(())
    }

    // ---- step 3b: upload a disk source (Ingest) ------------------------------

    async fn ingest(&mut self) -> Result<()> {
        let root_item = self
            .cat
            .remote_root(self.id())?
            .context("the library's root folder is not in the catalog yet")?;
        // Relative folder path -> its id on the server (None when not created yet).
        let mut dir_ids: HashMap<String, Option<String>> = HashMap::new();
        dir_ids.insert(String::new(), Some(root_item.id.clone()));
        let mut uploads = Vec::new();

        let mut walk = WalkDir::new(&self.root)
            .min_depth(1)
            .follow_links(true)
            .sort_by_file_name()
            .into_iter();
        loop {
            let entry = match walk.next() {
                None => break,
                Some(Err(e)) => {
                    self.report.skipped += 1;
                    self.warn(format!("skipped part of the source: {e}"));
                    continue;
                }
                Some(Ok(e)) => e,
            };
            let ft = entry.file_type();
            let Some(rel) = self.rel_of(entry.path()) else {
                self.report.skipped += 1;
                self.warn(format!("skipped a name that is not valid text: {}", entry.path().display()));
                if ft.is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if paths::is_ignored_local_name(&name) {
                if ft.is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            if let Some(why) = paths::invalid_server_name(&name) {
                self.report.skipped += 1;
                self.warn(format!("{rel} was not uploaded: the name {why}"));
                if ft.is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            let Some(parent_id) = dir_ids.get(parent_rel(&rel)).cloned() else {
                continue;
            };

            if ft.is_dir() {
                let id = match &parent_id {
                    None => {
                        // Its parent does not exist on the server yet (a preview).
                        self.note(format!("create folder {rel} on the server"));
                        self.report.remote_folders_created += 1;
                        None
                    }
                    Some(pid) => match self.cat.remote_child(self.id(), pid, &name)? {
                        Some(r) if r.kind == ItemKind::Folder => Some(r.id),
                        Some(_) => {
                            self.report.skipped += 1;
                            self.warn(format!("{rel} was not uploaded: the server has a file with that name"));
                            walk.skip_current_dir();
                            continue;
                        }
                        None => {
                            self.note(format!("create folder {rel} on the server"));
                            if self.opts.dry_run {
                                self.report.remote_folders_created += 1;
                                None
                            } else {
                                match self.remote.create_folder(&self.conn.drive_id, pid, &name).await {
                                    Ok(item) => {
                                        self.cat.upsert_remote_clean(&self.conn, &item)?;
                                        self.report.remote_folders_created += 1;
                                        Some(item.id)
                                    }
                                    Err(e) => {
                                        self.fail(format!("{rel}: {e}"));
                                        walk.skip_current_dir();
                                        continue;
                                    }
                                }
                            }
                        }
                    },
                };
                dir_ids.insert(rel, id);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Some(st) = stat(entry.path()) else { continue };
            if self.server_path_too_long(&rel) {
                self.report.skipped += 1;
                self.report.long_paths += 1;
                self.warn(format!(
                    "{rel} was not uploaded: its path on the server would exceed {} characters",
                    paths::SERVER_PATH_LIMIT
                ));
                continue;
            }

            if let Some(l) = self.cat.local_by_path(self.id(), &rel)? {
                if l.size == st.size && l.mtime_ns == st.mtime_ns {
                    self.report.unchanged += 1;
                } else if self.opts.ingest_update {
                    self.note(format!("upload {rel} ({} bytes, changed in the source)", st.size));
                    if self.opts.dry_run {
                        self.report.uploaded += 1;
                    } else {
                        let etag = self.cat.remote_get(self.id(), &l.id)?.and_then(|r| r.etag);
                        uploads.push(Upload {
                            abs: entry.path().to_path_buf(),
                            kind: UploadKind::Replace { item_id: l.id, etag },
                            rel,
                            st,
                        });
                    }
                } else {
                    self.report.skipped += 1;
                    self.warn(format!(
                        "{rel} changed in the source after it was ingested; it was not uploaded again (use --update to send new versions)"
                    ));
                }
                continue;
            }

            let existing = match &parent_id {
                Some(pid) => self.cat.remote_child(self.id(), pid, &name)?,
                None => None,
            };
            match existing {
                Some(r) if r.kind == ItemKind::File => {
                    let (size, h) = hash_file(entry.path())?;
                    let same = match &r.hash {
                        Some(rh) => *rh == h,
                        None => r.size == size,
                    };
                    if same {
                        if !self.opts.dry_run {
                            self.cat.local_upsert(
                                self.id(),
                                &LocalRow {
                                    id: r.id.clone(),
                                    rel_path: rel.clone(),
                                    kind: ItemKind::File,
                                    size: st.size,
                                    mtime_ns: st.mtime_ns,
                                    ctag: r.ctag.clone(),
                                    hash: r.hash.clone(),
                                },
                            )?;
                        }
                        self.report.adopted += 1;
                    } else {
                        self.report.conflicts += 1;
                        self.warn(format!(
                            "{rel} already exists on the server with different content; it was not uploaded"
                        ));
                    }
                }
                Some(_) => {
                    self.report.skipped += 1;
                    self.warn(format!("{rel} was not uploaded: the server has a folder with that name"));
                }
                None => {
                    self.note(format!("upload {rel} ({} bytes)", st.size));
                    match (&parent_id, self.opts.dry_run) {
                        (Some(pid), false) => uploads.push(Upload {
                            abs: entry.path().to_path_buf(),
                            kind: UploadKind::New {
                                parent_id: pid.clone(),
                                name,
                            },
                            rel,
                            st,
                        }),
                        _ => self.report.uploaded += 1,
                    }
                }
            }
        }
        self.run_uploads(uploads).await
    }
}

/// Download to `tmp` and check the result against what the server said to expect.
///
/// SharePoint sometimes reports a size or hash that does not match the bytes it
/// serves for files it rewrites. A second identical download is accepted with a note;
/// two different results are an error.
async fn fetch_verified<R: Remote>(
    remote: &R,
    drive: &str,
    item: &RemoteItem,
    tmp: &Path,
) -> Result<Option<&'static str>, RemoteError> {
    let first = remote.download(drive, &item.id, tmp).await?;
    if verifies(&first, item) {
        return Ok(None);
    }
    let second = remote.download(drive, &item.id, tmp).await?;
    if verifies(&second, item) {
        Ok(None)
    } else if first == second {
        Ok(Some(
            "the server's reported hash does not match the content it sent (downloaded twice, identical)",
        ))
    } else {
        Err(RemoteError::Other("the download did not verify".into()))
    }
}
