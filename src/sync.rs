use crate::git::Repo;
use crate::slot::{Entry, Manifest};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Stats {
    pub files: usize,
    pub copied: usize,
    pub removed: usize,
}

enum Outcome {
    Same(Entry),
    Copied(Entry),
    Gone,
}

/// Make `dst` mirror the worktree's files, touching only files whose
/// content changed. Untouched files keep their mtimes, so cargo's
/// fingerprints stay fresh for everything that didn't change.
pub fn sync(repo: &Repo, dst: &Path, manifest: &mut Manifest, same_origin: bool) -> Result<Stats> {
    fs::create_dir_all(dst)?;
    let files = repo.files()?;
    let old = &*manifest;

    let results: Vec<(String, Outcome)> = files
        .par_iter()
        .map(|rel| {
            let out = sync_one(&repo.root, dst, rel, old.get(rel), same_origin)
                .with_context(|| format!("syncing {rel}"))?;
            Ok((rel.clone(), out))
        })
        .collect::<Result<_>>()?;

    let mut next = Manifest::new();
    let mut copied = 0;
    for (rel, out) in results {
        match out {
            Outcome::Same(e) => {
                next.insert(rel, e);
            }
            Outcome::Copied(e) => {
                copied += 1;
                next.insert(rel, e);
            }
            Outcome::Gone => {}
        }
    }

    let keep: HashSet<&str> = next.keys().map(String::as_str).collect();
    let mut removed = 0;
    for rel in old.keys().filter(|k| !keep.contains(k.as_str())) {
        let p = dst.join(rel);
        if fs::symlink_metadata(&p).is_ok() {
            fs::remove_file(&p).ok();
            removed += 1;
            prune_empty_parents(dst, &p);
        }
    }

    write_git_pointer(repo, dst)?;
    let files = next.len();
    *manifest = next;
    Ok(Stats { files, copied, removed })
}

fn sync_one(root: &Path, dst: &Path, rel: &str, old: Option<&Entry>, same_origin: bool) -> Result<Outcome> {
    let s = root.join(rel);
    let d = dst.join(rel);
    let md = match fs::symlink_metadata(&s) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Outcome::Gone),
        Err(e) => return Err(e.into()),
    };
    // Submodules show up as directories; they're not mirrored.
    if md.is_dir() {
        return Ok(Outcome::Gone);
    }
    let dmd = fs::symlink_metadata(&d).ok();

    if md.file_type().is_symlink() {
        let target = fs::read_link(&s)?;
        let entry = Entry { size: 0, mtime: 0, hash: target.to_string_lossy().into_owned(), link: true };
        if old == Some(&entry) && dmd.is_some_and(|m| m.file_type().is_symlink()) {
            return Ok(Outcome::Same(entry));
        }
        clear(&d)?;
        std::os::unix::fs::symlink(&target, &d)?;
        return Ok(Outcome::Copied(entry));
    }

    let size = md.len();
    let mtime = nanos(md.modified()?);
    let dst_ok = dmd.is_some_and(|m| m.is_file() && m.len() == size);

    if let Some(o) = old {
        if same_origin && dst_ok && !o.link && o.size == size && o.mtime == mtime {
            return Ok(Outcome::Same(o.clone()));
        }
    }

    let hash = blake3::Hasher::new().update_reader(File::open(&s)?)?.finalize().to_hex().to_string();
    let entry = Entry { size, mtime, hash, link: false };
    if let Some(o) = old {
        if dst_ok && !o.link && o.hash == entry.hash {
            return Ok(Outcome::Same(entry));
        }
    }

    clear(&d)?;
    let name = d.file_name().unwrap().to_string_lossy();
    let tmp = d.with_file_name(format!(".{name}.cargo-slot-tmp"));
    fs::copy(&s, &tmp)?;
    // fs::copy may clone and keep the source mtime. Cargo decides freshness
    // by mtime, so a changed file must look newer than any prior build.
    File::options().write(true).open(&tmp)?.set_modified(SystemTime::now())?;
    fs::rename(&tmp, &d)?;
    Ok(Outcome::Copied(entry))
}

/// Remove whatever is at `p` and make sure its parent exists.
fn clear(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p)?,
        Ok(_) => fs::remove_file(p)?,
        Err(_) => {}
    }
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn prune_empty_parents(root: &Path, p: &Path) {
    let mut cur = p.parent();
    while let Some(dir) = cur {
        if dir == root || fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

/// Point the mirror's `.git` at the real worktree's git dir, so build
/// scripts that shell out to git (version stamps etc.) still work.
fn write_git_pointer(repo: &Repo, dst: &Path) -> Result<()> {
    let p = dst.join(".git");
    let want = format!("gitdir: {}\n", repo.git_dir.display());
    if fs::read_to_string(&p).ok().as_deref() != Some(want.as_str()) {
        clear(&p)?;
        fs::write(&p, want)?;
    }
    Ok(())
}

fn nanos(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}
