//! An in-memory file server for testing the engine without Microsoft 365.
#![allow(dead_code)]

use drivecraft::quickxor::hash_bytes;
use drivecraft::remote::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

struct Entry {
    item: RemoteItem,
    content: Vec<u8>,
}

#[derive(Default)]
struct State {
    items: HashMap<String, Entry>,
    log: Vec<RemoteItem>,
    next_id: u64,
    version: u64,
    writes: u64,
    downloads: u64,
    page_size: usize,
    expire_bookmarks: bool,
    refuse_bookmarks: bool,
    full_scans: u64,
    /// Items with permissions of their own, and whether the ordinary user may open them.
    protected: HashMap<String, bool>,
    /// The server is answering an ordinary user rather than the library's owner.
    as_user: bool,
    /// Downloads the server refuses outright.
    forbidden: HashSet<String>,
    /// The permissions recorded on items, by identifier; the library's own under "root".
    perms: HashMap<String, Vec<Permission>>,
    /// Who the ordinary user is.
    user: Identity,
    /// Items whose permissions the server will not show, even to the owner.
    perms_hidden: HashSet<String>,
    /// How many single items have been asked about.
    item_asks: u64,
}

pub struct FakeRemote {
    st: Mutex<State>,
}

impl State {
    fn bump(&mut self) -> u64 {
        self.version += 1;
        self.version
    }

    fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.items
            .values()
            .find(|e| e.item.parent_id.as_deref() == Some(parent) && e.item.name.eq_ignore_ascii_case(name))
            .map(|e| e.item.id.clone())
    }

    fn resolve(&self, path: &str) -> Option<String> {
        let mut cur = "root".to_string();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = self.child(&cur, seg)?;
        }
        Some(cur)
    }

    /// Whether whoever is asking may see the item: the owner sees everything; an
    /// ordinary user sees nothing inside a protected item they may not open.
    fn visible(&self, id: &str) -> bool {
        if !self.as_user {
            return true;
        }
        let mut cur = Some(id.to_string());
        for _ in 0..512 {
            let Some(c) = cur else { return true };
            if self.protected.get(&c) == Some(&false) {
                return false;
            }
            cur = self.items.get(&c).and_then(|e| e.item.parent_id.clone());
        }
        true
    }

    fn record(&mut self, id: &str) {
        let snap = self.items[id].item.clone();
        self.log.push(snap);
    }

    fn new_item(&mut self, parent: &str, name: &str, kind: ItemKind, content: Vec<u8>) -> String {
        self.next_id += 1;
        let id = format!("id{}", self.next_id);
        let v = self.bump();
        let item = RemoteItem {
            id: id.clone(),
            parent_id: Some(parent.to_string()),
            name: name.to_string(),
            kind,
            size: content.len() as u64,
            etag: Some(format!("e{v}")),
            ctag: Some(format!("c{v}")),
            hash: (kind == ItemKind::File).then(|| hash_bytes(&content)),
            mtime_ns: Some(1_700_000_000_000_000_000 + v as i64 * 1_000_000_000),
            is_root: false,
            deleted: false,
            own_permissions: false,
        };
        self.items.insert(id.clone(), Entry { item, content });
        self.record(&id);
        id
    }

    fn ensure_dirs(&mut self, path: &str) -> String {
        let mut cur = "root".to_string();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = match self.child(&cur, seg) {
                Some(id) => id,
                None => self.new_item(&cur, seg, ItemKind::Folder, vec![]),
            };
        }
        cur
    }

    fn set_content(&mut self, id: &str, content: Vec<u8>) {
        let v = self.bump();
        let e = self.items.get_mut(id).unwrap();
        e.item.size = content.len() as u64;
        e.item.hash = Some(hash_bytes(&content));
        e.item.etag = Some(format!("e{v}"));
        e.item.ctag = Some(format!("c{v}"));
        e.item.mtime_ns = Some(1_700_000_000_000_000_000 + v as i64 * 1_000_000_000);
        e.content = content;
        self.record(id);
    }

    fn remove_tree(&mut self, id: &str, log: bool) {
        let children: Vec<String> = self
            .items
            .values()
            .filter(|e| e.item.parent_id.as_deref() == Some(id))
            .map(|e| e.item.id.clone())
            .collect();
        for c in children {
            self.remove_tree(&c, log);
        }
        if let Some(e) = self.items.remove(id) {
            if log {
                let mut gone = e.item;
                gone.deleted = true;
                self.log.push(gone);
            }
        }
    }

    fn enumerate(&self) -> Vec<RemoteItem> {
        let mut out = vec![self.items["root"].item.clone()];
        let mut queue = vec!["root".to_string()];
        while let Some(parent) = queue.pop() {
            let mut kids: Vec<&Entry> = self
                .items
                .values()
                .filter(|e| e.item.parent_id.as_deref() == Some(parent.as_str()))
                .collect();
            kids.sort_by(|a, b| a.item.name.cmp(&b.item.name));
            for k in kids {
                if !self.visible(&k.item.id) {
                    continue;
                }
                out.push(k.item.clone());
                if k.item.kind == ItemKind::Folder {
                    queue.push(k.item.id.clone());
                }
            }
        }
        out
    }
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// A permission given to one person.
pub fn person(id: &str, name: &str, role: &str) -> Permission {
    Permission {
        perm_id: format!("p-{id}"),
        kind: PrincipalKind::User,
        principal_id: Some(id.into()),
        name: name.into(),
        email: Some(format!("{}@contoso.com", name.split(' ').next().unwrap_or(name).to_lowercase())),
        login: None,
        roles: vec![role.into()],
        link_scope: None,
        via_link: false,
    }
}

