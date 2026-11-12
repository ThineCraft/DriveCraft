//! Microsoft Graph: SharePoint document libraries and OneDrive for work.

pub use crate::auth::TokenSource;
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

/// How the change feed is asked for. Deletions come as items marked deleted, and
/// sharing information comes only on items that have permissions of their own, which
/// is how DriveCraft learns which parts of a library not everyone can open.
pub const DELTA_PREFER: &str = "deltashowremovedasdeleted, hierarchicalsharing";

/// Microsoft asks applications to identify themselves like this; identified traffic
/// is prioritised over anonymous traffic when a tenant is under load.
const USER_AGENT: &str = concat!("ISV|ThineCraft|DriveCraft/", env!("CARGO_PKG_VERSION"));

/// Counters for one client's traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct NetStats {
    pub requests: u64,
    pub retries: u64,
    pub throttled: u64,
    pub throttle_wait_ms: u64,
    /// The pace now allowed, in requests a second.
    pub pace: f64,
    /// How many times the pace was cut because the server asked for a wait.
    pub pace_cuts: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DriveInfo {
    pub id: String,
    pub name: String,
    pub web_url: String,
    pub drive_type: String,
}

/// What kind of value a library column holds, as far as DriveCraft can show or edit it.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    Text,
    /// Several lines of text.
    Note,
    Number,
    YesNo,
    Date,
    Choice,
    /// A person, a link to another list, a term, a calculation: shown, not edited.
    Other,
}

/// A column of a library.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Column {
    /// The name the server stores values under.
    pub name: String,
    /// The name people see.
    pub title: String,
    pub kind: ColumnKind,
    pub read_only: bool,
    pub required: bool,
    pub hidden: bool,
    /// For a choice: what can be picked.
    pub choices: Vec<String>,
    /// For a choice: several may be picked.
    pub multi: bool,
    /// For a date: the day only, with no time.
    pub date_only: bool,
    pub description: String,
}

/// A file's values in its library's columns.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ItemFields {
    pub values: serde_json::Map<String, Value>,
    pub web_url: String,
    pub modified_by: String,
    pub created_by: String,
}

fn parse_column(v: &Value) -> Option<Column> {
    let name = v["name"].as_str()?.to_string();
    let has = |facet: &str| v.get(facet).is_some_and(|f| !f.is_null());
    let (kind, multi) = if has("choice") {
        (ColumnKind::Choice, v["choice"]["displayAs"].as_str() == Some("checkBoxes"))
    } else if has("text") {
        (if v["text"]["allowMultipleLines"].as_bool() == Some(true) { ColumnKind::Note } else { ColumnKind::Text }, false)
    } else if has("number") || has("currency") {
        (ColumnKind::Number, false)
    } else if has("boolean") {
        (ColumnKind::YesNo, false)
    } else if has("dateTime") {
        (ColumnKind::Date, false)
    } else {
        (ColumnKind::Other, false)
    };
    Some(Column {
        title: v["displayName"].as_str().unwrap_or(&name).to_string(),
        kind,
        // A calculated column has a kind of its own but cannot be set.
        read_only: v["readOnly"].as_bool().unwrap_or(false) || has("calculated"),
        required: v["required"].as_bool().unwrap_or(false),
        hidden: v["hidden"].as_bool().unwrap_or(false),
        choices: v["choice"]["choices"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        multi,
        date_only: v["dateTime"]["format"].as_str() == Some("dateOnly"),
        description: v["description"].as_str().unwrap_or_default().to_string(),
        name,
    })
}

/// How the recycle bin is asked for: as many items at a time as the server gives.
/// Newest deletions come first, so what someone is most likely looking for arrives
/// in the first answer.
const BIN_PAGE: &str = "$top=1000&$orderby=deletedDateTime desc";

/// The site a library belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteRef {
    pub key: String,
    pub library_path: String,
}

/// Something in a site's recycle bin.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Binned {
    pub id: String,
    pub name: String,
    /// The folder it was deleted from, as a path on the server.
    pub from: String,
    pub size: u64,
    /// When it was deleted, as the server writes it (UTC).
    pub deleted_at: String,
    pub deleted_by: String,
}

