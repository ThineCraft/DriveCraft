//! The whole client stack over HTTP: engine -> Graph client -> a stand-in server that
//! speaks the same request shapes as Microsoft Graph and keeps real state.

mod common;

use common::FakeRemote;
use drivecraft::auth::StaticToken;
use drivecraft::catalog::{Catalog, Mode};
use drivecraft::engine::{Engine, Options, Report};
use drivecraft::graph::GraphClient;
use drivecraft::paths::percent_decode;
use drivecraft::remote::*;
use futures_util::FutureExt;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct StandIn {
    remote: Arc<FakeRemote>,
    base: String,
    scratch: std::path::PathBuf,
}

fn to_json(it: &RemoteItem) -> Value {
    if it.deleted {
        return json!({ "id": it.id, "deleted": { "state": "deleted" } });
    }
    let mut v = json!({
        "id": it.id, "name": it.name, "size": it.size,
        "eTag": it.etag, "cTag": it.ctag,
        "parentReference": { "id": it.parent_id, "driveId": "D" },
    });
    if let Some(ns) = it.mtime_ns {
        let t = time::OffsetDateTime::from_unix_timestamp(ns / 1_000_000_000).unwrap();
        let s = t.format(&time::format_description::well_known::Rfc3339).unwrap();
        v["fileSystemInfo"] = json!({ "lastModifiedDateTime": s });
    }
    match it.kind {
        ItemKind::Folder => v["folder"] = json!({}),
        _ => v["file"] = json!({ "hashes": { "quickXorHash": it.hash } }),
    }
    if it.is_root {
        v["root"] = json!({});
    }
    v
}

fn fail(e: RemoteError) -> ResponseTemplate {
    let (status, code) = match e {
        RemoteError::Conflict(_) => (412, "resourceModified"),
        RemoteError::NotFound(_) => (404, "itemNotFound"),
        RemoteError::ResyncRequired => (410, "resyncRequired"),
        _ => (500, "generalException"),
    };
    ResponseTemplate::new(status).set_body_json(json!({ "error": { "code": code, "message": "stand-in" } }))
}

impl StandIn {
    fn page(&self, link: Option<&str>) -> ResponseTemplate {
        match self.remote.delta("D", link).now_or_never().unwrap() {
            Ok(p) => {
                let mut v = json!({ "value": p.items.iter().map(to_json).collect::<Vec<_>>() });
                if let Some(n) = p.next_link {
                    v["@odata.nextLink"] = json!(format!("{}/link/{n}", self.base));
                }
                if let Some(d) = p.delta_link {
                    v["@odata.deltaLink"] = json!(format!("{}/link/{d}", self.base));
                }
                ResponseTemplate::new(200).set_body_json(v)
            }
            Err(e) => fail(e),
        }
    }

    fn upload(&self, target: UploadTarget<'_>, body: &[u8]) -> ResponseTemplate {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp = self.scratch.join(format!("upload-{n}"));
        std::fs::write(&tmp, body).unwrap();
        match self.remote.upload("D", target, &tmp, None).now_or_never().unwrap() {
            Ok(it) => ResponseTemplate::new(201).set_body_json(to_json(&it)),
            Err(RemoteError::Conflict(_)) if matches!(target, UploadTarget::New { .. }) => ResponseTemplate::new(409)
                .set_body_json(json!({ "error": { "code": "nameAlreadyExists", "message": "exists" } })),
            Err(e) => fail(e),
        }
    }
}

impl Respond for StandIn {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        assert_eq!(
            req.headers.get("authorization").and_then(|h| h.to_str().ok()),
            Some("Bearer TOKEN"),
            "every request carries the user's token"
        );
        let segs: Vec<String> = req.url.path_segments().unwrap().map(percent_decode).collect();
        let s: Vec<&str> = segs.iter().map(|x| x.as_str()).collect();
        let if_match = req.headers.get("if-match").and_then(|h| h.to_str().ok());
        let method = req.method.as_str();
        match (method, s.as_slice()) {
            ("GET", ["drives", "D", "root", "delta"]) => self.page(None),
            ("GET", ["link", link]) => self.page(Some(link)),
            ("GET", ["drives", "D", "items", id, "content"]) => match self.remote.content(id) {
                Some(c) => ResponseTemplate::new(200).set_body_bytes(c),
                None => fail(RemoteError::NotFound(String::new())),
            },
            ("PUT", ["drives", "D", "items", parent, name, "content"]) => {
                let (parent, name) = (parent.trim_end_matches(':'), name.trim_end_matches(':'));
                self.upload(UploadTarget::New { parent_id: parent, name }, &req.body)
            }
            ("PUT", ["drives", "D", "items", id, "content"]) => {
                self.upload(UploadTarget::Replace { item_id: id, etag: if_match }, &req.body)
            }
            ("POST", ["drives", "D", "items", parent, "children"]) => {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let name = body["name"].as_str().unwrap();
                match self.remote.create_folder("D", parent, name).now_or_never().unwrap() {
                    Ok(it) => ResponseTemplate::new(201).set_body_json(to_json(&it)),
                    Err(e) => fail(e),
                }
            }
            ("PATCH", ["drives", "D", "items", id]) => {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let parent = body["parentReference"]["id"].as_str();
                let name = body["name"].as_str();
                if parent.is_none() && name.is_none() {
                    // A timestamp update only.
                    return match self.remote.item(id) {
                        Some(it) => ResponseTemplate::new(200).set_body_json(to_json(&it)),
                        None => fail(RemoteError::NotFound(String::new())),
                    };
                }
                match self.remote.move_item("D", id, parent, name).now_or_never().unwrap() {
                    Ok(it) => ResponseTemplate::new(200).set_body_json(to_json(&it)),
                    Err(e) => fail(e),
                }
            }
            ("DELETE", ["drives", "D", "items", id]) => {
                match self.remote.delete("D", id, if_match).now_or_never().unwrap() {
                    Ok(()) => ResponseTemplate::new(204),
                    Err(e) => fail(e),
                }
            }
            other => panic!("the client sent a request the stand-in does not know: {other:?}"),
        }
    }
}