/// A permission given to a directory group.
pub fn group(id: &str, name: &str, role: &str) -> Permission {
    Permission { kind: PrincipalKind::Group, email: None, ..person(id, name, role) }
}

/// A permission given to one of the site's SharePoint groups.
pub fn site_group(id: &str, name: &str, role: &str) -> Permission {
    Permission { kind: PrincipalKind::SiteGroup, email: None, login: Some(name.into()), ..person(id, name, role) }
}

impl FakeRemote {
    pub fn new() -> Self {
        let mut st = State {
            page_size: 1000,
            user: Identity {
                user_id: "alex-id".into(),
                name: "Alex Morgan".into(),
                groups: Some(vec!["sales-team".into()]),
            },
            ..Default::default()
        };
        st.perms.insert(
            "root".into(),
            vec![site_group("3", "Contoso Owners", "owner"), site_group("5", "Contoso Members", "write")],
        );
        st.items.insert(
            "root".into(),
            Entry {
                item: RemoteItem {
                    id: "root".into(),
                    parent_id: None,
                    name: "root".into(),
                    kind: ItemKind::Folder,
                    size: 0,
                    etag: Some("e0".into()),
                    ctag: Some("c0".into()),
                    hash: None,
                    mtime_ns: None,
                    is_root: true,
                    deleted: false,
                    own_permissions: false,
                },
                content: vec![],
            },
        );
        Self { st: Mutex::new(st) }
    }

    // ---- what "someone else" does on the server ----

    pub fn put(&self, path: &str, content: &[u8]) -> String {
        let mut st = self.st.lock().unwrap();
        let (dir, name) = split(path);
        let parent = st.ensure_dirs(dir);
        match st.child(&parent, name) {
            Some(id) => {
                st.set_content(&id, content.to_vec());
                id
            }
            None => st.new_item(&parent, name, ItemKind::File, content.to_vec()),
        }
    }

    pub fn mkdir(&self, path: &str) -> String {
        self.st.lock().unwrap().ensure_dirs(path)
    }

