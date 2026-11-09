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
    db: rusqlite::Connection,
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
";

const REMOTE_COLS: &str =
    "id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted";

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

const CONN_COLS: &str = "id, name, drive_id, web_url, local_path, mode, delta_link, cursor, generation, resyncing, last_sync";

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
            let mut upsert = tx.prepare_cached(
                "INSERT INTO remote_items(conn_id, id, parent_id, name, kind, size, etag, ctag, hash, mtime_ns, is_root, deleted, dirty, seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,1,?12)
                 ON CONFLICT(conn_id, id) DO UPDATE SET
                   parent_id=excluded.parent_id, name=excluded.name, kind=excluded.kind, size=excluded.size,
                   etag=excluded.etag, ctag=excluded.ctag, hash=excluded.hash, mtime_ns=excluded.mtime_ns,
                   is_root=excluded.is_root, deleted=0, dirty=1, seen=excluded.seen",
            )?;
            let mut mark_deleted = tx.prepare_cached(
                "UPDATE remote_items SET deleted = 1, dirty = 1, seen = ?3 WHERE conn_id = ?1 AND id = ?2",
            )?;
            for it in items {
                if it.deleted {
                    mark_deleted.execute(params![conn.id, it.id, conn.generation])?;
                } else {
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
                        conn.generation
                    ])?;
                }
            }
        }
        if let Some(link) = delta_link {
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
                "SELECT {REMOTE_COLS} FROM remote_items
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
                   deleted=0, dirty=0, seen=excluded.seen",
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

    /// The path of an item relative to the library root, using `/`. `None` if the
    /// item is not connected to the root or sits inside something not synchronized.
    pub fn remote_path(&self, conn_id: i64, item: &RemoteItem) -> Result<Option<String>> {
        if item.is_root {
            return Ok(Some(String::new()));
        }
        let mut parts = vec![item.name.clone()];
        let mut parent = item.parent_id.clone();
        for _ in 0..512 {
            let Some(pid) = parent else { return Ok(None) };
            let Some(p) = self.remote_get(conn_id, &pid)? else {
                return Ok(None);
            };
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
            "SELECT COUNT(*), COALESCE(SUM(size),0) FROM remote_items WHERE conn_id = ? AND kind = 'file' AND deleted = 0",
            [conn_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let folders: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM remote_items WHERE conn_id = ? AND kind = 'folder' AND deleted = 0 AND is_root = 0",
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
        }
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
