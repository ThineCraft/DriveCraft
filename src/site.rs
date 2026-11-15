//! The organization address a person types on the sign-in screen, such as
//! `contoso.sharepoint.com` or `contoso.sharecraft.io`.
//!
//! A SharePoint address tells DriveCraft which Microsoft 365 directory (tenant) to
//! sign in to, so nobody has to look the directory ID up. Any other address is taken
//! to be a ShareCraft site, hosted or self-hosted.

use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    SharePoint,
    ShareCraft,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Address {
    /// The host name alone, in lower case: `contoso.sharepoint.com`.
    pub host: String,
    pub service: Service,
}

/// Reads what the person typed. Accepts a bare host, a full link to any page on
/// the site, or just the organization's name (taken as a SharePoint name).
pub fn read(input: &str) -> Result<Address> {
    let mut s = input.trim();
    if s.is_empty() {
        bail!("type your organization's address, such as contoso.sharepoint.com");
    }
    for scheme in ["https://", "http://"] {
        if s.len() >= scheme.len() && s[..scheme.len()].eq_ignore_ascii_case(scheme) {
            s = &s[scheme.len()..];
        }
    }
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host); // drop any user@ part
    let host = host.split(':').next().unwrap_or(host); // drop any port
    let mut host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        || host.starts_with(['.', '-'])
        || host.contains("..")
    {
        bail!("\"{}\" is not an address DriveCraft can use; type one like contoso.sharepoint.com", input.trim());
    }
    if !host.contains('.') {
        host = format!("{host}.sharepoint.com");
    }
    if host.ends_with(".sharepoint.com") {
        return Ok(Address { host, service: Service::SharePoint });
    }
    if host.contains(".sharepoint.") {
        bail!(
            "the address {host} is in one of Microsoft's separate government or national clouds. \
             DriveCraft works with Microsoft 365's worldwide cloud, where addresses end in .sharepoint.com"
        );
    }
    Ok(Address { host, service: Service::ShareCraft })
}

/// The organization's name in its SharePoint address: `contoso` for
/// contoso.sharepoint.com, contoso-my.sharepoint.com and contoso-admin.sharepoint.com.
pub fn sharepoint_name(host: &str) -> &str {
    let name = host.strip_suffix(".sharepoint.com").unwrap_or(host);
    name.strip_suffix("-my").or_else(|| name.strip_suffix("-admin")).unwrap_or(name)
}

/// Finds the Microsoft 365 directory (tenant) ID behind a SharePoint address.
///
/// SharePoint answers a request carrying an empty sign-in with "who are you?", and
/// that answer names the directory. If the site cannot be reached that way, the
/// organization's original `name.onmicrosoft.com` domain is looked up instead.
pub async fn sharepoint_tenant(host: &str) -> Result<String> {
    let client = crate::net::client_builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let asked = client
        .get(format!("https://{host}/_vti_bin/client.svc"))
        .header("Authorization", "Bearer")
        .send()
        .await;
    if let Ok(resp) = &asked {
        for value in resp.headers().get_all("www-authenticate") {
            if let Some(id) = value.to_str().ok().and_then(realm) {
                return Ok(id);
            }
        }
    }
    let domain = format!("{}.onmicrosoft.com", sharepoint_name(host));
    let found = client
        .get(format!("https://login.microsoftonline.com/{domain}/v2.0/.well-known/openid-configuration"))
        .send()
        .await
        .with_context(|| format!("could not reach Microsoft to look up {host}"))?;
    if found.status().is_success() {
        let doc: serde_json::Value = found.json().await?;
        if let Some(id) = doc["issuer"].as_str().and_then(tenant_in_issuer) {
            return Ok(id);
        }
    }
    match asked {
        Err(e) if e.is_connect() || e.is_timeout() => {
            bail!("could not reach {host}. Check the address and your network connection")
        }
        _ => bail!("could not find a Microsoft 365 organization at {host}. Check the address"),
    }
}

/// The directory ID in a `WWW-Authenticate: Bearer realm="…"` answer.
fn realm(header: &str) -> Option<String> {
    let at = header.find("realm=\"")? + "realm=\"".len();
    let id = &header[at..at + header[at..].find('"')?];
    is_guid(id).then(|| id.to_ascii_lowercase())
}

/// The directory ID in an issuer such as `https://login.microsoftonline.com/<id>/v2.0`.
fn tenant_in_issuer(issuer: &str) -> Option<String> {
    issuer.split('/').find(|p| is_guid(p)).map(|p| p.to_ascii_lowercase())
}

fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(host: &str) -> Address {
        Address { host: host.into(), service: Service::SharePoint }
    }

    #[test]
    fn sharepoint_addresses_in_any_form() {
        assert_eq!(read("contoso.sharepoint.com").unwrap(), sp("contoso.sharepoint.com"));
        assert_eq!(read("  Contoso.SharePoint.com/ ").unwrap(), sp("contoso.sharepoint.com"));
        assert_eq!(read("https://contoso.sharepoint.com/sites/Finance/Shared%20Documents/Forms/AllItems.aspx").unwrap(), sp("contoso.sharepoint.com"));
        assert_eq!(read("contoso-my.sharepoint.com/personal/alex").unwrap(), sp("contoso-my.sharepoint.com"));
        assert_eq!(read("contoso").unwrap(), sp("contoso.sharepoint.com"));
    }

    #[test]
    fn other_addresses_are_sharecraft() {
        let a = read("https://northwind.sharecraft.io/libraries").unwrap();
        assert_eq!(a, Address { host: "northwind.sharecraft.io".into(), service: Service::ShareCraft });
        assert_eq!(read("files.northwind.example:8443").unwrap().service, Service::ShareCraft);
    }

    #[test]
    fn unusable_addresses_say_why() {
        assert!(read("").is_err());
        assert!(read("not an address").is_err());
        assert!(read("contoso..sharepoint.com").is_err());
        let gov = read("contoso.sharepoint.us").unwrap_err().to_string();
        assert!(gov.contains("worldwide"), "{gov}");
    }

    #[test]
    fn organization_name_from_any_sharepoint_host() {
        assert_eq!(sharepoint_name("contoso.sharepoint.com"), "contoso");
        assert_eq!(sharepoint_name("contoso-my.sharepoint.com"), "contoso");
        assert_eq!(sharepoint_name("contoso-admin.sharepoint.com"), "contoso");
    }

    #[test]
    fn directory_id_from_sharepoint_and_microsoft_answers() {
        let header = r#"Bearer realm="3A5B1C2D-0000-4000-8000-1234567890AB",client_id="00000003-0000-0ff1-ce00-000000000000",trusted_issuers="x""#;
        assert_eq!(realm(header).as_deref(), Some("3a5b1c2d-0000-4000-8000-1234567890ab"));
        assert_eq!(realm(r#"Bearer realm="""#), None);
        assert_eq!(realm("NTLM"), None);
        assert_eq!(
            tenant_in_issuer("https://login.microsoftonline.com/3a5b1c2d-0000-4000-8000-1234567890ab/v2.0").as_deref(),
            Some("3a5b1c2d-0000-4000-8000-1234567890ab")
        );
    }
}
