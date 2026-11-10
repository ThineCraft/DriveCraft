//! Taking over the browser's "Sync" button, and giving it back.
//!
//! Which program answers a Sync link (see `synclink`) is one setting of the signed-in
//! Windows user. Taking the button over means pointing that setting at DriveCraft.
//! Nothing else is touched: OneDrive stays installed, keeps running, and keeps
//! synchronizing whatever it already synchronizes.
//!
//! It must be safe to try, so it is exactly reversible. Before changing anything,
//! DriveCraft writes down everything that was there, to a file in its data folder;
//! giving the button back removes DriveCraft's entry and puts back what was written
//! down, value for value. While DriveCraft has the button, a Sync link can still be
//! passed on to the program that had it before.

use anyhow::Result;
use std::path::Path;

/// Who answers the Sync button now.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Status {
    /// DriveCraft has the button.
    pub ours: bool,
    /// The program that has it when DriveCraft does not, or had it before DriveCraft
    /// took it: its name, for example "OneDrive". `None` when there is none.
    pub other: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct SavedValue {
    name: String,
    kind: u32,
    data: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct SavedKey {
    /// Below the setting's own key; empty for the key itself.
    path: String,
    values: Vec<SavedValue>,
}

/// What was there before DriveCraft took the button.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Default)]
struct Saved {
    keys: Vec<SavedKey>,
}

/// The program named at the start of a command line, without its folder or ".exe".
fn program_name(command: &str) -> Option<String> {
    let (exe, _) = split_command(command)?;
    Path::new(&exe).file_stem().map(|s| s.to_string_lossy().to_string())
}

/// A command line as Windows stores it: the program, then the rest as written.
fn split_command(command: &str) -> Option<(String, String)> {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        let end = rest.find('"')?;
        Some((rest[..end].to_string(), rest[end + 1..].trim_start().to_string()))
    } else {
        let end = c.find(' ').unwrap_or(c.len());
        (end > 0).then(|| (c[..end].to_string(), c[end..].trim_start().to_string()))
    }
}

fn text_of(v: &SavedValue) -> String {
    let wide: Vec<u16> = v.data.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    String::from_utf16_lossy(&wide).trim_end_matches('\0').to_string()
}

impl Saved {
    fn command(&self) -> Option<String> {
        self.keys
            .iter()
            .find(|k| k.path.eq_ignore_ascii_case(r"shell\open\command"))
            .and_then(|k| k.values.iter().find(|v| v.name.is_empty()))
            .map(text_of)
            .filter(|c| !c.trim().is_empty())
    }
}

