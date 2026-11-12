//! Problems: what a run could not do, kept so it can be shown and acted on later.
//!
//! A run's log says what went wrong, but a log scrolls away. Each thing that needs
//! the user's attention is also recorded in the catalog, against the library it
//! belongs to, and stays there until it is no longer true:
//!
//! * most problems are *recurring*: every run meets them again for as long as they
//!   last (a file that will not upload, a name the server refuses). They are replaced
//!   by what the latest completed run found, so one that is fixed disappears by itself;
//! * a few are *lasting*: they happen once and the run moves on (a copy kept beside a
//!   changed file, a path too long for some programs). They stay until the user
//!   dismisses them, or until the file they are about is gone.

use crate::catalog::{Catalog, Connection};
use crate::paths::join_rel;
use anyhow::Result;
use rusqlite::params;
use std::collections::HashSet;

/// A run lists at most this many problems; more than that is one problem, not many.
pub const MAX_LISTED: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The whole run stopped before it finished.
    Stopped,
    /// An item could not be transferred or changed.
    Failed,
    /// Changed in both places; nothing was overwritten and nothing was sent.
    Conflict,
    /// Changed in both places; the user's version was kept beside the server's.
    CopyKept,
    /// Left out of an upload: a name or path the server will not take, and the like.
    NotUploaded,
    /// Deletions that wait for the user's confirmation.
    Held,
    /// Deleted on the server but kept here, because it holds changes made here.
    Kept,
    /// A path longer than some programs on this computer can open. A notice only.
    LongPath,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::Stopped,
        Kind::Failed,
        Kind::Conflict,
        Kind::CopyKept,
        Kind::NotUploaded,
        Kind::Held,
        Kind::Kept,
        Kind::LongPath,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Stopped => "stopped",
            Kind::Failed => "failed",
            Kind::Conflict => "conflict",
            Kind::CopyKept => "copy_kept",
            Kind::NotUploaded => "not_uploaded",
            Kind::Held => "held",
            Kind::Kept => "kept",
            Kind::LongPath => "long_path",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Happens once and stays until dismissed, rather than being met again each run.
    pub fn lasting(self) -> bool {
        matches!(self, Kind::CopyKept | Kind::Kept | Kind::LongPath)
    }

    pub fn level(self) -> Level {
        match self {
            Kind::Stopped | Kind::Failed => Level::Bad,
            Kind::LongPath => Level::Notice,
            _ => Level::Warn,
        }
    }

    /// A short heading, for lists that are not the desktop app.
    pub fn title(self) -> &'static str {
        match self {
            Kind::Stopped => "The last run stopped",
            Kind::Failed => "Could not be transferred",
            Kind::Conflict => "Changed in both places",
            Kind::CopyKept => "Your version was kept as a copy",
            Kind::NotUploaded => "Not uploaded",
            Kind::Held => "Deletions waiting for confirmation",
            Kind::Kept => "Deleted in the library, kept here",
            Kind::LongPath => "Long paths",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Bad,
    Warn,
    Notice,
}

/// What to show when the antivirus stopped a file from being read or written.
pub const BLOCKED_BY_ANTIVIRUS: &str = "Blocked by your antivirus, which reports this file as harmful. \
It was not transferred. Look at your antivirus's alerts to deal with it, and do not open the file.";

/// A plainer way to say why a file failed, where the system's own words would
/// alarm or confuse. Windows answers with error 225 or 226 when the antivirus
/// refuses to let a file be opened.
pub fn plain_reason(why: &str) -> Option<&'static str> {
    let blocked = ["(os error 225)", "(os error 226)"].iter().any(|code| why.contains(code));
    (cfg!(windows) && blocked).then_some(BLOCKED_BY_ANTIVIRUS)
}

/// One problem met during a run.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Problem {
    pub kind: Kind,
    /// The item it is about, relative to the library's folder. Empty for the run itself.
    pub path: String,
    pub message: String,
    /// A lasting problem is over once this file (relative, like `path`) is gone.
    pub watch: Option<String>,
}

/// A problem as kept in the catalog.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Stored {
    pub kind: Kind,
    pub level: Level,
    pub lasting: bool,
    pub path: String,
    pub message: String,
    /// Unix seconds.
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Tally {
    pub bad: u64,
    pub warn: u64,
    pub notice: u64,
}