    pub fn rename(&self, from: &str, to: &str) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(from).expect("source exists");
        let (dir, name) = split(to);
        let parent = st.ensure_dirs(dir);
        let v = st.bump();
        let e = st.items.get_mut(&id).unwrap();
        e.item.parent_id = Some(parent);
        e.item.name = name.to_string();
        e.item.etag = Some(format!("e{v}"));
        st.record(&id);
    }

    pub fn remove(&self, path: &str) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(path).expect("exists");
        st.remove_tree(&id, true);
    }

    /// Remove without telling the change feed, as if the bookmark had been lost.
    pub fn remove_silently(&self, path: &str) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(path).expect("exists");
        st.remove_tree(&id, false);
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let st = self.st.lock().unwrap();
        let id = st.resolve(path)?;
        Some(st.items[&id].content.clone())
    }

    pub fn id_of(&self, path: &str) -> Option<String> {
        self.st.lock().unwrap().resolve(path)
    }

    pub fn exists(&self, path: &str) -> bool {
        self.id_of(path).is_some()
    }

    pub fn file_count(&self) -> usize {
        let st = self.st.lock().unwrap();
        st.items.values().filter(|e| e.item.kind == ItemKind::File).count()
    }

    /// Number of calls that changed the server through the client interface.
    pub fn writes(&self) -> u64 {
        self.st.lock().unwrap().writes
    }

    pub fn downloads(&self) -> u64 {
        self.st.lock().unwrap().downloads
    }

    /// The current description of an item, by identifier.
    pub fn item(&self, id: &str) -> Option<RemoteItem> {
        self.st.lock().unwrap().items.get(id).map(|e| e.item.clone())
    }

    pub fn content(&self, id: &str) -> Option<Vec<u8>> {
        self.st.lock().unwrap().items.get(id).map(|e| e.content.clone())
    }

    pub fn mtime_of(&self, path: &str) -> Option<i64> {
        let id = self.id_of(path)?;
        self.item(&id)?.mtime_ns
    }

    pub fn set_page_size(&self, n: usize) {
        self.st.lock().unwrap().page_size = n;
    }

    /// The next use of a bookmark is refused outright, as it might be when the
    /// bookmark was made by a different person.
    pub fn refuse_bookmarks(&self) {
        self.st.lock().unwrap().refuse_bookmarks = true;
    }

    /// How many times the whole library has been enumerated from the start.
    pub fn full_scans(&self) -> u64 {
        self.st.lock().unwrap().full_scans
    }

    /// Give an item permissions of its own. `user_may_open` says whether the ordinary
    /// user is among those allowed to open it.
    pub fn protect(&self, path: &str, user_may_open: bool) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(path).expect("exists");
        let v = st.bump();
        let e = st.items.get_mut(&id).unwrap();
        e.item.own_permissions = true;
        e.item.etag = Some(format!("e{v}"));
        st.protected.insert(id.clone(), user_may_open);
        st.record(&id);
    }

    /// The ordinary user is given access to a protected item; the change feed then
    /// mentions it to them.
    pub fn grant(&self, path: &str) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(path).expect("exists");
        st.protected.insert(id.clone(), true);
        st.record(&id);
    }

    /// Record who may open an item, as the server would list it to an owner.
    pub fn set_permissions(&self, path: &str, perms: Vec<Permission>) {
        let mut st = self.st.lock().unwrap();
        let id = if path.is_empty() { "root".to_string() } else { st.resolve(path).expect("exists") };
        st.perms.insert(id, perms);
    }

    /// The server will not show this item's permissions to anyone.
    pub fn hide_permissions(&self, path: &str) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(path).expect("exists");
        st.perms_hidden.insert(id);
    }

    /// Who the ordinary user is, and the groups the server says they are in.
    pub fn set_user(&self, user_id: &str, groups: Option<Vec<&str>>) {
        let mut st = self.st.lock().unwrap();
        st.user.user_id = user_id.into();
        st.user.groups = groups.map(|g| g.into_iter().map(String::from).collect());
    }

    /// How many single items have been asked about, one by one.
    pub fn item_asks(&self) -> u64 {
        self.st.lock().unwrap().item_asks
    }

    /// From now on the server answers as an ordinary user, or as the owner.
    pub fn as_user(&self, user: bool) {
        self.st.lock().unwrap().as_user = user;
    }

    /// Refuse to hand out a file's content (HTTP 403), or stop refusing.
    pub fn forbid_download(&self, path: &str, refuse: bool) {
        let mut st = self.st.lock().unwrap();
        let id = st.resolve(path).expect("exists");
        if refuse {
            st.forbidden.insert(id);
        } else {
            st.forbidden.remove(&id);
        }
    }

    pub fn expire_bookmarks(&self) {
        self.st.lock().unwrap().expire_bookmarks = true;
    }
}