fn load(file: &Path) -> Option<Saved> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn read() -> Option<Saved> {
        None
    }
    pub fn is_ours() -> bool {
        false
    }
    pub fn write_ours(_: &Path) -> Result<()> {
        anyhow::bail!("taking over the browser's Sync button is only available on Windows")
    }
    pub fn put_back(_: &Saved) -> Result<()> {
        Ok(())
    }
    pub fn start(_: &str, _: &str) -> Result<()> {
        anyhow::bail!("not available on this system")
    }
    pub fn remove_startup(_: &str, _: &str) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::anyhow;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegDeleteTreeW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, REG_SZ, REG_VALUE_TYPE,
    };

    /// The signed-in user's own setting for who answers `odopen:` addresses.
    const KEY: &str = r"Software\Classes\odopen";
    /// A value DriveCraft adds, so it can tell its own entry from anyone else's.
    const MARK: &str = "DriveCraft";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn open(path: &str) -> Option<HKEY> {
        let mut key = HKEY::default();
        let got = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr()), None, KEY_READ, &mut key) };
        (got == ERROR_SUCCESS).then_some(key)
    }

    fn read_key(path: &str, below: &str, out: &mut Vec<SavedKey>) -> Option<()> {
        let key = open(path)?;
        let mut values = Vec::new();
        let mut children = Vec::new();
        unsafe {
            for i in 0.. {
                let mut name = vec![0u16; 16384];
                let mut name_len = name.len() as u32;
                let mut kind = 0u32;
                let mut data = vec![0u8; 4096];
                let mut data_len = data.len() as u32;
                let mut got = RegEnumValueW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut name_len, None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut data_len));
                if got == ERROR_MORE_DATA {
                    data = vec![0u8; data_len as usize + 2];
                    name_len = name.len() as u32;
                    got = RegEnumValueW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut name_len, None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut data_len));
                }
                if got == ERROR_NO_MORE_ITEMS || got != ERROR_SUCCESS {
                    break;
                }
                data.truncate(data_len as usize);
                values.push(SavedValue {
                    name: String::from_utf16_lossy(&name[..name_len as usize]),
                    kind,
                    data,
                });
            }
            for i in 0.. {
                let mut name = vec![0u16; 300];
                let mut name_len = name.len() as u32;
                if RegEnumKeyExW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut name_len, None, None, None, None) != ERROR_SUCCESS {
                    break;
                }
                children.push(String::from_utf16_lossy(&name[..name_len as usize]));
            }
            let _ = RegCloseKey(key);
        }
        out.push(SavedKey {
            path: below.to_string(),
            values,
        });
        for child in children {
            let below = if below.is_empty() { child.clone() } else { format!(r"{below}\{child}") };
            read_key(&format!(r"{path}\{child}"), &below, out);
        }
        Some(())
    }

    /// Everything under the setting, or `None` when the user has no such setting.
    pub fn read() -> Option<Saved> {
        let mut keys = Vec::new();
        read_key(KEY, "", &mut keys)?;
        Some(Saved { keys })
    }

    pub fn is_ours() -> bool {
        read().is_some_and(|s| s.keys.iter().any(|k| k.path.is_empty() && k.values.iter().any(|v| v.name == MARK)))
    }

    fn set(path: &str, name: &str, kind: u32, data: &[u8]) -> Result<()> {
        unsafe {
            let mut key = HKEY::default();
            RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr()), &mut key).ok()?;
            let out = RegSetValueExW(key, PCWSTR(wide(name).as_ptr()), None, REG_VALUE_TYPE(kind), Some(data)).ok();
            let _ = RegCloseKey(key);
            Ok(out?)
        }
    }

    fn set_text(path: &str, name: &str, text: &str) -> Result<()> {
        let data: Vec<u8> = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
        set(path, name, REG_SZ.0, &data)
    }

    fn clear() {
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(KEY).as_ptr()));
        }
    }

    pub fn write_ours(app: &Path) -> Result<()> {
        clear();
        let app = app.to_string_lossy();
        set_text(KEY, "", "URL: DriveCraft (Sync button)")?;
        set_text(KEY, "URL Protocol", "")?;
        set_text(KEY, MARK, "1")?;
        set_text(&format!(r"{KEY}\DefaultIcon"), "", &format!("{app},0"))?;
        set_text(&format!(r"{KEY}\shell\open\command"), "", &format!("\"{app}\" \"%1\""))
    }

    /// Remove whatever is there and write `saved` back, value for value.
    pub fn put_back(saved: &Saved) -> Result<()> {
        clear();
        for key in &saved.keys {
            let path = if key.path.is_empty() { KEY.to_string() } else { format!(r"{KEY}\{}", key.path) };
            // A key with no values still has to exist.
            unsafe {
                let mut made = HKEY::default();
                RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(wide(&path).as_ptr()), &mut made).ok()?;
                let _ = RegCloseKey(made);
            }
            for v in &key.values {
                set(&path, &v.name, v.kind, &v.data)?;
            }
        }
        Ok(())
    }

    /// Remove the "start with Windows" entry called `name`, if it starts `program`.
    pub fn remove_startup(name: &str, program: &str) {
        use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RRF_RT_REG_SZ};
        const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
        unsafe {
            let mut data = vec![0u16; 2048];
            let mut len = (data.len() * 2) as u32;
            let got = RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(wide(RUN).as_ptr()),
                PCWSTR(wide(name).as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(data.as_mut_ptr().cast()),
                Some(&mut len),
            );
            if got != ERROR_SUCCESS {
                return;
            }
            let command = String::from_utf16_lossy(&data[..(len as usize / 2)]).to_lowercase();
            // Only ever an entry that is DriveCraft's own.
            if command.contains(&program.to_lowercase()) {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(wide(RUN).as_ptr()), PCWSTR(wide(name).as_ptr()));
            }
        }
    }

    /// Start `program` with the rest of its command line exactly as given.
    pub fn start(program: &str, rest: &str) -> Result<()> {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(program)
            .raw_arg(rest)
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow!("could not start {program}: {e}"))
    }
}

