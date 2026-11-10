//! An in-memory file server for testing the engine without Microsoft 365.
#![allow(dead_code)]

use drivecraft::quickxor::hash_bytes;
use drivecraft::remote::*;
use std::collections::HashMap;
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

impl FakeRemote {
    pub fn new() -> Self {
        let mut st = State {
            page_size: 1000,
            ..Default::default()
        };
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
                return Ok(DeltaPage {
                    items: st.log[from..].to_vec(),
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

    async fn children(&self, _drive: &str, parent_id: &str) -> RemoteResult<Vec<RemoteItem>> {
        let st = self.st.lock().unwrap();
        Ok(st
            .items
            .values()
            .filter(|e| e.item.parent_id.as_deref() == Some(parent_id))
            .map(|e| e.item.clone())
            .collect())
    }
}
