mod common;

use common::FakeRemote;
use drivecraft::catalog::{Catalog, Mode};
use drivecraft::engine::{Engine, Options, Report};
use std::path::Path;
use std::time::{Duration, SystemTime};

struct Rig {
    cat: Catalog,
    remote: FakeRemote,
    dir: tempfile::TempDir,
}

impl Rig {
    fn new(mode: Mode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory().unwrap();
        cat.add_connection("lib", "drive", "https://contoso.sharepoint.com/sites/Team/Docs", &dir.path().join("local"), mode)
            .unwrap();
        if mode == Mode::Ingest {
            std::fs::create_dir_all(dir.path().join("local")).unwrap();
        }
        Self {
            cat,
            remote: FakeRemote::new(),
            dir,
        }
    }

    fn local(&self) -> std::path::PathBuf {
        self.dir.path().join("local")
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        let mut p = self.local();
        for s in rel.split('/') {
            p.push(s);
        }
        p
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.path(rel)).ok()
    }

    /// Write a local file and make sure its timestamp differs from any earlier one.
    fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        bump_mtime(&p);
    }

    async fn run(&self) -> Report {
        self.run_with(Options::default()).await
    }

    async fn run_with(&self, opts: Options) -> Report {
        let conn = self.cat.connection("lib").unwrap().unwrap();
        Engine::new(&self.cat, &self.remote, conn, opts).run().await.unwrap()
    }
}

fn bump_mtime(p: &Path) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(10);
    let secs = NEXT.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
    let t = SystemTime::now() + Duration::from_secs(secs);
    std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------- Archive --

#[tokio::test]
async fn archive_downloads_then_follows_changes() {
    let rig = Rig::new(Mode::Archive);
    rig.remote.put("a.txt", b"alpha");
    rig.remote.put("docs/b.txt", b"bravo");
    rig.remote.put("docs/deep/c.txt", b"charlie");

    let r = rig.run().await;
    assert_eq!(r.downloaded, 3);
    assert_eq!(rig.read("a.txt").as_deref(), Some("alpha"));
    assert_eq!(rig.read("docs/deep/c.txt").as_deref(), Some("charlie"));

    rig.remote.put("docs/b.txt", b"bravo two");
    rig.remote.put("d.txt", b"delta");
    rig.remote.remove("a.txt");
    rig.remote.rename("docs/deep", "docs/renamed");

    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(rig.read("docs/b.txt").as_deref(), Some("bravo two"));
    assert_eq!(rig.read("d.txt").as_deref(), Some("delta"));
    assert!(!rig.path("a.txt").exists(), "deleted on the server, removed from the archive");
    assert!(!rig.path("docs/deep").exists());
    assert_eq!(rig.read("docs/renamed/c.txt").as_deref(), Some("charlie"));
    assert_eq!(r.downloaded, 2, "a rename must not download again");

    let r = rig.run().await;
    assert_eq!(r.changes(), 0, "nothing changed, nothing to do");
    assert_eq!(rig.remote.writes(), 0, "archive never changes the server");
}

#[tokio::test]
async fn archive_never_uploads_local_changes() {
    let rig = Rig::new(Mode::Archive);
    rig.remote.put("a.txt", b"alpha");
    rig.run().await;
    rig.write("a.txt", "edited here");
    rig.write("extra.txt", "only here");
    std::fs::remove_file(rig.path("a.txt")).ok();
    rig.write("another.txt", "x");
    rig.run().await;
    assert_eq!(rig.remote.writes(), 0);
    assert_eq!(rig.remote.file_count(), 1);
}

#[tokio::test]
async fn scan_is_paged_and_survives_a_lost_bookmark() {
    let rig = Rig::new(Mode::Archive);
    for i in 0..7 {
        rig.remote.put(&format!("f{i}.txt"), format!("file {i}").as_bytes());
    }
    rig.remote.set_page_size(2);
    let r = rig.run().await;
    assert_eq!(r.downloaded, 7);

    // The bookmark expires and a file vanished without the feed mentioning it.
    rig.remote.remove_silently("f3.txt");
    rig.remote.put("f0.txt", b"changed");
    rig.remote.expire_bookmarks();
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(!rig.path("f3.txt").exists(), "the rescan noticed the missing file");
    assert_eq!(rig.read("f0.txt").as_deref(), Some("changed"));
    assert_eq!(rig.read("f6.txt").as_deref(), Some("file 6"));
    assert_eq!(r.downloaded, 1, "a rescan must not download unchanged files again");
}

