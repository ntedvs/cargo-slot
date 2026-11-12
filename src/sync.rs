use crate::slot::{Entry, Manifest, Stamp};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::{self, File, Metadata, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: usize,
    pub copied: usize,
    pub removed: usize,
}

enum Outcome {
    Same(Entry),
    Copied(Entry),
    Gone { removed: bool },
}

/// Make `dst` an exact mirror of `files` under `root`, plus the root `.git`
/// pointer. Only files whose content differs are rewritten, so untouched
/// files keep their mtimes and cargo's fingerprints stay fresh.
///
/// Never writes outside `dst`: paths are normalized so no leaf is an
/// ancestor of another, then every parent is made a real directory before
/// any leaf is written. Leaves never change parents, so that holds while
/// leaves sync in parallel.
pub fn sync(root: &Path, files: &[String], dst: &Path, manifest: &mut Manifest, same_origin: bool) -> Result<Stats> {
    fs::create_dir_all(dst)?;
    let files = normalize(root, files)?;
    let wanted: HashSet<&str> = files.iter().map(String::as_str).collect();
    let dirs: HashSet<String> = files.iter().flat_map(|f| ancestors(f)).map(str::to_string).collect();
    let mut stats = Stats::default();

    // 1. Delete anything in the mirror that isn't wanted: stale entries,
    //    and files builds created. A leftover `src/bin/x.rs` would otherwise
    //    add a target that doesn't exist in the worktree.
    prune(dst, "", &wanted, &dirs, &mut stats.removed)?;

    // 2. Make every parent a real directory.
    let mut ready = HashSet::new();
    for rel in &files {
        ensure_parents(dst, rel, &mut ready)?;
    }

    // 3. Sync leaves.
    let old = &*manifest;
    let results: Vec<(&String, Outcome)> = files
        .par_iter()
        .map(|rel| {
            let out = sync_one(root, dst, rel, old.get(rel), same_origin).with_context(|| format!("syncing {rel}"))?;
            Ok((rel, out))
        })
        .collect::<Result<_>>()?;

    let mut next = Manifest::new();
    for (rel, out) in results {
        match out {
            Outcome::Same(e) => {
                next.insert(rel.clone(), e);
            }
            Outcome::Copied(e) => {
                stats.copied += 1;
                next.insert(rel.clone(), e);
            }
            Outcome::Gone { removed } => stats.removed += removed as usize,
        }
    }
    stats.files = next.len();
    *manifest = next;
    Ok(stats)
}

/// Keep only paths that name a real leaf in the source: every ancestor is a
/// real directory and no other listed path is an ancestor. Git can list both
/// `a` and `a/x` when a tracked directory was replaced by a symlink or file
/// without staging; then `a/x` isn't a real file and must be skipped.
fn normalize(root: &Path, files: &[String]) -> Result<Vec<String>> {
    for rel in files {
        check_rel(rel)?;
    }
    let listed: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut real_dirs: HashSet<&str> = HashSet::new();
    let mut out = Vec::with_capacity(files.len());
    'files: for rel in files {
        for dir in ancestors(rel) {
            if listed.contains(dir) {
                continue 'files;
            }
            if !real_dirs.contains(dir) {
                if !fs::symlink_metadata(root.join(dir)).is_ok_and(|m| m.is_dir()) {
                    continue 'files;
                }
                real_dirs.insert(dir);
            }
        }
        out.push(rel.clone());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Proper ancestors of a relative path, shallowest first: "a/b/c" -> ["a", "a/b"].
fn ancestors(rel: &str) -> impl Iterator<Item = &str> {
    rel.match_indices('/').map(move |(i, _)| &rel[..i])
}

/// Recursively delete entries under `dst/dir` that are neither wanted leaves
/// nor ancestors of one. Does not follow symlinks.
fn prune(dst: &Path, dir: &str, wanted: &HashSet<&str>, dirs: &HashSet<String>, removed: &mut usize) -> Result<()> {
    for entry in fs::read_dir(dst.join(dir))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if dir.is_empty() && name == ".git" {
            continue;
        }
        let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
        let ft = entry.file_type()?;
        if ft.is_dir() && dirs.contains(&rel) {
            prune(dst, &rel, wanted, dirs, removed)?;
        } else if wanted.contains(rel.as_str()) && !ft.is_dir() {
            // Kept; sync_one fixes type or content.
        } else {
            if ft.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
            *removed += 1;
        }
    }
    Ok(())
}

