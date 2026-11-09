//! The boundary between the sync engine and a file server.
//!
//! Microsoft Graph is the first implementation. The engine is written against this
//! trait so that it can be tested without a server and so that ShareCraft can be
//! added later as a second implementation.

use std::future::Future;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    File,
    Folder,
    /// Something DriveCraft does not synchronize, such as a OneNote notebook.
    Other,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::File => "file",
            ItemKind::Folder => "folder",
            ItemKind::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "file" => ItemKind::File,
            "folder" => ItemKind::Folder,
            _ => ItemKind::Other,
        }
    }
}

/// One file or folder as the server describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteItem {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: ItemKind,
    pub size: u64,
    /// Changes whenever anything about the item changes. Used for conditional writes.
    pub etag: Option<String>,
    /// Changes when the content changes.
    pub ctag: Option<String>,
    /// QuickXorHash of the content, base64.
    pub hash: Option<String>,
    /// Last modified time, nanoseconds since the Unix epoch.
    pub mtime_ns: Option<i64>,
    pub is_root: bool,
    pub deleted: bool,
}

#[derive(Debug, Default)]
pub struct DeltaPage {
    pub items: Vec<RemoteItem>,
    /// More pages follow; pass this back to continue.
    pub next_link: Option<String>,
    /// The enumeration is complete; pass this back next time to get only changes.
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Downloaded {
    pub size: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Copy)]
pub enum UploadTarget<'a> {
    /// A new file. Fails with `Conflict` if the name is already taken.
    New { parent_id: &'a str, name: &'a str },
    /// A new version of an existing file. Fails with `Conflict` if the server's copy
    /// has changed since `etag`.
    Replace {
        item_id: &'a str,
        etag: Option<&'a str>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("the server asked for a full rescan")]
    ResyncRequired,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("not signed in, or the sign-in has expired: {0}")]
    Unauthorized(String),
    #[error("{0}")]
    Other(String),
}

impl RemoteError {
    pub fn other(e: impl std::fmt::Display) -> Self {
        RemoteError::Other(e.to_string())
    }
}

pub type RemoteResult<T> = Result<T, RemoteError>;

pub trait Remote: Send + Sync {
    /// One page of the change feed for a drive. `link` is `None` to start a full
    /// enumeration, otherwise a link returned by a previous page.
    fn delta(
        &self,
        drive_id: &str,
        link: Option<&str>,
    ) -> impl Future<Output = RemoteResult<DeltaPage>> + Send;

    /// Download a file's content to `dest`, replacing it.
    fn download(
        &self,
        drive_id: &str,
        item_id: &str,
        dest: &Path,
    ) -> impl Future<Output = RemoteResult<Downloaded>> + Send;

    fn upload(
        &self,
        drive_id: &str,
        target: UploadTarget<'_>,
        src: &Path,
        mtime_ns: Option<i64>,
    ) -> impl Future<Output = RemoteResult<RemoteItem>> + Send;

    /// Create a folder, or return the existing one of that name.
    fn create_folder(
        &self,
        drive_id: &str,
        parent_id: &str,
        name: &str,
    ) -> impl Future<Output = RemoteResult<RemoteItem>> + Send;

    fn delete(
        &self,
        drive_id: &str,
        item_id: &str,
        etag: Option<&str>,
    ) -> impl Future<Output = RemoteResult<()>> + Send;

    fn move_item(
        &self,
        drive_id: &str,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: Option<&str>,
    ) -> impl Future<Output = RemoteResult<RemoteItem>> + Send;
}
