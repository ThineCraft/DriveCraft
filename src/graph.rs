//! Microsoft Graph: SharePoint document libraries and OneDrive for work.

use crate::auth::TokenSource;
use crate::paths;
use crate::quickxor::QuickXor;
use crate::remote::*;
use base64::Engine as _;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Microsoft asks applications to identify themselves like this; identified traffic
/// is prioritised over anonymous traffic when a tenant is under load.
const USER_AGENT: &str = concat!("ISV|ThineCraft|DriveCraft/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, PartialEq)]
pub struct DriveInfo {
    pub id: String,
    pub name: String,
    pub web_url: String,
    pub drive_type: String,
}

pub struct GraphClient<T: TokenSource> {
    http: reqwest::Client,
    tokens: T,
    base: String,
    /// No request is sent before this moment. Set when the server asks us to wait.
    not_before: Mutex<Instant>,
    throttled: AtomicU64,
    pub max_attempts: u32,
    /// Files up to this size are sent in one request.
    pub simple_upload_limit: u64,
    /// Larger files are sent in pieces of this size (a multiple of 320 KiB).
    pub chunk_size: u64,
}

impl<T: TokenSource> GraphClient<T> {
    pub fn new(tokens: T) -> Self {
        Self::with_base(tokens, GRAPH_BASE)
    }

    pub fn with_base(tokens: T, base: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                // A connection that goes silent is abandoned and retried, not waited on forever.
                .read_timeout(Duration::from_secs(120))
                .build()
                .expect("HTTP client"),
            tokens,
            base: base.trim_end_matches('/').to_string(),
            not_before: Mutex::new(Instant::now()),
            throttled: AtomicU64::new(0),
            max_attempts: 8,
            simple_upload_limit: 4 * 1024 * 1024,
            chunk_size: 32 * 320 * 1024,
        }
    }

    /// How many times the server asked this client to slow down.
    pub fn times_throttled(&self) -> u64 {
        self.throttled.load(Ordering::Relaxed)
    }

    fn url(&self, segments: &[&str]) -> reqwest::Url {
        let mut u = reqwest::Url::parse(&self.base).expect("base address");
        u.path_segments_mut()
            .expect("base address")
            .pop_if_empty()
            .extend(segments);
        u
    }

    async fn wait_for_gate(&self) {
        let until = *self.not_before.lock().unwrap();
        let now = Instant::now();
        if until > now {
            tokio::time::sleep(until - now).await;
        }
    }

    fn hold_off(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut gate = self.not_before.lock().unwrap();
        if until > *gate {
            *gate = until;
        }
    }

    fn backoff(attempt: u32) -> Duration {
        let base = 500u64.saturating_mul(1 << attempt.min(6));
        let jitter = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0))
            % 400;
        Duration::from_millis(base + jitter)
    }

    /// Send a request, honouring the server's throttling and retrying what is safe to
    /// retry. `make` is called once per attempt.
    async fn send(
        &self,
        authed: bool,
        make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> RemoteResult<reqwest::Response> {
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            self.wait_for_gate().await;
            let mut req = make(&self.http).header(reqwest::header::USER_AGENT, USER_AGENT);
            if authed {
                let token = self.tokens.token(false).await.map_err(RemoteError::Unauthorized)?;
                req = req.bearer_auth(token);
            }
            attempt += 1;
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= self.max_attempts {
                        return Err(RemoteError::Other(format!("network error: {e}")));
                    }
                    tokio::time::sleep(Self::backoff(attempt)).await;
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            match status.as_u16() {
                401 if authed && !refreshed => {
                    refreshed = true;
                    self.tokens.token(true).await.map_err(RemoteError::Unauthorized)?;
                }
                429 | 503 | 509 => {
                    // The server is asking us to slow down. Wait as long as it says.
                    self.throttled.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.max_attempts {
                        return Err(RemoteError::Other(
                            "the server kept asking DriveCraft to slow down; try again later".into(),
                        ));
                    }
                    let delay = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| Self::backoff(attempt));
                    self.hold_off(delay);
                }
                500 | 502 | 504 => {
                    if attempt >= self.max_attempts {
                        return Err(error_from(resp).await);
                    }
                    tokio::time::sleep(Self::backoff(attempt)).await;
                }
                _ => return Err(error_from(resp).await),
            }
        }
    }

    async fn get_json(&self, url: reqwest::Url) -> RemoteResult<Value> {
        let resp = self.send(true, |c| c.get(url.clone())).await?;
        resp.json::<Value>().await.map_err(RemoteError::other)
    }

    /// Links stored in the catalog are followed with the user's token attached, so
    /// they must point back at the same server and nowhere else.
    fn trusted_link(&self, link: &str) -> RemoteResult<reqwest::Url> {
        let u = reqwest::Url::parse(link).map_err(RemoteError::other)?;
        let base = reqwest::Url::parse(&self.base).map_err(RemoteError::other)?;
        if u.scheme() == base.scheme()
            && u.host_str() == base.host_str()
            && u.port_or_known_default() == base.port_or_known_default()
        {
            Ok(u)
        } else {
            Err(RemoteError::Other(
                "a stored change bookmark points at an unexpected server; refusing to use it".into(),
            ))
        }
    }

    // ---- finding a library -------------------------------------------------

    pub async fn me(&self) -> RemoteResult<(String, String)> {
        let mut u = self.url(&["me"]);
        u.set_query(Some("$select=displayName,userPrincipalName"));
        let v = self.get_json(u).await?;
        Ok((
            v["displayName"].as_str().unwrap_or_default().to_string(),
            v["userPrincipalName"].as_str().unwrap_or_default().to_string(),
        ))
    }

    async fn drive(&self, drive_id: &str) -> RemoteResult<DriveInfo> {
        let v = self.get_json(self.url(&["drives", drive_id])).await?;
        drive_info(&v).ok_or_else(|| RemoteError::Other("unexpected library description".into()))
    }

    /// Find the document library behind an address copied from the browser, or the
    /// signed-in user's own OneDrive for the word `onedrive`.
    pub async fn resolve_library(&self, input: &str) -> RemoteResult<DriveInfo> {
        if input.trim().eq_ignore_ascii_case("onedrive") {
            let v = self.get_json(self.url(&["me", "drive"])).await?;
            return drive_info(&v).ok_or_else(|| RemoteError::Other("no OneDrive found".into()));
        }
        let url = paths::normalize_library_url(input).map_err(RemoteError::Other)?;

        // First try: ask the server what the address points to.
        let token = format!(
            "u!{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url.as_str())
        );
        if let Ok(v) = self.get_json(self.url(&["shares", &token, "driveItem"])).await {
            if let Some(id) = v["parentReference"]["driveId"].as_str() {
                return self.drive(id).await;
            }
        }

        // Second try: list the site's libraries and match the address.
        let host = url.host_str().unwrap_or_default().to_string();
        let segs: Vec<String> = url
            .path_segments()
            .map(|s| s.filter(|p| !p.is_empty()).map(paths::percent_decode).collect())
            .unwrap_or_default();
        let managed = segs.len() >= 2
            && ["sites", "teams", "personal"].contains(&segs[0].to_ascii_lowercase().as_str());
        let drives_url = if managed {
            self.url(&["sites", &format!("{host}:"), &segs[0], &format!("{}:", segs[1]), "drives"])
        } else {
            self.url(&["sites", &host, "drives"])
        };
        let v = self.get_json(drives_url).await.map_err(|e| {
            RemoteError::Other(format!("could not find a SharePoint site at that address: {e}"))
        })?;
        let wanted = paths::percent_decode(url.path()).to_lowercase();
        let wanted = wanted.trim_end_matches('/');
        let drives: Vec<DriveInfo> = v["value"]
            .as_array()
            .map(|a| a.iter().filter_map(drive_info).collect())
            .unwrap_or_default();
        let mut best: Option<(usize, &DriveInfo)> = None;
        for d in &drives {
            let Ok(du) = reqwest::Url::parse(&d.web_url) else { continue };
            let path = paths::percent_decode(du.path()).to_lowercase();
            let path = path.trim_end_matches('/');
            if wanted == path || wanted.starts_with(&format!("{path}/")) {
                if best.map(|b| path.len() > b.0).unwrap_or(true) {
                    best = Some((path.len(), d));
                }
            }
        }
        match best {
            Some((_, d)) => Ok(d.clone()),
            None => {
                let list: Vec<String> = drives.iter().map(|d| format!("  {}  {}", d.name, d.web_url)).collect();
                Err(RemoteError::Other(format!(
                    "that address is not a document library. Libraries on that site:\n{}",
                    if list.is_empty() { "  (none visible to you)".into() } else { list.join("\n") }
                )))
            }
        }
    }

    // ---- check out and check in ----------------------------------------------

    pub async fn check_out(&self, drive_id: &str, item_id: &str) -> RemoteResult<()> {
        let u = self.url(&["drives", drive_id, "items", item_id, "checkout"]);
        self.send(true, |c| c.post(u.clone()).header(reqwest::header::CONTENT_LENGTH, 0))
            .await
            .map(|_| ())
    }

    pub async fn check_in(&self, drive_id: &str, item_id: &str, comment: &str) -> RemoteResult<()> {
        let u = self.url(&["drives", drive_id, "items", item_id, "checkin"]);
        let body = json!({ "comment": comment });
        self.send(true, |c| c.post(u.clone()).json(&body)).await.map(|_| ())
    }

    // ---- uploads -------------------------------------------------------------

    async fn upload_simple(
        &self,
        drive_id: &str,
        target: UploadTarget<'_>,
        src: &Path,
        mtime_ns: Option<i64>,
    ) -> RemoteResult<RemoteItem> {
        let bytes = bytes::Bytes::from(tokio::fs::read(src).await.map_err(RemoteError::other)?);
        let (url, etag) = match target {
            UploadTarget::New { parent_id, name } => {
                let mut u = self.url(&[
                    "drives",
                    drive_id,
                    "items",
                    &format!("{parent_id}:"),
                    &format!("{name}:"),
                    "content",
                ]);
                u.set_query(Some("@microsoft.graph.conflictBehavior=fail"));
                (u, None)
            }
            UploadTarget::Replace { item_id, etag } => {
                (self.url(&["drives", drive_id, "items", item_id, "content"]), etag)
            }
        };
        let resp = self
            .send(true, |c| {
                let mut r = c
                    .put(url.clone())
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(bytes.clone());
                if let Some(e) = etag {
                    r = r.header(reqwest::header::IF_MATCH, e);
                }
                r
            })
            .await?;
        let v: Value = resp.json().await.map_err(RemoteError::other)?;
        let item = parse_item(&v).ok_or_else(|| RemoteError::Other("unexpected upload response".into()))?;
        // A plain upload cannot carry the file's own timestamp; set it afterwards.
        let Some(stamp) = mtime_ns.and_then(format_time) else {
            return Ok(item);
        };
        let patch_url = self.url(&["drives", drive_id, "items", &item.id]);
        let body = json!({ "fileSystemInfo": { "lastModifiedDateTime": stamp } });
        match self.send(true, |c| c.patch(patch_url.clone()).json(&body)).await {
            Ok(r) => {
                let v: Value = r.json().await.map_err(RemoteError::other)?;
                Ok(parse_item(&v).unwrap_or(item))
            }
            // The content is safely uploaded; a timestamp that would not set is not a failure.
            Err(_) => Ok(item),
        }
    }

    async fn upload_session(
        &self,
        drive_id: &str,
        target: UploadTarget<'_>,
        src: &Path,
        size: u64,
        mtime_ns: Option<i64>,
    ) -> RemoteResult<RemoteItem> {
        let (url, etag, behaviour) = match target {
            UploadTarget::New { parent_id, name } => (
                self.url(&[
                    "drives",
                    drive_id,
                    "items",
                    &format!("{parent_id}:"),
                    &format!("{name}:"),
                    "createUploadSession",
                ]),
                None,
                "fail",
            ),
            UploadTarget::Replace { item_id, etag } => (
                self.url(&["drives", drive_id, "items", item_id, "createUploadSession"]),
                etag,
                "replace",
            ),
        };
        let mut item = json!({ "@microsoft.graph.conflictBehavior": behaviour });
        if let Some(stamp) = mtime_ns.and_then(format_time) {
            item["fileSystemInfo"] = json!({ "lastModifiedDateTime": stamp });
        }
        let body = json!({ "item": item });
        let resp = self
            .send(true, |c| {
                let mut r = c.post(url.clone()).json(&body);
                if let Some(e) = etag {
                    r = r.header(reqwest::header::IF_MATCH, e);
                }
                r
            })
            .await?;
        let v: Value = resp.json().await.map_err(RemoteError::other)?;
        let upload_url = v["uploadUrl"]
            .as_str()
            .ok_or_else(|| RemoteError::Other("the server did not start an upload".into()))?
            .to_string();

        let mut file = tokio::fs::File::open(src).await.map_err(RemoteError::other)?;
        let mut offset = 0u64;
        loop {
            let len = self.chunk_size.min(size - offset);
            let mut buf = vec![0u8; len as usize];
            file.seek(std::io::SeekFrom::Start(offset)).await.map_err(RemoteError::other)?;
            file.read_exact(&mut buf).await.map_err(|e| {
                RemoteError::Other(format!("the file changed while it was being uploaded: {e}"))
            })?;
            let chunk = bytes::Bytes::from(buf);
            let range = format!("bytes {}-{}/{}", offset, offset + len - 1, size);
            // The upload address carries its own authorisation; no token is sent to it.
            let resp = self
                .send(false, |c| {
                    c.put(&upload_url)
                        .header(reqwest::header::CONTENT_RANGE, &range)
                        .header(reqwest::header::CONTENT_LENGTH, len)
                        .body(chunk.clone())
                })
                .await?;
            offset += len;
            if offset >= size {
                let v: Value = resp.json().await.map_err(RemoteError::other)?;
                return parse_item(&v)
                    .ok_or_else(|| RemoteError::Other("unexpected response at the end of an upload".into()));
            }
        }
    }
}

