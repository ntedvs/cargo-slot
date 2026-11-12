use crate::git::Repo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-repo cache dir holding a small pool of build slots.
///
/// Layout:
///   store.lock        held briefly to pick, create, list or delete slots
///   locks/slot-N      one lock per slot; never deleted, so its identity is stable
///   slot-N/           src/, target/, info.json, manifest.json
pub struct Store {
    pub dir: PathBuf,
}

/// A build slot. Holding a `Slot` means holding its exclusive lock.
pub struct Slot {
    pub id: u32,
    pub dir: PathBuf,
    _lock: File,
}

pub struct SlotStatus {
    pub id: u32,
    pub info: Info,
    pub busy: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Info {
    /// Worktree whose sources were last synced in.
    pub origin: Option<PathBuf>,
    /// HEAD of that worktree at sync time.
    pub head: String,
    /// Unix seconds of last use.
    pub last_used: u64,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// Source size and mtime at last sync.
    pub size: u64,
    pub mtime: i128,
    /// blake3 of contents, or the link target for symlinks.
    pub hash: String,
    #[serde(default)]
    pub link: bool,
    /// Source permission bits.
    #[serde(default)]
    pub mode: u32,
    /// Mirrored file's metadata right after we wrote or verified it.
    /// If it no longer matches, something else touched the file.
    #[serde(default)]
    pub dst: Option<Stamp>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    #[serde(default)]
    pub mode: u32,
    pub mtime: i128,
    pub ctime: i128,
}

pub type Manifest = BTreeMap<String, Entry>;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn home() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("CARGO_SLOT_HOME") {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(p).join("cargo-slot"));
    }
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".cache").join("cargo-slot"))
}

impl Store {
    pub fn open(repo: &Repo) -> Result<Self> {
        let name = repo
            .common_dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        let hash = blake3::hash(repo.common_dir.to_string_lossy().as_bytes()).to_hex();
        Self::at(home()?.join(format!("{name}-{}", &hash[..12])))
    }

    pub fn at(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(dir.join("locks"))?;
        Ok(Store { dir: dir.canonicalize()? })
    }

    fn store_lock(&self) -> Result<File> {
        let f = open_lock(&self.dir.join("store.lock"))?;
        f.lock()?;
        Ok(f)
    }

