//! Signing in to Microsoft 365 with a work or school account.
//!
//! DriveCraft is a public client: it holds no secret of its own. It signs in as the
//! user (delegated access), so it can reach only what that user can already reach.
//! The sign-in runs in the system browser, which on a machine already signed in with
//! a work account normally completes without asking for a password.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::future::Future;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Work and school accounts only; consumer Microsoft accounts are out of scope.
pub const DEFAULT_TENANT: &str = "organizations";

const GRAPH: &str = "https://graph.microsoft.com/";

/// Permissions for normal use: read and write the user's files.
pub fn scopes_read_write() -> Vec<String> {
    scopes(&["Files.ReadWrite.All", "Sites.Read.All", "User.Read"])
}

/// Permissions that cannot change anything on the server. Enough for Archive mode.
pub fn scopes_read_only() -> Vec<String> {
    scopes(&["Files.Read.All", "Sites.Read.All", "User.Read"])
}

fn scopes(names: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = names.iter().map(|n| format!("{GRAPH}{n}")).collect();
    v.push("offline_access".into());
    v
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppConfig {
    /// The Application (client) ID of the DriveCraft app registration.
    pub client_id: String,
    /// A tenant ID or domain, or `organizations` for any work or school account.
    pub tenant: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    pub scope: String,
}

impl TokenSet {
    pub fn can_write(&self) -> bool {
        self.scope.to_ascii_lowercase().contains("readwrite")
    }
}

/// Something that can hand out an access token for the file server.
pub trait TokenSource: Send + Sync {
    fn token(&self, force_refresh: bool) -> impl Future<Output = Result<String, String>> + Send;
}

/// A fixed token, for tests.
pub struct StaticToken(pub String);

impl TokenSource for StaticToken {
    async fn token(&self, _force_refresh: bool) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: i64,
    interval: Option<u64>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_token(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("no secure random numbers available: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

pub struct Authenticator {
    app: AppConfig,
    http: reqwest::Client,
    store: PathBuf,
    authority: String,
    cached: tokio::sync::Mutex<Option<TokenSet>>,
}

impl Authenticator {
    pub fn new(app: AppConfig, store: PathBuf) -> Result<Self> {
        if app.client_id.trim().is_empty() {
            bail!("no application ID is configured");
        }
        Ok(Self {
            authority: format!("https://login.microsoftonline.com/{}/oauth2/v2.0", app.tenant),
            app,
            http: crate::net::client_builder().build()?,
            store,
            cached: tokio::sync::Mutex::new(None),
        })
    }

    /// Point at a different sign-in service. Used by tests.
    pub fn with_authority(mut self, authority: impl Into<String>) -> Self {
        self.authority = authority.into();
        self
    }

    /// Sign in through the system browser (authorization code with PKCE).
    pub async fn login_browser(&self, scopes: &[String], announce: impl Fn(&str)) -> Result<TokenSet> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("opening a local port for the sign-in to return to")?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://localhost:{port}");
        let verifier = random_token(48)?;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        let state = random_token(16)?;
        let scope = scopes.join(" ");

        let mut url = url::Url::parse(&format!("{}/authorize", self.authority))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.app.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_mode", "query")
            .append_pair("scope", &scope)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("prompt", "select_account");

        announce("Opening your browser to sign in. If it does not open, visit:");
        announce(url.as_str());
        let _ = open::that(url.as_str());

        let code = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            wait_for_code(&listener, &state),
        )
        .await
        .map_err(|_| anyhow!("timed out waiting for the browser sign-in"))??;

        let tokens = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("client_id", &self.app.client_id),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("code_verifier", &verifier),
                ("scope", &scope),
            ])
            .await?
            .map_err(|(e, d)| anyhow!("sign-in failed: {e}: {d}"))?;
        self.remember(tokens.clone()).await?;
        Ok(tokens)
    }

    /// Sign in by entering a short code on another device or browser window.
    pub async fn login_device_code(&self, scopes: &[String], announce: impl Fn(&str)) -> Result<TokenSet> {
        let scope = scopes.join(" ");
        let resp = self
            .http
            .post(format!("{}/devicecode", self.authority))
            .form(&[("client_id", self.app.client_id.as_str()), ("scope", scope.as_str())])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("could not start sign-in: {}", resp.text().await.unwrap_or_default());
        }
        let dc: DeviceCodeResponse = resp.json().await?;
        announce(&format!(
            "To sign in, open {} and enter the code {}",
            dc.verification_uri, dc.user_code
        ));
        let deadline = now() + dc.expires_in;
        let mut interval = dc.interval.unwrap_or(5).max(1);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            if now() > deadline {
                bail!("the sign-in code expired before it was used");
            }
            let r = self
                .token_request(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", &self.app.client_id),
                    ("device_code", &dc.device_code),
                ])
                .await?;
            match r {
                Ok(tokens) => {
                    self.remember(tokens.clone()).await?;
                    return Ok(tokens);
                }
                Err((e, _)) if e == "authorization_pending" => {}
                Err((e, _)) if e == "slow_down" => interval += 5,
                Err((e, d)) => bail!("sign-in failed: {e}: {d}"),
            }
        }
    }

    async fn token_request(
        &self,
        form: &[(&str, &str)],
    ) -> Result<std::result::Result<TokenSet, (String, String)>> {
        let resp = self
            .http
            .post(format!("{}/token", self.authority))
            .form(form)
            .send()
            .await
            .context("contacting the sign-in service")?;
        let body: TokenResponse = resp.json().await.context("reading the sign-in response")?;
        if let Some(access_token) = body.access_token {
            return Ok(Ok(TokenSet {
                access_token,
                refresh_token: body.refresh_token,
                expires_at: now() + body.expires_in.unwrap_or(3600),
                scope: body.scope.unwrap_or_default(),
            }));
        }
        Ok(Err((
            body.error.unwrap_or_else(|| "unknown_error".into()),
            body.error_description.unwrap_or_default(),
        )))
    }

    async fn remember(&self, tokens: TokenSet) -> Result<()> {
        save_tokens(&self.store, &tokens)?;
        *self.cached.lock().await = Some(tokens);
        Ok(())
    }

    /// The saved sign-in, if there is one.
    pub async fn current(&self) -> Option<TokenSet> {
        let mut guard = self.cached.lock().await;
        if guard.is_none() {
            *guard = load_tokens(&self.store);
        }
        guard.clone()
    }

    pub fn logout(&self) -> Result<()> {
        if self.store.exists() {
            std::fs::remove_file(&self.store)?;
        }
        Ok(())
    }

    async fn access_token(&self, force_refresh: bool) -> Result<String> {
        let mut guard = self.cached.lock().await;
        if guard.is_none() {
            *guard = load_tokens(&self.store);
        }
        let Some(cur) = guard.clone() else {
            bail!("not signed in. Run: drivecraft login");
        };
        if !force_refresh && cur.expires_at - 120 > now() {
            return Ok(cur.access_token);
        }
        let Some(refresh) = cur.refresh_token.clone() else {
            bail!("the sign-in has expired. Run: drivecraft login");
        };
        let r = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &self.app.client_id),
                ("refresh_token", &refresh),
            ])
            .await?;
        match r {
            Ok(mut fresh) => {
                if fresh.refresh_token.is_none() {
                    fresh.refresh_token = Some(refresh);
                }
                if fresh.scope.is_empty() {
                    fresh.scope = cur.scope;
                }
                save_tokens(&self.store, &fresh)?;
                let token = fresh.access_token.clone();
                *guard = Some(fresh);
                Ok(token)
            }
            Err((e, d)) => bail!("the sign-in could not be renewed ({e}). Run: drivecraft login\n{d}"),
        }
    }
}

