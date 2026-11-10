//! Checked-out files.
//!
//! A file that is checked out in a library can only be changed by the person who
//! holds it. The catalog keeps a list of the files checked out in each library, and
//! by whom, so the app and File Explorer can show it. The list is the server's: it is
//! fetched whole after each synchronization, and adjusted at once when DriveCraft
//! itself checks a file out or in.

use crate::catalog::{Catalog, Connection};
use crate::explorer;
use crate::graph::{GraphClient, TokenSource};
use crate::paths::join_rel;
use anyhow::Result;
use rusqlite::params;

/// One checked-out file, as shown to the user.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Listed {
    pub item_id: String,
    /// Where the file is in the library's folder; `None` when it is not on this computer.
    pub path: Option<String>,
    /// Who holds it. Empty when the server did not say.
    pub by: String,
    /// Held by the signed-in user.
    pub mine: bool,
    /// When DriveCraft first saw it checked out, Unix seconds.
    pub since: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Count {
    pub total: u64,
    pub mine: u64,
}

impl Catalog {
    /// Replace the list for a library with what the server reports now. Files that
    /// were already listed keep the time they were first seen.
    pub fn replace_checkouts(&self, conn_id: i64, now_out: &[(String, String, bool)], now: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("UPDATE checkouts SET stale = 1 WHERE conn_id = ?", [conn_id])?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO checkouts(conn_id, item_id, by_name, mine, since, stale) VALUES(?1,?2,?3,?4,?5,0)
                 ON CONFLICT(conn_id, item_id) DO UPDATE SET by_name = excluded.by_name, mine = excluded.mine, stale = 0",
            )?;
            for (item_id, by, mine) in now_out {
                upsert.execute(params![conn_id, item_id, by, *mine as i64, now])?;
            }
        }
        tx.execute("DELETE FROM checkouts WHERE conn_id = ? AND stale = 1", [conn_id])?;
        tx.commit()?;
        Ok(())
    }

    /// DriveCraft just checked this file out (`Some(who)`) or in (`None`).
    pub fn note_checkout(&self, conn_id: i64, item_id: &str, by_me: Option<&str>, now: i64) -> Result<()> {
        match by_me {
            Some(who) => self.db.execute(
                "INSERT INTO checkouts(conn_id, item_id, by_name, mine, since, stale) VALUES(?1,?2,?3,1,?4,0)
                 ON CONFLICT(conn_id, item_id) DO UPDATE SET by_name = excluded.by_name, mine = 1",
                params![conn_id, item_id, who, now],
            )?,
            None => self.db.execute(
                "DELETE FROM checkouts WHERE conn_id = ? AND item_id = ?",
                params![conn_id, item_id],
            )?,
        };
        Ok(())
    }

    /// The checked-out files of a library: the user's own first, then by path.
    pub fn checkouts(&self, conn_id: i64) -> Result<Vec<Listed>> {
        let mut out: Vec<Listed> = self
            .db
            .prepare(
                "SELECT c.item_id, l.rel_path, c.by_name, c.mine, c.since
                 FROM checkouts c LEFT JOIN local_items l ON l.conn_id = c.conn_id AND l.id = c.item_id
                 WHERE c.conn_id = ?",
            )?
            .query_map([conn_id], |r| {
                Ok(Listed {
                    item_id: r.get(0)?,
                    path: r.get(1)?,
                    by: r.get(2)?,
                    mine: r.get::<_, i64>(3)? != 0,
                    since: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        out.sort_by(|a, b| b.mine.cmp(&a.mine).then_with(|| a.path.cmp(&b.path)));
        Ok(out)
    }

    pub fn checkout_count(&self, conn_id: i64) -> Result<Count> {
        let (total, mine): (i64, i64) = self.db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(mine), 0) FROM checkouts WHERE conn_id = ?",
            [conn_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Count {
            total: total as u64,
            mine: mine as u64,
        })
    }
}

/// What File Explorer says beside a checked-out file.
fn label(c: &Listed) -> String {
    if c.mine {
        "Checked out by you".to_string()
    } else if c.by.is_empty() {
        "Checked out".to_string()
    } else {
        format!("Checked out by {}", c.by)
    }
}

/// Make File Explorer's marks match the list: set on what is listed now, cleared on
/// what was listed `before` and no longer is. Does nothing for a folder that is not
/// shown in File Explorer.
pub fn show_in_explorer(cat: &Catalog, conn: &Connection, before: &[Listed]) -> Result<()> {
    if explorer::shown_as(&conn.local_path).is_none() {
        return Ok(());
    }
    let now = cat.checkouts(conn.id)?;
    for gone in before.iter().filter(|b| !now.iter().any(|n| n.item_id == b.item_id)) {
        if let Some(rel) = &gone.path {
            let _ = explorer::set_checked_out(&join_rel(&conn.local_path, rel), None);
        }
    }
    for c in &now {
        if let Some(rel) = &c.path {
            let _ = explorer::set_checked_out(&join_rel(&conn.local_path, rel), Some(&label(c)));
        }
    }
    Ok(())
}

/// Ask the server which files of a library are checked out, and record the answer.
/// Returns how many there are.
pub async fn refresh<T: TokenSource>(cat: &Catalog, graph: &GraphClient<T>, conn: &Connection) -> Result<Count> {
    let me = graph.me_id().await?;
    let found = graph.checked_out(&conn.drive_id).await?;
    let before = cat.checkouts(conn.id)?;
    let rows: Vec<(String, String, bool)> = found
        .into_iter()
        .map(|c| {
            let mine = !me.is_empty() && c.by_id == me;
            (c.item_id, c.by_name, mine)
        })
        .collect();
    cat.replace_checkouts(conn.id, &rows, crate::problems::unix_now())?;
    show_in_explorer(cat, conn, &before)?;
    cat.checkout_count(conn.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{LocalRow, Mode};
    use crate::remote::ItemKind;

    #[test]
    fn the_list_follows_the_server_and_keeps_first_seen_times() {
        let cat = Catalog::open_in_memory().unwrap();
        let conn = cat
            .add_connection("lib", "d", "https://x/lib", std::path::Path::new("/tmp/none"), Mode::Sync)
            .unwrap();
        cat.local_upsert(
            conn.id,
            &LocalRow { id: "a".into(), rel_path: "docs/a.docx".into(), kind: ItemKind::File, size: 1, mtime_ns: 1, ctag: None, hash: None },
        )
        .unwrap();
        let row = |id: &str, by: &str, mine: bool| (id.to_string(), by.to_string(), mine);

        cat.replace_checkouts(conn.id, &[row("a", "Ann", false), row("b", "Me", true)], 100).unwrap();
        let list = cat.checkouts(conn.id).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].item_id.as_str(), list[0].mine, &list[0].path), ("b", true, &None), "mine first");
        assert_eq!(list[1].path.as_deref(), Some("docs/a.docx"));
        assert_eq!(cat.checkout_count(conn.id).unwrap(), Count { total: 2, mine: 1 });

        // Ann still has hers; mine was checked in elsewhere.
        cat.replace_checkouts(conn.id, &[row("a", "Ann", false)], 200).unwrap();
        let list = cat.checkouts(conn.id).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].since, 100);

        // Checking out and in through DriveCraft shows at once.
        cat.note_checkout(conn.id, "c", Some("Me"), 300).unwrap();
        assert_eq!(cat.checkout_count(conn.id).unwrap(), Count { total: 2, mine: 1 });
        cat.note_checkout(conn.id, "c", None, 400).unwrap();
        assert_eq!(cat.checkout_count(conn.id).unwrap(), Count { total: 1, mine: 0 });
        cat.remove_connection(conn.id).unwrap();
        assert_eq!(cat.checkout_count(conn.id).unwrap().total, 0);
    }
}
