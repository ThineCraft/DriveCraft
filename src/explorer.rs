//! File Explorer integration (Windows).
//!
//! A library's folder can be registered with Windows as a *sync root*, the same
//! mechanism OneDrive uses. Windows then:
//!
//! * lists the folder in File Explorer's navigation pane, under a name and an icon
//!   chosen on this computer;
//! * shows a status icon beside each item: a green tick once DriveCraft has marked
//!   it as synchronized, and the "pending" arrows from the moment it is changed,
//!   which Windows works out by itself.
//!
//! DriveCraft keeps every file whole on disk; it only tells Windows which items are
//! in step with the library. Turning the integration off puts the files back to
//! being ordinary files. Which folders are registered is kept by Windows, not in the
//! catalog: it is a fact about this computer.

use crate::catalog::{Catalog, Connection, LocalRow, Mode};
use crate::paths::join_rel;
use crate::remote::ItemKind;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// A folder registered with File Explorer.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Shown {
    pub folder: PathBuf,
    pub name: String,
    /// An icon file, or a program or library followed by `,` and an icon number.
    pub icon: String,
}

/// What `mark_tree` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Marked {
    pub done: u64,
    pub failed: u64,
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn shown() -> Vec<Shown> {
        Vec::new()
    }
    pub fn show(_: &Path, _: &str, _: &str, _: &str) -> Result<()> {
        anyhow::bail!("File Explorer integration is only available on Windows")
    }
    pub fn hide(_: &Path) -> Result<()> {
        Ok(())
    }
    pub fn mark_in_sync(_: &Path) -> Result<()> {
        Ok(())
    }
    pub fn revert(_: &Path) -> Result<()> {
        Ok(())
    }
    pub fn connect(_: &Path) -> Result<i64> {
        Ok(0)
    }
    pub fn set_menu(_: &Path, _: &[PathBuf]) -> Result<()> {
        Ok(())
    }
    pub fn set_checked_out(_: &Path, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    pub fn disconnect(_: i64) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{anyhow, Context};
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{HSTRING, PCWSTR, PWSTR};
    use windows::Security::Cryptography::{BinaryStringEncoding, CryptographicBuffer};
    use windows::Storage::Provider::{
        StorageProviderHardlinkPolicy, StorageProviderItemPropertyDefinition, StorageProviderHydrationPolicy, StorageProviderHydrationPolicyModifier,
        StorageProviderInSyncPolicy, StorageProviderPopulationPolicy, StorageProviderProtectionMode,
        StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
    };
    use windows::Storage::StorageFolder;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::Storage::CloudFilters::{
        CfConvertToPlaceholder, CfRevertPlaceholder, CfSetInSyncState, CfSetPinState, CF_CONVERT_FLAG_MARK_IN_SYNC,
        CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE,
        CF_IN_SYNC_STATE_IN_SYNC, CF_REVERT_FLAG_NONE, CF_SET_IN_SYNC_FLAG_NONE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, WRITE_DAC,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    const PROVIDER: &str = "DriveCraft";
    const CHECKED_OUT: i32 = 1;
    /// "This is not a cloud file": the item has not been handed to Windows yet.
    const NOT_A_CLOUD_FILE: i32 = 0x80070178u32 as i32;

    fn init() {
        // Harmless when the thread is already set up, in either mode.
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
    }

    /// The signed-in Windows user, as Windows writes it ("S-1-5-21-...").
    fn user_sid() -> Result<String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
            let mut buf = vec![0u8; 512];
            let mut len = 0u32;
            let got = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), buf.len() as u32, &mut len);
            let _ = CloseHandle(token);
            got?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text)?;
            let sid = text.to_string().context("reading the Windows account")?;
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            Ok(sid)
        }
    }

    /// Windows identifies a sync root as provider!user!account. The account part is
    /// this connection's key, with anything but letters and digits taken out.
    fn root_id(key: &str) -> Result<String> {
        let safe: String = key.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        Ok(format!("{PROVIDER}!{}!{safe}", user_sid()?))
    }

    /// Where Windows keeps its list of registered folders. Reading it is all that
    /// the ordinary run of a synchronization needs; it works on every edition of
    /// Windows, including those without File Explorer's cloud features, where
    /// asking Windows' own cloud interface can bring the program down.
    const ROOTS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";

    fn read_text(key: windows::Win32::System::Registry::HKEY, sub: &str, name: &str) -> Option<String> {
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};
        let mut data = vec![0u16; 4096];
        let mut len = (data.len() * 2) as u32;
        let got = unsafe {
            RegGetValueW(key, PCWSTR(wide(sub).as_ptr()), PCWSTR(wide(name).as_ptr()), RRF_RT_REG_SZ, None, Some(data.as_mut_ptr().cast()), Some(&mut len))
        };
        if got != ERROR_SUCCESS {
            return None;
        }
        let chars = (len as usize / 2).min(data.len());
        Some(String::from_utf16_lossy(&data[..chars]).trim_end_matches('\0').to_string())
    }

    /// DriveCraft's registered folders for this Windows user: (identifier, folder, name, icon).
    fn ours() -> Vec<(String, PathBuf, String, String)> {
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};
        let Ok(sid) = user_sid() else { return Vec::new() };
        let mine = format!("{PROVIDER}!{sid}!");
        let mut out = Vec::new();
        unsafe {
            let mut roots = HKEY::default();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(wide(ROOTS).as_ptr()), None, KEY_READ, &mut roots) != ERROR_SUCCESS {
                return out;
            }
            for i in 0.. {
                let mut name = vec![0u16; 600];
                let mut len = name.len() as u32;
                if RegEnumKeyExW(roots, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
                    break;
                }
                let id = String::from_utf16_lossy(&name[..len as usize]);
                if !id.starts_with(&mine) {
                    continue;
                }
                let Some(folder) = read_text(roots, &format!(r"{id}\UserSyncRoots"), &sid) else { continue };
                out.push((
                    id.clone(),
                    PathBuf::from(folder),
                    read_text(roots, &id, "DisplayNameResource").unwrap_or_default(),
                    read_text(roots, &id, "IconResource").unwrap_or_default(),
                ));
            }
            let _ = RegCloseKey(roots);
        }
        out
    }

    fn same(a: &Path, b: &Path) -> bool {
        a.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
    }

    pub fn shown() -> Vec<Shown> {
        ours().into_iter().map(|(_, folder, name, icon)| Shown { folder, name, icon }).collect()
    }

    /// Whether this edition of Windows has File Explorer's cloud features at all.
    fn supported() -> bool {
        init();
        StorageProviderSyncRootManager::IsSupported().unwrap_or(false)
    }

    pub fn show(folder: &Path, key: &str, name: &str, icon: &str) -> Result<()> {
        if !supported() {
            anyhow::bail!("this edition of Windows cannot list a synchronized folder in File Explorer");
        }
        let id = root_id(key)?;
        let place = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(folder.as_os_str()))?
            .join()
            .with_context(|| format!("opening {}", folder.display()))?;
        let info = StorageProviderSyncRootInfo::new()?;
        info.SetId(&HSTRING::from(&id))?;
        info.SetPath(&place)?;
        info.SetDisplayNameResource(&HSTRING::from(name))?;
        info.SetIconResource(&HSTRING::from(icon))?;
        info.SetVersion(&HSTRING::from("1"))?;
        // Every file is always whole on disk; Windows never has to ask for content.
        info.SetHydrationPolicy(StorageProviderHydrationPolicy::AlwaysFull)?;
        info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::None)?;
        info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)?;
        info.SetInSyncPolicy(StorageProviderInSyncPolicy::Default)?;
        info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
        info.SetProtectionMode(StorageProviderProtectionMode::Unknown)?;
        info.SetShowSiblingsAsGroup(false)?;
        info.SetAllowPinning(false)?;
        // The one extra thing DriveCraft says about a file: that it is checked out.
        let checked_out = StorageProviderItemPropertyDefinition::new()?;
        checked_out.SetId(CHECKED_OUT)?;
        checked_out.SetDisplayNameResource(&HSTRING::from("Checked out"))?;
        info.StorageProviderItemPropertyDefinitions()?.Append(&checked_out)?;
        info.SetContext(&CryptographicBuffer::ConvertStringToBinary(&HSTRING::from(&id), BinaryStringEncoding::Utf8)?)?;
        StorageProviderSyncRootManager::Register(&info)
            .map_err(|e| anyhow!("Windows would not add {} to File Explorer: {e}", folder.display()))
    }

    pub fn hide(folder: &Path) -> Result<()> {
        for (id, at, _, _) in ours() {
            if same(&at, folder) {
                init();
                StorageProviderSyncRootManager::Unregister(&HSTRING::from(&id))?;
            }
        }
        Ok(())
    }

    fn open(path: &Path) -> Result<HANDLE> {
        // The long form of the path, so that deep folders work.
        let text = path.as_os_str().to_string_lossy();
        let long = if text.starts_with(r"\\?\") {
            text.to_string()
        } else if let Some(unc) = text.strip_prefix(r"\\") {
            format!(r"\\?\UNC\{unc}")
        } else {
            format!(r"\\?\{text}")
        };
        let wide: Vec<u16> = std::ffi::OsStr::new(&long).encode_wide().chain(Some(0)).collect();
        let h = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                (FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | WRITE_DAC).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }?;
        Ok(h)
    }

    pub fn mark_in_sync(path: &Path) -> Result<()> {
        let h = open(path)?;
        let out = unsafe {
            match CfSetInSyncState(h, CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, None) {
                Err(e) if e.code().0 == NOT_A_CLOUD_FILE => {
                    // Windows keeps a short note of ours with the item; it is not used.
                    let note = b"dc";
                    CfConvertToPlaceholder(
                        h,
                        Some(note.as_ptr().cast()),
                        note.len() as u32,
                        CF_CONVERT_FLAG_MARK_IN_SYNC,
                        None,
                        None,
                    )
                }
                other => other,
            }
        };
        if out.is_ok() {
            // Everything DriveCraft synchronizes stays whole on this computer. Saying so
            // is what makes Windows show a folder as present rather than "online only".
            unsafe {
                let _ = CfSetPinState(h, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, None);
            }
        }
        unsafe {
            let _ = CloseHandle(h);
        }
        Ok(out?)
    }

    /// Show, or stop showing, a padlock and a few words beside a file.
    pub fn set_checked_out(path: &Path, who: Option<&str>) -> Result<()> {
        use windows::Storage::Provider::{StorageProviderItemProperties, StorageProviderItemProperty};
        use windows::Storage::StorageFile;
        use windows_collections::IIterable;
        init();
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
        let mut marks = Vec::new();
        if let Some(who) = who {
            // Windows' own padlock picture.
            let windows = std::env::var("SystemRoot").unwrap_or_else(|_| "C:/Windows".into());
            let padlock = PathBuf::from(windows).join("System32").join("shell32.dll");
            let mark = StorageProviderItemProperty::new()?;
            mark.SetId(CHECKED_OUT)?;
            mark.SetValue(&HSTRING::from(who))?;
            mark.SetIconResource(&HSTRING::from(format!("{},-48", padlock.display())))?;
            marks.push(Some(mark));
        }
        StorageProviderItemProperties::SetAsync(&file, &IIterable::<StorageProviderItemProperty>::from(marks))?.join()?;
        Ok(())
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Write text values under a key of the signed-in user's own settings.
    fn write_key(path: &str, values: &[(&str, &str)]) -> Result<()> {
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, REG_SZ,
        };
        unsafe {
            let mut key = HKEY::default();
            RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr()), &mut key).ok()?;
            let mut out = Ok(());
            for (name, value) in values {
                let data = wide(value);
                let bytes = std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len() * 2);
                let set = RegSetValueExW(key, PCWSTR(wide(name).as_ptr()), None, REG_SZ, Some(bytes)).ok();
                if set.is_err() {
                    out = set;
                }
            }
            let _ = RegCloseKey(key);
            Ok(out?)
        }
    }

    fn remove_key(path: &str) {
        use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr()));
        }
    }

    /// Offer "Check out" and "Check in" when a file inside one of `folders` is
    /// right-clicked. With no folders, the entries are taken away.
    pub fn set_menu(app: &Path, folders: &[PathBuf]) -> Result<()> {
        const VERBS: [(&str, &str, &str); 3] = [
            ("DriveCraft.CheckOut", "Check out", "--check-out"),
            ("DriveCraft.CheckIn", "Check in", "--check-in"),
            ("DriveCraft.Details", "Library details", "--details"),
        ];
        for (verb, _, _) in VERBS {
            remove_key(&format!(r"Software\Classes\*\shell\{verb}"));
        }
        if folders.is_empty() {
            return Ok(());
        }
        // Only for files whose path starts with one of the folders.
        let within: Vec<String> = folders
            .iter()
            .map(|f| format!("System.ItemPathDisplay:~<\"{}\\\"", f.to_string_lossy().trim_end_matches('\\')))
            .collect();
        let within = within.join(" OR ");
        let app = app.to_string_lossy();
        for (verb, label, arg) in VERBS {
            let key = format!(r"Software\Classes\*\shell\{verb}");
            write_key(&key, &[("MUIVerb", label), ("Icon", &format!("{app},0")), ("AppliesTo", &within)])?;
            write_key(&format!(r"{key}\command"), &[("", &format!("\"{app}\" {arg} \"%1\""))])?;
        }
        Ok(())
    }

    /// Tell Windows that DriveCraft is running and looking after this folder.
    pub fn connect(folder: &Path) -> Result<i64> {
        use windows::Win32::Storage::CloudFilters::{
            CfConnectSyncRoot, CfUpdateSyncProviderStatus, CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE_NONE,
            CF_CONNECT_FLAG_NONE, CF_PROVIDER_STATUS_IDLE,
        };
        // Files are always whole, so Windows has nothing to ask DriveCraft for.
        let none = [CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        }];
        let key = unsafe { CfConnectSyncRoot(&HSTRING::from(folder.as_os_str()), none.as_ptr(), None, CF_CONNECT_FLAG_NONE) }?;
        unsafe {
            let _ = CfUpdateSyncProviderStatus(key, CF_PROVIDER_STATUS_IDLE);
        }
        Ok(key.0)
    }

    pub fn disconnect(key: i64) {
        use windows::Win32::Storage::CloudFilters::{CfDisconnectSyncRoot, CF_CONNECTION_KEY};
        unsafe {
            let _ = CfDisconnectSyncRoot(CF_CONNECTION_KEY(key));
        }
    }

    pub fn revert(path: &Path) -> Result<()> {
        let h = open(path)?;
        let out = unsafe { CfRevertPlaceholder(h, CF_REVERT_FLAG_NONE, None) };
        unsafe {
            let _ = CloseHandle(h);
        }
        match out {
            Err(e) if e.code().0 != NOT_A_CLOUD_FILE => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// The folders registered with File Explorer by DriveCraft, for this Windows user.
pub fn shown() -> Vec<Shown> {
    imp::shown()
}

/// How `folder` is shown in File Explorer, if it is.
pub fn shown_as(folder: &Path) -> Option<Shown> {
    let want = folder.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    shown()
        .into_iter()
        .find(|s| s.folder.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase() == want)
}

/// List `folder` in File Explorer under `name` and `icon`, or change how it is listed.
/// `key` tells this connection apart from the user's others.
pub fn show(folder: &Path, key: &str, name: &str, icon: &str) -> Result<()> {
    imp::show(folder, key, name, icon)
}

/// Take `folder` out of File Explorer's list. Its items keep whatever marks they have;
/// use `revert_tree` first to make them ordinary files again.
pub fn hide(folder: &Path) -> Result<()> {
    imp::hide(folder)
}

/// Tell Windows that this file or folder matches the library.
pub fn mark_in_sync(path: &Path) -> Result<()> {
    imp::mark_in_sync(path)
}

/// DriveCraft's standing connection to a registered folder, held while it runs.
pub struct Link(i64);

impl Drop for Link {
    fn drop(&mut self) {
        imp::disconnect(self.0);
    }
}

/// Tell Windows that DriveCraft is running and looking after this registered folder.
pub fn connect(folder: &Path) -> Result<Link> {
    imp::connect(folder).map(Link)
}

/// Offer "Check out" and "Check in" in the right-click menu of files inside
/// `folders`, carried out by the program at `app`. With no folders, remove them.
pub fn set_menu(app: &Path, folders: &[PathBuf]) -> Result<()> {
    imp::set_menu(app, folders)
}

/// Show beside a file that it is checked out (`Some(words)`), or stop showing it.
pub fn set_checked_out(path: &Path, who: Option<&str>) -> Result<()> {
    imp::set_checked_out(path, who)
}

/// Make this file or folder an ordinary one again.
pub fn revert(path: &Path) -> Result<()> {
    imp::revert(path)
}

/// Whether the file at `path` still looks as it did when it was last synchronized.
fn untouched(path: &Path, row: &LocalRow) -> bool {
    let Ok(m) = std::fs::metadata(path) else { return false };
    if row.kind == ItemKind::Folder {
        return m.is_dir();
    }
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    m.is_file() && m.len() == row.size && mtime == row.mtime_ns
}

/// Mark everything the catalog knows to be synchronized, and unchanged since, as in
/// step with the library. Items changed here since are left for Windows to show as
/// pending.
pub fn mark_all(cat: &Catalog, conn: &Connection) -> Result<Marked> {
    let mut m = Marked::default();
    for row in cat.local_all(conn.id)? {
        let path = join_rel(&conn.local_path, &row.rel_path);
        if !untouched(&path, &row) {
            continue;
        }
        match mark_in_sync(&path) {
            Ok(()) => m.done += 1,
            Err(_) => m.failed += 1,
        }
    }
    Ok(m)
}

/// Mark every synchronized folder as in step with the library.
pub fn mark_folders(cat: &Catalog, conn: &Connection) -> Result<Marked> {
    let mut m = Marked::default();
    for row in cat.local_all(conn.id)?.into_iter().filter(|r| r.kind == ItemKind::Folder) {
        match mark_in_sync(&join_rel(&conn.local_path, &row.rel_path)) {
            Ok(()) => m.done += 1,
            Err(_) => m.failed += 1,
        }
    }
    Ok(m)
}

/// List a library's folder in File Explorer and give its items their status icons.
pub fn turn_on(cat: &Catalog, conn: &Connection, name: &str, icon: &str) -> Result<Marked> {
    if conn.mode == Mode::Ingest {
        anyhow::bail!("an ingest source is only read; DriveCraft does not change how it looks in File Explorer");
    }
    if !conn.local_path.is_dir() {
        anyhow::bail!("{} does not exist yet; synchronize first", conn.local_path.display());
    }
    let already = shown_as(&conn.local_path).is_some();
    show(&conn.local_path, &conn.name, name, icon)?;
    if already {
        // Only the name or the icon changed; the items keep their marks.
        return Ok(Marked::default());
    }
    mark_all(cat, conn)
}

/// Take a library's folder out of File Explorer and make its items ordinary again.
pub fn turn_off(conn: &Connection) -> Result<Marked> {
    let mut m = Marked::default();
    if shown_as(&conn.local_path).is_none() {
        return Ok(m);
    }
    if conn.local_path.is_dir() {
        // Deepest first, and everything that is there, whether the catalog knows it or not.
        for entry in walkdir::WalkDir::new(&conn.local_path).min_depth(1).contents_first(true).into_iter().flatten() {
            match revert(entry.path()) {
                Ok(()) => m.done += 1,
                Err(_) => m.failed += 1,
            }
        }
    }
    hide(&conn.local_path)?;
    Ok(m)
}

/// The icon used when the user has not chosen one: DriveCraft's own.
pub fn default_icon() -> String {
    std::env::current_exe()
        .map(|p| format!("{},0", p.display()))
        .unwrap_or_default()
}