#[tokio::test]
async fn existing_identical_files_are_adopted_not_downloaded() {
    let rig = Rig::new(Mode::Archive);
    rig.remote.put("same.txt", b"identical");
    rig.remote.put("differs.txt", b"server version");
    rig.write("same.txt", "identical");
    rig.write("differs.txt", "local version");

    let r = rig.run().await;
    assert_eq!(r.adopted, 1);
    assert_eq!(r.downloaded, 1);
    assert_eq!(rig.read("differs.txt").as_deref(), Some("server version"));
    assert_eq!(rig.read("differs (local copy).txt").as_deref(), Some("local version"));
}

#[tokio::test]
async fn server_delete_keeps_a_file_changed_here() {
    let rig = Rig::new(Mode::Archive);
    rig.remote.put("keep.txt", b"one");
    rig.remote.put("folder/inside.txt", b"two");
    rig.run().await;
    rig.write("keep.txt", "edited locally");
    rig.write("folder/mine.txt", "not from the server");
    rig.remote.remove("keep.txt");
    rig.remote.remove("folder");
    let r = rig.run().await;
    assert_eq!(rig.read("keep.txt").as_deref(), Some("edited locally"));
    assert!(!rig.path("folder/inside.txt").exists());
    assert_eq!(rig.read("folder/mine.txt").as_deref(), Some("not from the server"));
    assert!(r.warnings.len() >= 2);
}

#[tokio::test]
async fn folder_renamed_and_its_old_name_reused() {
    let rig = Rig::new(Mode::Archive);
    rig.remote.put("A/old.txt", b"old");
    rig.run().await;
    rig.remote.rename("A", "B");
    rig.remote.put("A/new.txt", b"new");
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(rig.read("B/old.txt").as_deref(), Some("old"));
    assert_eq!(rig.read("A/new.txt").as_deref(), Some("new"));
    assert_eq!(names(&rig.local()), vec!["A", "B"]);
    assert_eq!(names(&rig.path("A")), vec!["new.txt"]);
}

