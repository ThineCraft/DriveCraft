//! The Microsoft Graph client against a local stand-in server. These tests check the
//! requests DriveCraft sends and how it reacts to responses; they do not prove that
//! Microsoft 365 behaves the way the stand-in does.

use drivecraft::auth::StaticToken;
use drivecraft::graph::GraphClient;
use drivecraft::quickxor::hash_bytes;
use drivecraft::remote::*;
use serde_json::json;
use wiremock::matchers::{header, headers, method, path};
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
        .and(headers("Prefer", vec!["deltashowremovedasdeleted", "hierarchicalsharing"]))
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
    // One lone request says nothing about pace: the wait is honoured, the pace kept.
    // (Cutting the pace when busy is covered by the pacer's own tests.)
    assert_eq!(stats.pace_cuts, 0);
    assert_eq!(stats.pace, drivecraft::pace::MAX_RATE);
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
async fn what_the_server_refuses_is_told_apart_from_other_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drives/D/items/LOCKED/content"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({ "error": { "code": "accessDenied", "message": "no" } })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/D/items/HR"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "error": { "code": "itemNotFound", "message": "x" } })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/D/items/BOARD"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "BOARD", "name": "Board", "folder": { "childCount": 1 }, "shared": {},
            "parentReference": { "id": "R", "driveId": "D" }
        })))
        .mount(&server)
        .await;
    let c = client(&server);
    let dir = tempfile::tempdir().unwrap();
    let err = c.download("D", "LOCKED", &dir.path().join("x")).await.unwrap_err();
    assert!(matches!(err, RemoteError::Forbidden(_)), "{err:?}");
    assert!(matches!(c.item("D", "HR").await, Err(RemoteError::NotFound(_))));
    let board = c.item("D", "BOARD").await.unwrap();
    assert_eq!((board.kind, board.own_permissions, board.parent_id.as_deref()), (ItemKind::Folder, true, Some("R")));
}

/// Permissions as the server lists them for an item: a SharePoint group, a person
/// (with their SharePoint login), a directory group, and a link for named people.
fn permissions_json() -> serde_json::Value {
    json!({ "value": [
        { "id": "3", "roles": ["owner"], "grantedToV2": { "siteGroup": { "id": "3", "displayName": "Contoso Owners", "loginName": "Contoso Owners" } } },
        { "id": "aW", "roles": ["write"], "grantedToV2": {
            "user": { "id": "dana-id", "displayName": "Dana Whitfield", "email": "dana@contoso.com" },
            "siteUser": { "id": "12", "displayName": "Dana Whitfield", "loginName": "i:0#.f|membership|dana@contoso.com" } } },
        { "id": "bX", "roles": ["read"], "grantedToV2": { "group": { "id": "hr-team", "displayName": "HR Team", "email": "hr@contoso.com" } } },
        { "id": "cY", "roles": ["read"], "link": { "scope": "users", "type": "view" },
          "grantedToIdentitiesV2": [ { "user": { "id": "alex-id", "displayName": "Alex Morgan", "email": "alex@contoso.com" } } ] }
    ]})
}

#[test]
fn permissions_are_read_one_entry_per_person_group_or_link() {
    let perms = drivecraft::graph::parse_permissions(&permissions_json());
    let seen: Vec<(PrincipalKind, Option<&str>, &str, String, bool)> = perms
        .iter()
        .map(|p| (p.kind, p.principal_id.as_deref(), p.name.as_str(), p.roles.join(","), p.via_link))
        .collect();
    assert_eq!(
        seen,
        vec![
            (PrincipalKind::SiteGroup, Some("3"), "Contoso Owners", "owner".into(), false),
            (PrincipalKind::User, Some("dana-id"), "Dana Whitfield", "write".into(), false),
            (PrincipalKind::Group, Some("hr-team"), "HR Team", "read".into(), false),
            (PrincipalKind::Link, None, "view link (users)", "read".into(), false),
            (PrincipalKind::User, Some("alex-id"), "Alex Morgan", "read".into(), true),
        ]
    );
    assert_eq!(perms[1].login.as_deref(), Some("i:0#.f|membership|dana@contoso.com"));
    assert_eq!(perms[1].email.as_deref(), Some("dana@contoso.com"));
}

