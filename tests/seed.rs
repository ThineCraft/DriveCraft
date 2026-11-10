//! The per-library seed, between two simulated computers that share one library.

mod common;

use common::FakeRemote;
use drivecraft::catalog::{Catalog, Mode};
use drivecraft::engine::{Engine, Options, Report};
use drivecraft::seed;
use std::path::PathBuf;

struct Machine {
    cat: Catalog,
    dir: tempfile::TempDir,
}

impl Machine {
    fn new(mode: Mode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory().unwrap();
        cat.add_connection("lib", "drive", "https://contoso.sharepoint.com/sites/T/Docs", &dir.path().join("files"), mode)
            .unwrap();
        Self { cat, dir }
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn read(&self, rel: &str) -> Option<String> {
        let mut p = self.dir.path().join("files");
        for s in rel.split('/') {
            p.push(s);
        }
        std::fs::read_to_string(p).ok()
    }

    async fn sync(&self, remote: &FakeRemote) -> Report {
        let conn = self.cat.connection("lib").unwrap().unwrap();
        let opts = Options {
            seed_dir: Some(self.work()),
            ..Default::default()
        };
        Engine::new(&self.cat, remote, conn, opts).run().await.unwrap()
    }

    async fn publish(&self, remote: &FakeRemote) -> seed::Published {
        let conn = self.cat.connection("lib").unwrap().unwrap();
        seed::publish(&self.cat, remote, &conn, &self.work()).await.unwrap()
    }
}

fn library() -> FakeRemote {
    let remote = FakeRemote::new();
    for i in 0..20 {
        remote.put(&format!("Projects/P{i:02}/plan.txt"), format!("plan {i}").as_bytes());
    }
    remote.put("readme.txt", b"hello");
    remote
}

#[tokio::test]
async fn second_computer_starts_from_the_seed_and_skips_the_full_scan() {
    let remote = library();
    let a = Machine::new(Mode::Archive);
    a.sync(&remote).await;
    assert_eq!(remote.full_scans(), 1);

    let published = a.publish(&remote).await;
    assert_eq!(published.items, 43, "21 files, 21 folders and the root");
    assert!(remote.exists(".drivecraft/About this folder.txt"));

    // Things change after the seed was made.
    remote.put("readme.txt", b"hello, updated");
    remote.put("Projects/P00/extra.txt", b"new since the seed");
    remote.remove("Projects/P19");

    let b = Machine::new(Mode::Archive);
    let r = b.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.seeded, 43);
    assert_eq!(remote.full_scans(), 1, "the second computer did not scan the library from the start");
    assert_eq!(b.read("readme.txt").as_deref(), Some("hello, updated"));
    assert_eq!(b.read("Projects/P00/extra.txt").as_deref(), Some("new since the seed"));
    assert_eq!(b.read("Projects/P07/plan.txt").as_deref(), Some("plan 7"));
    assert_eq!(b.read("Projects/P19/plan.txt"), None, "deleted after the seed, so never downloaded");
    assert!(!b.dir.path().join("files").join(".drivecraft").exists(), "the seed folder is not content");

    // From here the second computer keeps itself current without the seed.
    remote.put("later.txt", b"later");
    let r = b.sync(&remote).await;
    assert_eq!(r.seeded, 0);
    assert_eq!(r.downloaded, 1);
    assert_eq!(remote.full_scans(), 1);

    // The first computer is not disturbed by its own seed folder.
    let r = a.sync(&remote).await;
    assert_eq!(r.failed, 0);
    assert!(!a.dir.path().join("files").join(".drivecraft").exists());
}

#[tokio::test]
async fn a_bookmark_that_is_not_accepted_falls_back_to_a_full_scan() {
    let remote = library();
    let a = Machine::new(Mode::Archive);
    a.sync(&remote).await;
    a.publish(&remote).await;
    remote.remove("readme.txt");

    remote.refuse_bookmarks();
    let b = Machine::new(Mode::Archive);
    let r = b.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(r.warnings.iter().any(|w| w.contains("bookmark was not accepted")));
    assert_eq!(remote.full_scans(), 2, "it scanned in full instead");
    assert_eq!(b.read("Projects/P03/plan.txt").as_deref(), Some("plan 3"));
    assert_eq!(b.read("readme.txt"), None, "what the seed listed but the server no longer has is dropped");
}

