//! The per-library seed: a snapshot of what the server holds, stored in the library
//! itself so that another computer can skip the first full scan.
//!
//! A seed is only a starting point. A computer that imports one asks the server for
//! everything that changed since the seed was made, and from then on keeps itself
//! current on its own. A seed carries the server part of the catalog only: nothing
//! about any one computer, and no settings.
//!
//! Seeds live in a folder named `.drivecraft` at the top of the library. DriveCraft
//! never synchronizes that folder as content.
//!
//! Whoever can open the library can read its seed, but not everyone can open
//! everything in a library. When the person publishing can read everyone's
//! permissions (an owner of the library), the seed lists every item and, for each one
//! with permissions of its own, who may open it; a computer that starts from the seed
//! works out from that what its own user may open and removes the rest from its
//! catalog, asking the server whenever the list cannot settle it. Anyone who can read
//! the seed can see those names and permissions. When the person publishing cannot
//! read everyone's permissions, each restricted item is left out instead, with
//! everything inside it, and only its identifier is kept for each computer to ask the
//! server about with its own user's sign-in.

use crate::catalog::{Catalog, Connection, SeedInfo};
use crate::remote::{ItemKind, Remote, RemoteItem, UploadTarget};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The folder in the library that holds seeds.
pub const FOLDER: &str = ".drivecraft";
const PREFIX: &str = "seed-";
const SUFFIX: &str = ".sqlite.gz";
const README: &str = "About this folder.txt";
const README_TEXT: &str = "This folder is maintained by DriveCraft.\r\n\r\n\
It holds a snapshot of this library's file list, called a seed. When another\r\n\
computer connects to this library with DriveCraft, it reads the seed so that it\r\n\
does not have to scan the whole library from the beginning.\r\n\r\n\
The seed lists the names, sizes and identifiers of the files in the library. It\r\n\
does not contain the files themselves. When it was made by an owner of the\r\n\
library, it also lists who may open each folder and file that has permissions of\r\n\
its own; otherwise those are left out of it. It is safe to delete this folder; the\r\n\
next computer to connect will simply do a full scan.\r\n";

/// True for the seed folder and anything inside it.
pub fn is_seed_path(rel: &str) -> bool {
    rel == FOLDER || rel.starts_with(&format!("{FOLDER}/"))
}

fn is_seed_file(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

fn gzip(src: &Path, dest: &Path) -> Result<()> {
    let mut input = std::fs::File::open(src)?;
    let out = std::fs::File::create(dest)?;
    let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        enc.write_all(&buf[..n])?;
    }
    enc.finish()?;
    Ok(())
}

/// Unpack a seed, refusing one that expands beyond `limit` bytes.
fn gunzip(src: &Path, dest: &Path, limit: u64) -> Result<()> {
    let mut dec = flate2::read::GzDecoder::new(std::fs::File::open(src)?);
    let mut out = std::fs::File::create(dest)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = dec.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            bail!("the seed is larger than DriveCraft accepts");
        }
        out.write_all(&buf[..n])?;
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Published {
    pub items: u64,
    /// Parts of the library with permissions of their own, left out of the seed.
    pub withheld: u64,
    pub bytes: u64,
    pub name: String,
    /// Older seeds that were removed.
    pub replaced: u64,
    /// The seed carries who may open each item with permissions of its own.
    pub permissions: bool,
    /// Why the permissions could not be included, in plain words.
    pub note: Option<String>,
}

fn work_file(work_dir: &Path, conn: &Connection, ext: &str) -> PathBuf {
    work_dir.join(format!("seed-{}.{ext}", conn.id))
}