#[tokio::test]
async fn dry_run_changes_nothing() {
    let rig = Rig::new(Mode::Sync);
    rig.remote.put("a.txt", b"alpha");
    let r = rig
        .run_with(Options {
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert_eq!(r.downloaded, 1, "reported as planned");
    assert!(!rig.local().exists() || names(&rig.local()).is_empty());

    rig.run().await;
    rig.write("new.txt", "local");
    std::fs::remove_file(rig.path("a.txt")).unwrap();
    let r = rig
        .run_with(Options {
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert_eq!(r.uploaded, 1);
    assert_eq!(r.deleted_remote, 1);
    assert_eq!(rig.remote.writes(), 0);
    assert!(rig.remote.exists("a.txt"));
}

// ------------------------------------------------------------------- Sync --

#[tokio::test]
async fn sync_sends_new_and_changed_files() {
    let rig = Rig::new(Mode::Sync);
    rig.remote.put("a.txt", b"alpha");
    rig.run().await;

    rig.write("a.txt", "alpha edited");
    rig.write("new/sub/n.txt", "new file");
    rig.write("~$lock.docx", "office lock file");
    rig.write("my_vti_notes.txt", "x");
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.uploaded, 2);
    assert_eq!(rig.remote.read("a.txt").unwrap(), b"alpha edited");
    assert_eq!(rig.remote.read("new/sub/n.txt").unwrap(), b"new file");
    assert!(!rig.remote.exists("~$lock.docx"));

    let writes = rig.remote.writes();
    let r = rig.run().await;
    assert_eq!(r.changes(), 0, "second run is a no-op: {:?}", r);
    assert_eq!(rig.remote.writes(), writes);
    assert_eq!(rig.remote.downloads(), 1, "our own uploads are not downloaded back");
}

#[tokio::test]
async fn sync_conflict_keeps_both_versions() {
    let rig = Rig::new(Mode::Sync);
    rig.remote.put("report.txt", b"original");
    rig.run().await;

    rig.write("report.txt", "my edit");
    rig.remote.put("report.txt", b"their edit");
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.conflicts, 1);
    assert_eq!(rig.read("report.txt").as_deref(), Some("their edit"));
    assert_eq!(rig.read("report (local copy).txt").as_deref(), Some("my edit"));
    assert_eq!(rig.remote.read("report.txt").unwrap(), b"their edit", "nothing overwritten");
    assert_eq!(rig.remote.read("report (local copy).txt").unwrap(), b"my edit");
}

#[tokio::test]
async fn sync_local_delete_reaches_the_server() {
    let rig = Rig::new(Mode::Sync);
    rig.remote.put("a.txt", b"alpha");
    rig.remote.put("dir/b.txt", b"bravo");
    rig.remote.put("dir/c.txt", b"charlie");
    rig.remote.put("keep.txt", b"keep");
    rig.run().await;

    std::fs::remove_file(rig.path("a.txt")).unwrap();
    std::fs::remove_dir_all(rig.path("dir")).unwrap();
    let r = rig.run().await;
    assert_eq!(r.deleted_remote, 2, "one file and one folder: {:?}", r.warnings);
    assert!(!rig.remote.exists("a.txt"));
    assert!(!rig.remote.exists("dir"));
    assert!(rig.remote.exists("keep.txt"));

    let r = rig.run().await;
    assert_eq!(r.changes(), 0, "{:?}", r);
    assert_eq!(rig.read("keep.txt").as_deref(), Some("keep"));
}

#[tokio::test]
async fn sync_holds_back_mass_deletion() {
    let rig = Rig::new(Mode::Sync);
    for i in 0..30 {
        rig.remote.put(&format!("f{i:02}.txt"), b"x");
    }
    rig.remote.put("stay.txt", b"stay");
    rig.run().await;
    for i in 0..30 {
        std::fs::remove_file(rig.path(&format!("f{i:02}.txt"))).unwrap();
    }
    let r = rig.run().await;
    assert_eq!(r.held_deletes, 30);
    assert_eq!(r.deleted_remote, 0);
    assert_eq!(rig.remote.file_count(), 31);

    let r = rig
        .run_with(Options {
            confirm_deletes: true,
            ..Default::default()
        })
        .await;
    assert_eq!(r.deleted_remote, 30);
    assert_eq!(rig.remote.file_count(), 1);
}

#[tokio::test]
async fn sync_refuses_when_the_local_folder_vanishes() {
    let rig = Rig::new(Mode::Sync);
    rig.remote.put("a.txt", b"alpha");
    rig.remote.put("b.txt", b"bravo");
    rig.run().await;

    // Emptied folder: held back even though it is under the usual limit.
    std::fs::remove_file(rig.path("a.txt")).unwrap();
    std::fs::remove_file(rig.path("b.txt")).unwrap();
    let r = rig.run().await;
    assert_eq!(r.held_deletes, 2);
    assert_eq!(rig.remote.file_count(), 2);

    // Missing folder (an unplugged drive, say): the run stops outright.
    std::fs::remove_dir_all(rig.local()).unwrap();
    let conn = rig.cat.connection("lib").unwrap().unwrap();
    let err = Engine::new(&rig.cat, &rig.remote, conn, Options::default()).run().await;
    assert!(err.is_err());
    assert_eq!(rig.remote.file_count(), 2);
}

#[tokio::test]
async fn sync_local_rename_is_a_move_not_a_reupload() {
    let rig = Rig::new(Mode::Sync);
    let id = rig.remote.put("docs/plan.txt", b"the plan");
    rig.run().await;
    std::fs::create_dir_all(rig.path("archive")).unwrap();
    std::fs::rename(rig.path("docs/plan.txt"), rig.path("archive/plan-2026.txt")).unwrap();
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.moved_remote, 1);
    assert_eq!(r.uploaded, 0);
    assert_eq!(r.deleted_remote, 0);
    assert_eq!(rig.remote.id_of("archive/plan-2026.txt"), Some(id), "same item, so history is kept");
    assert!(!rig.remote.exists("docs/plan.txt"));
}

#[tokio::test]
async fn sync_delete_here_but_changed_there_restores_the_file() {
    let rig = Rig::new(Mode::Sync);
    rig.remote.put("a.txt", b"one");
    rig.remote.put("other.txt", b"other");
    rig.run().await;
    std::fs::remove_file(rig.path("a.txt")).unwrap();
    rig.remote.put("a.txt", b"two");
    let r = rig.run().await;
    assert_eq!(r.deleted_remote, 0);
    assert_eq!(rig.read("a.txt").as_deref(), Some("two"));
    assert!(rig.remote.exists("a.txt"));
}

// ----------------------------------------------------------------- Ingest --

fn snapshot(dir: &Path) -> Vec<(String, u64, SystemTime)> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let e = e.unwrap();
        let m = e.metadata().unwrap();
        // Directory timestamps settle lazily on NTFS, so only files are compared by time.
        let when = if m.is_dir() { SystemTime::UNIX_EPOCH } else { m.modified().unwrap() };
        out.push((e.path().display().to_string(), m.len(), when));
    }
    out
}

