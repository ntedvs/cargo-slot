use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The git worktree the user invoked us from.
pub struct Repo {
    /// Worktree root (canonical).
    pub root: PathBuf,
    /// The shared git dir for all worktrees of this repo.
    pub common_dir: PathBuf,
    /// Current HEAD commit, or empty if there is none yet.
    pub head: String,
    /// Invocation directory relative to `root`.
    pub cwd_rel: PathBuf,
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim_end_matches('\n').to_string())
}

impl Repo {
    pub fn discover() -> Result<Self> {
        Self::discover_at(&std::env::current_dir()?)
    }

    pub fn discover_at(dir: &Path) -> Result<Self> {
        let cwd = dir.canonicalize()?;
        let root = PathBuf::from(
            git(&cwd, &["rev-parse", "--show-toplevel"]).context("not inside a git worktree")?,
        )
        .canonicalize()?;
        let common_dir = PathBuf::from(git(
            &cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?)
        .canonicalize()?;
        let head = git(&cwd, &["rev-parse", "-q", "--verify", "HEAD"]).unwrap_or_default();
        let cwd_rel = cwd.strip_prefix(&root).unwrap_or(Path::new("")).to_path_buf();
        Ok(Repo { root, common_dir, head, cwd_rel })
    }

    /// Files to mirror, relative to root: tracked plus untracked-but-not-ignored
    /// files, plus ignored files cargo needs anyway:
    /// - `Cargo.lock` next to any `Cargo.toml` (libraries often ignore it, but
    ///   without it every build re-resolves dependencies);
    /// - anything matching `CARGO_SLOT_INCLUDE`, a comma-separated list of
    ///   git globs such as `.env,config/*.local.toml`.
    pub fn files(&self) -> Result<Vec<String>> {
        let mut files = self.ls(&["--cached", "--others", "--exclude-standard"], &[])?;
        let locks: Vec<String> = files
            .iter()
            .filter_map(|f| f.strip_suffix("Cargo.toml").filter(|d| d.is_empty() || d.ends_with('/')))
            .map(|d| format!("{d}Cargo.lock"))
            .filter(|l| self.root.join(l).is_file())
            .collect();
        files.extend(locks);
        if let Ok(include) = std::env::var("CARGO_SLOT_INCLUDE") {
            let globs: Vec<String> =
                include.split(',').map(str::trim).filter(|g| !g.is_empty()).map(|g| format!(":(glob){g}")).collect();
            if !globs.is_empty() {
                files.extend(self.ls(&["--others", "--ignored", "--exclude-standard"], &globs)?);
            }
        }
        // Never mirror a build dir, even if the repo forgot to ignore it.
        files.retain(|f| !f.starts_with("target/"));
        files.sort();
        files.dedup();
        Ok(files)
    }

    fn ls(&self, flags: &[&str], pathspecs: &[String]) -> Result<Vec<String>> {
        let out = Command::new("git")
            .args(["ls-files", "-z"])
            .args(flags)
            .arg("--")
            .args(pathspecs)
            .current_dir(&self.root)
            .output()?;
        if !out.status.success() {
            bail!("git ls-files failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut files = Vec::new();
        for raw in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            // Manifest keys are strings; a lossy name would silently not
            // match the real file, so refuse instead.
            let f = String::from_utf8(raw.to_vec())
                .map_err(|_| anyhow::anyhow!("non-UTF-8 path not supported: {:?}", String::from_utf8_lossy(raw)))?;
            files.push(f);
        }
        Ok(files)
    }

    /// Register `slot_src` as a linked worktree of this repo with its own
    /// index and a detached HEAD at this worktree's commit. Build scripts
    /// that run git see the right commit and dirty state, and anything they
    /// do (refresh the index, commit, check out) stays in the slot instead of
    /// touching the user's index or branch.
    ///
    /// Returns the admin dir under `.git/worktrees`, or None if there's no
    /// commit yet (then the slot has no `.git` at all).
    pub fn link_slot(&self, slot_src: &Path, name: &str) -> Result<Option<PathBuf>> {
        let dot_git = slot_src.join(".git");
        if self.head.is_empty() {
            remove_any(&dot_git)?;
            return Ok(None);
        }
        let admin = self.common_dir.join("worktrees").join(name);
        fs::create_dir_all(&admin)?;
        write_if_changed(&admin.join("gitdir"), &format!("{}\n", dot_git.display()))?;
        write_if_changed(&admin.join("commondir"), "../..\n")?;
        let moved = write_if_changed(&admin.join("HEAD"), &format!("{}\n", self.head))?;
        if !fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_file()) {
            remove_any(&dot_git)?;
        }
        write_if_changed(&dot_git, &format!("gitdir: {}\n", admin.display()))?;
        if moved || !admin.join("index").exists() {
            git(slot_src, &["read-tree", &self.head])?;
        }
        Ok(Some(admin))
    }

    /// Number of files that differ between two commits, if both are known.
    pub fn distance(&self, from: &str) -> Option<usize> {
        if from.is_empty() || self.head.is_empty() {
            return None;
        }
        if from == self.head {
            return Some(0);
        }
        let out = git(&self.root, &["diff", "--name-only", from, &self.head]).ok()?;
        Some(out.lines().count())
    }
}

/// Write `body` unless the file already holds it. Returns whether it wrote.
fn write_if_changed(p: &Path, body: &str) -> Result<bool> {
    if fs::read_to_string(p).ok().as_deref() == Some(body) {
        return Ok(false);
    }
    fs::write(p, body)?;
    Ok(true)
}

fn remove_any(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p)?,
        Ok(_) => fs::remove_file(p)?,
        Err(_) => {}
    }
    Ok(())
}