async fn run(cat: &Catalog, graph: &GraphClient<StaticToken>, opts: Options) -> Report {
    let conn = cat.connection("lib").unwrap().unwrap();
    Engine::new(cat, graph, conn, opts).run().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_then_two_way_sync_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("Northwind");
    let remote = Arc::new(FakeRemote::new());
    remote.set_page_size(3);
    remote.put("Q1 report #3 100%.txt", b"quarterly");
    remote.put("A\u{f1}o 2026/ni\u{f1}o & co.txt", b"unicode and ampersand");
    remote.put("Deep/er/est/file.bin", &vec![7u8; 100_000]);
    remote.put("to-delete.txt", b"bye");
    remote.put("to-rename.txt", b"rename me");

    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(StandIn {
            remote: remote.clone(),
            base: server.uri(),
            scratch: dir.path().to_path_buf(),
        })
        .mount(&server)
        .await;
    let graph = GraphClient::with_base(StaticToken("TOKEN".into()), &server.uri());
    let cat = Catalog::open(&dir.path().join("catalog.db")).unwrap();
    cat.add_connection("lib", "D", "https://contoso.sharepoint.com/sites/T/Docs", &local, Mode::Archive)
        .unwrap();

    // Archive: download everything, change nothing.
    let r = run(&cat, &graph, Options::default()).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(r.downloaded, 5);
    assert_eq!(std::fs::read(local.join("Q1 report #3 100%.txt")).unwrap(), b"quarterly");
    assert_eq!(
        std::fs::read(local.join("A\u{f1}o 2026").join("ni\u{f1}o & co.txt")).unwrap(),
        b"unicode and ampersand"
    );
    assert_eq!(
        std::fs::metadata(local.join("Deep").join("er").join("est").join("file.bin")).unwrap().len(),
        100_000
    );
    assert_eq!(remote.writes(), 0);
    // The local file carries the server's timestamp.
    let want = remote.mtime_of("to-rename.txt").unwrap() / 1_000_000_000;
    let got = std::fs::metadata(local.join("to-rename.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(got, want);

    // Switch to two-way and make local changes of every kind.
    let conn = cat.connection("lib").unwrap().unwrap();
    cat.set_mode(conn.id, Mode::Sync).unwrap();
    std::fs::write(local.join("new & improved 50%.txt"), b"brand new").unwrap();
    std::fs::create_dir_all(local.join("New Folder")).unwrap();
    std::fs::write(local.join("New Folder").join("inside.txt"), b"inside").unwrap();
    std::fs::remove_file(local.join("to-delete.txt")).unwrap();
    std::fs::rename(local.join("to-rename.txt"), local.join("renamed #1.txt")).unwrap();
    std::fs::write(local.join("Q1 report #3 100%.txt"), b"quarterly, revised").unwrap();
    let rename_id = remote.id_of("to-rename.txt").unwrap();

    let r = run(&cat, &graph, Options::default()).await;
    assert_eq!(r.failed, 0, "{:?}", r.warnings);
    assert_eq!(remote.read("new & improved 50%.txt").unwrap(), b"brand new");
    assert_eq!(remote.read("New Folder/inside.txt").unwrap(), b"inside");
    assert_eq!(remote.read("Q1 report #3 100%.txt").unwrap(), b"quarterly, revised");
    assert!(!remote.exists("to-delete.txt"));
    assert_eq!(remote.id_of("renamed #1.txt"), Some(rename_id));
    assert_eq!(
        (r.uploaded, r.moved_remote, r.deleted_remote, r.remote_folders_created),
        (3, 1, 1, 1)
    );

    // Settled: another run does nothing in either direction.
    let (writes, downloads) = (remote.writes(), remote.downloads());
    let r = run(&cat, &graph, Options::default()).await;
    assert_eq!(r.changes(), 0, "{r:?}");
    assert_eq!((remote.writes(), remote.downloads()), (writes, downloads));

    // A change made by someone else arrives.
    remote.put("from a colleague.txt", b"hello");
    let r = run(&cat, &graph, Options::default()).await;
    assert_eq!(r.downloaded, 1);
    assert_eq!(std::fs::read(local.join("from a colleague.txt")).unwrap(), b"hello");
}
