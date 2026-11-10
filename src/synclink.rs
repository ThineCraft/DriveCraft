//! The link a browser follows when someone presses "Sync" on a SharePoint library.
//!
//! SharePoint does not start a program itself. Its Sync button opens an address that
//! begins `odopen://sync/` and carries, as parameters, which site and which library
//! were being looked at. Windows hands that address to whichever program has said it
//! handles `odopen` addresses: normally OneDrive, or DriveCraft once it has taken
//! the button over (see `takeover`). This module only reads the address.

/// What a Sync link says.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SyncLink {
    /// The site's address, for example `https://contoso.sharepoint.com/sites/Team`.
    pub site_url: String,
    pub site_id: String,
    pub web_id: String,
    pub list_id: String,
    pub site_title: String,
    pub library_title: String,
    /// The account the browser was signed in with.
    pub account: String,
    /// Set when Sync was pressed inside a folder rather than at the top of a library.
    pub folder: Option<String>,
}

/// The address scheme the Sync button uses.
pub const SCHEME: &str = "odopen";

pub fn is_sync_link(text: &str) -> bool {
    text.len() > SCHEME.len() + 1
        && text[..SCHEME.len()].eq_ignore_ascii_case(SCHEME)
        && text.as_bytes()[SCHEME.len()] == b':'
}

/// Identifiers arrive as `{guid}`; the server wants the bare form.
fn bare(id: &str) -> String {
    id.trim().trim_start_matches('{').trim_end_matches('}').to_lowercase()
}

fn looks_like_id(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn parse(text: &str) -> Result<SyncLink, String> {
    if !is_sync_link(text) {
        return Err("this is not a Sync link".into());
    }
    let url = url::Url::parse(text).map_err(|e| format!("the Sync link could not be read: {e}"))?;
    let get = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let site_url = get("webUrl").ok_or("the Sync link does not say which site it is for")?;
    // The link is followed with the user's sign-in, so it may only name a real site.
    let site = url::Url::parse(&site_url).map_err(|_| "the Sync link names a site that is not a web address")?;
    if site.scheme() != "https" || site.host_str().is_none() {
        return Err("the Sync link names a site that is not a secure web address".into());
    }
    let id = |name: &str| -> Result<String, String> {
        let v = bare(&get(name).ok_or(format!("the Sync link has no {name}"))?);
        if looks_like_id(&v) { Ok(v) } else { Err(format!("the Sync link has an unreadable {name}")) }
    };
    Ok(SyncLink {
        site_id: id("siteId")?,
        web_id: id("webId")?,
        list_id: id("listId")?,
        site_title: get("webTitle").unwrap_or_default(),
        library_title: get("listTitle").unwrap_or_default(),
        account: get("userEmail").unwrap_or_default(),
        folder: get("folderName").or(get("folderUrl")),
        site_url: site_url.trim_end_matches('/').to_string(),
    })
}

impl SyncLink {
    /// The site's server name, for example `contoso.sharepoint.com`.
    pub fn host(&self) -> String {
        url::Url::parse(&self.site_url)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default()
    }

    /// A name for the local folder and the connection: the site and the library,
    /// without anything a folder name cannot hold.
    pub fn suggested_name(&self) -> String {
        let join = match (self.site_title.trim(), self.library_title.trim()) {
            ("", "") => "Library".to_string(),
            ("", l) => l.to_string(),
            (s, "") => s.to_string(),
            (s, l) => format!("{s} - {l}"),
        };
        let clean: String = join
            .chars()
            .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c })
            .collect();
        let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
        let clean = clean.trim_matches(|c| c == '.' || c == ' ').to_string();
        if clean.is_empty() { "Library".into() } else { clean }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "odopen://sync/?siteId=%7B11111111%2D2222%2D3333%2D4444%2D555555555555%7D&webId=%7Baaaaaaaa%2Dbbbb%2Dcccc%2Ddddd%2Deeeeeeeeeeee%7D&webTitle=Finance%20Team&listId=%7B99999999%2D8888%2D7777%2D6666%2D555555555555%7D&listTitle=Documents&userEmail=pat%40contoso.com&webUrl=https%3A%2F%2Fcontoso.sharepoint.com%2Fsites%2FFinance&version=1&scope=OPENLIST";

    #[test]
    fn reads_what_the_sync_button_sends() {
        let l = parse(LINK).unwrap();
        assert_eq!(l.site_url, "https://contoso.sharepoint.com/sites/Finance");
        assert_eq!(l.host(), "contoso.sharepoint.com");
        assert_eq!(l.site_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(l.web_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(l.list_id, "99999999-8888-7777-6666-555555555555");
        assert_eq!(l.account, "pat@contoso.com");
        assert_eq!(l.suggested_name(), "Finance Team - Documents");
        assert_eq!(l.folder, None);
    }

    #[test]
    fn refuses_what_is_not_a_sync_link_for_a_real_site() {
        assert!(parse("https://contoso.sharepoint.com/").is_err());
        assert!(parse("odopen://sync/?siteId=x").is_err());
        assert!(parse(&LINK.replace("https%3A", "file%3A")).is_err());
        assert!(parse(&LINK.replace("%7B99999999", "%7B..%2F..%2F")).is_err(), "identifiers go into an address");
        assert!(is_sync_link("ODOPEN://sync/?x=1") && !is_sync_link("odopenx://") && !is_sync_link("odopen"));
    }

    #[test]
    fn names_are_made_safe_for_a_folder() {
        let mut l = parse(LINK).unwrap();
        l.site_title = "R&D: Plans / 2026?".into();
        l.library_title = "Shared Documents.".into();
        assert_eq!(l.suggested_name(), "R&D Plans 2026 - Shared Documents");
    }
}