impl Tally {
    pub fn total(&self) -> u64 {
        self.bad + self.warn + self.notice
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const UPSERT: &str = "INSERT INTO problems(conn_id, kind, path, message, watch, first_seen, last_seen)
     VALUES(?1,?2,?3,?4,?5,?6,?6)
     ON CONFLICT(conn_id, kind, path) DO UPDATE SET
       message = excluded.message, watch = excluded.watch, last_seen = excluded.last_seen";

impl Catalog {
    /// A run finished. What it found replaces the recurring problems recorded before;
    /// lasting ones are added to, and those whose file is gone are dropped.
    pub fn record_run(&self, conn: &Connection, found: &[Problem], now: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let still: HashSet<(&str, &str)> = found.iter().map(|p| (p.kind.as_str(), p.path.as_str())).collect();
            let old: Vec<(String, String)> = tx
                .prepare("SELECT kind, path FROM problems WHERE conn_id = ?")?
                .query_map([conn.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut delete = tx.prepare("DELETE FROM problems WHERE conn_id = ? AND kind = ? AND path = ?")?;
            for (kind, path) in &old {
                let lasting = Kind::parse(kind).is_some_and(Kind::lasting);
                if !lasting && !still.contains(&(kind.as_str(), path.as_str())) {
                    delete.execute(params![conn.id, kind, path])?;
                }
            }
            let mut upsert = tx.prepare(UPSERT)?;
            for p in found {
                upsert.execute(params![conn.id, p.kind.as_str(), p.path, p.message, p.watch, now])?;
            }
        }
        tx.commit()?;
        self.prune_problems(conn)
    }

    /// A run stopped before it finished. What it would have found is unknown, so
    /// everything recorded before stays.
    pub fn record_stopped(&self, conn_id: i64, why: &str, now: i64) -> Result<()> {
        self.db.execute(
            UPSERT,
            params![conn_id, Kind::Stopped.as_str(), "", why, None::<String>, now],
        )?;
        Ok(())
    }

    /// Drop lasting problems about files that are no longer there. Nothing is dropped
    /// while the library's folder itself is missing (a disconnected drive).
    pub fn prune_problems(&self, conn: &Connection) -> Result<()> {
        if !conn.local_path.is_dir() {
            return Ok(());
        }
        let watched: Vec<(String, String, String)> = self
            .db
            .prepare("SELECT kind, path, watch FROM problems WHERE conn_id = ? AND watch IS NOT NULL")?
            .query_map([conn.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (kind, path, watch) in watched {
            if std::fs::symlink_metadata(join_rel(&conn.local_path, &watch)).is_err() {
                self.db.execute(
                    "DELETE FROM problems WHERE conn_id = ? AND kind = ? AND path = ?",
                    params![conn.id, kind, path],
                )?;
            }
        }
        Ok(())
    }

    /// Every recorded problem, the most serious first.
    pub fn problems(&self, conn_id: i64) -> Result<Vec<Stored>> {
        let mut out: Vec<Stored> = self
            .db
            .prepare("SELECT kind, path, message, first_seen, last_seen FROM problems WHERE conn_id = ?")?
            .query_map([conn_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<(String, String, String, i64, i64)>>>()?
            .into_iter()
            .filter_map(|(kind, path, message, first_seen, last_seen)| {
                let kind = Kind::parse(&kind)?;
                Some(Stored {
                    kind,
                    level: kind.level(),
                    lasting: kind.lasting(),
                    path,
                    message,
                    first_seen,
                    last_seen,
                })
            })
            .collect();
        let order = |k: Kind| Kind::ALL.iter().position(|x| *x == k).unwrap_or(0);
        out.sort_by(|a, b| order(a.kind).cmp(&order(b.kind)).then_with(|| a.path.cmp(&b.path)));
        Ok(out)
    }

    pub fn problem_tally(&self, conn_id: i64) -> Result<Tally> {
        let mut t = Tally::default();
        let mut st = self
            .db
            .prepare_cached("SELECT kind, COUNT(*) FROM problems WHERE conn_id = ? GROUP BY kind")?;
        let rows = st.query_map([conn_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (kind, n) = row?;
            match Kind::parse(&kind).map(Kind::level) {
                Some(Level::Bad) => t.bad += n as u64,
                Some(Level::Warn) => t.warn += n as u64,
                Some(Level::Notice) => t.notice += n as u64,
                None => {}
            }
        }
        Ok(t)
    }

    /// Forget one lasting problem. A recurring one cannot be dismissed: it would only
    /// come back on the next run.
    pub fn dismiss_problem(&self, conn_id: i64, kind: Kind, path: &str) -> Result<bool> {
        if !kind.lasting() {
            return Ok(false);
        }
        let n = self.db.execute(
            "DELETE FROM problems WHERE conn_id = ? AND kind = ? AND path = ?",
            params![conn_id, kind.as_str(), path],
        )?;
        Ok(n > 0)
    }

    /// Forget every lasting problem of a library. Returns how many there were.
    pub fn dismiss_lasting(&self, conn_id: i64) -> Result<u64> {
        let mut n = 0;
        for kind in Kind::ALL.into_iter().filter(|k| k.lasting()) {
            n += self.db.execute(
                "DELETE FROM problems WHERE conn_id = ? AND kind = ?",
                params![conn_id, kind.as_str()],
            )?;
        }
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Mode;

    #[cfg(windows)]
    #[test]
    fn a_file_the_antivirus_blocked_is_said_plainly() {
        let windows = "Operation did not complete successfully because the file contains a virus or potentially unwanted software. (os error 225)";
        assert_eq!(plain_reason(windows), Some(BLOCKED_BY_ANTIVIRUS));
        assert_eq!(plain_reason("the file is open in another program (os error 32)"), None);
    }

    fn p(kind: Kind, path: &str, watch: Option<&str>) -> Problem {
        Problem {
            kind,
            path: path.into(),
            message: "why".into(),
            watch: watch.map(String::from),
        }
    }

    #[test]
    fn a_later_run_replaces_recurring_problems_and_keeps_lasting_ones() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory().unwrap();
        let conn = cat.add_connection("lib", "d", "https://x/lib", dir.path(), Mode::Sync).unwrap();
        std::fs::write(dir.path().join("a (local copy).txt"), "mine").unwrap();

        cat.record_run(
            &conn,
            &[
                p(Kind::Failed, "one.txt", None),
                p(Kind::Failed, "two.txt", None),
                p(Kind::CopyKept, "a.txt", Some("a (local copy).txt")),
            ],
            100,
        )
        .unwrap();
        assert_eq!(cat.problem_tally(conn.id).unwrap(), Tally { bad: 2, warn: 1, notice: 0 });

        // The next run still cannot send two.txt; one.txt went through.
        cat.record_run(&conn, &[p(Kind::Failed, "two.txt", None)], 200).unwrap();
        let now = cat.problems(conn.id).unwrap();
        let seen: Vec<(Kind, &str, i64, i64)> =
            now.iter().map(|s| (s.kind, s.path.as_str(), s.first_seen, s.last_seen)).collect();
        assert_eq!(seen, vec![(Kind::Failed, "two.txt", 100, 200), (Kind::CopyKept, "a.txt", 100, 100)]);

        // A stopped run says nothing about the rest; a finished one clears it.
        cat.record_stopped(conn.id, "not signed in", 300).unwrap();
        assert_eq!(cat.problem_tally(conn.id).unwrap().bad, 2);
        cat.record_run(&conn, &[], 400).unwrap();
        assert_eq!(cat.problem_tally(conn.id).unwrap(), Tally { bad: 0, warn: 1, notice: 0 });

        // The copy is dealt with: the problem goes with it.
        std::fs::remove_file(dir.path().join("a (local copy).txt")).unwrap();
        cat.prune_problems(&conn).unwrap();
        assert_eq!(cat.problem_tally(conn.id).unwrap().total(), 0);
    }

    #[test]
    fn only_lasting_problems_can_be_dismissed() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory().unwrap();
        let conn = cat.add_connection("lib", "d", "https://x/lib", dir.path(), Mode::Sync).unwrap();
        std::fs::write(dir.path().join("long.txt"), "x").unwrap();
        cat.record_run(
            &conn,
            &[p(Kind::Failed, "one.txt", None), p(Kind::LongPath, "long.txt", Some("long.txt"))],
            1,
        )
        .unwrap();
        assert!(!cat.dismiss_problem(conn.id, Kind::Failed, "one.txt").unwrap());
        assert!(cat.dismiss_problem(conn.id, Kind::LongPath, "long.txt").unwrap());
        assert_eq!(cat.problem_tally(conn.id).unwrap(), Tally { bad: 1, warn: 0, notice: 0 });
        cat.remove_connection(conn.id).unwrap();
        assert_eq!(cat.problem_tally(conn.id).unwrap().total(), 0);
    }
}