impl TokenSource for Authenticator {
    async fn token(&self, force_refresh: bool) -> Result<String, String> {
        self.access_token(force_refresh).await.map_err(|e| format!("{e:#}"))
    }
}

/// Wait for the browser to come back to the local port with an authorization code.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let parsed = url::Url::parse(&format!("http://localhost{target}"));
        let params: std::collections::HashMap<String, String> = parsed
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default();

        let (status, message, outcome) = if let Some(err) = params.get("error") {
            let desc = params.get("error_description").cloned().unwrap_or_default();
            (
                "400 Bad Request",
                "Sign-in did not complete. You can close this window and check DriveCraft.",
                Some(Err(anyhow!("sign-in was refused: {err}: {desc}"))),
            )
        } else if let Some(code) = params.get("code") {
            if params.get("state").map(|s| s.as_str()) == Some(state) {
                (
                    "200 OK",
                    "DriveCraft is signed in. You can close this window.",
                    Some(Ok(code.clone())),
                )
            } else {
                (
                    "400 Bad Request",
                    "This sign-in response did not match the request. Nothing was signed in.",
                    Some(Err(anyhow!("the sign-in response did not match the request"))),
                )
            }
        } else {
            ("404 Not Found", "", None)
        };
        let body = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>DriveCraft</title></head>\
             <body style=\"font-family:sans-serif;margin:3rem\"><h1>DriveCraft</h1><p>{message}</p></body></html>"
        );
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(reply.as_bytes()).await;
        let _ = sock.shutdown().await;
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

