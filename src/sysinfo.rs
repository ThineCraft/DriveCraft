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

/// The most memory this process has used so far, where the system can tell us.
#[cfg(windows)]
pub fn peak_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: `c` is a correctly sized, writable structure and the handle is the
    // always-valid pseudo-handle for this process.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (ok != 0).then_some(c.PeakWorkingSetSize as u64)
}

#[cfg(not(windows))]
pub fn peak_memory_bytes() -> Option<u64> {
    None
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
