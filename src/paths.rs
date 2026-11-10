//! Name and path rules shared by the engine and the command line.

use std::path::{Path, PathBuf};

/// Longest path SharePoint accepts, measured from after the host name.
pub const SERVER_PATH_LIMIT: usize = 400;
/// The traditional Windows limit that many programs still enforce.
pub const WINDOWS_PATH_LIMIT: usize = 260;

/// Why SharePoint would reject a file or folder name, if it would.
pub fn invalid_server_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty name");
    }
    if name.chars().any(|c| "\"*:<>?/\\|".contains(c) || (c as u32) < 32) {
        return Some("contains a character SharePoint does not allow (\" * : < > ? / \\ |)");
    }
    if name.starts_with(' ') || name.ends_with(' ') {
        return Some("starts or ends with a space");
    }
    if name.ends_with('.') {
        return Some("ends with a period");
    }
    if name.starts_with("~$") {
        return Some("starts with ~$");
    }
    if name.contains("_vti_") {
        return Some("contains _vti_");
    }
    let upper = name.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    let reserved = matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Some("is a reserved device name");
    }
    if upper == ".LOCK" || upper == "DESKTOP.INI" {
        return Some("is a reserved name");
    }
    None
}

/// Whether a name from the server, or from a seed, is safe to create on disk as a
/// single file or folder. This is what stops a name from reaching outside the
/// connected folder.
pub fn safe_local_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '\0'))
}

/// Local files that are never uploaded: editor locks, temporary files, operating
/// system clutter, and DriveCraft's own partial downloads.
pub fn is_ignored_local_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    is_partial_name(name)
        || lower.starts_with("~$")
        || lower.starts_with(".~lock.")
        || lower.ends_with(".tmp")
        || lower.ends_with(".partial")
        || lower.ends_with(".crdownload")
        || matches!(
            lower.as_str(),
            "thumbs.db" | "desktop.ini" | ".ds_store" | "ehthumbs.db"
        )
}

/// Name of the temporary file a download is written to before it replaces the target.
pub fn partial_name(item_id: &str) -> String {
    let safe: String = item_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let tail = if safe.len() > 24 {
        &safe[safe.len() - 24..]
    } else {
        &safe
    };
    format!(".dc-{tail}.part")
}

pub fn is_partial_name(name: &str) -> bool {
    name.starts_with(".dc-") && name.ends_with(".part")
}

/// Join a `/`-separated relative path onto a root using the platform separator.
pub fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

pub fn parent_rel(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

pub fn file_name_rel(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

pub fn child_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

pub fn depth(rel: &str) -> usize {
    if rel.is_empty() {
        0
    } else {
        rel.matches('/').count() + 1
    }
}

/// A free name next to `path` for keeping a local file that would otherwise be
/// overwritten: `report.docx` becomes `report (local copy).docx`.
pub fn local_copy_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{stem} (local copy){ext}")
        } else {
            format!("{stem} (local copy {n}){ext}")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Tidy a library address pasted from the browser into the library's own address.
///
/// Browser addresses carry view pages (`/Forms/AllItems.aspx`), query strings and
/// sharing prefixes (`/:f:/r/`) that are not part of the library's location.
pub fn normalize_library_url(input: &str) -> Result<url::Url, String> {
    let mut u = url::Url::parse(input.trim()).map_err(|e| format!("not a web address: {e}"))?;
    if u.scheme() != "https" {
        return Err("the library address must start with https://".into());
    }
    u.set_query(None);
    u.set_fragment(None);
    let decoded: Vec<String> = u
        .path_segments()
        .map(|s| {
            s.filter(|p| !p.is_empty())
                .map(|p| percent_decode(p))
                .collect()
        })
        .unwrap_or_default();
    let mut segs: Vec<String> = decoded;
    // Sharing-style prefix: /:f:/r/sites/...
    if segs.len() >= 2 && segs[0].starts_with(':') && segs[0].ends_with(':') {
        segs.drain(0..2);
    }
    if let Some(i) = segs.iter().position(|s| s.eq_ignore_ascii_case("Forms")) {
        segs.truncate(i);
    }
    if let Some(last) = segs.last() {
        if last.to_ascii_lowercase().ends_with(".aspx") {
            segs.pop();
        }
    }
    u.path_segments_mut()
        .map_err(|_| "unsupported address".to_string())?
        .clear()
        .extend(segs.iter().map(|s| s.as_str()));
    Ok(u)
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_names_sharepoint_rejects() {
        assert!(invalid_server_name("report.docx").is_none());
        assert!(invalid_server_name("a:b.txt").is_some());
        assert!(invalid_server_name("trailing.").is_some());
        assert!(invalid_server_name(" lead").is_some());
        assert!(invalid_server_name("CON").is_some());
        assert!(invalid_server_name("com1.txt").is_some());
        assert!(invalid_server_name("COMPANY.txt").is_none());
        assert!(invalid_server_name("~$draft.docx").is_some());
    }

    #[test]
    fn refuses_names_that_could_escape_the_folder() {
        assert!(safe_local_name("report.docx"));
        assert!(safe_local_name(".drivecraft"));
        for bad in ["", ".", "..", "a/b", "..\\up", "x:stream"] {
            assert!(!safe_local_name(bad), "{bad}");
        }
    }

    #[test]
    fn ignores_temporary_files() {
        assert!(is_ignored_local_name("~$Budget.xlsx"));
        assert!(is_ignored_local_name("Thumbs.db"));
        assert!(is_ignored_local_name(&partial_name("01ABC!123")));
        assert!(!is_ignored_local_name("Budget.xlsx"));
    }

    #[test]
    fn normalizes_browser_addresses() {
        let u = normalize_library_url(
            "https://contoso.sharepoint.com/sites/Team/Shared%20Documents/Forms/AllItems.aspx?viewid=1",
        )
        .unwrap();
        assert_eq!(
            u.as_str(),
            "https://contoso.sharepoint.com/sites/Team/Shared%20Documents"
        );
        let u = normalize_library_url(
            "https://contoso.sharepoint.com/:f:/r/sites/Team/Shared%20Documents?csf=1",
        )
        .unwrap();
        assert_eq!(
            u.as_str(),
            "https://contoso.sharepoint.com/sites/Team/Shared%20Documents"
        );
        assert!(normalize_library_url("http://contoso.sharepoint.com/x").is_err());
    }

    #[test]
    fn relative_path_helpers() {
        assert_eq!(parent_rel("a/b/c.txt"), "a/b");
        assert_eq!(parent_rel("c.txt"), "");
        assert_eq!(file_name_rel("a/b/c.txt"), "c.txt");
        assert_eq!(child_rel("", "x"), "x");
        assert_eq!(child_rel("a", "x"), "a/x");
        assert_eq!(depth(""), 0);
        assert_eq!(depth("a/b"), 2);
    }
}