impl<T: TokenSource> Remote for GraphClient<T> {
    async fn delta(&self, drive_id: &str, link: Option<&str>) -> RemoteResult<DeltaPage> {
        let url = match link {
            Some(l) => self.trusted_link(l)?,
            None => self.url(&["drives", drive_id, "root", "delta"]),
        };
        let resp = self
            .send(true, |c| c.get(url.clone()).header("Prefer", "deltashowremovedasdeleted"))
            .await?;
        let v: Value = resp.json().await.map_err(RemoteError::other)?;
        let items = v["value"]
            .as_array()
            .map(|a| a.iter().filter_map(parse_item).collect())
            .unwrap_or_default();
        Ok(DeltaPage {
            items,
            next_link: v["@odata.nextLink"].as_str().map(String::from),
            delta_link: v["@odata.deltaLink"].as_str().map(String::from),
        })
    }

    async fn download(&self, drive_id: &str, item_id: &str, dest: &Path) -> RemoteResult<Downloaded> {
        let url = self.url(&["drives", drive_id, "items", item_id, "content"]);
        let mut last = RemoteError::Other("download failed".into());
        for _ in 0..3 {
            let resp = self.send(true, |c| c.get(url.clone())).await?;
            let mut file = tokio::fs::File::create(dest).await.map_err(RemoteError::other)?;
            let mut hash = QuickXor::new();
            let mut size = 0u64;
            let mut stream = resp.bytes_stream();
            let mut broken = None;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(b) => {
                        hash.update(&b);
                        size += b.len() as u64;
                        file.write_all(&b).await.map_err(RemoteError::other)?;
                    }
                    Err(e) => {
                        broken = Some(e);
                        break;
                    }
                }
            }
            match broken {
                None => {
                    file.flush().await.map_err(RemoteError::other)?;
                    return Ok(Downloaded {
                        size,
                        hash: hash.finalize_base64(),
                    });
                }
                Some(e) => last = RemoteError::Other(format!("the download was interrupted: {e}")),
            }
        }
        Err(last)
    }

    async fn upload(
        &self,
        drive_id: &str,
        target: UploadTarget<'_>,
        src: &Path,
        mtime_ns: Option<i64>,
    ) -> RemoteResult<RemoteItem> {
        let size = tokio::fs::metadata(src).await.map_err(RemoteError::other)?.len();
        if size <= self.simple_upload_limit {
            self.upload_simple(drive_id, target, src, mtime_ns).await
        } else {
            self.upload_session(drive_id, target, src, size, mtime_ns).await
        }
    }

    async fn create_folder(&self, drive_id: &str, parent_id: &str, name: &str) -> RemoteResult<RemoteItem> {
        let url = self.url(&["drives", drive_id, "items", parent_id, "children"]);
        let body = json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        match self.send(true, |c| c.post(url.clone()).json(&body)).await {
            Ok(resp) => {
                let v: Value = resp.json().await.map_err(RemoteError::other)?;
                parse_item(&v).ok_or_else(|| RemoteError::Other("unexpected folder response".into()))
            }
            Err(RemoteError::Conflict(msg)) => {
                // Already there: use it, provided it is a folder.
                let existing =
                    self.url(&["drives", drive_id, "items", &format!("{parent_id}:"), &format!("{name}:")]);
                let v = self.get_json(existing).await?;
                match parse_item(&v) {
                    Some(it) if it.kind == ItemKind::Folder => Ok(it),
                    _ => Err(RemoteError::Conflict(msg)),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, drive_id: &str, item_id: &str, etag: Option<&str>) -> RemoteResult<()> {
        let url = self.url(&["drives", drive_id, "items", item_id]);
        self.send(true, |c| {
            let mut r = c.delete(url.clone());
            if let Some(e) = etag {
                r = r.header(reqwest::header::IF_MATCH, e);
            }
            r
        })
        .await
        .map(|_| ())
    }

    async fn move_item(
        &self,
        drive_id: &str,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: Option<&str>,
    ) -> RemoteResult<RemoteItem> {
        let url = self.url(&["drives", drive_id, "items", item_id]);
        let mut body = json!({});
        if let Some(p) = new_parent_id {
            body["parentReference"] = json!({ "id": p });
        }
        if let Some(n) = new_name {
            body["name"] = json!(n);
        }
        let resp = self.send(true, |c| c.patch(url.clone()).json(&body)).await?;
        let v: Value = resp.json().await.map_err(RemoteError::other)?;
        parse_item(&v).ok_or_else(|| RemoteError::Other("unexpected move response".into()))
    }
}

async fn error_from(resp: reqwest::Response) -> RemoteError {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let (code, message) = serde_json::from_str::<Value>(&text)
        .ok()
        .map(|v| {
            (
                v["error"]["code"].as_str().unwrap_or_default().to_string(),
                v["error"]["message"].as_str().unwrap_or_default().to_string(),
            )
        })
        .unwrap_or_default();
    let detail = if message.is_empty() {
        text.chars().take(200).collect::<String>()
    } else {
        message
    };
    match status {
        410 => RemoteError::ResyncRequired,
        404 => RemoteError::NotFound(detail),
        409 | 412 => RemoteError::Conflict(detail),
        401 => RemoteError::Unauthorized(detail),
        403 => RemoteError::Other(format!("access denied ({code}): {detail}")),
        423 => RemoteError::Other(format!("the file is locked or checked out by someone else: {detail}")),
        507 => RemoteError::Other(format!("the library is out of space: {detail}")),
        _ => RemoteError::Other(format!("server error {status} ({code}): {detail}")),
    }
}

fn drive_info(v: &Value) -> Option<DriveInfo> {
    Some(DriveInfo {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str().unwrap_or("Documents").to_string(),
        web_url: v["webUrl"].as_str().unwrap_or_default().to_string(),
        drive_type: v["driveType"].as_str().unwrap_or_default().to_string(),
    })
}

fn parse_time(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos()).ok()
}

fn format_time(ns: i64) -> Option<String> {
    if ns <= 0 {
        return None;
    }
    // Whole seconds: the server does not keep more than that reliably.
    let t = time::OffsetDateTime::from_unix_timestamp(ns / 1_000_000_000).ok()?;
    t.format(&time::format_description::well_known::Rfc3339).ok()
}

/// Read one drive item from the server's JSON.
pub fn parse_item(v: &Value) -> Option<RemoteItem> {
    let id = v["id"].as_str()?.to_string();
    let deleted = v.get("deleted").is_some() || v.get("@removed").is_some();
    let is_root = v.get("root").is_some();
    let kind = if v.get("package").is_some() {
        ItemKind::Other
    } else if v.get("folder").is_some() || is_root {
        ItemKind::Folder
    } else if v.get("file").is_some() || deleted {
        ItemKind::File
    } else {
        ItemKind::Other
    };
    let mtime_ns = v["fileSystemInfo"]["lastModifiedDateTime"]
        .as_str()
        .or_else(|| v["lastModifiedDateTime"].as_str())
        .and_then(parse_time);
    Some(RemoteItem {
        id,
        parent_id: if is_root {
            None
        } else {
            v["parentReference"]["id"].as_str().map(String::from)
        },
        name: v["name"].as_str().unwrap_or_default().to_string(),
        kind,
        size: v["size"].as_u64().unwrap_or(0),
        etag: v["eTag"].as_str().map(String::from),
        ctag: v["cTag"].as_str().map(String::from),
        hash: v["file"]["hashes"]["quickXorHash"].as_str().map(String::from),
        mtime_ns,
        is_root,
        deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_files_folders_and_deletions() {
        let file = json!({
            "id": "F1", "name": "Report.docx", "size": 1234,
            "eTag": "\"{A},2\"", "cTag": "\"c:{A},3\"",
            "parentReference": { "id": "P1", "driveId": "D" },
            "file": { "hashes": { "quickXorHash": "abc=" } },
            "fileSystemInfo": { "lastModifiedDateTime": "2026-01-02T03:04:05Z" }
        });
        let it = parse_item(&file).unwrap();
        assert_eq!(it.kind, ItemKind::File);
        assert_eq!(it.parent_id.as_deref(), Some("P1"));
        assert_eq!(it.hash.as_deref(), Some("abc="));
        assert_eq!(it.mtime_ns, Some(1_767_323_045_000_000_000));
        assert_eq!(format_time(it.mtime_ns.unwrap()).as_deref(), Some("2026-01-02T03:04:05Z"));

        let root = parse_item(&json!({ "id": "R", "name": "root", "root": {}, "folder": {} })).unwrap();
        assert!(root.is_root && root.parent_id.is_none());

        let gone = parse_item(&json!({ "id": "X", "deleted": { "state": "deleted" } })).unwrap();
        assert!(gone.deleted);

        let notebook = parse_item(&json!({ "id": "N", "name": "Notes", "package": { "type": "oneNote" }, "folder": {} })).unwrap();
        assert_eq!(notebook.kind, ItemKind::Other);
    }
}