fn sync_one(root: &Path, dst: &Path, rel: &str, old: Option<&Entry>, same_origin: bool) -> Result<Outcome> {
    let s = root.join(rel);
    let d = dst.join(rel);
    let md = match fs::symlink_metadata(&s) {
        Ok(m) if !m.is_dir() => m,
        // Deleted but still in the index, or a submodule: not mirrored.
        Ok(_) => return Ok(Outcome::Gone { removed: remove_leaf(&d)? }),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Outcome::Gone { removed: remove_leaf(&d)? }),
        Err(e) => return Err(e.into()),
    };
    let dmd = fs::symlink_metadata(&d).ok();

    if md.file_type().is_symlink() {
        let target = fs::read_link(&s)?;
        let entry = Entry { hash: target.to_string_lossy().into_owned(), link: true, ..Entry::default() };
        let matches = dmd.is_some_and(|m| m.file_type().is_symlink()) && fs::read_link(&d).ok() == Some(target.clone());
        if matches {
            return Ok(Outcome::Same(entry));
        }
        clear(&d)?;
        std::os::unix::fs::symlink(&target, &d)?;
        return Ok(Outcome::Copied(entry));
    }

    let size = md.len();
    let mtime = nanos(md.modified()?);
    let mode = md.mode() & 0o7777;
    let dst_stamp = dmd.as_ref().filter(|m| m.is_file()).map(stamp);

    // Fast path: source unchanged and destination exactly as we left it.
    if let (Some(o), Some(st)) = (old, &dst_stamp) {
        if !o.link && o.dst.as_ref() == Some(st) && same_origin && o.size == size && o.mtime == mtime && o.mode == mode {
            return Ok(Outcome::Same(o.clone()));
        }
    }

    let hash = hash_file(&s)?;
    let mut entry = Entry { size, mtime, mode, hash, link: false, dst: None };

    if let Some(st) = dst_stamp {
        let trusted = old.is_some_and(|o| !o.link && o.dst.as_ref() == Some(&st));
        // Destination untouched since we wrote it: trust the recorded hash.
        // Otherwise (mutated by a build, cloned, or unknown) check its bytes.
        let same = if trusted {
            old.is_some_and(|o| o.hash == entry.hash)
        } else {
            st.size == size && hash_file(&d)? == entry.hash
        };
        if same {
            if st.mode == mode {
                entry.dst = Some(st);
                return Ok(Outcome::Same(entry));
            }
            // Only permissions differ. chmod leaves mtime alone, so cargo
            // doesn't rebuild for it.
            fs::set_permissions(&d, Permissions::from_mode(mode))?;
            entry.dst = Some(stamp(&fs::symlink_metadata(&d)?));
            return Ok(Outcome::Copied(entry));
        }
    }

    clear(&d)?;
    let name = d.file_name().context("empty path")?.to_string_lossy();
    let tmp = d.with_file_name(format!(".{name}.cargo-slot-tmp"));
    clear(&tmp)?;
    fs::copy(&s, &tmp)?;
    fs::set_permissions(&tmp, Permissions::from_mode(mode))?;
    // fs::copy may clone and keep the source mtime. Cargo decides freshness
    // by mtime, so a changed file must look newer than any prior build.
    File::options().write(true).open(&tmp)?.set_modified(SystemTime::now())?;
    fs::rename(&tmp, &d)?;
    entry.dst = Some(stamp(&fs::symlink_metadata(&d)?));
    Ok(Outcome::Copied(entry))
}

/// Reject anything but plain relative paths.
fn check_rel(rel: &str) -> Result<()> {
    let ok = !rel.is_empty() && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)));
    if !ok {
        bail!("refusing unsafe path {rel:?}");
    }
    Ok(())
}

/// Ensure each ancestor of `rel` under `dst` is a real directory, replacing
/// files or symlinks in the way.
fn ensure_parents(dst: &Path, rel: &str, ready: &mut HashSet<PathBuf>) -> Result<()> {
    for dir in ancestors(rel) {
        let cur = dst.join(dir);
        if ready.contains(&cur) {
            continue;
        }
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                fs::remove_file(&cur)?;
                fs::create_dir(&cur)?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => fs::create_dir(&cur)?,
            Err(e) => return Err(e.into()),
        }
        ready.insert(cur);
    }
    Ok(())
}

fn remove_leaf(p: &Path) -> Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(m) if !m.is_dir() => {
            fs::remove_file(p)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Remove whatever is at `p`. Parents must already be real directories.
fn clear(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p)?,
        Ok(_) => fs::remove_file(p)?,
        Err(_) => {}
    }
    Ok(())
}