impl Remote for FakeRemote {
    async fn delta(&self, _drive: &str, link: Option<&str>) -> RemoteResult<DeltaPage> {
        let mut st = self.st.lock().unwrap();
        let (offset, mark) = match link {
            None => {
                st.full_scans += 1;
                (0usize, st.log.len())
            }
            Some(l) if l.starts_with("E:") => {
                let mut p = l[2..].split(':');
                (p.next().unwrap().parse().unwrap(), p.next().unwrap().parse().unwrap())
            }
            Some(l) if l.starts_with("L:") => {
                if st.expire_bookmarks {
                    st.expire_bookmarks = false;
                    return Err(RemoteError::ResyncRequired);
                }
                if st.refuse_bookmarks {
                    st.refuse_bookmarks = false;
                    return Err(RemoteError::Other("access denied: this bookmark belongs to someone else".into()));
                }
                let from: usize = l[2..].parse().unwrap();
                let items = st.log[from..]
                    .iter()
                    .filter(|it| it.deleted || st.visible(&it.id))
                    .cloned()
                    .collect();
                return Ok(DeltaPage {
                    items,
                    next_link: None,
                    delta_link: Some(format!("L:{}", st.log.len())),
                });
            }
            Some(other) => return Err(RemoteError::Other(format!("bad link {other}"))),
        };
        let all = st.enumerate();
        let end = (offset + st.page_size).min(all.len());
        let items = all[offset..end].to_vec();
        if end < all.len() {
            Ok(DeltaPage {
                items,
                next_link: Some(format!("E:{end}:{mark}")),
                delta_link: None,
            })
        } else {
            Ok(DeltaPage {
                items,
                next_link: None,
                delta_link: Some(format!("L:{mark}")),
            })
        }
    }

    async fn download(&self, _drive: &str, item_id: &str, dest: &Path) -> RemoteResult<Downloaded> {
        let content = {
            let mut st = self.st.lock().unwrap();
            st.downloads += 1;
            if st.forbidden.contains(item_id) {
                return Err(RemoteError::Forbidden(item_id.into()));
            }
            if !st.visible(item_id) {
                return Err(RemoteError::NotFound(item_id.into()));
            }
            st.items
                .get(item_id)
                .ok_or_else(|| RemoteError::NotFound(item_id.into()))?
                .content
                .clone()
        };
        std::fs::write(dest, &content).map_err(RemoteError::other)?;
        Ok(Downloaded {
            size: content.len() as u64,
            hash: hash_bytes(&content),
        })
    }

    async fn upload(
        &self,
        _drive: &str,
        target: UploadTarget<'_>,
        src: &Path,
        mtime_ns: Option<i64>,
    ) -> RemoteResult<RemoteItem> {
        let content = std::fs::read(src).map_err(RemoteError::other)?;
        let mut st = self.st.lock().unwrap();
        st.writes += 1;
        let id = match target {
            UploadTarget::New { parent_id, name } => {
                if !st.items.contains_key(parent_id) {
                    return Err(RemoteError::NotFound(parent_id.into()));
                }
                if st.child(parent_id, name).is_some() {
                    return Err(RemoteError::Conflict(format!("{name} exists")));
                }
                st.new_item(parent_id, name, ItemKind::File, content)
            }
            UploadTarget::Replace { item_id, etag } => {
                let cur = st
                    .items
                    .get(item_id)
                    .ok_or_else(|| RemoteError::NotFound(item_id.into()))?;
                if etag.is_some() && cur.item.etag.as_deref() != etag {
                    return Err(RemoteError::Conflict("changed on the server".into()));
                }
                st.set_content(item_id, content);
                item_id.to_string()
            }
        };
        if let Some(m) = mtime_ns {
            st.items.get_mut(&id).unwrap().item.mtime_ns = Some(m);
        }
        Ok(st.items[&id].item.clone())
    }