/// Make a seed from this computer's catalog and store it in the library.
///
/// The new seed is uploaded under a new name and older ones are removed only after
/// it is safely there, so another computer never finds a half-written seed.
pub async fn publish<R: Remote>(
    cat: &Catalog,
    remote: &R,
    conn: &Connection,
    work_dir: &Path,
) -> Result<Published> {
    let mut conn = cat.connection_by_id(conn.id)?;
    if conn.delta_link.is_none() {
        bail!("this library has not been fully scanned on this computer yet; synchronize it first");
    }
    if !conn.perms_known {
        // The catalog was filled before DriveCraft asked which items have permissions
        // of their own (or it came from a seed). Without that, the seed could show
        // everyone the names of things only some may open, so read the library's
        // list once more, noting only that.
        crate::permissions::survey(cat, remote, &conn).await?;
        conn = cat.connection_by_id(conn.id)?;
    }
    // Bring the bookmark up to this moment first. Otherwise the seed would carry a
    // bookmark from before this computer's own last uploads, and every computer that
    // imports it would have to read all of those changes again.
    loop {
        let link = conn.cursor.clone().or(conn.delta_link.clone());
        let page = remote
            .delta(&conn.drive_id, link.as_deref())
            .await
            .context("reading the latest changes before making the seed; synchronize the library and try again")?;
        cat.apply_delta_page(
            &conn,
            &page.items,
            page.next_link.as_deref(),
            page.delta_link.as_deref(),
        )?;
        conn = cat.connection_by_id(conn.id)?;
        if page.delta_link.is_some() {
            break;
        }
        if page.next_link.is_none() {
            bail!("the server's change feed ended without a bookmark");
        }
    }
    let root = cat
        .remote_root(conn.id)?
        .context("the library's root folder is not in the catalog yet; synchronize it first")?;
    std::fs::create_dir_all(work_dir)?;
    let plain = work_file(work_dir, &conn, "sqlite");
    let packed = work_file(work_dir, &conn, "sqlite.gz");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Who may open each restricted item, read now with the publisher's sign-in. Only
    // an owner sees everyone's permissions; anyone else would record lists that leave
    // people out, so then restricted items are left out of the seed instead.
    let note = match crate::permissions::read_all(cat, remote, &conn, |_, _| {}).await {
        Ok(read) if read.full => None,
        Ok(_) => Some(
            "You are not an owner of this library, so the server shows you only the permissions that apply to you. Folders and files with permissions of their own were left out of the seed instead."
                .to_string(),
        ),
        Err(e) => Some(format!(
            "The permissions could not be read ({e:#}). Folders and files with permissions of their own were left out of the seed instead."
        )),
    };
    let info = cat.export_seed(&conn, &plain, now, note.is_none())?;
    gzip(&plain, &packed)?;
    let _ = std::fs::remove_file(&plain);
    let bytes = std::fs::metadata(&packed)?.len();

    let folder = remote
        .create_folder(&conn.drive_id, &root.id, FOLDER)
        .await
        .context("creating the seed folder in the library")?;
    let name = format!("{PREFIX}{now}{SUFFIX}");
    let uploaded = remote
        .upload(
            &conn.drive_id,
            UploadTarget::New {
                parent_id: &folder.id,
                name: &name,
            },
            &packed,
            None,
        )
        .await
        .context("uploading the seed")?;
    let _ = std::fs::remove_file(&packed);

    // Housekeeping: a note for people who find the folder, and the old seeds.
    let existing = remote.children(&conn.drive_id, &folder.id).await.unwrap_or_default();
    if !existing.iter().any(|i| i.name == README) {
        let note = work_dir.join("seed-readme.txt");
        std::fs::write(&note, README_TEXT)?;
        let _ = remote
            .upload(
                &conn.drive_id,
                UploadTarget::New {
                    parent_id: &folder.id,
                    name: README,
                },
                &note,
                None,
            )
            .await;
        let _ = std::fs::remove_file(&note);
    }
    let mut replaced = 0;
    for old in existing.iter().filter(|i| is_seed_file(&i.name) && i.id != uploaded.id) {
        if remote.delete(&conn.drive_id, &old.id, None).await.is_ok() {
            replaced += 1;
        }
    }
    Ok(Published {
        items: info.items,
        withheld: info.withheld,
        bytes,
        name,
        replaced,
        permissions: info.permissions,
        note,
    })
}


/// The newest seed in the library, if it has one.
async fn newest<R: Remote>(remote: &R, drive_id: &str) -> Result<Option<RemoteItem>> {
    let Some(folder) = remote.child(drive_id, "root", FOLDER).await? else {
        return Ok(None);
    };
    if folder.kind != ItemKind::Folder {
        return Ok(None);
    }
    let mut seeds: Vec<RemoteItem> = remote
        .children(drive_id, &folder.id)
        .await?
        .into_iter()
        .filter(|i| i.kind == ItemKind::File && is_seed_file(&i.name))
        .collect();
    seeds.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(seeds.pop())
}

/// Look for a seed in the library and load it into a connection that has never been
/// scanned. Returns `None` when there is no seed.
pub async fn fetch<R: Remote>(
    cat: &Catalog,
    remote: &R,
    conn: &Connection,
    work_dir: &Path,
) -> Result<Option<SeedInfo>> {
    let Some(seed) = newest(remote, &conn.drive_id).await? else {
        return Ok(None);
    };
    std::fs::create_dir_all(work_dir)?;
    let packed = work_file(work_dir, conn, "download.gz");
    let plain = work_file(work_dir, conn, "download.sqlite");
    let result = async {
        remote.download(&conn.drive_id, &seed.id, &packed).await?;
        // A seed describes file names; even millions of them stay far below this.
        gunzip(&packed, &plain, 8 * 1024 * 1024 * 1024)?;
        cat.import_seed(conn, &plain)
    }
    .await;
    let _ = std::fs::remove_file(&packed);
    let _ = std::fs::remove_file(&plain);
    result.map(Some)
}
