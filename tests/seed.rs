//! The per-library seed, between two simulated computers that share one library.

mod common;

use common::{group, person, site_group, FakeRemote};
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
            own_permissions: false,
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
            own_permissions: false,
        },
    ];
    cat.apply_delta_page(&a, &items, None, Some("L:1")).unwrap();
    let a = cat.connection("a").unwrap().unwrap();
    let seed_file = dir.path().join("seed.sqlite");
    cat.export_seed(&a, &seed_file, 100, false).unwrap();

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

// ---- parts of a library not everyone may open ------------------------------------

/// A library with four folders that have permissions of their own:
///
/// * HR: Dana Whitfield and the HR Team group; the ordinary user (Alex Morgan, in
///   the Sales Team group) may not open it;
/// * Board: Alex Morgan by name;
/// * Legal: the site's "Legal Visitors" group, which Alex is not in;
/// * Marketing: the site's "Marketing Members" group, which Alex is in.
///
/// Who is in a SharePoint group is not something DriveCraft can read, so Legal and
/// Marketing can only be settled by asking the server.
fn library_with_restricted_folders() -> FakeRemote {
    restricted_library(false)
}

fn restricted_library(user_may_open_hr: bool) -> FakeRemote {
    let remote = library();
    remote.put("HR/Salaries 2026.xlsx", b"salaries");
    remote.put("HR/Reviews/Northwind review.docx", b"review");
    remote.put("Board/Minutes.docx", b"minutes");
    remote.put("Legal/Contract.docx", b"contract");
    remote.put("Marketing/Launch plan.pptx", b"launch");
    remote.protect("HR", user_may_open_hr);
    remote.protect("Board", true);
    remote.protect("Legal", false);
    remote.protect("Marketing", true);
    remote.set_permissions(
        "HR",
        vec![person("dana-id", "Dana Whitfield", "owner"), group("hr-team", "HR Team", "write")],
    );
    remote.set_permissions("Board", vec![person("alex-id", "Alex Morgan", "read")]);
    remote.set_permissions("Legal", vec![site_group("7", "Legal Visitors", "read")]);
    remote.set_permissions("Marketing", vec![site_group("8", "Marketing Members", "write")]);
    remote
}

/// What a published seed holds, read from the library as anyone could: its names,
/// and the names of the people and groups in its permissions.
fn seed_contents(remote: &FakeRemote, published: &seed::Published) -> (Vec<String>, Vec<String>) {
    let packed = remote.read(&format!(".drivecraft/{}", published.name)).expect("the seed is in the library");
    let mut plain = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&packed[..]), &mut plain).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("seed.sqlite");
    std::fs::write(&file, plain).unwrap();
    let db = rusqlite::Connection::open(&file).unwrap();
    let list = |sql: &str| -> Vec<String> {
        db.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    (list("SELECT name FROM items"), list("SELECT name FROM permissions"))
}

fn names_in_seed(remote: &FakeRemote, published: &seed::Published) -> Vec<String> {
    seed_contents(remote, published).0
}

/// Whether a machine's catalog still knows a folder at the top of the library.
fn knows(m: &Machine, name: &str) -> bool {
    let conn = m.cat.connection("lib").unwrap().unwrap();
    let root = m.cat.remote_root(conn.id).unwrap().unwrap();
    m.cat.remote_child(conn.id, &root.id, name).unwrap().is_some()
}

#[tokio::test]
async fn a_seed_made_by_an_owner_lists_restricted_items_and_who_may_open_them() {
    let remote = library_with_restricted_folders();
    let admin = Machine::new(Mode::Archive);
    admin.sync(&remote).await;
    assert_eq!(admin.read("HR/Salaries 2026.xlsx").as_deref(), Some("salaries"), "the owner has everything");

    let published = admin.publish(&remote).await;
    assert!(published.permissions, "{:?}", published.note);
    assert_eq!(published.note, None);
    assert_eq!(published.withheld, 0);
    let (names, people) = seed_contents(&remote, &published);
    for listed in ["HR", "Salaries 2026.xlsx", "Board", "Legal", "Marketing"] {
        assert!(names.iter().any(|n| n == listed), "the seed lists {listed}");
    }
    for who in ["Dana Whitfield", "HR Team", "Alex Morgan", "Legal Visitors", "Contoso Owners"] {
        assert!(people.iter().any(|n| n == who), "the seed records {who}");
    }
}

