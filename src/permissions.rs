//! Who may open what in a library: the permissions of each item that does not simply
//! take its folder's, read from the server and kept in the catalog.
//!
//! SharePoint has no single view of a library's permission structure. DriveCraft
//! already learns from the change feed which items have permissions of their own, so
//! only those need to be asked about: a library of 200,000 files may have a few
//! dozen. Their permissions are read twenty at a time and kept, so that looking at
//! them, searching them, and putting them in a seed is instant afterwards.

use crate::catalog::{Catalog, Connection};
use crate::remote::{ItemKind, Permission, PrincipalKind, Remote, RemoteError};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};

/// How many items' permissions are asked for between reports of progress.
const READ_STEP: usize = 200;

fn perm_from_row(r: &rusqlite::Row) -> rusqlite::Result<Permission> {
    let roles: String = r.get(7)?;
    Ok(Permission {
        perm_id: r.get(0)?,
        kind: PrincipalKind::parse(&r.get::<_, String>(1)?),
        principal_id: r.get(2)?,
        name: r.get(3)?,
        email: r.get(4)?,
        login: r.get(5)?,
        link_scope: r.get(6)?,
        roles: roles.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
        via_link: r.get::<_, i64>(8)? != 0,
    })
}

const PERM_COLS: &str = "perm_id, kind, principal_id, name, email, login, link_scope, roles, via_link";

/// What the catalog knows about one item's permissions.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    /// Never read.
    Unknown,
    /// The server would not show them.
    Refused,
    Read(Vec<Permission>),
}

