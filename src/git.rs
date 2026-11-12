use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The git worktree the user invoked us from.
pub struct Repo {
    /// Worktree root (canonical).
    pub root: PathBuf,
    /// This worktree's git dir (e.g. `.git` or `.git/worktrees/foo`).
    pub git_dir: PathBuf,
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
        let git_dir = PathBuf::from(git(&cwd, &["rev-parse", "--absolute-git-dir"])?);
        let common_dir = PathBuf::from(git(
            &cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?)
        .canonicalize()?;
        let head = git(&cwd, &["rev-parse", "-q", "--verify", "HEAD"]).unwrap_or_default();
        let cwd_rel = cwd.strip_prefix(&root).unwrap_or(Path::new("")).to_path_buf();
        Ok(Repo { root, git_dir, common_dir, head, cwd_rel })
    }

    /// Tracked plus untracked-but-not-ignored files, relative to root.
    pub fn files(&self) -> Result<Vec<String>> {
        let out = Command::new("git")
            .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
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
            // Never mirror a build dir, even if the repo forgot to ignore it.
            if !f.starts_with("target/") {
                files.push(f);
            }
        }
        files.sort();
        files.dedup();
        Ok(files)
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