    fn ids(&self) -> Result<Vec<u32>> {
        let mut ids: Vec<u32> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("slot-")?.parse().ok())
            .collect();
        ids.sort();
        Ok(ids)
    }

    fn slot_dir(&self, id: u32) -> PathBuf {
        self.dir.join(format!("slot-{id}"))
    }

    pub fn info(&self, id: u32) -> Info {
        read_json(&self.slot_dir(id).join("info.json")).unwrap_or_default()
    }

    fn lock_path(&self, id: u32) -> PathBuf {
        self.dir.join("locks").join(format!("slot-{id}"))
    }

    /// Try to lock a slot without waiting. `Ok(None)` means it is busy.
    fn try_lock(&self, id: u32) -> Result<Option<Slot>> {
        let f = open_lock(&self.lock_path(id))?;
        match f.try_lock() {
            Ok(()) => Ok(Some(Slot { id, dir: self.slot_dir(id), _lock: f })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    fn lock(&self, id: u32) -> Result<Slot> {
        let f = open_lock(&self.lock_path(id))?;
        f.lock()?;
        Ok(Slot { id, dir: self.slot_dir(id), _lock: f })
    }

    /// Lower is better: how much work reusing slot `id` would take.
    fn score(&self, id: u32, repo: &Repo) -> u64 {
        let info = self.info(id);
        if info.origin.as_deref() == Some(repo.root.as_path()) && info.head == repo.head {
            return 0;
        }
        match repo.distance(&info.head) {
            Some(d) => 1 + d as u64,
            None => u64::MAX / 2,
        }
    }

    /// Pick the best slot for this worktree, creating or waiting as needed.
    pub fn acquire(&self, repo: &Repo, max: u32) -> Result<Slot> {
        loop {
            let guard = self.store_lock()?;
            let ids = self.ids()?;
            let mut idle = Vec::new();
            let mut busy = Vec::new();
            for &id in &ids {
                match self.try_lock(id)? {
                    Some(s) => idle.push((self.score(id, repo), s)),
                    None => busy.push((self.score(id, repo), id)),
                }
            }

            if !idle.is_empty() {
                idle.sort_by_key(|(s, slot)| (*s, slot.id));
                return Ok(idle.into_iter().next().unwrap().1);
            }

            busy.sort();
            if (ids.len() as u32) < max.max(1) {
                let id = ids.iter().max().map_or(1, |m| m + 1);
                let slot = self.try_lock(id)?.context("fresh slot lock is held")?;
                fs::create_dir(&slot.dir)?;
                drop(guard);
                if let Some(&(_, from)) = busy.first() {
                    eprintln!("cargo-slot: all slots busy, cloning slot {from} into slot {id}");
                    clone_slot(&self.slot_dir(from), &slot.dir)?;
                }
                return Ok(slot);
            }

            let (_, id) = busy[0];
            drop(guard);
            eprintln!("cargo-slot: all {} slots busy, waiting for slot {id}", ids.len());
            let slot = self.lock(id)?;
            // It may have been garbage collected while we waited.
            if slot.dir.is_dir() {
                return Ok(slot);
            }
        }
    }

    pub fn status(&self) -> Result<Vec<SlotStatus>> {
        let _guard = self.store_lock()?;
        self.ids()?
            .into_iter()
            .map(|id| Ok(SlotStatus { id, info: self.info(id), busy: self.try_lock(id)?.is_none() }))
            .collect()
    }

    /// Delete idle slots beyond the `keep` most recently used.
    /// Returns (id, removed) for each candidate; busy slots are skipped.
    pub fn gc(&self, keep: usize) -> Result<Vec<(u32, bool)>> {
        let _guard = self.store_lock()?;
        let mut ids = self.ids()?;
        ids.sort_by_key(|&id| std::cmp::Reverse(self.info(id).last_used));
        let mut out = Vec::new();
        for id in ids.into_iter().skip(keep) {
            match self.try_lock(id)? {
                Some(s) => {
                    fs::remove_dir_all(&s.dir)?;
                    out.push((id, true));
                }
                None => out.push((id, false)),
            }
        }
        Ok(out)
    }
}

impl Slot {
    pub fn src(&self) -> PathBuf {
        self.dir.join("src")
    }
    pub fn target(&self) -> PathBuf {
        self.dir.join("target")
    }
    pub fn manifest(&self) -> Manifest {
        read_json(&self.dir.join("manifest.json")).unwrap_or_default()
    }
    pub fn info(&self) -> Info {
        read_json(&self.dir.join("info.json")).unwrap_or_default()
    }
    pub fn save(&self, info: &Info, manifest: &Manifest) -> Result<()> {
        write_json(&self.dir.join("manifest.json"), manifest)?;
        write_json(&self.dir.join("info.json"), info)
    }
}

fn open_lock(p: &Path) -> Result<File> {
    Ok(fs::OpenOptions::new().create(true).truncate(false).write(true).open(p)?)
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(v)?)?;
    fs::rename(tmp, p)?;
    Ok(())
}

/// Copy-on-write clone of a slot's sources, target dir and metadata.
/// Falls back to a regular copy on filesystems without reflinks.
/// The source may be mid-build, so a partial clone is fine: sync verifies
/// every mirrored file, and cargo rebuilds anything missing or inconsistent.
fn clone_slot(from: &Path, to: &Path) -> Result<()> {
    for name in ["src", "target", "info.json", "manifest.json"] {
        let src = from.join(name);
        if !src.exists() {
            continue;
        }
        let mut cmd = Command::new("cp");
        if cfg!(target_os = "macos") {
            cmd.arg("-cRp");
        } else {
            cmd.args(["-a", "--reflink=auto"]);
        }
        let status = cmd.arg(&src).arg(to.join(name)).stderr(Stdio::null()).status()?;
        if !status.success() {
            eprintln!("cargo-slot: partial clone of {name} (source was busy), cargo will fill the gaps");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn fake_repo(root: &Path) -> Repo {
        Repo {
            root: root.to_path_buf(),
            git_dir: root.join(".git"),
            common_dir: root.join(".git"),
            head: String::new(),
            cwd_rel: PathBuf::new(),
        }
    }

    #[test]
    fn gc_skips_busy_and_keeps_lock_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("store")).unwrap();
        let repo = fake_repo(tmp.path());
        let a = store.acquire(&repo, 2).unwrap();
        let b = store.acquire(&repo, 2).unwrap();
        assert_ne!(a.id, b.id);
        drop(b);
        let out = store.gc(0).unwrap();
        assert!(out.contains(&(a.id, false)));
        assert_eq!(out.iter().filter(|(_, r)| *r).count(), 1);
        assert!(a.dir.is_dir());
        assert!(store.lock_path(1).exists() && store.lock_path(2).exists());
    }

    #[test]
    fn lock_identity_survives_gc() {
        // A waiter opens the slot lock, then GC deletes the slot and a new
        // holder recreates it. The waiter's handle must still conflict.
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("store")).unwrap();
        let repo = fake_repo(tmp.path());
        drop(store.acquire(&repo, 1).unwrap());
        let waiter = open_lock(&store.lock_path(1)).unwrap();
        assert_eq!(store.gc(0).unwrap(), vec![(1, true)]);
        let holder = store.acquire(&repo, 1).unwrap();
        assert_eq!(holder.id, 1);
        assert!(matches!(waiter.try_lock(), Err(TryLockError::WouldBlock)));
    }

    #[test]
    fn acquire_and_gc_never_share_a_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        Store::at(dir.clone()).unwrap();
        let held: Arc<Mutex<HashSet<u32>>> = Default::default();
        let deadline = Instant::now() + Duration::from_secs(2);

        std::thread::scope(|s| {
            for _ in 0..6 {
                let (dir, held) = (dir.clone(), held.clone());
                let root = tmp.path().to_path_buf();
                s.spawn(move || {
                    let store = Store::at(dir).unwrap();
                    let repo = fake_repo(&root);
                    while Instant::now() < deadline {
                        let slot = store.acquire(&repo, 2).unwrap();
                        assert!(held.lock().unwrap().insert(slot.id), "slot {} held twice", slot.id);
                        assert!(slot.dir.is_dir(), "slot {} deleted while held", slot.id);
                        fs::write(slot.dir.join("probe"), "x").unwrap();
                        std::thread::sleep(Duration::from_millis(1));
                        assert!(slot.dir.join("probe").exists(), "slot {} deleted while held", slot.id);
                        held.lock().unwrap().remove(&slot.id);
                    }
                });
            }
            let dir = dir.clone();
            s.spawn(move || {
                let store = Store::at(dir).unwrap();
                while Instant::now() < deadline {
                    store.gc(0).unwrap();
                }
            });
        });
    }
}