#[tokio::test]
async fn a_publisher_who_cannot_read_everyones_permissions_leaves_restricted_items_out() {
    let remote = library_with_restricted_folders();
    remote.as_user(true);
    let alex = Machine::new(Mode::Sync);
    alex.sync(&remote).await;
    assert_eq!(alex.read("Board/Minutes.docx").as_deref(), Some("minutes"));

    let published = alex.publish(&remote).await;
    assert!(!published.permissions);
    let note = published.note.clone().expect("the publisher is told why");
    assert!(note.contains("not an owner"), "{note}");
    assert_eq!(published.withheld, 2, "Board and Marketing, which Alex can see, by identifier only");
    let (names, people) = seed_contents(&remote, &published);
    assert!(!names.iter().any(|n| n == "Minutes.docx" || n == "Board"));
    assert!(people.is_empty(), "no permissions in a seed made without full lists");
}

#[tokio::test]
async fn a_user_starting_from_a_seed_with_permissions_keeps_only_what_they_may_open() {
    let remote = library_with_restricted_folders();
    let admin = Machine::new(Mode::Archive);
    admin.sync(&remote).await;
    admin.publish(&remote).await;

    remote.as_user(true);
    let asks = remote.item_asks();
    let user = Machine::new(Mode::Archive);
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(r.problems.is_empty(), "nothing to report: {:?}", r.problems);
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert_eq!(remote.full_scans(), 1, "the user did not scan the library from the start");
    assert_eq!(remote.item_asks() - asks, 2, "only Legal and Marketing needed the server; HR and Board were settled by the list");
    assert_eq!(r.no_access, 2, "HR and Legal are left alone");

    // Gone from this computer's catalog, not only from its folder.
    assert!(!knows(&user, "HR") && !knows(&user, "Legal"));
    assert!(knows(&user, "Board") && knows(&user, "Marketing"));
    assert_eq!(user.read("HR/Salaries 2026.xlsx"), None);
    assert!(!user.dir.path().join("files").join("HR").exists(), "not even an empty folder");
    assert_eq!(user.read("Board/Minutes.docx").as_deref(), Some("minutes"));
    assert_eq!(user.read("Marketing/Launch plan.pptx").as_deref(), Some("launch"));
    assert_eq!(user.read("Projects/P07/plan.txt").as_deref(), Some("plan 7"));

    // The permissions came with the seed, without what was removed.
    let conn = user.cat.connection("lib").unwrap().unwrap();
    let view = drivecraft::permissions::view(&user.cat, &conn).unwrap();
    let paths: Vec<&str> = view.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["", "Board", "Marketing"]);

    // The next run is just as quiet.
    let r = user.sync(&remote).await;
    assert_eq!((r.failed, r.downloaded, r.no_access), (0, 0, 2));
    assert!(r.problems.is_empty());

    // Alex is given access to HR: the change feed mentions it, and it arrives.
    remote.grant("HR");
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.no_access, 1, "only Legal now");
    assert_eq!(user.read("HR/Salaries 2026.xlsx").as_deref(), Some("salaries"));
    assert_eq!(user.read("HR/Reviews/Northwind review.docx").as_deref(), Some("review"));
}