/// Who answers the Sync button. `backup` is where DriveCraft wrote down what was
/// there before it took the button.
pub fn status(backup: &Path) -> Status {
    let ours = imp::is_ours();
    let command = if ours { load(backup).and_then(|s| s.command()) } else { imp::read().and_then(|s| s.command()) };
    Status {
        ours,
        other: command.as_deref().and_then(program_name),
    }
}

/// Point the Sync button at the program `app`, after writing down what was there.
pub fn take(app: &Path, backup: &Path) -> Result<()> {
    if !imp::is_ours() {
        // What is there now belongs to someone else: that is what must come back.
        let was = imp::read().unwrap_or_default();
        if let Some(dir) = backup.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(backup, serde_json::to_string_pretty(&was)?)?;
        // Never change anything without a readable record of how it was.
        if load(backup).as_ref() != Some(&was) {
            anyhow::bail!("could not write down the current setting, so nothing was changed");
        }
    }
    imp::write_ours(app)
}

/// Give the Sync button back: remove DriveCraft's entry and restore what was there.
/// If another program has already taken the button back, nothing is changed.
pub fn give_back(backup: &Path) -> Result<()> {
    if imp::is_ours() {
        imp::put_back(&load(backup).unwrap_or_default())?;
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

/// Remove the "start with Windows" entry called `name`, provided it starts `program`.
pub fn remove_startup(name: &str, program: &str) {
    imp::remove_startup(name, program)
}

/// Keep DriveCraft's entry pointing at where the program is now.
pub fn refresh(app: &Path) -> Result<()> {
    if imp::is_ours() {
        imp::write_ours(app)?;
    }
    Ok(())
}

/// Pass a Sync link on to the program that had the button before DriveCraft.
pub fn pass_on(backup: &Path, link: &str) -> Result<()> {
    if !crate::synclink::is_sync_link(link) || link.contains('"') || link.chars().any(char::is_control) {
        anyhow::bail!("this is not a Sync link");
    }
    let command = load(backup)
        .and_then(|s| s.command())
        .ok_or_else(|| anyhow::anyhow!("no other program was handling the Sync button before DriveCraft"))?;
    let (program, rest) = split_command(&command).ok_or_else(|| anyhow::anyhow!("the earlier setting could not be read"))?;
    imp::start(&program, &rest.replace("%1", link))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<u8> {
        s.encode_utf16().chain(Some(0)).flat_map(|c| c.to_le_bytes()).collect()
    }

    #[test]
    fn reads_the_program_out_of_a_saved_command() {
        let saved = Saved {
            keys: vec![
                SavedKey { path: String::new(), values: vec![SavedValue { name: String::new(), kind: 1, data: text("URL: OneDrive Client Protocol") }] },
                SavedKey {
                    path: r"shell\open\command".into(),
                    values: vec![SavedValue { name: String::new(), kind: 1, data: text(r#""C:\Users\pat\AppData\Local\Microsoft\OneDrive\OneDrive.exe" /url:"%1""#) }],
                },
            ],
        };
        let command = saved.command().unwrap();
        assert_eq!(program_name(&command).as_deref(), Some("OneDrive"));
        let (program, rest) = split_command(&command).unwrap();
        assert!(program.ends_with("OneDrive.exe"));
        assert_eq!(rest, r#"/url:"%1""#);
        assert_eq!(split_command(r"C:\tools\sync.exe %1").unwrap(), (r"C:\tools\sync.exe".to_string(), "%1".to_string()));
        assert_eq!(Saved::default().command(), None);
        // The record survives being written to a file and read back.
        let again: Saved = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert_eq!(again, saved);
    }
}
