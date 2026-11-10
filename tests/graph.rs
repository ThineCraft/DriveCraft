//! The Microsoft Graph client against a local stand-in server. These tests check the
//! requests DriveCraft sends and how it reacts to responses; they do not prove that
//! Microsoft 365 behaves the way the stand-in does.

use drivecraft::auth::StaticToken;
use drivecraft::graph::GraphClient;
use drivecraft::quickxor::hash_bytes;
use drivecraft::remote::*;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GraphClient<StaticToken> {
    GraphClient::with_base(StaticToken("TESTTOKEN".into()), &server.uri())
}

fn item_json(id: &str, name: &str) -> serde_json::Value {
    json!({
        "id": id, "name": name, "size": 5, "eTag": "\"e1\"", "cTag": "\"c1\"",
        "parentReference": { "id": "P", "driveId": "D" },
        "file": { "hashes": { "quickXorHash": hash_bytes(b"hello") } }
    })
}

#[tokio::test]
async fn change_feed_is_read_page_by_page_with_the_users_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drives/D/root/delta"))
        .and(header("Authorization", "Bearer TESTTOKEN"))
        .and(header("Prefer", "deltashowremovedasdeleted"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [ { "id": "R", "name": "root", "root": {}, "folder": {} }, item_json("F1", "a.txt") ],
            "@odata.nextLink": format!("{}/page2", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [ { "id": "F2", "deleted": { "state": "deleted" } } ],
            "@odata.deltaLink": format!("{}/bookmark", server.uri())
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    let p1 = c.delta("D", None).await.unwrap();
    assert_eq!(p1.items.len(), 2);
    assert!(p1.items[0].is_root);
    assert_eq!(p1.items[1].name, "a.txt");
    let p2 = c.delta("D", p1.next_link.as_deref()).await.unwrap();
    assert!(p2.items[0].deleted);
    assert!(p2.delta_link.unwrap().ends_with("/bookmark"));
}

#[tokio::test]
async fn waits_as_long_as_the_server_asks_when_throttled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drives/D/root/delta"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/D/root/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [], "@odata.deltaLink": "x" })))
        .mount(&server)
        .await;

    let c = client(&server);
    let started = std::time::Instant::now();
    c.delta("D", None).await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(2), "waited {:?}", started.elapsed());
    assert_eq!(c.times_throttled(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 2, "exactly one retry");
    let stats = c.stats();
    assert_eq!(stats.pace_cuts, 1, "being asked to wait also slows the pace afterwards");
    assert!(stats.pace < drivecraft::pace::MAX_RATE);
}

#[tokio::test]
async fn refuses_to_send_the_token_to_another_server() {
    let server = MockServer::start().await;
    let c = client(&server);
    let err = c.delta("D", Some("https://evil.example/steal")).await.unwrap_err();
    assert!(matches!(err, RemoteError::Other(_)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn expired_bookmark_asks_for_a_rescan() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({ "error": { "code": "resyncRequired", "message": "x" } })))
        .mount(&server)
        .await;
    let c = client(&server);
    let link = format!("{}/old", server.uri());
    assert!(matches!(c.delta("D", Some(&link)).await, Err(RemoteError::ResyncRequired)));
}

#[tokio::test]
async fn download_writes_the_file_and_reports_its_hash() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drives/D/items/F1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let d = client(&server).download("D", "F1", &dest).await.unwrap();
    assert_eq!(d.size, 5);
    assert_eq!(d.hash, hash_bytes(b"hello"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
}

#[tokio::test]
async fn small_upload_escapes_the_name_and_sets_the_timestamp() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/drives/D/items/P:/My%20File%20%231.txt:/content"))
        .respond_with(ResponseTemplate::new(201).set_body_json(item_json("NEW", "My File #1.txt")))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/drives/D/items/NEW"))
        .respond_with(ResponseTemplate::new(200).set_body_json(item_json("NEW", "My File #1.txt")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    std::fs::write(&src, b"hello").unwrap();
    let item = client(&server)
        .upload("D", UploadTarget::New { parent_id: "P", name: "My File #1.txt" }, &src, Some(1_767_323_045_000_000_000))
        .await
        .unwrap();
    assert_eq!(item.id, "NEW");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.query(), Some("@microsoft.graph.conflictBehavior=fail"));
    assert_eq!(reqs[0].body, b"hello");
    let patch: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(patch["fileSystemInfo"]["lastModifiedDateTime"], "2026-01-02T03:04:05Z");
}

#[tokio::test]
async fn replacing_a_changed_file_is_a_conflict_not_an_overwrite() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/drives/D/items/F1/content"))
        .and(header("If-Match", "\"e1\""))
        .respond_with(ResponseTemplate::new(412).set_body_json(json!({ "error": { "code": "resourceModified", "message": "changed" } })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    std::fs::write(&src, b"mine").unwrap();
    let err = client(&server)
        .upload("D", UploadTarget::Replace { item_id: "F1", etag: Some("\"e1\"") }, &src, None)
        .await
        .unwrap_err();
    assert!(matches!(err, RemoteError::Conflict(_)), "{err:?}");
}

#[tokio::test]
async fn large_upload_goes_in_pieces_without_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/drives/D/items/P:/big.bin:/createUploadSession"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "uploadUrl": format!("{}/upload/abc", server.uri()) })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/abc"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "nextExpectedRanges": ["8-"] })))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/abc"))
        .respond_with(ResponseTemplate::new(201).set_body_json(item_json("BIG", "big.bin")))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    let data: Vec<u8> = (0..20u8).collect();
    std::fs::write(&src, &data).unwrap();
    let mut c = client(&server);
    c.simple_upload_limit = 10;
    c.chunk_size = 8;
    let item = c
        .upload("D", UploadTarget::New { parent_id: "P", name: "big.bin" }, &src, None)
        .await
        .unwrap();
    assert_eq!(item.id, "BIG");

    let reqs = server.received_requests().await.unwrap();
    let session: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(session["item"]["@microsoft.graph.conflictBehavior"], "fail");
    let ranges: Vec<String> = reqs[1..]
        .iter()
        .map(|r| r.headers.get("content-range").unwrap().to_str().unwrap().to_string())
        .collect();
    assert_eq!(ranges, vec!["bytes 0-7/20", "bytes 8-15/20", "bytes 16-19/20"]);
    let sent: Vec<u8> = reqs[1..].iter().flat_map(|r| r.body.clone()).collect();
    assert_eq!(sent, data);
    assert!(reqs[1..].iter().all(|r| r.headers.get("authorization").is_none()));
}

