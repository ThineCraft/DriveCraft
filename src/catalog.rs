//! The local catalog: a SQLite database describing each connected library.
//!
//! Two kinds of record are kept deliberately apart:
//!
//! * `remote_items` describes the server: every file and folder, its identifiers,
//!   hashes, and the change bookmark. This is the part a seed can share.
//! * `local_items` describes this machine: which items are present on disk and what
//!   they looked like when last synchronized.

use crate::remote::{ItemKind, RemoteItem};
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension, Row};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Two-way synchronization.
    Sync,
    /// One-way from a disk source into the library. The source is never changed.
    Ingest,
    /// One-way from the library to disk. The server is never changed.
    Archive,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Sync => "sync",
            Mode::Ingest => "ingest",
            Mode::Archive => "archive",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sync" => Some(Mode::Sync),
            "ingest" => Some(Mode::Ingest),
            "archive" => Some(Mode::Archive),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Connection {
    pub id: i64,
    pub name: String,
    pub drive_id: String,
    pub web_url: String,
    pub local_path: PathBuf,
    pub mode: Mode,
    /// Bookmark for "changes since last time"; set once a full scan has completed.
    pub delta_link: Option<String>,
    /// Position inside an unfinished scan, so an interrupted scan can resume.
    pub cursor: Option<String>,
    pub generation: i64,
    pub resyncing: bool,
    pub last_sync: Option<i64>,
    /// The catalog knows which items have permissions of their own: a full scan has
    /// completed since DriveCraft began asking. Until then a seed cannot be made safely.
    pub perms_known: bool,
}

/// What this machine recorded about an item the last time it was synchronized.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalRow {
    pub id: String,
    pub rel_path: String,
    pub kind: ItemKind,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctag: Option<String>,
    pub hash: Option<String>,
}

