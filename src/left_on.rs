//! What DriveCraft had switched on when it was last removed from this computer.
//!
//! Removing DriveCraft puts back what it changed outside its own folders: the
//! folders it listed in File Explorer, the browser's Sync button, and starting with
//! Windows. An upgrade removes the old version before installing the new one, so
//! without a record those settings would be lost on every upgrade. Removal writes
//! them down here first; the next DriveCraft to start puts them back and deletes the
//! record. The record lives in DriveCraft's data folder, which removal leaves alone.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A folder DriveCraft listed in File Explorer, with the name and icon it had there.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Listed {
    pub folder: PathBuf,
    pub name: String,
    pub icon: String,
}

/// Everything DriveCraft had switched on outside its own folders.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LeftOn {
    #[serde(default)]
    pub explorer: Vec<Listed>,
    #[serde(default)]
    pub sync_button: bool,
    #[serde(default)]
    pub start_with_windows: bool,
}

impl LeftOn {
    /// True when there is nothing to put back.
    pub fn is_empty(&self) -> bool {
        self.explorer.is_empty() && !self.sync_button && !self.start_with_windows
    }
}

fn record(dir: &Path) -> PathBuf {
    dir.join("left-on-at-removal.json")
}

/// Write down what is switched on, in DriveCraft's data folder `dir`. Nothing is
/// written when nothing is on, and an older record is removed.
pub fn write(dir: &Path, left: &LeftOn) -> Result<()> {
    let file = record(dir);
    if left.is_empty() {
        let _ = std::fs::remove_file(&file);
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(&file, serde_json::to_string_pretty(left)?)?;
    Ok(())
}

/// What was switched on at the last removal, if a record is waiting.
pub fn read(dir: &Path) -> Option<LeftOn> {
    let text = std::fs::read_to_string(record(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Delete the record once it has been acted on.
pub fn forget(dir: &Path) {
    let _ = std::fs::remove_file(record(dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LeftOn {
        LeftOn {
            explorer: vec![Listed {
                folder: PathBuf::from(r"C:\Work\Northwind"),
                name: "Northwind".into(),
                icon: r"C:\Icons\northwind.ico".into(),
            }],
            sync_button: true,
            start_with_windows: false,
        }
    }

    #[test]
    fn what_was_written_is_read_back_until_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
        write(dir.path(), &sample()).unwrap();
        assert_eq!(read(dir.path()), Some(sample()));
        forget(dir.path());
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn nothing_switched_on_leaves_no_record_and_clears_an_old_one() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &sample()).unwrap();
        write(dir.path(), &LeftOn::default()).unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn a_record_missing_newer_fields_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(record(dir.path()), r#"{ "sync_button": true }"#).unwrap();
        assert_eq!(read(dir.path()), Some(LeftOn { sync_button: true, ..Default::default() }));
    }
}
