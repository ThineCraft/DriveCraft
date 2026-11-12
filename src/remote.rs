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
    /// The item has permissions of its own instead of taking them from its folder, so
    /// some people who can open the library may not be able to open it. A seed leaves
    /// such items, and everything inside them, out.
    pub own_permissions: bool,
}

/// Who or what a permission is given to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// One person, known by their directory identifier.
    User,
    /// A Microsoft 365 or security group, known by its directory identifier.
    Group,
    /// A SharePoint group of the site, such as its Members or Visitors. Who is in it
    /// is kept by SharePoint, and DriveCraft cannot read that.
    SiteGroup,
    /// Someone known only to SharePoint, such as "Everyone except external users".
    SiteUser,
    /// A sharing link; anyone who has it may open the item, as far as its scope allows.
    Link,
    /// A program rather than a person.
    Application,
    Unknown,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::User => "user",
            PrincipalKind::Group => "group",
            PrincipalKind::SiteGroup => "site_group",
            PrincipalKind::SiteUser => "site_user",
            PrincipalKind::Link => "link",
            PrincipalKind::Application => "application",
            PrincipalKind::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "user" => PrincipalKind::User,
            "group" => PrincipalKind::Group,
            "site_group" => PrincipalKind::SiteGroup,
            "site_user" => PrincipalKind::SiteUser,
            "link" => PrincipalKind::Link,
            "application" => PrincipalKind::Application,
            _ => PrincipalKind::Unknown,
        }
    }
    /// How the kind is said to people.
    pub fn said(self) -> &'static str {
        match self {
            PrincipalKind::User => "person",
            PrincipalKind::Group => "group",
            PrincipalKind::SiteGroup => "SharePoint group",
            PrincipalKind::SiteUser => "SharePoint",
            PrincipalKind::Link => "sharing link",
            PrincipalKind::Application => "app",
            PrincipalKind::Unknown => "other",
        }
    }
}

/// One permission on one item: who has it and what they may do. A sharing link given
/// to named people is recorded once for the link and once for each person.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Permission {
    /// The server's identifier of the permission (shared by a link and its people).
    pub perm_id: String,
    pub kind: PrincipalKind,
    /// The directory or SharePoint identifier of the person or group, when given.
    pub principal_id: Option<String>,
    pub name: String,
    pub email: Option<String>,
    /// A SharePoint login name, when given.
    pub login: Option<String>,
    /// "read", "write", "owner" and the like.
    pub roles: Vec<String>,
    /// For a link: "anonymous", "organization", "users", "existingAccess".
    pub link_scope: Option<String>,
    /// Given through a sharing link rather than directly.
    pub via_link: bool,
}

/// Who is signed in, as far as permissions are concerned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Identity {
    pub user_id: String,
    pub name: String,
    /// The directory groups the person belongs to, directly or through other groups.
    /// `None` when the server would not say.
    pub groups: Option<Vec<String>>,
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
    /// The server knows the item but will not let the signed-in person have it.
    #[error("access denied: {0}")]
    Forbidden(String),
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

    /// A folder's child by name, or `None`. `parent_id` may be `root`.
    fn child(
        &self,
        drive_id: &str,
        parent_id: &str,
        name: &str,
    ) -> impl Future<Output = RemoteResult<Option<RemoteItem>>> + Send;

    /// One item by its identifier, as the signed-in person sees it. An item they may
    /// not open is `NotFound` or `Forbidden`, depending on the server.
    fn item(
        &self,
        drive_id: &str,
        item_id: &str,
    ) -> impl Future<Output = RemoteResult<RemoteItem>> + Send;

    /// The permissions of several items, each with its own answer: an item whose
    /// permissions the server will not show is `Forbidden` (or `NotFound`).
    fn permissions(
        &self,
        drive_id: &str,
        item_ids: &[String],
    ) -> impl Future<Output = RemoteResult<Vec<(String, RemoteResult<Vec<Permission>>)>>> + Send;

    /// Who is signed in, with their group memberships if the server will say.
    fn identity(&self) -> impl Future<Output = RemoteResult<Identity>> + Send;

    /// Everything directly inside a folder.
    fn children(
        &self,
        drive_id: &str,
        parent_id: &str,
    ) -> impl Future<Output = RemoteResult<Vec<RemoteItem>>> + Send;
}