/// Point the mirror's `.git` at the real worktree's git dir, so build
/// scripts that shell out to git (version stamps etc.) still work.
pub fn write_git_pointer(git_dir: &Path, dst: &Path) -> Result<()> {
    let p = dst.join(".git");
    let want = format!("gitdir: {}\n", git_dir.display());
    if fs::read_to_string(&p).ok().as_deref() != Some(want.as_str()) || !fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
        clear(&p)?;
        fs::write(&p, want)?;
    }
    Ok(())
}

fn hash_file(p: &Path) -> Result<String> {
    Ok(blake3::Hasher::new().update_reader(File::open(p)?)?.finalize().to_hex().to_string())
}

fn stamp(m: &Metadata) -> Stamp {
    Stamp {
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mode: m.mode() & 0o7777,
        mtime: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
        ctime: m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128,
    }
}

fn nanos(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Env {
        _tmp: tempfile::TempDir,
        src: PathBuf,
        dst: PathBuf,
        outside: PathBuf,
        manifest: Manifest,
    }

    impl Env {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let base = tmp.path().canonicalize().unwrap();
            let (src, dst, outside) = (base.join("src"), base.join("dst"), base.join("outside"));
            for d in [&src, &dst, &outside] {
                fs::create_dir_all(d).unwrap();
            }
            Env { _tmp: tmp, src, dst, outside, manifest: Manifest::new() }
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.src.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        fn sync(&mut self, files: &[&str]) -> Stats {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            sync(&self.src, &files, &self.dst, &mut self.manifest, true).unwrap()
        }
        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.dst.join(rel)).unwrap()
        }
    }

    #[test]
    fn unchanged_files_keep_mtime() {
        let mut e = Env::new();
        e.write("a.rs", "a");
        e.write("b.rs", "b");
        e.sync(&["a.rs", "b.rs"]);
        let before = fs::metadata(e.dst.join("a.rs")).unwrap().modified().unwrap();
        e.write("b.rs", "B");
        let st = e.sync(&["a.rs", "b.rs"]);
        assert_eq!(st, Stats { files: 2, copied: 1, removed: 0 });
        assert_eq!(fs::metadata(e.dst.join("a.rs")).unwrap().modified().unwrap(), before);
        assert_eq!(e.read("b.rs"), "B");
    }

    #[test]
    fn restores_same_length_mutation_in_slot() {
        let mut e = Env::new();
        e.write("lib.rs", "good");
        e.sync(&["lib.rs"]);
        fs::write(e.dst.join("lib.rs"), "evil").unwrap();
        let st = e.sync(&["lib.rs"]);
        assert_eq!(st.copied, 1);
        assert_eq!(e.read("lib.rs"), "good");
    }

    #[test]
    fn symlink_to_dir_does_not_write_outside() {
        let mut e = Env::new();
        symlink(&e.outside, e.src.join("a")).unwrap();
        e.sync(&["a"]);
        assert!(fs::symlink_metadata(e.dst.join("a")).unwrap().file_type().is_symlink());

        fs::remove_file(e.src.join("a")).unwrap();
        e.write("a/f.rs", "x");
        e.sync(&["a/f.rs"]);
        assert!(!e.outside.join("f.rs").exists(), "wrote through symlink");
        assert!(fs::symlink_metadata(e.dst.join("a")).unwrap().is_dir());
        assert_eq!(e.read("a/f.rs"), "x");
    }

    #[test]
    fn untracked_symlink_parent_in_slot_is_replaced() {
        let mut e = Env::new();
        symlink(&e.outside, e.dst.join("a")).unwrap();
        e.write("a/f.rs", "x");
        e.sync(&["a/f.rs"]);
        assert!(!e.outside.join("f.rs").exists());
        assert_eq!(e.read("a/f.rs"), "x");
    }

    #[test]
    fn file_to_dir_and_back() {
        let mut e = Env::new();
        e.write("a", "file");
        e.sync(&["a"]);

        fs::remove_file(e.src.join("a")).unwrap();
        e.write("a/b.rs", "child");
        let st = e.sync(&["a/b.rs"]);
        assert_eq!(st, Stats { files: 1, copied: 1, removed: 1 });
        assert_eq!(e.read("a/b.rs"), "child");

        fs::remove_dir_all(e.src.join("a")).unwrap();
        e.write("a", "file again");
        e.sync(&["a"]);
        assert_eq!(e.read("a"), "file again");
    }

    #[test]
    fn removes_deleted_files() {
        let mut e = Env::new();
        e.write("x/y.rs", "y");
        e.sync(&["x/y.rs"]);
        let st = e.sync(&[]);
        assert_eq!(st.removed, 1);
        assert!(!e.dst.join("x").exists());
    }

    #[test]
    fn listed_but_missing_source_is_removed() {
        let mut e = Env::new();
        e.write("a.rs", "a");
        e.sync(&["a.rs"]);
        fs::remove_file(e.src.join("a.rs")).unwrap();
        let st = e.sync(&["a.rs"]);
        assert_eq!(st.removed, 1);
        assert!(!e.dst.join("a.rs").exists());
    }

    #[test]
    fn cloned_slot_is_verified_without_touching_mtimes() {
        let mut e = Env::new();
        e.write("a.rs", "a");
        e.sync(&["a.rs"]);
        // Simulate a clone: same bytes, new inode.
        let p = e.dst.join("a.rs");
        fs::copy(&p, e.dst.join("tmp")).unwrap();
        fs::rename(e.dst.join("tmp"), &p).unwrap();
        let st = e.sync(&["a.rs"]);
        assert_eq!(st.copied, 0);
    }

    #[test]
    fn rejects_traversal() {
        let mut m = Manifest::new();
        let e = Env::new();
        assert!(sync(&e.src, &["../x".into()], &e.dst, &mut m, true).is_err());
    }

    #[test]
    fn listed_leaf_and_descendant_does_not_traverse() {
        // Git lists both `a` (now a symlink) and stale tracked `a/x`.
        let mut e = Env::new();
        e.write("a/x", "old");
        e.sync(&["a/x"]);
        fs::remove_dir_all(e.src.join("a")).unwrap();
        fs::write(e.outside.join("x"), "victim").unwrap();
        symlink(&e.outside, e.src.join("a")).unwrap();
        e.sync(&["a", "a/x"]);
        assert_eq!(fs::read_to_string(e.outside.join("x")).unwrap(), "victim");
        assert!(fs::symlink_metadata(e.dst.join("a")).unwrap().file_type().is_symlink());
        assert!(!e.manifest.contains_key("a/x"));
    }

    #[test]
    fn descendant_of_source_symlink_is_skipped() {
        // Only `a/x` listed, but `a` is a symlink in the source: not a real file.
        let mut e = Env::new();
        fs::write(e.outside.join("x"), "outside").unwrap();
        symlink(&e.outside, e.src.join("a")).unwrap();
        let st = e.sync(&["a/x"]);
        assert_eq!(st.files, 0);
        assert!(!e.dst.join("a").exists());
    }

    #[test]
    fn mode_changes_sync_without_touching_mtime() {
        let mut e = Env::new();
        e.write("run.sh", "#!/bin/sh");
        fs::set_permissions(e.src.join("run.sh"), Permissions::from_mode(0o644)).unwrap();
        e.sync(&["run.sh"]);
        let before = fs::metadata(e.dst.join("run.sh")).unwrap().modified().unwrap();

        fs::set_permissions(e.src.join("run.sh"), Permissions::from_mode(0o755)).unwrap();
        assert_eq!(e.sync(&["run.sh"]).copied, 1);
        let md = fs::metadata(e.dst.join("run.sh")).unwrap();
        assert_eq!(md.mode() & 0o777, 0o755);
        assert_eq!(md.modified().unwrap(), before);

        // A chmod inside the slot is also undone.
        fs::set_permissions(e.dst.join("run.sh"), Permissions::from_mode(0o600)).unwrap();
        e.sync(&["run.sh"]);
        assert_eq!(fs::metadata(e.dst.join("run.sh")).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn removes_files_created_in_slot() {
        let mut e = Env::new();
        e.write("src/main.rs", "fn main() {}");
        e.sync(&["src/main.rs"]);
        fs::create_dir_all(e.dst.join("src/bin")).unwrap();
        fs::write(e.dst.join("src/bin/ghost.rs"), "fn main() {}").unwrap();
        fs::write(e.dst.join("stray"), "").unwrap();
        fs::write(e.dst.join(".git"), "gitdir: x").unwrap();
        let st = e.sync(&["src/main.rs"]);
        assert_eq!(st.removed, 2);
        assert!(!e.dst.join("src/bin").exists());
        assert!(!e.dst.join("stray").exists());
        assert!(e.dst.join(".git").exists(), "git pointer is kept");
        assert!(e.dst.join("src/main.rs").exists());
    }
}