// ---- keeping the sign-in between runs --------------------------------------
//
// On Windows the saved sign-in is encrypted with the Windows account (DPAPI), so
// another user of the machine, or a copy of the file, cannot use it. Elsewhere it is
// a file readable only by its owner.

fn save_tokens(path: &Path, tokens: &TokenSet) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let plain = serde_json::to_vec(tokens)?;
    let data = protect(&plain)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn load_tokens(path: &Path) -> Option<TokenSet> {
    let data = std::fs::read(path).ok()?;
    let plain = unprotect(&data).ok()?;
    serde_json::from_slice(&plain).ok()
}

#[cfg(windows)]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, true)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, false)
}

#[cfg(windows)]
fn dpapi(data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: `input` points at `data`, which outlives the call. On success the
    // system allocates `output.pbData`, which is copied and then freed with LocalFree.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut output,
            )
        };
        if ok == 0 || output.pbData.is_null() {
            bail!("Windows could not {} the saved sign-in", if encrypt { "protect" } else { "read" });
        }
        let out = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as _);
        Ok(out)
    }
}

#[cfg(not(windows))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_sign_in_round_trips_and_is_not_plain_text_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signin.bin");
        let t = TokenSet {
            access_token: "ACCESS-TOKEN-VALUE".into(),
            refresh_token: Some("REFRESH-TOKEN-VALUE".into()),
            expires_at: 123,
            scope: "https://graph.microsoft.com/Files.ReadWrite.All".into(),
        };
        save_tokens(&path, &t).unwrap();
        let back = load_tokens(&path).unwrap();
        assert_eq!(back.refresh_token, t.refresh_token);
        assert!(back.can_write());
        if cfg!(windows) {
            let raw = std::fs::read(&path).unwrap();
            let needle = b"REFRESH-TOKEN-VALUE";
            assert!(!raw.windows(needle.len()).any(|w| w == needle));
        }
    }

    async fn browser_returns(query: &str, state: &str) -> Result<String> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request = format!("GET /{query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let browser = tokio::spawn(async move {
            // A stray request first, as browsers make for icons.
            for req in ["GET /favicon.ico HTTP/1.1\r\n\r\n".to_string(), request] {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                s.write_all(req.as_bytes()).await.unwrap();
                let mut page = String::new();
                let _ = s.read_to_string(&mut page).await;
            }
        });
        let out = wait_for_code(&listener, state).await;
        browser.await.unwrap();
        out
    }

    #[tokio::test]
    async fn browser_sign_in_accepts_only_a_matching_response() {
        assert_eq!(browser_returns("?code=THECODE&state=S1", "S1").await.unwrap(), "THECODE");
        assert!(browser_returns("?code=THECODE&state=OTHER", "S1").await.is_err());
        let refused = browser_returns("?error=access_denied&error_description=No", "S1").await;
        assert!(refused.unwrap_err().to_string().contains("access_denied"));
    }

    #[test]
    fn read_only_scopes_cannot_write() {
        assert!(scopes_read_only().iter().all(|s| !s.contains("ReadWrite")));
        assert!(scopes_read_write().iter().any(|s| s.contains("Files.ReadWrite.All")));
    }
}