impl Catalog {
    /// Replace what is recorded for one item.
    pub fn store_permissions(&self, conn_id: i64, item_id: &str, answer: Option<&[Permission]>) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM permissions WHERE conn_id = ? AND item_id = ?", params![conn_id, item_id])?;
        {
            let mut add = tx.prepare_cached(
                "INSERT INTO permissions(conn_id, item_id, seq, perm_id, kind, principal_id, name, email, login, roles, link_scope, via_link)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            for (seq, p) in answer.unwrap_or(&[]).iter().enumerate() {
                add.execute(params![
                    conn_id,
                    item_id,
                    seq as i64,
                    p.perm_id,
                    p.kind.as_str(),
                    p.principal_id,
                    p.name,
                    p.email,
                    p.login,
                    p.roles.join(","),
                    p.link_scope,
                    p.via_link as i64
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO perm_items(conn_id, item_id, state) VALUES(?1, ?2, ?3)",
            params![conn_id, item_id, if answer.is_some() { "read" } else { "refused" }],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn recorded_permissions(&self, conn_id: i64, item_id: &str) -> Result<Recorded> {
        let state: Option<String> = self
            .db
            .prepare_cached("SELECT state FROM perm_items WHERE conn_id = ? AND item_id = ?")?
            .query_row(params![conn_id, item_id], |r| r.get(0))
            .optional()?;
        Ok(match state.as_deref() {
            None => Recorded::Unknown,
            Some("read") => {
                let mut st = self.db.prepare_cached(&format!(
                    "SELECT {PERM_COLS} FROM permissions WHERE conn_id = ? AND item_id = ? ORDER BY seq"
                ))?;
                let rows = st.query_map(params![conn_id, item_id], perm_from_row)?;
                Recorded::Read(rows.collect::<rusqlite::Result<_>>()?)
            }
            Some(_) => Recorded::Refused,
        })
    }

    /// Forget every recorded permission of a library before reading them again.
    pub fn clear_permissions(&self, conn_id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM permissions WHERE conn_id = ?", [conn_id])?;
        tx.execute("DELETE FROM perm_items WHERE conn_id = ?", [conn_id])?;
        tx.execute("UPDATE connections SET perms_read_at = NULL, perms_full = 0 WHERE id = ?", [conn_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_permissions_read(&self, conn_id: i64, at: i64, full: bool) -> Result<()> {
        self.db.execute(
            "UPDATE connections SET perms_read_at = ?2, perms_full = ?3 WHERE id = ?1",
            params![conn_id, at, full as i64],
        )?;
        Ok(())
    }

    /// When the permissions were last read, and whether that read saw the full lists.
    pub fn permissions_read(&self, conn_id: i64) -> Result<(Option<i64>, bool)> {
        Ok(self.db.query_row(
            "SELECT perms_read_at, perms_full FROM connections WHERE id = ?",
            [conn_id],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
        )?)
    }

    /// The identifiers of the items whose permissions are their own: the library's
    /// top folder first, then the rest.
    pub fn permission_item_ids(&self, conn_id: i64) -> Result<Vec<String>> {
        let mut st = self.db.prepare(
            "SELECT id FROM remote_items WHERE conn_id = ? AND deleted = 0 AND no_access = 0
               AND (is_root = 1 OR own_perms = 1)
             ORDER BY is_root DESC, id",
        )?;
        let rows = st.query_map([conn_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remove an item and everything beneath it from the catalog, keeping only its
    /// identifier, so that the server can be asked about it again later (`checked`
    /// says when it was last refused; 0 means "ask on the next run"). Returns how
    /// many items were removed.
    pub fn prune_subtree(&self, conn_id: i64, root_id: &str, checked: i64) -> Result<u64> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS temp.pruned;
             CREATE TEMP TABLE pruned(id TEXT PRIMARY KEY) WITHOUT ROWID;",
        )?;
        tx.execute(
            "INSERT INTO temp.pruned
             WITH RECURSIVE sub(id) AS (
               SELECT ?2
               UNION
               SELECT r.id FROM remote_items r INDEXED BY remote_by_parent JOIN sub ON r.conn_id = ?1 AND r.parent_id = sub.id
             )
             SELECT id FROM sub",
            params![conn_id, root_id],
        )?;
        let n = tx.execute(
            "DELETE FROM remote_items WHERE conn_id = ?1 AND id IN (SELECT id FROM temp.pruned)",
            [conn_id],
        )?;
        tx.execute("DELETE FROM permissions WHERE conn_id = ?1 AND item_id IN (SELECT id FROM temp.pruned)", [conn_id])?;
        tx.execute("DELETE FROM perm_items WHERE conn_id = ?1 AND item_id IN (SELECT id FROM temp.pruned)", [conn_id])?;
        tx.execute("DELETE FROM withheld WHERE conn_id = ?1 AND id IN (SELECT id FROM temp.pruned)", [conn_id])?;
        tx.execute(
            "INSERT OR REPLACE INTO withheld(conn_id, id, checked) VALUES(?1, ?2, ?3)",
            params![conn_id, root_id, checked],
        )?;
        tx.execute_batch("DROP TABLE temp.pruned;")?;
        tx.commit()?;
        Ok(n as u64)
    }
}

/// One item in the view of a library's permissions.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Entry {
    pub id: String,
    /// Relative to the library, using `/`; empty for the library itself.
    pub path: String,
    pub folder: bool,
    /// How many items above this one in the list contain it.
    pub nesting: usize,
    /// "read", "refused" (the server would not show them) or "unknown" (not read yet).
    pub state: &'static str,
    pub permissions: Vec<Permission>,
}

/// A library's permission structure, as recorded.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct View {
    /// When the permissions were read, Unix seconds.
    pub read_at: Option<i64>,
    /// The read saw full lists (the reader could see everyone's permissions).
    pub full: bool,
    /// The library itself first, then each item with permissions of its own, in
    /// path order. Everything not listed takes the permissions of the nearest listed
    /// folder above it.
    pub entries: Vec<Entry>,
}

/// The recorded permission structure of a library.
pub fn view(cat: &Catalog, conn: &Connection) -> Result<View> {
    let (read_at, full) = cat.permissions_read(conn.id)?;
    let mut entries = Vec::new();
    for id in cat.permission_item_ids(conn.id)? {
        let Some(item) = cat.remote_get(conn.id, &id)? else { continue };
        let Some(path) = cat.remote_path(conn.id, &item)? else { continue };
        if crate::seed::is_seed_path(&path) {
            continue;
        }
        let (state, permissions) = match cat.recorded_permissions(conn.id, &id)? {
            Recorded::Read(p) => ("read", p),
            Recorded::Refused => ("refused", vec![]),
            Recorded::Unknown => ("unknown", vec![]),
        };
        entries.push(Entry { id, path, folder: item.kind == ItemKind::Folder, nesting: 0, state, permissions });
    }
    entries.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    // The library's top folder (empty path) sorts first; each other entry is nested
    // under the listed folders whose paths start its own.
    let mut open: Vec<String> = Vec::new();
    for e in entries.iter_mut() {
        if e.path.is_empty() {
            continue;
        }
        let lower = e.path.to_lowercase();
        while open.last().is_some_and(|p| !lower.starts_with(&format!("{p}/"))) {
            open.pop();
        }
        e.nesting = open.len() + 1;
        if e.folder {
            open.push(lower);
        }
    }
    Ok(View { read_at, full, entries })
}

/// The entries whose path, or whose people, groups or links, mention every word of
/// `find` (case is ignored). The library's own entry is always kept, for context.
pub fn find(view: &View, find: &str) -> Vec<Entry> {
    let words: Vec<String> = find.to_lowercase().split_whitespace().map(String::from).collect();
    view.entries
        .iter()
        .filter(|e| {
            e.path.is_empty()
                || words.iter().all(|w| {
                    e.path.to_lowercase().contains(w)
                        || e.permissions.iter().any(|p| {
                            p.name.to_lowercase().contains(w)
                                || p.email.as_deref().is_some_and(|m| m.to_lowercase().contains(w))
                                || p.login.as_deref().is_some_and(|m| m.to_lowercase().contains(w))
                        })
                })
        })
        .cloned()
        .collect()
}

/// How a read of the permissions went.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ReadOutcome {
    /// Items whose permissions were asked for, the library included.
    pub items: u64,
    /// Items whose permissions the server would not show.
    pub refused: u64,
    /// The signed-in person sees full lists. Microsoft shows someone who is not an
    /// owner only the permissions that apply to them; an owner entry on the library
    /// itself is taken as the sign that the lists are complete.
    pub full: bool,
}

/// Read the whole library's list from the start and record which items have
/// permissions of their own. Nothing else in the catalog changes, and the bookmark
/// this read ends with is not kept. On a large library this takes minutes, so
/// `progress` hears, in plain words, how far it has got.
pub async fn survey<R: Remote>(
    cat: &Catalog,
    remote: &R,
    conn: &Connection,
    progress: &dyn Fn(&str),
) -> Result<()> {
    cat.begin_permission_survey(conn.id)?;
    let mut link: Option<String> = None;
    let mut seen = 0u64;
    progress("Reading the library's list to find what has permissions of its own");
    loop {
        let page = remote
            .delta(&conn.drive_id, link.as_deref())
            .await
            .context("reading the library to find what has permissions of its own")?;
        cat.record_permissions(conn.id, &page.items)?;
        seen += page.items.len() as u64;
        progress(&format!(
            "Reading the library's list to find what has permissions of its own: {} items so far",
            crate::engine::count(seen)
        ));
        if page.delta_link.is_some() {
            break;
        }
        link = match page.next_link {
            Some(next) => Some(next),
            None => bail!("the server's change feed ended without a bookmark"),
        };
    }
    cat.end_permission_survey(conn.id)
}

/// Read, from the server, the permissions of the library and of every item that has
/// permissions of its own, and keep them. `progress` hears, in plain words, what is
/// being read and how far it has got.
pub async fn read_all<R: Remote>(
    cat: &Catalog,
    remote: &R,
    conn: &Connection,
    progress: impl Fn(&str),
) -> Result<ReadOutcome> {
    let conn = cat.connection_by_id(conn.id)?;
    if conn.delta_link.is_none() {
        bail!("this library has not been fully scanned on this computer yet; synchronize it first");
    }
    if !conn.perms_known {
        survey(cat, remote, &conn, &progress).await?;
    }
    let root = cat
        .remote_root(conn.id)?
        .context("the library's root folder is not in the catalog yet; synchronize it first")?;
    let ids = cat.permission_item_ids(conn.id)?;
    let total = ids.len() as u64;
    cat.clear_permissions(conn.id)?;
    progress("Reading who may open the library");
    let (mut done, mut refused, mut full) = (0u64, 0u64, false);
    for chunk in ids.chunks(READ_STEP) {
        let answers = remote
            .permissions(&conn.drive_id, chunk)
            .await
            .context("reading permissions from the library")?;
        for (id, answer) in answers {
            match answer {
                Ok(perms) => {
                    if id == root.id {
                        full = perms.iter().any(|p| p.roles.iter().any(|r| r == "owner"));
                    }
                    cat.store_permissions(conn.id, &id, Some(&perms))?;
                }
                Err(RemoteError::Forbidden(_) | RemoteError::NotFound(_)) => {
                    refused += 1;
                    cat.store_permissions(conn.id, &id, None)?;
                }
                Err(RemoteError::Unauthorized(e)) => bail!("not signed in, or the sign-in has expired: {e}"),
                Err(e) => return Err(anyhow::anyhow!("reading the permissions of an item: {e}")),
            }
            done += 1;
        }
        if total > 1 {
            progress(&format!(
                "Reading who may open each one: {} of {} folders and files with permissions of their own",
                crate::engine::count(done.saturating_sub(1)),
                crate::engine::count(total - 1)
            ));
        }
    }
    cat.set_permissions_read(conn.id, crate::problems::unix_now(), full)?;
    Ok(ReadOutcome { items: total, refused, full })
}

/// The view as lines of text, for the command line.
pub fn lines(view: &View, entries: &[Entry]) -> Vec<String> {
    let mut out = Vec::new();
    if view.read_at.is_none() {
        out.push("Permissions have not been read for this library yet. Run with --refresh.".into());
        return out;
    }
    if !view.full {
        out.push("Note: the account that read these is not an owner of the library, so the server showed it only".into());
        out.push("the permissions that apply to that account. The lists below may be incomplete.".into());
    }
    for e in entries {
        let indent = "  ".repeat(e.nesting);
        if e.path.is_empty() {
            out.push("The library itself (everything below takes these unless listed):".into());
        } else {
            out.push(format!("{indent}{}{} (permissions of its own)", e.path, if e.folder { "/" } else { "" }));
        }
        let inner = "  ".repeat(e.nesting + 1);
        match e.state {
            "refused" => out.push(format!("{inner}(the server would not show its permissions)")),
            "unknown" => out.push(format!("{inner}(not read yet)")),
            _ if e.permissions.is_empty() => out.push(format!("{inner}(nobody is listed)")),
            _ => {
                for p in &e.permissions {
                    let mut who = p.name.clone();
                    if let Some(m) = p.email.as_deref().filter(|m| !p.name.contains(*m)) {
                        who.push_str(&format!(" <{m}>"));
                    }
                    let via = if p.via_link { ", through a sharing link" } else { "" };
                    out.push(format!("{inner}{who} ({}{via}): {}", p.kind.said(), p.roles.join(", ")));
                }
            }
        }
    }
    out
}