#[tokio::test]
async fn ingest_uploads_and_never_touches_the_source() {
    let rig = Rig::new(Mode::Ingest);
    rig.write("top.txt", "top");
    rig.write("Projects/2024/plan.txt", "plan");
    rig.write("Projects/my_vti_notes.txt", "rejected by the server");
    rig.write("Projects/Thumbs.db", "clutter");
    rig.remote.put("already.txt", b"same content");
    rig.write("already.txt", "same content");
    rig.remote.put("clash.txt", b"server content");
    rig.write("clash.txt", "source content");
    let before = snapshot(&rig.local());

    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.uploaded, 2);
    assert_eq!(r.adopted, 1);
    assert_eq!(r.conflicts, 1);
    assert_eq!(rig.remote.read("Projects/2024/plan.txt").unwrap(), b"plan");
    assert_eq!(rig.remote.read("clash.txt").unwrap(), b"server content", "never overwritten");
    assert!(!rig.remote.exists("Projects/Thumbs.db"));
    assert_eq!(snapshot(&rig.local()), before, "the source is untouched");

    // Server-side changes are never pulled into the source.
    rig.remote.put("from-server.txt", b"x");
    rig.remote.remove("top.txt");
    let r = rig.run().await;
    assert_eq!(r.uploaded, 0, "already ingested: {:?}", r.warnings);
    assert_eq!(r.unchanged, 3);
    assert_eq!(snapshot(&rig.local()), before);
    assert!(!rig.remote.exists("top.txt"), "ingest does not re-upload what was removed later");
}

#[tokio::test]
async fn ingest_reuploads_changed_source_only_when_asked() {
    let rig = Rig::new(Mode::Ingest);
    rig.write("a.txt", "one");
    rig.run().await;
    rig.write("a.txt", "two!");
    let r = rig.run().await;
    assert_eq!(r.uploaded, 0);
    assert_eq!(rig.remote.read("a.txt").unwrap(), b"one");
    let r = rig
        .run_with(Options {
            ingest_update: true,
            ..Default::default()
        })
        .await;
    assert_eq!(r.uploaded, 1);
    assert_eq!(rig.remote.read("a.txt").unwrap(), b"two!");
}

#[tokio::test]
async fn ingest_follows_symbolic_links() {
    let rig = Rig::new(Mode::Ingest);
    let outside = rig.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("linked.txt"), "via link").unwrap();
    rig.write("real.txt", "real");
    let link = rig.path("shortcut");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(&outside, &link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&outside, &link);
    if made.is_err() {
        eprintln!("skipped: this account cannot create symbolic links");
        return;
    }
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(rig.remote.read("shortcut/linked.txt").unwrap(), b"via link");
    assert_eq!(std::fs::read_to_string(outside.join("linked.txt")).unwrap(), "via link");
}

#[tokio::test]
async fn paths_longer_than_the_windows_limit_still_sync() {
    let rig = Rig::new(Mode::Sync);
    let deep: Vec<String> = (0..12).map(|i| format!("a-rather-long-folder-name-{i:02}")).collect();
    let rel = format!("{}/final document.txt", deep.join("/"));
    assert!(rig.path(&rel).as_os_str().len() > 300);
    rig.remote.put(&rel, b"deep content");
    let r = rig.run().await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(rig.read(&rel).as_deref(), Some("deep content"));
    if cfg!(windows) {
        assert_eq!(r.long_paths, 1, "the long path is reported");
    }
    rig.write(&rel, "deep content, edited");
    let r = rig.run().await;
    assert_eq!(r.uploaded, 1, "{:?}", r.warnings);
    assert_eq!(rig.remote.read(&rel).unwrap(), b"deep content, edited");
}

#[tokio::test]
async fn ingest_preview_counts_every_folder_and_changes_nothing() {
    let rig = Rig::new(Mode::Ingest);
    rig.write("a/b/c/deep.txt", "deep");
    rig.write("a/top.txt", "top");
    let r = rig
        .run_with(Options {
            dry_run: true,
            ..Default::default()
        })
        .await;
    assert_eq!(r.remote_folders_created, 3, "a, a/b and a/b/c");
    assert_eq!(r.uploaded, 2);
    assert_eq!(rig.remote.writes(), 0);
}