#[tokio::test]
async fn permissions_are_asked_for_in_batches_and_a_throttled_part_is_sent_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/$batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "responses": [
            { "id": "1", "status": 429, "headers": { "Retry-After": "1" }, "body": { "error": { "code": "tooManyRequests", "message": "slow down" } } },
            { "id": "0", "status": 200, "body": permissions_json() },
            { "id": "2", "status": 403, "body": { "error": { "code": "accessDenied", "message": "no" } } }
        ]})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/$batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "responses": [
            { "id": "0", "status": 200, "body": { "value": [] } }
        ]})))
        .mount(&server)
        .await;
    let c = client(&server);
    let ids: Vec<String> = ["HR", "BOARD", "LEGAL"].iter().map(|s| s.to_string()).collect();
    let started = std::time::Instant::now();
    let answers = c.permissions("D", &ids).await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(1), "waited as the server asked");

    let get = |id: &str| answers.iter().find(|(i, _)| i == id).map(|(_, a)| a).unwrap();
    assert_eq!(get("HR").as_ref().unwrap().len(), 5);
    assert!(get("BOARD").as_ref().unwrap().is_empty(), "answered on the second try");
    assert!(matches!(get("LEGAL"), Err(RemoteError::Forbidden(_))));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(first["requests"][0]["url"], "/drives/D/items/HR/permissions");
    assert_eq!(first["requests"].as_array().unwrap().len(), 3);
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(second["requests"][0]["url"], "/drives/D/items/BOARD/permissions");
}

#[tokio::test]
async fn group_memberships_are_read_with_the_ordinary_sign_in_when_the_server_allows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "alex-id", "displayName": "Alex Morgan" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/transitiveMemberOf"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "@odata.type": "#microsoft.graph.group", "id": "sales-team" },
                { "@odata.type": "#microsoft.graph.directoryRole", "id": "role-1" }
            ],
            "@odata.nextLink": format!("{}/groups-page-2", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/groups-page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [ { "@odata.type": "#microsoft.graph.group", "id": "hr-team" } ]
        })))
        .mount(&server)
        .await;
    let who = client(&server).identity().await.unwrap();
    assert_eq!(who.user_id, "alex-id");
    assert_eq!(who.groups, Some(vec!["sales-team".to_string(), "hr-team".to_string()]));

    let refusing = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "alex-id", "displayName": "Alex Morgan" })))
        .mount(&refusing)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/transitiveMemberOf"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({ "error": { "code": "Authorization_RequestDenied", "message": "no" } })))
        .mount(&refusing)
        .await;
    let who = client(&refusing).identity().await.unwrap();
    assert_eq!(who.groups, None, "not known, rather than none");
}

#[tokio::test]
async fn the_probe_says_in_plain_words_what_the_server_answered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "alex-id", "displayName": "Alex Morgan" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/transitiveMemberOf"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/D/root/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "R", "name": "root", "root": {}, "folder": {}, "shared": {} },
                { "id": "HR", "name": "HR", "folder": {}, "shared": {}, "parentReference": { "id": "R" } },
                item_json("F1", "a.txt")
            ],
            "@odata.deltaLink": format!("{}/bookmark", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/$batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "responses": [
            { "id": "0", "status": 200, "body": permissions_json() },
            { "id": "1", "status": 200, "body": { "value": [] } }
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drives/D/items/DriveCraftProbeNoSuchItem"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "error": { "code": "itemNotFound", "message": "gone" } })))
        .mount(&server)
        .await;
    let text = client(&server).probe_permissions("D", None).await.join("\n");
    assert!(text.contains("Signed in as Alex Morgan"), "{text}");
    assert!(text.contains("Group memberships CAN be read"), "{text}");
    assert!(text.contains("DOES mark items with permissions of their own: 1 of the first 3"), "{text}");
    assert!(text.contains("Permissions of the library's top folder CAN be listed: 5 entries"), "{text}");
    assert!(text.contains("including owners"), "{text}");
    assert!(text.contains("\"not found\" (404)"), "{text}");
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.iter().all(|r| r.method.as_str() == "GET" || r.url.path() == "/$batch"), "nothing but reads");
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
