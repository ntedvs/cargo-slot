use crate::git::Repo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-repo cache dir holding a small pool of build slots.
pub struct Store {
    pub dir: PathBuf,
}

/// A build slot: a stable-path source mirror plus its target dir.
/// Holding a `Slot` means holding its exclusive lock.
pub struct Slot {
    pub id: u32,
    pub dir: PathBuf,
    _lock: File,
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

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Entry {
    pub size: u64,
    pub mtime: i128,
    /// blake3 of contents, or the link target for symlinks.
    pub hash: String,
    #[serde(default)]
    pub link: bool,
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
        let dir = home()?.join(format!("{name}-{}", &hash[..12]));
        fs::create_dir_all(&dir)?;
        Ok(Store { dir: dir.canonicalize()? })
    }

    pub fn ids(&self) -> Result<Vec<u32>> {
        let mut ids: Vec<u32> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("slot-")?.parse().ok())
            .collect();
        ids.sort();
        Ok(ids)
    }

    pub fn slot_dir(&self, id: u32) -> PathBuf {
        self.dir.join(format!("slot-{id}"))
    }

    pub fn info(&self, id: u32) -> Info {
        read_json(&self.slot_dir(id).join("info.json")).unwrap_or_default()
    }

    /// Try to lock a slot without waiting. `Ok(None)` means it is busy.
    pub fn try_lock(&self, id: u32) -> Result<Option<Slot>> {
        let dir = self.slot_dir(id);
        let f = lock_file(&dir)?;
        match f.try_lock() {
            Ok(()) => Ok(Some(Slot { id, dir, _lock: f })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    fn lock(&self, id: u32) -> Result<Slot> {
        let dir = self.slot_dir(id);
        let f = lock_file(&dir)?;
        f.lock()?;
        Ok(Slot { id, dir, _lock: f })
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
        let guard = File::create(self.dir.join("store.lock"))?;
        guard.lock()?;

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
            fs::create_dir_all(self.slot_dir(id))?;
            let slot = self.lock(id)?;
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
        self.lock(id)
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
    pub fn save(&self, info: &Info, manifest: &Manifest) -> Result<()> {
        write_json(&self.dir.join("manifest.json"), manifest)?;
        write_json(&self.dir.join("info.json"), info)
    }
}

fn lock_file(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir)?;
    Ok(fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock"))?)
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
/// The source may be mid-build, so a partial clone is fine: cargo
/// rebuilds anything missing or inconsistent.
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
