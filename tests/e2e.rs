//! End-to-end: real git worktrees, real cargo, the built `cargo-slot` binary.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    main: PathBuf,
    home: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(p: impl AsRef<Path>, body: &str) {
    let p = p.as_ref();
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

impl Fixture {
    /// A dependency-free crate committed in a fresh repo.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let main = base.join("main");
        write(main.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write(main.join("src/main.rs"), "mod util;\nfn main() { println!(\"{}\", util::msg()); }\n");
        write(main.join("src/util.rs"), "pub fn msg() -> &'static str { \"v1\" }\n");
        write(main.join(".gitignore"), "/target\nsecret.env\n");
        git(&main, &["init", "-q"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "init"]);
        let home = base.join("cache");
        Fixture { _tmp: tmp, base, main, home }
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let p = self.base.join(name);
        git(&self.main, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", name]);
        p
    }

    fn slot(&self, args: &[&str], dir: &Path) -> Output {
        let out = Command::new(env!("CARGO_BIN_EXE_cargo-slot"))
            .args(args)
            .current_dir(dir)
            .env("CARGO_SLOT_HOME", &self.home)
            .env("CARGO_SLOT_MAX", "2")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        assert!(out.status.success(), "cargo slot {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
        out
    }

    fn build(&self, dir: &Path) -> String {
        String::from_utf8(self.slot(&["build"], dir).stderr).unwrap()
    }

    /// The only slot dir (tests use one slot unless noted).
    fn slot_dir(&self, id: u32) -> PathBuf {
        let store = fs::read_dir(&self.home).unwrap().next().unwrap().unwrap().path();
        store.join(format!("slot-{id}"))
    }
}

fn compiled(stderr: &str) -> bool {
    stderr.contains("Compiling demo")
}

#[test]
fn worktree_switch_reuses_build() {
    let f = Fixture::new();
    let wt = f.worktree("wt");

    assert!(compiled(&f.build(&f.main)));
    assert!(!compiled(&f.build(&wt)), "same commit in another worktree should be fresh");

    write(wt.join("src/util.rs"), "pub fn msg() -> &'static str { \"v2\" }\n");
    let err = f.build(&wt);
    assert!(compiled(&err));
    assert!(err.contains(&format!("({})", wt.display())), "paths rewritten to worktree:\n{err}");

    let run = f.slot(&["run", "-q"], &wt);
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "v2");
    let run = f.slot(&["run", "-q"], &f.main);
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "v1");
}

#[test]
fn error_paths_point_at_worktree() {
    let f = Fixture::new();
    write(f.main.join("src/util.rs"), "pub fn msg() -> &'static str { 1 }\n");
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-slot"))
        .args(["build", "--message-format=short"])
        .current_dir(&f.main)
        .env("CARGO_SLOT_HOME", &f.home)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("src/util.rs:1"), "{err}");
    assert!(!err.contains(&f.slot_dir(1).display().to_string()), "slot path leaked:\n{err}");
}

#[test]
fn mirrors_git_view_of_files() {
    let f = Fixture::new();
    write(f.main.join("untracked.txt"), "u");
    write(f.main.join("secret.env"), "s");
    f.build(&f.main);
    let src = f.slot_dir(1).join("src");
    assert!(src.join("untracked.txt").exists(), "untracked, not ignored: mirrored");
    assert!(!src.join("secret.env").exists(), "ignored: not mirrored");
    assert!(!src.join("target").exists(), "worktree target never mirrored");
}

#[test]
fn dir_replaced_by_symlink_does_not_escape() {
    // Tracked `a/x`, then `a` replaced by a symlink into the slot's target
    // without staging. Git lists both `a` and `a/x`.
    let f = Fixture::new();
    write(f.main.join("a/x"), "tracked");
    git(&f.main, &["add", "-A"]);
    git(&f.main, &["commit", "-qm", "a"]);
    f.build(&f.main);

    let victim = f.slot_dir(1).join("target/x");
    fs::write(&victim, "victim").unwrap();
    // `a/x` resolves through the symlink to a real file (from the worktree,
    // `../target` is outside the repo; from the slot, it is the slot's target).
    write(f.base.join("target/x"), "attacker");
    fs::remove_dir_all(f.main.join("a")).unwrap();
    symlink("../target", f.main.join("a")).unwrap();
    // Leaves sync in parallel, so give any ordering race a few chances.
    for _ in 0..5 {
        f.build(&f.main);
        assert_eq!(fs::read_to_string(&victim).ok().as_deref(), Some("victim"), "slot target overwritten or deleted");
    }
}

#[test]
fn exec_bit_is_mirrored() {
    let f = Fixture::new();
    write(f.main.join("run.sh"), "#!/bin/sh\n");
    fs::set_permissions(f.main.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    f.build(&f.main);
    fs::set_permissions(f.main.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let err = f.build(&f.main);
    let mode = fs::metadata(f.slot_dir(1).join("src/run.sh")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!compiled(&err), "mode change alone shouldn't rebuild");
}

#[test]
fn files_created_in_slot_do_not_leak_into_builds() {
    let f = Fixture::new();
    f.build(&f.main);
    let ghost = f.slot_dir(1).join("src/src/bin/ghost.rs");
    write(&ghost, "fn main() {}\n");
    f.build(&f.main);
    assert!(!ghost.exists());
    assert!(!f.slot_dir(1).join("target/debug/ghost").exists(), "ghost target was built");
}

#[test]
fn concurrent_builds_use_separate_slots() {
    let f = Fixture::new();
    let wt = f.worktree("wt");
    f.build(&f.main);
    std::thread::scope(|s| {
        let a = s.spawn(|| f.slot(&["build", "--release"], &f.main));
        let b = s.spawn(|| f.slot(&["build", "--release"], &wt));
        a.join().unwrap();
        b.join().unwrap();
    });
    assert!(f.slot_dir(1).is_dir() && f.slot_dir(2).is_dir());
    // Slot affinity: main goes back to the slot it built in, which is fresh.
    assert!(!compiled(&f.build(&f.main)));
}