#[tokio::test]
async fn a_user_in_a_recorded_group_keeps_the_folder_without_asking() {
    let remote = restricted_library(true);
    let admin = Machine::new(Mode::Archive);
    admin.sync(&remote).await;
    admin.publish(&remote).await;

    remote.as_user(true);
    remote.set_user("jordan-id", Some(vec!["hr-team"]));
    let asks = remote.item_asks();
    let user = Machine::new(Mode::Archive);
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(knows(&user, "HR"));
    assert_eq!(user.read("HR/Salaries 2026.xlsx").as_deref(), Some("salaries"));
    assert!(!knows(&user, "Board"), "Board names only Alex");
    assert_eq!(remote.item_asks() - asks, 2, "only the SharePoint groups were asked about");
}

#[tokio::test]
async fn when_group_memberships_cannot_be_read_the_server_is_asked() {
    let remote = restricted_library(true);
    let admin = Machine::new(Mode::Archive);
    admin.sync(&remote).await;
    admin.publish(&remote).await;

    remote.as_user(true);
    remote.set_user("jordan-id", None);
    let asks = remote.item_asks();
    let user = Machine::new(Mode::Archive);
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(remote.item_asks() - asks, 3, "HR (given to a group) as well as Legal and Marketing");
    assert!(knows(&user, "HR"), "the server said yes, so HR is kept");
    assert_eq!(user.read("HR/Salaries 2026.xlsx").as_deref(), Some("salaries"));
    assert!(!knows(&user, "Board"));
}

#[tokio::test]
async fn an_item_whose_permissions_cannot_be_read_is_left_out_of_the_seed() {
    let remote = library_with_restricted_folders();
    remote.hide_permissions("Legal");
    let admin = Machine::new(Mode::Archive);
    admin.sync(&remote).await;
    let published = admin.publish(&remote).await;
    assert!(published.permissions);
    assert_eq!(published.withheld, 1, "Legal, by identifier only");
    let names = names_in_seed(&remote, &published);
    assert!(names.iter().any(|n| n == "HR"));
    assert!(!names.iter().any(|n| n == "Legal" || n == "Contract.docx"));

    remote.as_user(true);
    let user = Machine::new(Mode::Archive);
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(!knows(&user, "Legal") && !knows(&user, "HR"));
    assert_eq!(r.no_access, 2);
}

#[tokio::test]
async fn the_permissions_view_lists_who_has_what_and_finds_people_and_names() {
    let remote = library_with_restricted_folders();
    remote.put("HR/Reviews/Private/Note.txt", b"note");
    remote.protect("HR/Reviews/Private", false);
    remote.set_permissions("HR/Reviews/Private", vec![person("dana-id", "Dana Whitfield", "owner")]);
    let admin = Machine::new(Mode::Archive);
    admin.sync(&remote).await;
    let conn = admin.cat.connection("lib").unwrap().unwrap();
    let read = drivecraft::permissions::read_all(&admin.cat, &remote, &conn, |_| {}).await.unwrap();
    assert_eq!((read.items, read.refused, read.full), (6, 0, true));

    let view = drivecraft::permissions::view(&admin.cat, &conn).unwrap();
    let shape: Vec<(&str, usize)> = view.entries.iter().map(|e| (e.path.as_str(), e.nesting)).collect();
    assert_eq!(
        shape,
        vec![("", 0), ("Board", 1), ("HR", 1), ("HR/Reviews/Private", 2), ("Legal", 1), ("Marketing", 1)]
    );
    assert_eq!(view.entries[0].permissions[0].name, "Contoso Owners");

    let found: Vec<String> = drivecraft::permissions::find(&view, "dana").into_iter().map(|e| e.path).collect();
    assert_eq!(found, vec!["", "HR", "HR/Reviews/Private"], "the library itself is always shown, for context");
    let found: Vec<String> = drivecraft::permissions::find(&view, "alex morgan").into_iter().map(|e| e.path).collect();
    assert_eq!(found, vec!["", "Board"]);
    let found: Vec<String> = drivecraft::permissions::find(&view, "legal").into_iter().map(|e| e.path).collect();
    assert_eq!(found, vec!["", "Legal"], "by name, or by the group's name");

    let text = drivecraft::permissions::lines(&view, &view.entries).join("\n");
    assert!(text.contains("The library itself"), "{text}");
    assert!(text.contains("  HR/ (permissions of its own)"), "{text}");
    assert!(text.contains("    HR/Reviews/Private/ (permissions of its own)"), "{text}");
    assert!(text.contains("Dana Whitfield <dana@contoso.com> (person): owner"), "{text}");
    assert!(text.contains("Legal Visitors (SharePoint group): read"), "{text}");
}