#[tokio::test]
async fn creating_a_folder_that_exists_returns_the_existing_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/drives/D/items/P/children"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({ "error": { "code": "nameAlreadyExists", "message": "exists" } })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/D/items/P:/Reports:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "DIR", "name": "Reports", "folder": {}, "parentReference": { "id": "P" } })))
        .mount(&server)
        .await;
    let item = client(&server).create_folder("D", "P", "Reports").await.unwrap();
    assert_eq!(item.id, "DIR");
    assert_eq!(item.kind, ItemKind::Folder);
}

#[tokio::test]
async fn finds_a_library_from_a_browser_address() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/shares/u![A-Za-z0-9_-]+/driveItem$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "R", "root": {}, "parentReference": { "driveId": "DRIVE1" } })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/DRIVE1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "DRIVE1", "name": "Documents", "driveType": "documentLibrary",
            "webUrl": "https://contoso.sharepoint.com/sites/Team/Shared%20Documents"
        })))
        .mount(&server)
        .await;
    let d = client(&server)
        .resolve_library("https://contoso.sharepoint.com/sites/Team/Shared%20Documents/Forms/AllItems.aspx?viewid=9")
        .await
        .unwrap();
    assert_eq!(d.id, "DRIVE1");
    assert_eq!(d.name, "Documents");
}

#[tokio::test]
async fn falls_back_to_listing_the_sites_libraries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/shares/.*"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({ "error": { "code": "accessDenied", "message": "no" } })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sites/contoso.sharepoint.com:/sites/Team:/drives"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [
            { "id": "D1", "name": "Documents", "webUrl": "https://contoso.sharepoint.com/sites/Team/Shared%20Documents" },
            { "id": "D2", "name": "Contracts", "webUrl": "https://contoso.sharepoint.com/sites/Team/Contracts" }
        ]})))
        .mount(&server)
        .await;
    let c = client(&server);
    let d = c.resolve_library("https://contoso.sharepoint.com/sites/Team/Contracts/2026").await.unwrap();
    assert_eq!(d.id, "D2");
    let err = c.resolve_library("https://contoso.sharepoint.com/sites/Team").await.unwrap_err();
    assert!(err.to_string().contains("Contracts"), "{err}");
}