pub struct Catalog {
    pub(crate) db: rusqlite::Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS connections(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  drive_id TEXT NOT NULL,
  web_url TEXT NOT NULL,
  local_path TEXT NOT NULL,
  mode TEXT NOT NULL,
  delta_link TEXT,
  cursor TEXT,
  generation INTEGER NOT NULL DEFAULT 1,
  resyncing INTEGER NOT NULL DEFAULT 0,
  last_sync INTEGER
);
CREATE TABLE IF NOT EXISTS remote_items(
  conn_id INTEGER NOT NULL,
  id TEXT NOT NULL,
  parent_id TEXT,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  size INTEGER NOT NULL,
  etag TEXT,
  ctag TEXT,
  hash TEXT,
  mtime_ns INTEGER,
  is_root INTEGER NOT NULL DEFAULT 0,
  deleted INTEGER NOT NULL DEFAULT 0,
  dirty INTEGER NOT NULL DEFAULT 1,
  seen INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(conn_id, id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS remote_by_parent ON remote_items(conn_id, parent_id, name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS remote_dirty ON remote_items(conn_id) WHERE dirty = 1;
CREATE TABLE IF NOT EXISTS local_items(
  conn_id INTEGER NOT NULL,
  id TEXT NOT NULL,
  rel_path TEXT NOT NULL,
  kind TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  ctag TEXT,
  hash TEXT,
  PRIMARY KEY(conn_id, id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS local_by_path ON local_items(conn_id, rel_path);
CREATE TABLE IF NOT EXISTS checkouts(
  conn_id INTEGER NOT NULL,
  item_id TEXT NOT NULL,
  by_name TEXT NOT NULL,
  mine INTEGER NOT NULL,
  since INTEGER NOT NULL,
  stale INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(conn_id, item_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS problems(
  conn_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  path TEXT NOT NULL,
  message TEXT NOT NULL,
  watch TEXT,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  PRIMARY KEY(conn_id, kind, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS withheld(
  conn_id INTEGER NOT NULL,
  id TEXT NOT NULL,
  checked INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(conn_id, id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS permissions(
  conn_id INTEGER NOT NULL,
  item_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  perm_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  principal_id TEXT,
  name TEXT NOT NULL,
  email TEXT,
  login TEXT,
  roles TEXT NOT NULL,
  link_scope TEXT,
  via_link INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(conn_id, item_id, seq)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS perm_items(
  conn_id INTEGER NOT NULL,
  item_id TEXT NOT NULL,
  state TEXT NOT NULL,
  PRIMARY KEY(conn_id, item_id)
) WITHOUT ROWID;
";

/// Columns added after the first release, with what an older catalog gets. Each is
/// added the first time a newer DriveCraft opens the catalog; nothing else changes,
/// and an older DriveCraft can still use the catalog afterwards.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    // The item has permissions of its own (see `RemoteItem::own_permissions`).
    ("remote_items", "own_perms", "INTEGER NOT NULL DEFAULT 0"),
    // The server refused this person the item; it is left alone until the change
    // feed mentions it again.
    ("remote_items", "no_access", "INTEGER NOT NULL DEFAULT 0"),
    // Known only from a seed: the change feed has not mentioned it to this person.
    ("remote_items", "from_seed", "INTEGER NOT NULL DEFAULT 0"),
    ("connections", "perms_known", "INTEGER NOT NULL DEFAULT 0"),
    // When the permissions of the items that have their own were last read (Unix
    // seconds), and whether that read saw the full lists, as an owner does.
    ("connections", "perms_read_at", "INTEGER"),
    ("connections", "perms_full", "INTEGER NOT NULL DEFAULT 0"),
];

/// A seed's restricted folder that this person was refused is asked about again
/// after this long, in case they have been given access since.
pub const RECHECK_WITHHELD_SECS: i64 = 24 * 60 * 60;

const REMOTE_COLS: &str =
    "id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted, own_perms";

fn remote_from_row(r: &Row) -> rusqlite::Result<RemoteItem> {
    Ok(RemoteItem {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        name: r.get(2)?,
        kind: ItemKind::parse(&r.get::<_, String>(3)?),
        size: r.get::<_, i64>(4)? as u64,
        etag: r.get(5)?,
        ctag: r.get(6)?,
        hash: r.get(7)?,
        mtime_ns: r.get(8)?,
        is_root: r.get::<_, i64>(9)? != 0,
        deleted: r.get::<_, i64>(10)? != 0,
        own_permissions: r.get::<_, i64>(11)? != 0,
    })
}

const LOCAL_COLS: &str = "id, rel_path, kind, size, mtime_ns, ctag, hash";

fn local_from_row(r: &Row) -> rusqlite::Result<LocalRow> {
    Ok(LocalRow {
        id: r.get(0)?,
        rel_path: r.get(1)?,
        kind: ItemKind::parse(&r.get::<_, String>(2)?),
        size: r.get::<_, i64>(3)? as u64,
        mtime_ns: r.get(4)?,
        ctag: r.get(5)?,
        hash: r.get(6)?,
    })
}

const CONN_COLS: &str = "id, name, drive_id, web_url, local_path, mode, delta_link, cursor, generation, resyncing, last_sync, perms_known";

fn conn_from_row(r: &Row) -> rusqlite::Result<Connection> {
    Ok(Connection {
        id: r.get(0)?,
        name: r.get(1)?,
        drive_id: r.get(2)?,
        web_url: r.get(3)?,
        local_path: PathBuf::from(r.get::<_, String>(4)?),
        mode: Mode::parse(&r.get::<_, String>(5)?).unwrap_or(Mode::Archive),
        delta_link: r.get(6)?,
        cursor: r.get(7)?,
        generation: r.get(8)?,
        resyncing: r.get::<_, i64>(9)? != 0,
        last_sync: r.get(10)?,
        perms_known: r.get::<_, i64>(11)? != 0,
    })
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let db = rusqlite::Connection::open(path)
            .with_context(|| format!("opening catalog {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(db: rusqlite::Connection) -> Result<Self> {
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        db.execute_batch(SCHEMA)?;
        for (table, column, def) in ADDED_COLUMNS {
            let has: bool = db.query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                [column],
                |r| r.get::<_, i64>(0).map(|n| n > 0),
            )?;
            if !has {
                db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {def}"))
                    .with_context(|| format!("updating the catalog: adding {table}.{column}"))?;
            }
        }
        Ok(Self { db })
    }

    // ---- connections -------------------------------------------------------

    pub fn add_connection(
        &self,
        name: &str,
        drive_id: &str,
        web_url: &str,
        local_path: &Path,
        mode: Mode,
    ) -> Result<Connection> {
        self.db.execute(
            "INSERT INTO connections(name, drive_id, web_url, local_path, mode) VALUES(?,?,?,?,?)",
            params![
                name,
                drive_id,
                web_url,
                local_path.to_string_lossy(),
                mode.as_str()
            ],
        )?;
        Ok(self.connection(name)?.expect("just inserted"))
    }

    pub fn connections(&self) -> Result<Vec<Connection>> {
        let mut st = self
            .db
            .prepare(&format!("SELECT {CONN_COLS} FROM connections ORDER BY name"))?;
        let rows = st.query_map([], conn_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn connection(&self, name: &str) -> Result<Option<Connection>> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {CONN_COLS} FROM connections WHERE name = ? COLLATE NOCASE"),
                [name],
                conn_from_row,
            )
            .optional()?)
    }

    pub fn connection_by_id(&self, id: i64) -> Result<Connection> {
        Ok(self.db.query_row(
            &format!("SELECT {CONN_COLS} FROM connections WHERE id = ?"),
            [id],
            conn_from_row,
        )?)
    }

    pub fn remove_connection(&self, id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM remote_items WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM local_items WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM problems WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM checkouts WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM withheld WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM permissions WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM perm_items WHERE conn_id = ?", [id])?;
        tx.execute("DELETE FROM connections WHERE id = ?", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_mode(&self, id: i64, mode: Mode) -> Result<()> {
        self.db.execute(
            "UPDATE connections SET mode = ? WHERE id = ?",
            params![mode.as_str(), id],
        )?;
        Ok(())
    }

    pub fn set_last_sync(&self, id: i64, unix_secs: i64) -> Result<()> {
        self.db.execute(
            "UPDATE connections SET last_sync = ? WHERE id = ?",
            params![unix_secs, id],
        )?;
        Ok(())
    }

    // ---- change feed -------------------------------------------------------

    /// Record one page of the change feed and where to continue from, atomically.
    pub fn apply_delta_page(
        &self,
        conn: &Connection,
        items: &[RemoteItem],
        cursor: Option<&str>,
        delta_link: Option<&str>,
    ) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            // The change feed mentioning an item is the server saying this person can
            // see it now: any "no access" mark from before is dropped and it is tried again.
            let mut upsert = tx.prepare_cached(
                "INSERT INTO remote_items(conn_id, id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted, dirty, seen, own_perms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,1,?12,?13)
                 ON CONFLICT(conn_id, id) DO UPDATE SET
                   parent_id=excluded.parent_id, name=excluded.name, kind=excluded.kind, size=excluded.size,
                   etag=excluded.etag, ctag=excluded.ctag, hash=excluded.hash, mtime_ns=excluded.mtime_ns,
                   is_root=excluded.is_root, deleted=0, dirty=1, seen=excluded.seen,
                   own_perms=excluded.own_perms, no_access=0, from_seed=0",
            )?;
            let mut mark_deleted = tx.prepare_cached(
                "UPDATE remote_items SET deleted = 1, dirty = 1, seen = ?3 WHERE conn_id = ?1 AND id = ?2",
            )?;
            // A part of the library a seed left out, mentioned by the feed: ask about
            // it again on this run, since this person may have been given access.
            let mut recheck = tx.prepare_cached("UPDATE withheld SET checked = 0 WHERE conn_id = ?1 AND id = ?2")?;
            let mut forget = tx.prepare_cached("DELETE FROM withheld WHERE conn_id = ?1 AND id = ?2")?;
            for it in items {
                if it.deleted {
                    mark_deleted.execute(params![conn.id, it.id, conn.generation])?;
                    forget.execute(params![conn.id, it.id])?;
                } else {
                    recheck.execute(params![conn.id, it.id])?;
                    upsert.execute(params![
                        conn.id,
                        it.id,
                        it.parent_id,
                        it.name,
                        it.kind.as_str(),
                        it.size as i64,
                        it.etag,
                        it.ctag,
                        it.hash,
                        it.mtime_ns,
                        it.is_root as i64,
                        conn.generation,
                        it.own_permissions as i64
                    ])?;
                }
            }
        }
        if let Some(link) = delta_link {
            if conn.delta_link.is_none() {
                // A whole enumeration has just completed, and every item in it said
                // whether it has permissions of its own.
                tx.execute("UPDATE connections SET perms_known = 1 WHERE id = ?", [conn.id])?;
            }
            if conn.resyncing {
                // A full rescan has finished: anything it did not mention no longer exists.
                tx.execute(
                    "UPDATE remote_items SET deleted = 1, dirty = 1 WHERE conn_id = ? AND seen < ? AND deleted = 0",
                    params![conn.id, conn.generation],
                )?;
            }
            tx.execute(
                "UPDATE connections SET delta_link = ?, cursor = NULL, resyncing = 0 WHERE id = ?",
                params![link, conn.id],
            )?;
        } else {
            tx.execute(
                "UPDATE connections SET cursor = ? WHERE id = ?",
                params![cursor, conn.id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget the change bookmark and start a full rescan. Items the rescan does not
    /// mention are treated as deleted when it completes.
    pub fn begin_resync(&self, conn_id: i64) -> Result<()> {
        self.db.execute(
            "UPDATE connections SET delta_link = NULL, cursor = NULL, resyncing = 1, generation = generation + 1 WHERE id = ?",
            [conn_id],
        )?;
        Ok(())
    }

    // ---- remote items ------------------------------------------------------

    pub fn remote_get(&self, conn_id: i64, id: &str) -> Result<Option<RemoteItem>> {
        Ok(self
            .db
            .prepare_cached(&format!(
                "SELECT {REMOTE_COLS} FROM remote_items WHERE conn_id = ? AND id = ?"
            ))?
            .query_row(params![conn_id, id], remote_from_row)
            .optional()?)
    }

    pub fn remote_root(&self, conn_id: i64) -> Result<Option<RemoteItem>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {REMOTE_COLS} FROM remote_items WHERE conn_id = ? AND is_root = 1 AND deleted = 0"
                ),
                [conn_id],
                remote_from_row,
            )
            .optional()?)
    }

    /// A live child of a folder by name. Names compare without regard to case, as
    /// they do on the server.
    pub fn remote_child(
        &self,
        conn_id: i64,
        parent_id: &str,
        name: &str,
    ) -> Result<Option<RemoteItem>> {
        Ok(self
            .db
            .prepare_cached(&format!(
                // The index is named outright. Left to choose, SQLite reads every row
                // of the library for each name asked about: 60 ms a file at 200,000
                // files, which made checking a large folder take most of an hour.
                "SELECT {REMOTE_COLS} FROM remote_items INDEXED BY remote_by_parent
                 WHERE conn_id = ? AND parent_id = ? AND name = ? COLLATE NOCASE AND deleted = 0"
            ))?
            .query_row(params![conn_id, parent_id, name], remote_from_row)
            .optional()?)
    }

    /// Items the change feed has touched that have not been reconciled with disk yet.
    pub fn dirty_items(&self, conn_id: i64) -> Result<Vec<RemoteItem>> {
        let mut st = self.db.prepare(&format!(
            "SELECT {REMOTE_COLS} FROM remote_items WHERE conn_id = ? AND dirty = 1"
        ))?;
        let rows = st.query_map([conn_id], remote_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn clear_dirty(&self, conn_id: i64, id: &str) -> Result<()> {
        self.db
            .prepare_cached("UPDATE remote_items SET dirty = 0 WHERE conn_id = ? AND id = ?")?
            .execute(params![conn_id, id])?;
        Ok(())
    }

    pub fn mark_dirty(&self, conn_id: i64, id: &str) -> Result<()> {
        self.db
            .prepare_cached("UPDATE remote_items SET dirty = 1 WHERE conn_id = ? AND id = ?")?
            .execute(params![conn_id, id])?;
        Ok(())
    }

    pub fn clear_all_dirty(&self, conn_id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM remote_items WHERE conn_id = ? AND deleted = 1",
            [conn_id],
        )?;
        tx.execute(
            "UPDATE remote_items SET dirty = 0 WHERE conn_id = ? AND dirty = 1",
            [conn_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn purge_remote(&self, conn_id: i64, id: &str) -> Result<()> {
        self.db
            .prepare_cached("DELETE FROM remote_items WHERE conn_id = ? AND id = ?")?
            .execute(params![conn_id, id])?;
        Ok(())
    }

    /// Record an item as the server returned it after one of our own writes. It is
    /// already reconciled, so it is not marked dirty.
    pub fn upsert_remote_clean(&self, conn: &Connection, it: &RemoteItem) -> Result<()> {
        self.db
            .prepare_cached(
                "INSERT INTO remote_items(conn_id, id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted, dirty, seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,0,?12)
                 ON CONFLICT(conn_id, id) DO UPDATE SET
                   parent_id=excluded.parent_id, name=excluded.name, kind=excluded.kind, size=excluded.size,
                   etag=excluded.etag, ctag=excluded.ctag, hash=excluded.hash, mtime_ns=excluded.mtime_ns,
                   deleted=0, dirty=0, seen=excluded.seen, no_access=0, from_seed=0",
            )?
            .execute(params![
                conn.id,
                it.id,
                it.parent_id,
                it.name,
                it.kind.as_str(),
                it.size as i64,
                it.etag,
                it.ctag,
                it.hash,
                it.mtime_ns,
                it.is_root as i64,
                conn.generation
            ])?;
        Ok(())
    }

    // ---- what this person may not open ----------------------------------------

    /// The server refused this person an item. It is left alone, with no problem
    /// recorded, until the change feed mentions it again.
    pub fn mark_no_access(&self, conn_id: i64, id: &str) -> Result<()> {
        self.db
            .prepare_cached("UPDATE remote_items SET no_access = 1, dirty = 0 WHERE conn_id = ? AND id = ?")?
            .execute(params![conn_id, id])?;
        Ok(())
    }

    /// Whether the catalog knows this item only from a seed, so far.
    pub fn from_seed(&self, conn_id: i64, id: &str) -> Result<bool> {
        Ok(self
            .db
            .prepare_cached("SELECT from_seed FROM remote_items WHERE conn_id = ? AND id = ?")?
            .query_row(params![conn_id, id], |r| r.get::<_, i64>(0))
            .optional()?
            .is_some_and(|v| v != 0))
    }

    /// How many items the server has refused this person.
    pub fn no_access_count(&self, conn_id: i64) -> Result<u64> {
        let n: i64 = self.db.query_row(
            "SELECT (SELECT COUNT(*) FROM remote_items WHERE conn_id = ?1 AND no_access = 1)
                  + (SELECT COUNT(*) FROM withheld WHERE conn_id = ?1 AND checked > 0)",
            [conn_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Parts of the library a seed left out that are due to be asked about: never
    /// asked yet, or last refused before `refused_before` (Unix seconds).
    pub fn withheld_due(&self, conn_id: i64, refused_before: i64) -> Result<Vec<String>> {
        let mut st = self
            .db
            .prepare("SELECT id FROM withheld WHERE conn_id = ? AND checked < ? ORDER BY id")?;
        let rows = st.query_map(params![conn_id, refused_before], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The server refused this person a part of the library a seed left out.
    pub fn withheld_refused(&self, conn_id: i64, id: &str, now: i64) -> Result<()> {
        self.db.execute(
            "UPDATE withheld SET checked = ?3 WHERE conn_id = ?1 AND id = ?2",
            params![conn_id, id, now.max(1)],
        )?;
        Ok(())
    }

    /// This person can open a part of the library a seed left out, and what is in it
    /// has been read: record it all as still to be brought to this computer.
    pub fn withheld_found(&self, conn: &Connection, id: &str, items: &[RemoteItem], refused: &[String]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO remote_items(conn_id, id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted, dirty, seen, own_perms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,0,1,?11,?12)
                 ON CONFLICT(conn_id, id) DO UPDATE SET
                   parent_id=excluded.parent_id, name=excluded.name, kind=excluded.kind, size=excluded.size,
                   etag=excluded.etag, ctag=excluded.ctag, hash=excluded.hash, mtime_ns=excluded.mtime_ns,
                   deleted=0, dirty=1, seen=excluded.seen, no_access=0",
            )?;
            for it in items.iter().filter(|i| !i.deleted && !i.is_root) {
                upsert.execute(params![
                    conn.id,
                    it.id,
                    it.parent_id,
                    it.name,
                    it.kind.as_str(),
                    it.size as i64,
                    it.etag,
                    it.ctag,
                    it.hash,
                    it.mtime_ns,
                    conn.generation,
                    it.own_permissions as i64
                ])?;
            }
            let mut refuse = tx.prepare_cached(
                "UPDATE remote_items SET no_access = 1, dirty = 0 WHERE conn_id = ? AND id = ?",
            )?;
            for r in refused {
                refuse.execute(params![conn.id, r])?;
            }
        }
        tx.execute("DELETE FROM withheld WHERE conn_id = ? AND id = ?", params![conn.id, id])?;
        tx.commit()?;
        Ok(())
    }

    /// Before a whole-library read that records only which items have permissions of
    /// their own: forget what was known.
    pub fn begin_permission_survey(&self, conn_id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("UPDATE connections SET perms_known = 0 WHERE id = ?", [conn_id])?;
        tx.execute("UPDATE remote_items SET own_perms = 0 WHERE conn_id = ? AND own_perms = 1", [conn_id])?;
        tx.commit()?;
        Ok(())
    }

    /// One page of that read: nothing else about the items changes.
    pub fn record_permissions(&self, conn_id: i64, items: &[RemoteItem]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let mut mark = tx.prepare_cached("UPDATE remote_items SET own_perms = 1 WHERE conn_id = ? AND id = ?")?;
            for it in items.iter().filter(|i| i.own_permissions && !i.deleted) {
                mark.execute(params![conn_id, it.id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn end_permission_survey(&self, conn_id: i64) -> Result<()> {
        self.db.execute("UPDATE connections SET perms_known = 1 WHERE id = ?", [conn_id])?;
        Ok(())
    }

    /// The path of an item relative to the library root, using `/`. `None` if the
    /// item is not connected to the root or sits inside something not synchronized.
    pub fn remote_path(&self, conn_id: i64, item: &RemoteItem) -> Result<Option<String>> {
        if item.is_root {
            return Ok(Some(String::new()));
        }
        if !crate::paths::safe_local_name(&item.name) {
            return Ok(None);
        }
        let mut parts = vec![item.name.clone()];
        let mut parent = item.parent_id.clone();
        for _ in 0..512 {
            let Some(pid) = parent else { return Ok(None) };
            let Some(p) = self.remote_get(conn_id, &pid)? else {
                return Ok(None);
            };
            if !p.is_root && !crate::paths::safe_local_name(&p.name) {
                return Ok(None);
            }
            if p.is_root {
                parts.reverse();
                return Ok(Some(parts.join("/")));
            }
            if p.kind != ItemKind::Folder {
                return Ok(None);
            }
            parts.push(p.name);
            parent = p.parent_id;
        }
        Ok(None)
    }

    // ---- local items -------------------------------------------------------

    pub fn local_get(&self, conn_id: i64, id: &str) -> Result<Option<LocalRow>> {
        Ok(self
            .db
            .prepare_cached(&format!(
                "SELECT {LOCAL_COLS} FROM local_items WHERE conn_id = ? AND id = ?"
            ))?
            .query_row(params![conn_id, id], local_from_row)
            .optional()?)
    }

    pub fn local_by_path(&self, conn_id: i64, rel_path: &str) -> Result<Option<LocalRow>> {
        Ok(self
            .db
            .prepare_cached(&format!(
                "SELECT {LOCAL_COLS} FROM local_items WHERE conn_id = ? AND rel_path = ?"
            ))?
            .query_row(params![conn_id, rel_path], local_from_row)
            .optional()?)
    }

    pub fn local_upsert(&self, conn_id: i64, row: &LocalRow) -> Result<()> {
        self.db
            .prepare_cached(
                "INSERT INTO local_items(conn_id, id, rel_path, kind, size, mtime_ns, ctag, hash)
                 VALUES(?,?,?,?,?,?,?,?)
                 ON CONFLICT(conn_id, id) DO UPDATE SET rel_path=excluded.rel_path, kind=excluded.kind,
                   size=excluded.size, mtime_ns=excluded.mtime_ns, ctag=excluded.ctag, hash=excluded.hash",
            )?
            .execute(params![
                conn_id,
                row.id,
                row.rel_path,
                row.kind.as_str(),
                row.size as i64,
                row.mtime_ns,
                row.ctag,
                row.hash
            ])?;
        Ok(())
    }

    pub fn local_delete(&self, conn_id: i64, id: &str) -> Result<()> {
        self.db
            .prepare_cached("DELETE FROM local_items WHERE conn_id = ? AND id = ?")?
            .execute(params![conn_id, id])?;
        Ok(())
    }

    /// After a folder moves on disk, move every recorded path beneath it.
    pub fn local_rename_prefix(&self, conn_id: i64, old: &str, new: &str) -> Result<()> {
        let lo = format!("{old}/");
        let hi = format!("{old}0"); // '0' is the character after '/'
        self.db.execute(
            "UPDATE local_items SET rel_path = ?1 || substr(rel_path, ?2)
             WHERE conn_id = ?3 AND rel_path >= ?4 AND rel_path < ?5",
            params![new, old.chars().count() as i64 + 1, conn_id, lo, hi],
        )?;
        Ok(())
    }

    /// Recorded items inside a folder, deepest first.
    pub fn local_under(&self, conn_id: i64, folder_rel: &str) -> Result<Vec<LocalRow>> {
        let lo = format!("{folder_rel}/");
        let hi = format!("{folder_rel}0");
        let mut st = self.db.prepare(&format!(
            "SELECT {LOCAL_COLS} FROM local_items
             WHERE conn_id = ? AND rel_path >= ? AND rel_path < ? ORDER BY rel_path DESC"
        ))?;
        let rows = st.query_map(params![conn_id, lo, hi], local_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn local_all(&self, conn_id: i64) -> Result<Vec<LocalRow>> {
        let mut st = self.db.prepare(&format!(
            "SELECT {LOCAL_COLS} FROM local_items WHERE conn_id = ? ORDER BY rel_path"
        ))?;
        let rows = st.query_map([conn_id], local_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- summaries ---------------------------------------------------------

    /// (files on server, folders on server, bytes on server, items on disk, waiting)
    pub fn counts(&self, conn_id: i64) -> Result<Counts> {
        let (files, bytes): (i64, i64) = self.db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size),0) FROM remote_items WHERE conn_id = ? AND kind = 'file' AND deleted = 0 AND no_access = 0",
            [conn_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let folders: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM remote_items WHERE conn_id = ? AND kind = 'folder' AND deleted = 0 AND is_root = 0 AND no_access = 0",
            [conn_id],
            |r| r.get(0),
        )?;
        let local: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM local_items WHERE conn_id = ?",
            [conn_id],
            |r| r.get(0),
        )?;
        let pending: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM remote_items WHERE conn_id = ? AND dirty = 1 AND is_root = 0",
            [conn_id],
            |r| r.get(0),
        )?;
        Ok(Counts {
            remote_files: files as u64,
            remote_folders: folders as u64,
            remote_bytes: bytes as u64,
            local_items: local as u64,
            pending: pending as u64,
        })
    }
}

/// What a seed holds.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SeedInfo {
    pub items: u64,
    /// When the seed was made, Unix seconds.
    pub created_at: i64,
    /// Parts of the library with permissions of their own that the seed left out,
    /// listed by identifier only.
    pub withheld: u64,
    /// The seed carries the permissions of the items that have their own.
    pub permissions: bool,
}

const SEED_FORMAT: &str = "drivecraft-seed";
/// Version 3 lists, for each item with permissions of its own, who may open it, when
/// the person who made the seed could read that; otherwise it leaves such items out,
/// as version 2 always did. Version 1 seeds, which list everything and no permissions,
/// are still accepted.
const SEED_VERSION: &str = "3";
const SEED_VERSIONS: [&str; 3] = ["1", "2", SEED_VERSION];

/// The most permission entries one item may carry in a seed. More than this is not a
/// list of people; it is a seed that cannot be trusted.
const SEED_MAX_PERMS_PER_ITEM: i64 = 5000;

impl Catalog {
    /// Write the server part of one connection's catalog to a new file: the items,
    /// the change bookmark and, with `with_permissions`, who may open each item that
    /// has permissions of its own. Nothing about this computer is included.
    ///
    /// Not everyone who can open the library can open everything in it.
    ///
    /// * With `with_permissions`, every item is listed, restricted ones included, with
    ///   the recorded permissions, so that each computer can work out for itself what
    ///   its user may open. Anyone who can read the seed can then see the names of
    ///   restricted items and who may open them. An item whose permissions could not
    ///   be read is still left out, as below.
    /// * Without, each item with permissions of its own, and everything inside it, is
    ///   left out, so the seed never shows anyone the name of something they may not
    ///   open. Only the identifiers of the topmost such items are kept, so that each
    ///   computer can ask the server, as its own user, whether it may have them.
    pub fn export_seed(&self, conn: &Connection, dest: &Path, now: i64, with_permissions: bool) -> Result<SeedInfo> {
        let link = conn
            .delta_link
            .as_deref()
            .context("the library has not been fully scanned yet")?;
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        // What is left out: with permissions, only what was refused to the publisher
        // or whose permissions could not be read; without, everything restricted.
        let restricted = if with_permissions {
            "(no_access = 1 OR (own_perms = 1 AND NOT EXISTS (
                SELECT 1 FROM perm_items p WHERE p.conn_id = ?1 AND p.item_id = remote_items.id AND p.state = 'read')))"
        } else {
            "(own_perms = 1 OR no_access = 1)"
        };
        self.db
            .execute("ATTACH DATABASE ?1 AS seed", [dest.to_string_lossy().to_string()])?;
        let written = (|| -> Result<(u64, u64)> {
            self.db.execute_batch(
                "CREATE TABLE seed.meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE seed.items(
                   id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                   size INTEGER NOT NULL, etag TEXT, ctag TEXT, hash TEXT, mtime_ns INTEGER,
                   is_root INTEGER NOT NULL, own_perms INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE seed.withheld(id TEXT PRIMARY KEY);
                 CREATE TABLE seed.permissions(
                   item_id TEXT NOT NULL, seq INTEGER NOT NULL, perm_id TEXT NOT NULL, kind TEXT NOT NULL,
                   principal_id TEXT, name TEXT NOT NULL, email TEXT, login TEXT, roles TEXT NOT NULL,
                   link_scope TEXT, via_link INTEGER NOT NULL, PRIMARY KEY(item_id, seq));
                 DROP TABLE IF EXISTS temp.seed_hidden;
                 CREATE TEMP TABLE seed_hidden(id TEXT PRIMARY KEY) WITHOUT ROWID;",
            )?;
            // The items left out, and everything beneath them.
            self.db.execute(
                &format!(
                    "INSERT INTO temp.seed_hidden
                     WITH RECURSIVE hidden(id) AS (
                       SELECT id FROM remote_items
                       WHERE conn_id = ?1 AND deleted = 0 AND is_root = 0 AND {restricted}
                       UNION
                       SELECT r.id FROM remote_items r INDEXED BY remote_by_parent JOIN hidden h ON r.conn_id = ?1 AND r.parent_id = h.id
                       WHERE r.deleted = 0
                     )
                     SELECT id FROM hidden"
                ),
                [conn.id],
            )?;
            let n = self.db.execute(
                "INSERT INTO seed.items
                 SELECT id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, own_perms
                 FROM remote_items WHERE conn_id = ?1 AND deleted = 0
                   AND id NOT IN (SELECT id FROM temp.seed_hidden)",
                [conn.id],
            )?;
            // The topmost of them: what each computer asks the server about.
            let w = self.db.execute(
                &format!(
                    "INSERT INTO seed.withheld
                     SELECT id FROM remote_items
                     WHERE conn_id = ?1 AND deleted = 0 AND is_root = 0 AND {restricted}
                       AND COALESCE(parent_id, '') NOT IN (SELECT id FROM temp.seed_hidden)"
                ),
                [conn.id],
            )?;
            if with_permissions {
                self.db.execute(
                    "INSERT INTO seed.permissions
                     SELECT item_id, seq, perm_id, kind, principal_id, name, email, login, roles, link_scope, via_link
                     FROM permissions WHERE conn_id = ?1 AND item_id IN (SELECT id FROM seed.items)",
                    [conn.id],
                )?;
            }
            for (k, v) in [
                ("format", SEED_FORMAT.to_string()),
                ("version", SEED_VERSION.to_string()),
                ("drive_id", conn.drive_id.clone()),
                ("delta_link", link.to_string()),
                ("created_at", now.to_string()),
                ("items", n.to_string()),
                ("withheld", w.to_string()),
                ("permissions", if with_permissions { "yes" } else { "no" }.to_string()),
            ] {
                self.db
                    .execute("INSERT INTO seed.meta(key, value) VALUES(?1, ?2)", params![k, v])?;
            }
            Ok((n as u64, w as u64))
        })();
        let _ = self.db.execute_batch("DROP TABLE IF EXISTS temp.seed_hidden;");
        self.db.execute("DETACH DATABASE seed", [])?;
        let (items, withheld) = written?;
        Ok(SeedInfo {
            items,
            created_at: now,
            withheld,
            permissions: with_permissions,
        })
    }

    /// Load a seed into a connection that has never been scanned. Every item is
    /// marked as still to be reconciled with this computer, and the seed's bookmark
    /// becomes the point from which changes are requested.
    ///
    /// A seed is a file from the library, so it is not trusted: it must be for this
    /// library, in a format this version understands, with names that are safe.
    pub fn import_seed(&self, conn: &Connection, src: &Path) -> Result<SeedInfo> {
        let seed = rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("the seed could not be opened")?;
        let meta = |key: &str| -> Result<String> {
            seed.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .with_context(|| format!("the seed has no '{key}' entry"))
        };
        let version = meta("version")?;
        if meta("format")? != SEED_FORMAT || !SEED_VERSIONS.contains(&version.as_str()) {
            anyhow::bail!("the seed was made by a version of DriveCraft this one does not understand");
        }
        if meta("drive_id")? != conn.drive_id {
            anyhow::bail!("the seed belongs to a different library");
        }
        let link = meta("delta_link")?;
        let created_at: i64 = meta("created_at")?.parse().unwrap_or(0);
        let with_permissions = version == SEED_VERSION && meta("permissions").is_ok_and(|v| v == "yes");
        let existing: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM remote_items WHERE conn_id = ?1",
            [conn.id],
            |r| r.get(0),
        )?;
        if existing > 0 || conn.delta_link.is_some() {
            anyhow::bail!("this connection has already been scanned; a seed is only for a first scan");
        }
        let tx = self.db.unchecked_transaction()?;
        let mut count = 0u64;
        {
            let mut insert = tx.prepare(
                "INSERT INTO remote_items(conn_id, id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted, dirty, seen, from_seed, own_perms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,1,?12,1,?13)",
            )?;
            let own = if version == SEED_VERSION { "own_perms" } else { "0" };
            let mut read = seed.prepare(&format!(
                "SELECT id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, {own} FROM items"
            ))?;
            let mut rows = read.query([])?;
            while let Some(r) = rows.next()? {
                let name: String = r.get(2)?;
                let is_root: i64 = r.get(9)?;
                if is_root == 0 && !crate::paths::safe_local_name(&name) {
                    anyhow::bail!("the seed contains a name that is not safe to use: {name:?}");
                }
                let kind: String = r.get(3)?;
                insert.execute(params![
                    conn.id,
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    name,
                    ItemKind::parse(&kind).as_str(),
                    r.get::<_, i64>(4)?.max(0),
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    (is_root != 0) as i64,
                    conn.generation,
                    (r.get::<_, i64>(10)? != 0) as i64
                ])?;
                count += 1;
            }
        }
        // What the seed left out, to be asked about with this person's own sign-in.
        let mut withheld = 0u64;
        if version != "1" {
            let mut add = tx.prepare("INSERT OR IGNORE INTO withheld(conn_id, id, checked) VALUES(?1, ?2, 0)")?;
            let mut read = seed.prepare("SELECT id FROM withheld")?;
            let mut rows = read.query([])?;
            while let Some(r) = rows.next()? {
                let id: String = r.get(0)?;
                if id.is_empty() || id.len() > 256 {
                    anyhow::bail!("the seed lists an identifier that is not valid");
                }
                withheld += add.execute(params![conn.id, id])? as u64;
            }
        }
        // Who may open what, as the person who made the seed read it.
        if with_permissions {
            let mut add = tx.prepare(
                "INSERT OR REPLACE INTO permissions(conn_id, item_id, seq, perm_id, kind, principal_id, name, email, login, roles, link_scope, via_link)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            let mut mark = tx.prepare("INSERT OR REPLACE INTO perm_items(conn_id, item_id, state) VALUES(?1, ?2, 'read')")?;
            let mut read = seed.prepare(
                "SELECT item_id, seq, perm_id, kind, principal_id, name, email, login, roles, link_scope, via_link FROM permissions",
            )?;
            let mut rows = read.query([])?;
            while let Some(r) = rows.next()? {
                let item: String = r.get(0)?;
                let seq: i64 = r.get(1)?;
                if !(0..SEED_MAX_PERMS_PER_ITEM).contains(&seq) {
                    anyhow::bail!("the seed lists permissions that are not valid");
                }
                add.execute(params![
                    conn.id,
                    item,
                    seq,
                    r.get::<_, String>(2)?,
                    crate::remote::PrincipalKind::parse(&r.get::<_, String>(3)?).as_str(),
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    (r.get::<_, i64>(10)? != 0) as i64
                ])?;
                mark.execute(params![conn.id, item])?;
            }
            // An item marked as having permissions of its own, with none listed, was
            // read and found to name nobody.
            tx.execute(
                "INSERT OR IGNORE INTO perm_items(conn_id, item_id, state)
                 SELECT conn_id, id, 'read' FROM remote_items WHERE conn_id = ?1 AND (own_perms = 1 OR is_root = 1)",
                [conn.id],
            )?;
        }
        // Which items have permissions of their own is learned in full only on this
        // computer's own full scan, so a seed made from here would read them first.
        tx.execute(
            "UPDATE connections SET delta_link = ?1, cursor = NULL, perms_known = 0,
               perms_read_at = ?3, perms_full = ?4 WHERE id = ?2",
            params![
                link,
                conn.id,
                with_permissions.then_some(created_at),
                with_permissions as i64
            ],
        )?;
        tx.commit()?;
        Ok(SeedInfo {
            items: count,
            created_at,
            withheld,
            permissions: with_permissions,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Counts {
    pub remote_files: u64,
    pub remote_folders: u64,
    pub remote_bytes: u64,
    pub local_items: u64,
    pub pending: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, parent: Option<&str>, name: &str, kind: ItemKind) -> RemoteItem {
        RemoteItem {
            id: id.into(),
            parent_id: parent.map(|s| s.into()),
            name: name.into(),
            kind,
            size: 1,
            etag: Some("e1".into()),
            ctag: Some("c1".into()),
            hash: None,
            mtime_ns: Some(0),
            is_root: parent.is_none(),
            deleted: false,
            own_permissions: false,
        }
    }

    /// The catalog as the first release of DriveCraft made it.
    const FIRST_SCHEMA: &str = "
        CREATE TABLE connections(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, drive_id TEXT NOT NULL,
          web_url TEXT NOT NULL, local_path TEXT NOT NULL, mode TEXT NOT NULL, delta_link TEXT, cursor TEXT,
          generation INTEGER NOT NULL DEFAULT 1, resyncing INTEGER NOT NULL DEFAULT 0, last_sync INTEGER);
        CREATE TABLE remote_items(conn_id INTEGER NOT NULL, id TEXT NOT NULL, parent_id TEXT, name TEXT NOT NULL,
          kind TEXT NOT NULL, size INTEGER NOT NULL, etag TEXT, ctag TEXT, hash TEXT, mtime_ns INTEGER,
          is_root INTEGER NOT NULL DEFAULT 0, deleted INTEGER NOT NULL DEFAULT 0, dirty INTEGER NOT NULL DEFAULT 1,
          seen INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(conn_id, id)) WITHOUT ROWID;
        INSERT INTO connections(name, drive_id, web_url, local_path, mode, delta_link)
          VALUES('lib', 'drive1', 'https://x/lib', '/tmp/lib', 'sync', 'L:7');
        INSERT INTO remote_items(conn_id, id, parent_id, name, kind, size, is_root, dirty)
          VALUES(1, 'root', NULL, 'root', 'folder', 0, 1, 0), (1, 'f', 'root', 'Budget.xlsx', 'file', 10, 0, 0);
    ";

    #[test]
    fn an_older_catalog_is_brought_up_to_date_and_keeps_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.sqlite");
        rusqlite::Connection::open(&path).unwrap().execute_batch(FIRST_SCHEMA).unwrap();

        let cat = Catalog::open(&path).unwrap();
        let conn = cat.connection("lib").unwrap().unwrap();
        assert_eq!(conn.delta_link.as_deref(), Some("L:7"), "the bookmark is kept");
        assert!(!conn.perms_known, "an older catalog does not know about permissions yet");
        let f = cat.remote_get(conn.id, "f").unwrap().unwrap();
        assert_eq!(f.name, "Budget.xlsx");
        assert!(!f.own_permissions);
        assert!(!cat.from_seed(conn.id, "f").unwrap());
        assert_eq!(cat.counts(conn.id).unwrap().remote_files, 1);
        drop(cat);

        // Opening it again changes nothing more.
        let cat = Catalog::open(&path).unwrap();
        assert_eq!(cat.counts(conn.id).unwrap().remote_files, 1);
    }

    #[test]
    fn a_seed_leaves_out_what_has_permissions_of_its_own() {
        let (cat, conn) = setup();
        let mut hr = item("hr", Some("root"), "HR", ItemKind::Folder);
        hr.own_permissions = true;
        let mut memo = item("memo", Some("pub"), "Board memo.docx", ItemKind::File);
        memo.own_permissions = true;
        let mut inner = item("inner", Some("hr"), "Reviews", ItemKind::Folder);
        inner.own_permissions = true;
        let items = vec![
            item("root", None, "root", ItemKind::Folder),
            item("pub", Some("root"), "Public", ItemKind::Folder),
            item("plan", Some("pub"), "Plan.docx", ItemKind::File),
            memo,
            hr,
            item("pay", Some("hr"), "Salaries.xlsx", ItemKind::File),
            inner,
            item("rev", Some("inner"), "Review.docx", ItemKind::File),
        ];
        cat.apply_delta_page(&conn, &items, None, Some("L:1")).unwrap();
        let conn = cat.connection_by_id(conn.id).unwrap();
        assert!(conn.perms_known, "a completed full scan knows about permissions");

        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed.sqlite");
        let info = cat.export_seed(&conn, &seed, 100, false).unwrap();
        assert_eq!((info.items, info.withheld), (3, 2), "root, Public and Plan.docx; HR and the memo withheld");

        let s = rusqlite::Connection::open(&seed).unwrap();
        let names: Vec<String> = s
            .prepare("SELECT name FROM items ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(names, vec!["Plan.docx", "Public", "root"]);
        let ids: Vec<String> = s
            .prepare("SELECT id FROM withheld ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["hr", "memo"], "only the topmost restricted items, by identifier");
        drop(s);

        let other = cat
            .add_connection("other", "drive1", "https://x/lib", Path::new("/tmp/other"), Mode::Archive)
            .unwrap();
        let got = cat.import_seed(&other, &seed).unwrap();
        assert_eq!((got.items, got.withheld), (3, 2));
        assert_eq!(cat.withheld_due(other.id, 1).unwrap(), vec!["hr", "memo"]);
        assert!(cat.from_seed(other.id, "plan").unwrap());
        assert!(!cat.connection_by_id(other.id).unwrap().perms_known);
    }

    fn setup() -> (Catalog, Connection) {
        let cat = Catalog::open_in_memory().unwrap();
        let conn = cat
            .add_connection("lib", "drive1", "https://x/lib", Path::new("/tmp/lib"), Mode::Archive)
            .unwrap();
        (cat, conn)
    }

    #[test]
    fn builds_paths_from_parent_chain() {
        let (cat, conn) = setup();
        let items = vec![
            item("root", None, "root", ItemKind::Folder),
            item("a", Some("root"), "Alpha", ItemKind::Folder),
            item("b", Some("a"), "Beta", ItemKind::Folder),
            item("f", Some("b"), "file.txt", ItemKind::File),
            item("orphan", Some("missing"), "lost.txt", ItemKind::File),
        ];
        cat.apply_delta_page(&conn, &items, None, Some("link1")).unwrap();
        let f = cat.remote_get(conn.id, "f").unwrap().unwrap();
        assert_eq!(cat.remote_path(conn.id, &f).unwrap().unwrap(), "Alpha/Beta/file.txt");
        let o = cat.remote_get(conn.id, "orphan").unwrap().unwrap();
        assert_eq!(cat.remote_path(conn.id, &o).unwrap(), None);
        assert_eq!(cat.connection("lib").unwrap().unwrap().delta_link.as_deref(), Some("link1"));
        assert!(cat.remote_child(conn.id, "root", "alpha").unwrap().is_some());
        // Finding a name in a folder must go by the index, never by reading the
        // whole library: it is asked once for every new file.
        let plan: String = cat
            .db
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM remote_items INDEXED BY remote_by_parent
                 WHERE conn_id = ? AND parent_id = ? AND name = ? COLLATE NOCASE AND deleted = 0",
                params![conn.id, "root", "alpha"],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("remote_by_parent (conn_id=? AND parent_id=? AND name=?)"), "{plan}");
    }

    #[test]
    fn resync_marks_unmentioned_items_deleted() {
        let (cat, conn) = setup();
        let items = vec![
            item("root", None, "root", ItemKind::Folder),
            item("keep", Some("root"), "keep.txt", ItemKind::File),
            item("gone", Some("root"), "gone.txt", ItemKind::File),
        ];
        cat.apply_delta_page(&conn, &items, None, Some("l1")).unwrap();
        cat.clear_all_dirty(conn.id).unwrap();
        cat.begin_resync(conn.id).unwrap();
        let conn = cat.connection_by_id(conn.id).unwrap();
        assert!(conn.resyncing && conn.delta_link.is_none());
        cat.apply_delta_page(&conn, &items[..2], None, Some("l2")).unwrap();
        let gone = cat.remote_get(conn.id, "gone").unwrap().unwrap();
        assert!(gone.deleted);
        let keep = cat.remote_get(conn.id, "keep").unwrap().unwrap();
        assert!(!keep.deleted);
        assert!(!cat.connection_by_id(conn.id).unwrap().resyncing);
    }

    #[test]
    fn renames_recorded_paths_under_a_folder() {
        let (cat, conn) = setup();
        for (id, p) in [("1", "A"), ("2", "A/x.txt"), ("3", "A/sub/y.txt"), ("4", "AB/z.txt"), ("5", "A.txt")] {
            cat.local_upsert(
                conn.id,
                &LocalRow { id: id.into(), rel_path: p.into(), kind: ItemKind::File, size: 0, mtime_ns: 0, ctag: None, hash: None },
            )
            .unwrap();
        }
        cat.local_rename_prefix(conn.id, "A", "Renamed").unwrap();
        let paths: Vec<String> = cat.local_all(conn.id).unwrap().into_iter().map(|r| r.rel_path).collect();
        assert_eq!(paths, vec!["A", "A.txt", "AB/z.txt", "Renamed/sub/y.txt", "Renamed/x.txt"]);
        assert_eq!(cat.local_under(conn.id, "Renamed").unwrap().len(), 2);
    }
}