#[tokio::test]
async fn a_download_the_server_refuses_is_left_alone_until_the_feed_mentions_it_again() {
    let remote = library();
    remote.forbid_download("readme.txt", true);
    let user = Machine::new(Mode::Archive);
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.no_access, 1);
    assert_eq!(user.read("readme.txt"), None);
    assert_eq!(user.read("Projects/P01/plan.txt").as_deref(), Some("plan 1"));

    // Not tried again on every run.
    let downloads = remote.downloads();
    let r = user.sync(&remote).await;
    assert_eq!((r.failed, r.no_access), (0, 1));
    assert_eq!(remote.downloads(), downloads, "the refused file was not asked for again");

    // Access is given and the file changes: the feed mentions it, and it comes.
    remote.forbid_download("readme.txt", false);
    remote.put("readme.txt", b"hello again");
    let r = user.sync(&remote).await;
    assert_eq!((r.failed, r.no_access, r.downloaded), (0, 0, 1));
    assert_eq!(user.read("readme.txt").as_deref(), Some("hello again"));
}

/// A machine whose catalog is a file, so it can be changed behind DriveCraft's back.
fn on_disk(mode: Mode) -> (Machine, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    let cat = Catalog::open(&path).unwrap();
    cat.add_connection("lib", "drive", "https://contoso.sharepoint.com/sites/T/Docs", &dir.path().join("files"), mode)
        .unwrap();
    (Machine { cat, dir }, path)
}

#[tokio::test]
async fn a_catalog_from_before_permissions_were_kept_reads_them_before_its_first_seed() {
    let remote = library_with_restricted_folders();
    let (admin, path) = on_disk(Mode::Archive);
    admin.sync(&remote).await;
    // As an older DriveCraft left it: nothing known about permissions.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("UPDATE connections SET perms_known = 0; UPDATE remote_items SET own_perms = 0;")
        .unwrap();
    let admin = Machine { cat: Catalog::open(&path).unwrap(), dir: admin.dir };

    let published = admin.publish(&remote).await;
    assert_eq!(remote.full_scans(), 2, "the library's list was read once more, for permissions");
    assert!(published.permissions);
    let (_, people) = seed_contents(&remote, &published);
    assert!(people.iter().any(|n| n == "Dana Whitfield"), "HR was found to be restricted and its permissions read");

    // Only once: the catalog now keeps them.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    admin.publish(&remote).await;
    assert_eq!(remote.full_scans(), 2);
}

#[tokio::test]
async fn items_a_seed_should_not_have_listed_are_still_skipped_quietly() {
    // A seed that lists HR as if anyone could open it: made by an older DriveCraft,
    // or from a server that did not say HR has permissions of its own.
    let remote = library_with_restricted_folders();
    let (admin, path) = on_disk(Mode::Archive);
    admin.sync(&remote).await;
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("UPDATE remote_items SET own_perms = 0;")
        .unwrap();
    let admin = Machine { cat: Catalog::open(&path).unwrap(), dir: admin.dir };
    let published = admin.publish(&remote).await;
    assert_eq!(published.withheld, 0);

    remote.as_user(true);
    let user = Machine::new(Mode::Archive);
    let r = user.sync(&remote).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.no_access, 3, "the two HR files and the Legal one the server would not hand over");
    assert_eq!(user.read("HR/Salaries 2026.xlsx"), None);
    assert_eq!(user.read("Board/Minutes.docx").as_deref(), Some("minutes"));

    let r = user.sync(&remote).await;
    assert_eq!((r.failed, r.no_access), (0, 3), "and they stay skipped");
}
