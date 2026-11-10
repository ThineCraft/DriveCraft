//! A plain-text log of what DriveCraft did, kept on this computer.
//!
//! One file per day in the `logs` folder of DriveCraft's data folder. Every line
//! starts with a UTC timestamp. The log holds file names and paths; it never holds
//! sign-in tokens or download addresses.

use crate::engine::Report;
use crate::graph::NetStats;
use crate::sysinfo::{self, human_bytes};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const KEEP_DAYS: u64 = 30;

pub struct RunLog {
    file: Mutex<std::fs::File>,
    pub path: PathBuf,
}

fn stamp() -> (String, String) {
    let t = time::OffsetDateTime::now_utc();
    let date = format!("{:04}-{:02}-{:02}", t.year(), t.month() as u8, t.day());
    let clock = format!("{:02}:{:02}:{:02}Z", t.hour(), t.minute(), t.second());
    (date, clock)
}

pub fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

impl RunLog {
    /// Open today's log for appending, and remove logs older than a month.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = logs_dir(data_dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        prune(&dir);
        let (date, _) = stamp();
        let path = dir.join(format!("drivecraft-{date}.log"));
        let file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
            path,
        })
    }

    pub fn line(&self, text: &str) {
        let (date, clock) = stamp();
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{date} {clock}  {text}");
        }
    }
}

fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let limit = std::time::Duration::from_secs(KEEP_DAYS * 24 * 3600);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !(name.starts_with("drivecraft-") && name.ends_with(".log")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > limit)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub fn human_duration(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 {
        format!("{secs:.1}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// The measurements of one run, as lines for the log and the screen.
pub fn run_details(report: &Report, net: &NetStats) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "timing: reading the server's changes {}, updating this computer {}, sending local changes {}",
        human_duration(report.scan_secs),
        human_duration(report.apply_secs),
        human_duration(report.push_secs)
    ));
    if report.scanned > 0 && report.scan_secs > 0.0 {
        out.push(format!(
            "scan: {} items read from the server ({:.0} per second)",
            report.scanned,
            report.scanned as f64 / report.scan_secs
        ));
    }
    if report.downloaded > 0 && report.apply_secs > 0.0 {
        out.push(format!(
            "downloads: {} files, {} ({:.1} files per second, {} per second)",
            report.downloaded,
            human_bytes(report.bytes_down),
            report.downloaded as f64 / report.apply_secs,
            human_bytes((report.bytes_down as f64 / report.apply_secs) as u64)
        ));
    }
    if report.uploaded > 0 && report.push_secs > 0.0 {
        out.push(format!(
            "uploads: {} files, {} ({:.1} files per second, {} per second)",
            report.uploaded,
            human_bytes(report.bytes_up),
            report.uploaded as f64 / report.push_secs,
            human_bytes((report.bytes_up as f64 / report.push_secs) as u64)
        ));
    }
    out.push(format!(
        "network: {} requests, {} retried after errors, asked to slow down {} times (waited {} in total)",
        net.requests,
        net.retries,
        net.throttled,
        human_duration(net.throttle_wait_ms as f64 / 1000.0)
    ));
    if net.pace_cuts > 0 {
        out.push(format!(
            "pacing: slowed down {} times; now sending at most {:.1} requests a second",
            net.pace_cuts, net.pace
        ));
    }
    if let Some(peak) = sysinfo::peak_memory_bytes() {
        out.push(format!("memory: peak {}", human_bytes(peak)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_timestamped_lines_and_formats_durations() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::open(dir.path()).unwrap();
        log.line("first");
        log.line("second line");
        let text = std::fs::read_to_string(&log.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("  first"));
        assert!(lines[1].contains("Z  second line"));
        assert_eq!(human_duration(4.25), "4.2s");
        assert_eq!(human_duration(125.0), "2m 05s");
        assert_eq!(human_duration(7384.0), "2h 03m");
    }
}
