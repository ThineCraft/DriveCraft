//! Facts about the machine DriveCraft runs on.

/// Whether Windows long path support is switched on. `None` when it cannot be
/// determined or does not apply (not Windows).
#[cfg(windows)]
pub fn windows_long_paths_enabled() -> Option<bool> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
    let key: Vec<u16> = "SYSTEM\\CurrentControlSet\\Control\\FileSystem\0".encode_utf16().collect();
    let value: Vec<u16> = "LongPathsEnabled\0".encode_utf16().collect();
    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; `data` and
    // `size` describe a valid 4-byte buffer.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        )
    };
    (status == 0).then_some(data != 0)
}

#[cfg(not(windows))]
pub fn windows_long_paths_enabled() -> Option<bool> {
    None
}

/// Switch on Windows long path support. This is a machine-wide setting and needs
/// administrator rights; without them Windows refuses and nothing changes.
#[cfg(windows)]
pub fn enable_windows_long_paths() -> Result<(), String> {
    let out = std::process::Command::new("reg")
        .args([
            "add",
            "HKLM\\SYSTEM\\CurrentControlSet\\Control\\FileSystem",
            "/v",
            "LongPathsEnabled",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(not(windows))]
pub fn enable_windows_long_paths() -> Result<(), String> {
    Err("this setting only exists on Windows".into())
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} bytes")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