    async fn create_folder(&self, _drive: &str, parent_id: &str, name: &str) -> RemoteResult<RemoteItem> {
        let mut st = self.st.lock().unwrap();
        st.writes += 1;
        let id = match st.child(parent_id, name) {
            Some(id) => id,
            None => st.new_item(parent_id, name, ItemKind::Folder, vec![]),
        };
        Ok(st.items[&id].item.clone())
    }

    async fn delete(&self, _drive: &str, item_id: &str, etag: Option<&str>) -> RemoteResult<()> {
        let mut st = self.st.lock().unwrap();
        st.writes += 1;
        let cur = st
            .items
            .get(item_id)
            .ok_or_else(|| RemoteError::NotFound(item_id.into()))?;
        if etag.is_some() && cur.item.etag.as_deref() != etag {
            return Err(RemoteError::Conflict("changed on the server".into()));
        }
        st.remove_tree(item_id, true);
        Ok(())
    }

    async fn move_item(
        &self,
        _drive: &str,
        item_id: &str,
        new_parent_id: Option<&str>,
        new_name: Option<&str>,
    ) -> RemoteResult<RemoteItem> {
        let mut st = self.st.lock().unwrap();
        st.writes += 1;
        let v = st.bump();
        let e = st
            .items
            .get_mut(item_id)
            .ok_or_else(|| RemoteError::NotFound(item_id.into()))?;
        if let Some(p) = new_parent_id {
            e.item.parent_id = Some(p.to_string());
        }
        if let Some(n) = new_name {
            e.item.name = n.to_string();
        }
        e.item.etag = Some(format!("e{v}"));
        st.record(item_id);
        Ok(st.items[item_id].item.clone())
    }

    async fn child(&self, _drive: &str, parent_id: &str, name: &str) -> RemoteResult<Option<RemoteItem>> {
        let st = self.st.lock().unwrap();
        Ok(st.child(parent_id, name).map(|id| st.items[&id].item.clone()))
    }

    async fn item(&self, _drive: &str, item_id: &str) -> RemoteResult<RemoteItem> {
        let mut st = self.st.lock().unwrap();
        st.item_asks += 1;
        match st.items.get(item_id) {
            Some(e) if st.visible(item_id) => Ok(e.item.clone()),
            _ => Err(RemoteError::NotFound(item_id.into())),
        }
    }

    async fn permissions(
        &self,
        _drive: &str,
        item_ids: &[String],
    ) -> RemoteResult<Vec<(String, RemoteResult<Vec<Permission>>)>> {
        let st = self.st.lock().unwrap();
        Ok(item_ids
            .iter()
            .map(|id| {
                let key = if st.items.get(id).is_some_and(|e| e.item.is_root) { "root" } else { id.as_str() };
                let answer = if !st.items.contains_key(id) || !st.visible(id) {
                    Err(RemoteError::NotFound(id.clone()))
                } else if st.perms_hidden.contains(id) {
                    Err(RemoteError::Forbidden(id.clone()))
                } else {
                    let all = st.perms.get(key).cloned().unwrap_or_default();
                    // Someone who is not an owner sees only what applies to them.
                    Ok(if st.as_user {
                        all.into_iter()
                            .filter(|p| p.principal_id.as_deref() == Some(st.user.user_id.as_str()))
                            .collect()
                    } else {
                        all
                    })
                };
                (id.clone(), answer)
            })
            .collect())
    }

    async fn identity(&self) -> RemoteResult<Identity> {
        let st = self.st.lock().unwrap();
        Ok(if st.as_user {
            st.user.clone()
        } else {
            Identity { user_id: "owner-id".into(), name: "Library Owner".into(), groups: Some(vec![]) }
        })
    }

    async fn children(&self, _drive: &str, parent_id: &str) -> RemoteResult<Vec<RemoteItem>> {
        let st = self.st.lock().unwrap();
        if !st.items.contains_key(parent_id) || !st.visible(parent_id) {
            return Err(RemoteError::NotFound(parent_id.into()));
        }
        Ok(st
            .items
            .values()
            .filter(|e| e.item.parent_id.as_deref() == Some(parent_id) && st.visible(&e.item.id))
            .map(|e| e.item.clone())
            .collect())
    }
}