#[tokio::test]
async fn publishing_again_replaces_the_old_seed() {
    let remote = library();
    let a = Machine::new(Mode::Sync);
    a.sync(&remote).await;
    let first = a.publish(&remote).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    remote.put("another.txt", b"x");
    a.sync(&remote).await;
    let second = a.publish(&remote).await;
    assert_ne!(first.name, second.name);
    assert_eq!(second.replaced, 1);
    assert!(!remote.exists(&format!(".drivecraft/{}", first.name)));
    assert!(remote.exists(&format!(".drivecraft/{}", second.name)));
}

#[tokio::test]
async fn no_seed_means_an_ordinary_full_scan() {
    let remote = library();
    let b = Machine::new(Mode::Archive);
    let r = b.sync(&remote).await;
    assert_eq!(r.seeded, 0);
    assert_eq!(remote.full_scans(), 1);
    assert_eq!(b.read("readme.txt").as_deref(), Some("hello"));
}

#[tokio::test]
async fn a_seed_cannot_be_published_before_the_first_scan() {
    let remote = library();
    let a = Machine::new(Mode::Archive);
    let conn = a.cat.connection("lib").unwrap().unwrap();
    let err = seed::publish(&a.cat, &remote, &conn, &a.work()).await.unwrap_err();
    assert!(err.to_string().contains("synchronize it first"), "{err}");
    assert_eq!(remote.writes(), 0);
}

#[test]
fn a_seed_for_another_library_or_with_unsafe_names_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalog::open_in_memory().unwrap();
    let a = cat
        .add_connection("a", "driveA", "https://x/a", &dir.path().join("a"), Mode::Archive)
        .unwrap();
    let items = vec![
        drivecraft::remote::RemoteItem {
            id: "root".into(),
            parent_id: None,
            name: "root".into(),
            kind: drivecraft::remote::ItemKind::Folder,
            size: 0,
            etag: None,
            ctag: None,
            hash: None,
            mtime_ns: None,
            is_root: true,
            deleted: false,
        },
        drivecraft::remote::RemoteItem {
            id: "f".into(),
            parent_id: Some("root".into()),
            name: "..\\..\\escape.txt".into(),
            kind: drivecraft::remote::ItemKind::File,
            size: 1,
            etag: None,
            ctag: None,
            hash: None,
            mtime_ns: None,
            is_root: false,
            deleted: false,
        },
    ];
    cat.apply_delta_page(&a, &items, None, Some("L:1")).unwrap();
    let a = cat.connection("a").unwrap().unwrap();
    let seed_file = dir.path().join("seed.sqlite");
    cat.export_seed(&a, &seed_file, 100).unwrap();

    let other = cat
        .add_connection("b", "driveB", "https://x/b", &dir.path().join("b"), Mode::Archive)
        .unwrap();
    let err = cat.import_seed(&other, &seed_file).unwrap_err();
    assert!(err.to_string().contains("different library"), "{err}");

    let same = cat
        .add_connection("c", "driveA", "https://x/a", &dir.path().join("c"), Mode::Archive)
        .unwrap();
    let err = cat.import_seed(&same, &seed_file).unwrap_err();
    assert!(err.to_string().contains("not safe"), "{err}");
    assert_eq!(cat.counts(same.id).unwrap().remote_files, 0, "nothing from a refused seed is kept");
}

#[tokio::test]
async fn a_seed_carries_a_current_bookmark_even_after_the_publisher_uploaded() {
    let remote = library();
    let a = Machine::new(Mode::Sync);
    a.sync(&remote).await;
    // The publisher uploads files of its own; its stored bookmark is now behind.
    for i in 0..10 {
        let p = a.dir.path().join("files").join(format!("mine-{i}.txt"));
        std::fs::write(p, format!("mine {i}")).unwrap();
    }
    let r = a.sync(&remote).await;
    assert_eq!(r.uploaded, 10);
    a.publish(&remote).await;

    let b = Machine::new(Mode::Archive);
    let r = b.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(r.seeded >= 53);
    assert!(r.scanned <= 4, "only the seed folder itself is new since the seed: read {}", r.scanned);
    assert_eq!(b.read("mine-7.txt").as_deref(), Some("mine 7"));
}