impl Binned {
    /// Whether it was deleted from inside the library at `library_path`.
    pub fn is_from(&self, library_path: &str) -> bool {
        self.from == library_path
            || self.from.strip_prefix(library_path).is_some_and(|rest| rest.starts_with('/'))
    }
}

/// A file that is checked out in a library.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckedOut {
    pub item_id: String,
    pub by_id: String,
    pub by_name: String,
}

pub struct GraphClient<T: TokenSource> {
    http: reqwest::Client,
    tokens: T,
    base: String,
    /// No request is sent before this moment. Set when the server asks us to wait.
    not_before: Mutex<Instant>,
    /// Spaces requests out, and learns from the server how fast is too fast.
    pacer: crate::pace::Pacer,
    throttled: AtomicU64,
    throttle_wait_ms: AtomicU64,
    requests: AtomicU64,
    retries: AtomicU64,
    /// Told about throttling, retries and network errors as they happen.
    hook: Option<Box<dyn Fn(&str) + Send + Sync>>,
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
            http: crate::net::client_builder()
                .connect_timeout(Duration::from_secs(30))
                // A connection that goes silent is abandoned and retried, not waited on forever.
                .read_timeout(Duration::from_secs(120))
                .build()
                .expect("HTTP client"),
            tokens,
            base: base.trim_end_matches('/').to_string(),
            not_before: Mutex::new(Instant::now()),
            pacer: crate::pace::Pacer::new(Instant::now()),
            throttled: AtomicU64::new(0),
            throttle_wait_ms: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            hook: None,
            max_attempts: 8,
            simple_upload_limit: 4 * 1024 * 1024,
            chunk_size: 32 * 320 * 1024,
        }
    }

    /// How many times the server asked this client to slow down.
    /// Receive a line whenever the server throttles a request or one is retried.
    pub fn set_event_hook(&mut self, hook: impl Fn(&str) + Send + Sync + 'static) {
        self.hook = Some(Box::new(hook));
    }

    fn event(&self, text: String) {
        if let Some(h) = &self.hook {
            h(&text);
        }
    }

    pub fn stats(&self) -> NetStats {
        NetStats {
            requests: self.requests.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            throttled: self.throttled.load(Ordering::Relaxed),
            throttle_wait_ms: self.throttle_wait_ms.load(Ordering::Relaxed),
            pace: self.pacer.rate(),
            pace_cuts: self.pacer.cuts(),
        }
    }

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

    /// Hold every request back for `delay`. Several requests in flight are often
    /// throttled together; only the time this actually adds to the wait is counted.
    fn hold_off(&self, delay: Duration) {
        let now = Instant::now();
        let until = now + delay;
        let mut gate = self.not_before.lock().unwrap();
        let already = (*gate).max(now);
        if until > already {
            self.throttle_wait_ms
                .fetch_add((until - already).as_millis() as u64, Ordering::Relaxed);
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
            let slot = self.pacer.reserve(Instant::now());
            if !slot.is_zero() {
                tokio::time::sleep(slot).await;
            }
            let mut req = make(&self.http).header(reqwest::header::USER_AGENT, USER_AGENT);
            if authed {
                let token = self.tokens.token(false).await.map_err(RemoteError::Unauthorized)?;
                req = req.bearer_auth(token);
            }
            attempt += 1;
            let (client, request) = req.build_split();
            let request = request.map_err(RemoteError::other)?;
            // What is being asked, for the log: the method and the path only. Query
            // strings can carry access tokens for downloads and are never recorded.
            let what = {
                let path: String = request.url().path().chars().take(90).collect();
                format!("{} {}", request.method(), path)
            };
            self.requests.fetch_add(1, Ordering::Relaxed);
            let resp = match client.execute(request).await {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= self.max_attempts {
                        self.event(format!("network: {what} failed after {attempt} attempts: {e}"));
                        return Err(RemoteError::Other(format!("network error: {e}")));
                    }
                    self.retries.fetch_add(1, Ordering::Relaxed);
                    let wait = Self::backoff(attempt);
                    self.event(format!(
                        "network: {what} failed ({e}); retrying in {:.1}s (attempt {attempt})",
                        wait.as_secs_f64()
                    ));
                    tokio::time::sleep(wait).await;
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
                    self.event(format!(
                        "throttled: the server answered {} to {what}; waiting {:.0}s as asked (attempt {attempt})",
                        status.as_u16(),
                        delay.as_secs_f64()
                    ));
                    self.hold_off(delay);
                    if let Some(pace) = self.pacer.slow_down(Instant::now(), delay) {
                        self.event(format!(
                            "pacing: slowing to {pace:.1} requests a second, and easing back in after the wait"
                        ));
                    }
                }
                500 | 502 | 504 => {
                    if attempt >= self.max_attempts {
                        return Err(error_from(resp).await);
                    }
                    self.retries.fetch_add(1, Ordering::Relaxed);
                    let wait = Self::backoff(attempt);
                    self.event(format!(
                        "network: the server answered {} to {what}; retrying in {:.1}s (attempt {attempt})",
                        status.as_u16(),
                        wait.as_secs_f64()
                    ));
                    tokio::time::sleep(wait).await;
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

    /// The document library a Sync link points at.
    pub async fn library_of_link(&self, link: &crate::synclink::SyncLink) -> RemoteResult<DriveInfo> {
        let site = format!("{},{},{}", link.host(), link.site_id, link.web_id);
        let v = self.get_json(self.url(&["sites", &site, "lists", &link.list_id, "drive"])).await?;
        drive_info(&v).ok_or_else(|| RemoteError::Other("unexpected library description".into()))
    }

    // ---- a file's columns (library metadata) --------------------------------

    /// The columns a library defines, in the library's own order.
    pub async fn library_columns(&self, drive_id: &str) -> RemoteResult<Vec<Column>> {
        let mut url = self.url(&["drives", drive_id, "list", "columns"]);
        let mut out = Vec::new();
        loop {
            let v = self.get_json(url).await?;
            out.extend(v["value"].as_array().into_iter().flatten().filter_map(parse_column));
            match v["@odata.nextLink"].as_str() {
                Some(next) => url = self.trusted_link(next)?,
                None => return Ok(out),
            }
        }
    }

    /// The values a file has in its library's columns, by column name, with a few
    /// facts about the file itself.
    pub async fn item_fields(&self, drive_id: &str, item_id: &str) -> RemoteResult<ItemFields> {
        let mut u = self.url(&["drives", drive_id, "items", item_id, "listItem"]);
        u.set_query(Some("$expand=fields"));
        let v = self.get_json(u).await?;
        Ok(ItemFields {
            values: v["fields"].as_object().cloned().unwrap_or_default(),
            web_url: v["webUrl"].as_str().unwrap_or_default().to_string(),
            modified_by: v["lastModifiedBy"]["user"]["displayName"].as_str().unwrap_or_default().to_string(),
            created_by: v["createdBy"]["user"]["displayName"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Change some of a file's column values. `changes` maps column names to values.
    pub async fn set_item_fields(&self, drive_id: &str, item_id: &str, changes: &Value) -> RemoteResult<()> {
        let u = self.url(&["drives", drive_id, "items", item_id, "listItem", "fields"]);
        self.send(true, |c| c.patch(u.clone()).json(changes)).await.map(|_| ())
    }

    // ---- the recycle bin ---------------------------------------------------
    //
    // Microsoft offers the recycle bin only through the preview ("beta") edition of
    // its interface, which it may change. Everything that depends on it is here.

    fn preview_url(&self, segments: &[&str]) -> reqwest::Url {
        let base = match self.base.strip_suffix("/v1.0") {
            Some(root) => format!("{root}/beta"),
            None => self.base.clone(),
        };
        let mut u = reqwest::Url::parse(&base).expect("base address");
        u.path_segments_mut().expect("base address").pop_if_empty().extend(segments);
        u
    }

    /// The site a library belongs to: the key the server knows it by, and the
    /// library's own path on the server (no leading slash).
    pub async fn site_of(&self, drive_id: &str) -> RemoteResult<SiteRef> {
        let mut u = self.url(&["drives", drive_id]);
        u.set_query(Some("$select=id,webUrl,sharePointIds"));
        let v = self.get_json(u).await?;
        let ids = &v["sharePointIds"];
        let host = ids["siteUrl"]
            .as_str()
            .and_then(|s| reqwest::Url::parse(s).ok())
            .and_then(|s| s.host_str().map(String::from));
        match (host, ids["siteId"].as_str(), ids["webId"].as_str(), v["webUrl"].as_str()) {
            (Some(host), Some(site), Some(web), Some(web_url)) => Ok(SiteRef {
                key: format!("{host},{site},{web}"),
                library_path: reqwest::Url::parse(web_url)
                    .map(|u| paths::percent_decode(u.path()).trim_matches('/').to_string())
                    .unwrap_or_default(),
            }),
            _ => Err(RemoteError::Other("this library does not say which site it belongs to".into())),
        }
    }

    /// One page of a site's recycle bin, and where the next page starts. Pass `None`
    /// for the first page, then what the previous call returned.
    pub async fn recycle_bin_page(&self, site: &SiteRef, next: Option<&str>) -> RemoteResult<(Vec<Binned>, Option<String>)> {
        self.recycle_bin_query(site, next, BIN_PAGE).await
    }

    async fn recycle_bin_query(&self, site: &SiteRef, next: Option<&str>, query: &str) -> RemoteResult<(Vec<Binned>, Option<String>)> {
        let url = match next {
            Some(link) => self.trusted_link(link)?,
            None => {
                let mut u = self.preview_url(&["sites", &site.key, "recycleBin", "items"]);
                u.set_query(Some(query));
                u
            }
        };
        let v = self.get_json(url).await?;
        let mut out = Vec::new();
        for it in v["value"].as_array().into_iter().flatten() {
            let Some(id) = it["id"].as_str() else { continue };
            out.push(Binned {
                id: id.to_string(),
                name: it["name"].as_str().unwrap_or_default().to_string(),
                from: it["deletedFromLocation"].as_str().unwrap_or_default().trim_matches('/').to_string(),
                size: it["size"].as_u64().unwrap_or(0),
                deleted_at: it["deletedDateTime"].as_str().unwrap_or_default().to_string(),
                deleted_by: it["deletedBy"]["user"]["displayName"].as_str().unwrap_or_default().to_string(),
            });
        }
        Ok((out, v["@odata.nextLink"].as_str().map(String::from)))
    }

    /// What is in a site's recycle bin, newest deletions first. At most `limit` items.
    pub async fn recycle_bin(&self, site: &SiteRef, limit: usize) -> RemoteResult<Vec<Binned>> {
        let mut out: Vec<Binned> = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let (mut page, more) = self.recycle_bin_page(site, next.as_deref()).await?;
            out.append(&mut page);
            match more {
                Some(link) if out.len() < limit => next = Some(link),
                _ => break,
            }
        }
        out.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
        out.truncate(limit);
        Ok(out)
    }

    /// Put items from the recycle bin back where they were deleted from.
    pub async fn restore_binned(&self, site: &SiteRef, ids: &[String]) -> RemoteResult<()> {
        let u = self.preview_url(&["sites", &site.key, "recycleBin", "items", "restore"]);
        let body = json!({ "ids": ids });
        self.send(true, |c| c.post(u.clone()).json(&body)).await.map(|_| ())
    }

    /// The signed-in user's identifier, as the server uses it for "who".
    pub async fn me_id(&self) -> RemoteResult<String> {
        let mut u = self.url(&["me"]);
        u.set_query(Some("$select=id"));
        let v = self.get_json(u).await?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    /// Every file in the library that is checked out, and who holds it.
    pub async fn checked_out(&self, drive_id: &str) -> RemoteResult<Vec<CheckedOut>> {
        let mut url = self.url(&["drives", drive_id, "list", "items"]);
        url.set_query(Some(
            "$expand=fields($select=CheckoutUserLookupId),driveItem($select=id,publication)             &$filter=fields/CheckoutUserLookupId ne null&$top=200",
        ));
        let mut out = Vec::new();
        loop {
            // The check-out column is not indexed; the server wants to hear that this
            // is understood before it will filter on it.
            let resp = self
                .send(true, |c| c.get(url.clone()).header("Prefer", "HonorNonIndexedQueriesWarningMayFailRandomly"))
                .await?;
            let v: Value = resp.json().await.map_err(RemoteError::other)?;
            for it in v["value"].as_array().into_iter().flatten() {
                let Some(id) = it["driveItem"]["id"].as_str() else { continue };
                let who = &it["driveItem"]["publication"]["checkedOutBy"]["user"];
                out.push(CheckedOut {
                    item_id: id.to_string(),
                    by_id: who["id"].as_str().unwrap_or_default().to_string(),
                    by_name: who["displayName"].as_str().unwrap_or_default().to_string(),
                });
            }
            match v["@odata.nextLink"].as_str() {
                Some(next) => url = self.trusted_link(next)?,
                None => return Ok(out),
            }
        }
    }

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

    // ---- several reads in one request -------------------------------------------

    /// Send up to twenty reads as one request (Microsoft's JSON batching) and return
    /// each answer by its key. The server judges each read on its own, so one that it
    /// asks to slow down is waited for and sent again, as a single request would be.
    async fn batch_get(&self, asks: Vec<(String, reqwest::Url)>) -> RemoteResult<Vec<(String, RemoteResult<Value>)>> {
        let base_path = reqwest::Url::parse(&self.base)
            .map(|u| u.path().trim_end_matches('/').to_string())
            .unwrap_or_default();
        let mut pending: Vec<(String, reqwest::Url)> = asks;
        let mut done: Vec<(String, RemoteResult<Value>)> = Vec::new();
        let mut attempt = 0u32;
        while !pending.is_empty() {
            attempt += 1;
            let requests: Vec<Value> = pending
                .iter()
                .enumerate()
                .map(|(i, (_, u))| {
                    let rel = u.path().strip_prefix(&base_path).unwrap_or(u.path()).to_string();
                    let rel = match u.query() {
                        Some(q) => format!("{rel}?{q}"),
                        None => rel,
                    };
                    json!({ "id": i.to_string(), "method": "GET", "url": rel })
                })
                .collect();
            let body = json!({ "requests": requests });
            let url = self.url(&["$batch"]);
            let resp = self.send(true, |c| c.post(url.clone()).json(&body)).await?;
            let v: Value = resp.json().await.map_err(RemoteError::other)?;
            let mut again = Vec::new();
            let mut wait: Option<Duration> = None;
            let answers = v["responses"].as_array().cloned().unwrap_or_default();
            for (i, ask) in pending.into_iter().enumerate() {
                let answer = answers.iter().find(|a| a["id"].as_str() == Some(i.to_string().as_str()));
                let Some(a) = answer else {
                    done.push((ask.0, Err(RemoteError::Other("the server did not answer this part of a batch".into()))));
                    continue;
                };
                let status = a["status"].as_u64().unwrap_or(0) as u16;
                match status {
                    200..=299 => done.push((ask.0, Ok(a["body"].clone()))),
                    429 | 503 | 509 | 500 | 502 | 504 if attempt < self.max_attempts => {
                        let after = a["headers"]
                            .as_object()
                            .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("retry-after")))
                            .and_then(|(_, v)| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()).or(v.as_u64()))
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| Self::backoff(attempt));
                        wait = Some(wait.map_or(after, |w| w.max(after)));
                        again.push(ask);
                    }
                    _ => done.push((ask.0, Err(error_for(status, &a["body"].to_string())))),
                }
            }
            if let Some(delay) = wait {
                self.throttled.fetch_add(1, Ordering::Relaxed);
                self.event(format!(
                    "throttled: the server asked part of a batch to wait {:.0}s (attempt {attempt})",
                    delay.as_secs_f64()
                ));
                self.hold_off(delay);
                let _ = self.pacer.slow_down(Instant::now(), delay);
            }
            pending = again;
        }
        Ok(done)
    }

    // ---- finding out what the server says about permissions -------------------

    /// A few read-only questions about permissions, answered in plain words: whether
    /// the change feed says which items have permissions of their own, whether item
    /// permissions can be listed, whether group memberships can be read, and what an
    /// item that cannot be had looks like. Nothing is changed anywhere.
    pub async fn probe_permissions(&self, drive_id: &str, refused_id: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        let mut say = |s: String| out.push(s);

        match self.identity().await {
            Ok(me) => {
                say(format!("Signed in as {} (identifier {}).", me.name, me.user_id));
                match me.groups {
                    Some(g) => say(format!(
                        "Group memberships CAN be read with the current sign-in: {} group(s).",
                        g.len()
                    )),
                    None => say("Group memberships could NOT be read with the current sign-in; DriveCraft will ask the server about folders given to groups.".into()),
                }
            }
            Err(e) => say(format!("Could not ask who is signed in: {e}")),
        }

        // The change feed, first pages only.
        let mut link: Option<String> = None;
        let (mut seen, mut with_facet, mut examples) = (0usize, 0usize, Vec::new());
        for _ in 0..3 {
            match self.delta(drive_id, link.as_deref()).await {
                Ok(page) => {
                    seen += page.items.len();
                    for it in page.items.iter().filter(|i| i.own_permissions) {
                        with_facet += 1;
                        if examples.len() < 3 {
                            examples.push(it.id.clone());
                        }
                    }
                    match page.next_link {
                        Some(n) => link = Some(n),
                        None => break,
                    }
                }
                Err(e) => {
                    say(format!("The change feed could not be read: {e}"));
                    break;
                }
            }
        }
        if with_facet > 0 {
            say(format!(
                "The change feed DOES mark items with permissions of their own: {with_facet} of the first {seen} items carry the sharing mark."
            ));
        } else {
            say(format!(
                "None of the first {seen} items in the change feed carried the sharing mark. Either none of them has permissions of its own, or the server does not send the mark with this sign-in."
            ));
        }

        // The library's own permissions.
        let mut ask = vec!["root".to_string()];
        ask.extend(examples.iter().cloned());
        match self.permissions(drive_id, &ask).await {
            Ok(answers) => {
                for (id, answer) in answers {
                    let what = if id == "root" { "the library's top folder".to_string() } else { format!("item {id}") };
                    match answer {
                        Ok(perms) => {
                            let owner = perms.iter().any(|p| p.roles.iter().any(|r| r == "owner"));
                            let mut kinds: Vec<&str> = perms.iter().map(|p| p.kind.said()).collect();
                            kinds.sort();
                            kinds.dedup();
                            say(format!(
                                "Permissions of {what} CAN be listed: {} entries ({}){}.",
                                perms.len(),
                                kinds.join(", "),
                                if owner { ", including owners, so this sign-in sees the full list" } else { "; no owner entry, so this sign-in probably sees only the entries that apply to it" }
                            ));
                        }
                        Err(e) => say(format!("Permissions of {what} could NOT be listed: {e}")),
                    }
                }
            }
            Err(e) => say(format!("Permissions could not be asked for: {e}")),
        }

        // What refusal looks like.
        let probe_id = refused_id.unwrap_or("DriveCraftProbeNoSuchItem");
        match self.item(drive_id, probe_id).await {
            Ok(_) => say(format!("Item {probe_id} could be opened.")),
            Err(e) => {
                let shape = match &e {
                    RemoteError::NotFound(_) => "\"not found\" (404)",
                    RemoteError::Forbidden(_) => "\"access denied\" (403)",
                    _ => "another error",
                };
                let which = if refused_id.is_some() { "An item this computer was refused" } else { "An item that does not exist" };
                say(format!("{which} is answered with {shape}: {e}"));
            }
        }
        out
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
            .send(true, |c| c.get(url.clone()).header("Prefer", DELTA_PREFER))
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

    async fn child(&self, drive_id: &str, parent_id: &str, name: &str) -> RemoteResult<Option<RemoteItem>> {
        let url = self.url(&["drives", drive_id, "items", &format!("{parent_id}:"), &format!("{name}:")]);
        match self.get_json(url).await {
            Ok(v) => Ok(parse_item(&v)),
            Err(RemoteError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn permissions(
        &self,
        drive_id: &str,
        item_ids: &[String],
    ) -> RemoteResult<Vec<(String, RemoteResult<Vec<Permission>>)>> {
        let mut out = Vec::new();
        for chunk in item_ids.chunks(BATCH_LIMIT) {
            let asks: Vec<(String, reqwest::Url)> = chunk
                .iter()
                .map(|id| (id.clone(), self.url(&["drives", drive_id, "items", id, "permissions"])))
                .collect();
            for (id, answer) in self.batch_get(asks).await? {
                out.push((id, answer.map(|v| parse_permissions(&v))));
            }
        }
        Ok(out)
    }

    async fn identity(&self) -> RemoteResult<Identity> {
        let mut u = self.url(&["me"]);
        u.set_query(Some("$select=id,displayName"));
        let me = self.get_json(u).await?;
        // Group memberships need only the sign-in DriveCraft already has (User.Read).
        // Without a directory permission the groups come back with little more than
        // their identifier, which is all that is needed here.
        let mut groups = Vec::new();
        let mut next = Some(self.url(&["me", "transitiveMemberOf"]));
        let mut known = true;
        while let Some(url) = next {
            match self.get_json(url).await {
                Ok(v) => {
                    for g in v["value"].as_array().into_iter().flatten() {
                        if g["@odata.type"].as_str() == Some("#microsoft.graph.group") {
                            if let Some(id) = g["id"].as_str() {
                                groups.push(id.to_string());
                            }
                        }
                    }
                    next = match v["@odata.nextLink"].as_str() {
                        Some(link) => Some(self.trusted_link(link)?),
                        None => None,
                    };
                }
                Err(RemoteError::Unauthorized(e)) => return Err(RemoteError::Unauthorized(e)),
                Err(_) => {
                    known = false;
                    next = None;
                }
            }
        }
        Ok(Identity {
            user_id: me["id"].as_str().unwrap_or_default().to_string(),
            name: me["displayName"].as_str().unwrap_or_default().to_string(),
            groups: known.then_some(groups),
        })
    }

    async fn item(&self, drive_id: &str, item_id: &str) -> RemoteResult<RemoteItem> {
        let v = self.get_json(self.url(&["drives", drive_id, "items", item_id])).await?;
        parse_item(&v).ok_or_else(|| RemoteError::Other("unexpected item description".into()))
    }

    async fn children(&self, drive_id: &str, parent_id: &str) -> RemoteResult<Vec<RemoteItem>> {
        let mut out = Vec::new();
        let mut next = Some(self.url(&["drives", drive_id, "items", parent_id, "children"]));
        while let Some(url) = next {
            let v = self.get_json(url).await?;
            if let Some(items) = v["value"].as_array() {
                out.extend(items.iter().filter_map(parse_item));
            }
            next = match v["@odata.nextLink"].as_str() {
                Some(link) => Some(self.trusted_link(link)?),
                None => None,
            };
        }
        Ok(out)
    }
}

async fn error_from(resp: reqwest::Response) -> RemoteError {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    error_for(status, &text)
}

fn error_for(status: u16, text: &str) -> RemoteError {
    let (code, message) = serde_json::from_str::<Value>(text)
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
        403 => RemoteError::Forbidden(format!("({code}) {detail}")),
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

/// Microsoft Graph takes at most this many reads in one batch.
const BATCH_LIMIT: usize = 20;

/// The person, group or link a permission names, from one of the server's
/// "identity sets".
fn principal(set: &Value) -> (PrincipalKind, Option<String>, String, Option<String>, Option<String>) {
    let s = |v: &Value, k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(String::from);
    let site_login = s(&set["siteUser"], "loginName");
    let user = &set["user"];
    if user.is_object() && s(user, "id").is_some() {
        return (
            PrincipalKind::User,
            s(user, "id"),
            s(user, "displayName").unwrap_or_default(),
            s(user, "email").or_else(|| s(&set["siteUser"], "email")),
            site_login,
        );
    }
    let group = &set["group"];
    if group.is_object() && s(group, "id").is_some() {
        return (PrincipalKind::Group, s(group, "id"), s(group, "displayName").unwrap_or_default(), s(group, "email"), None);
    }
    let sg = &set["siteGroup"];
    if sg.is_object() {
        return (PrincipalKind::SiteGroup, s(sg, "id"), s(sg, "displayName").unwrap_or_default(), None, s(sg, "loginName"));
    }
    let su = &set["siteUser"];
    if su.is_object() {
        return (PrincipalKind::SiteUser, s(su, "id"), s(su, "displayName").unwrap_or_default(), s(su, "email"), site_login);
    }
    let app = &set["application"];
    if app.is_object() {
        return (PrincipalKind::Application, s(app, "id"), s(app, "displayName").unwrap_or_default(), None, None);
    }
    (PrincipalKind::Unknown, None, String::new(), None, None)
}

/// Read an item's permissions from the server's JSON, one entry per person, group
/// or link. A link given to named people becomes one entry for the link and one for
/// each person.
pub fn parse_permissions(v: &Value) -> Vec<Permission> {
    let mut out = Vec::new();
    for p in v["value"].as_array().into_iter().flatten() {
        let perm_id = p["id"].as_str().unwrap_or_default().to_string();
        let roles: Vec<String> = p["roles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(String::from))
            .collect();
        let entry = |set: &Value, via_link: bool, scope: Option<String>| {
            let (kind, principal_id, mut name, email, login) = principal(set);
            if name.is_empty() {
                name = email.clone().or_else(|| login.clone()).unwrap_or_else(|| "(no name given)".into());
            }
            Permission { perm_id: perm_id.clone(), kind, principal_id, name, email, login, roles: roles.clone(), link_scope: scope, via_link }
        };
        if p["link"].is_object() {
            let link = &p["link"];
            let scope = link["scope"].as_str().unwrap_or("unknown").to_string();
            let kind = link["type"].as_str().unwrap_or("view");
            out.push(Permission {
                perm_id: perm_id.clone(),
                kind: PrincipalKind::Link,
                principal_id: None,
                name: format!("{kind} link ({scope})"),
                email: None,
                login: None,
                roles: roles.clone(),
                link_scope: Some(scope.clone()),
                via_link: false,
            });
            let people = p["grantedToIdentitiesV2"].as_array().or_else(|| p["grantedToIdentities"].as_array());
            for set in people.into_iter().flatten() {
                out.push(entry(set, true, Some(scope.clone())));
            }
        } else if p["grantedToV2"].is_object() || p["grantedTo"].is_object() {
            let set = if p["grantedToV2"].is_object() { &p["grantedToV2"] } else { &p["grantedTo"] };
            out.push(entry(set, false, None));
        } else if let Some(email) = p["invitation"]["email"].as_str() {
            // Invited by address, not yet accepted: nobody known can use it yet.
            let mut e = entry(&json!({}), false, None);
            e.name = email.to_string();
            e.email = Some(email.to_string());
            out.push(e);
        }
    }
    out
}

/// Read one drive item from the server's JSON.
pub fn parse_item(v: &Value) -> Option<RemoteItem> {
    let id = v["id"].as_str()?.to_string();
    let deleted = v.get("deleted").is_some() || v.get("@removed").is_some();
    let is_root = v.get("root").is_some();
    let kind = if v.get("package").is_some() || v.get("malware").is_some() {
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
        // With `hierarchicalsharing` asked for, the server puts a sharing facet only on
        // an item where a permission hierarchy starts: one that does not simply take
        // its folder's permissions. The facet is there, possibly empty, even when the
        // item is not shared with anyone. The library's top folder always starts one.
        own_permissions: !is_root && v.get("shared").is_some_and(|s| !s.is_null()),
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

        let infected = parse_item(&json!({ "id": "M", "name": "bad.pdf", "file": {}, "malware": { "description": "x" } })).unwrap();
        assert_eq!(infected.kind, ItemKind::Other, "flagged files are skipped, not downloaded");

        let notebook = parse_item(&json!({ "id": "N", "name": "Notes", "package": { "type": "oneNote" }, "folder": {} })).unwrap();
        assert_eq!(notebook.kind, ItemKind::Other);
    }

    #[test]
    fn a_sharing_facet_marks_permissions_of_its_own() {
        let restricted = parse_item(&json!({ "id": "H", "name": "HR", "folder": {}, "shared": {} })).unwrap();
        assert!(restricted.own_permissions, "an empty facet still starts a permission hierarchy");
        let shared = parse_item(&json!({ "id": "S", "name": "x.docx", "file": {}, "shared": { "scope": "users" } })).unwrap();
        assert!(shared.own_permissions);
        let plain = parse_item(&json!({ "id": "P", "name": "y.docx", "file": {} })).unwrap();
        assert!(!plain.own_permissions);
        let root = parse_item(&json!({ "id": "R", "name": "root", "root": {}, "folder": {}, "shared": {} })).unwrap();
        assert!(!root.own_permissions, "the top folder is where everything starts; it is never left out");
    }
}
