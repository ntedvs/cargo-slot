//! End-to-end: real git worktrees, real cargo, the built `cargo-slot` binary.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Parent of all worktrees. Not above the slot cache, so configs and
    /// sibling crates here are only reachable the way a worktree sees them.
    work: PathBuf,
    main: PathBuf,
    home: PathBuf,
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
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
        let work = base.join("work");
        let main = work.join("main");
        write(main.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write(main.join("src/main.rs"), "mod util;\nfn main() { println!(\"{}\", util::msg()); }\n");
        write(main.join("src/util.rs"), "pub fn msg() -> &'static str { \"v1\" }\n");
        write(main.join(".gitignore"), "/target\nsecret.env\n");
        git(&main, &["init", "-q"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "init"]);
        let home = base.join("cache");
        Fixture { _tmp: tmp, work, main, home }
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let p = self.work.join(name);
        git(&self.main, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", name]);
        p
    }

    fn slot(&self, args: &[&str], dir: &Path) -> Output {
        let out = self.slot_raw(args, dir);
        assert!(out.status.success(), "cargo slot {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
        out
    }

    fn slot_raw(&self, args: &[&str], dir: &Path) -> Output {
        self.cmd(args, dir).output().unwrap()
    }

    /// `cargo-slot` with an environment independent of the caller's. CI sets
    /// things like `CARGO_TERM_COLOR=always`, which breaks matching on
    /// cargo's output, and `CARGO_INCREMENTAL=0`.
    fn cmd(&self, args: &[&str], dir: &Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cargo-slot"));
        c.args(args).current_dir(dir).env("CARGO_SLOT_HOME", &self.home).env("CARGO_SLOT_MAX", "2");
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && !matches!(&*k, "CARGO_HOME")) || k.starts_with("RUSTFLAGS") {
                c.env_remove(&*k);
            }
        }
        c.env("CARGO_TERM_COLOR", "never");
        c
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
    assert!(stderr.contains("    Finished "), "no cargo summary in output:\n{stderr}");
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
    let out = f.slot_raw(&["build", "--message-format=short"], &f.main);
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
    write(f.work.join("target/x"), "attacker");
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

#[test]
fn source_editing_commands_run_in_worktree() {
    let f = Fixture::new();
    write(f.main.join("src/util.rs"), "pub fn msg()->&'static str{\"v1\"}\n");
    let err = String::from_utf8(f.slot(&["fmt"], &f.main).stderr).unwrap();
    assert!(err.contains("running plain cargo"), "{err}");
    assert_eq!(fs::read_to_string(f.main.join("src/util.rs")).unwrap(), "pub fn msg() -> &'static str {\n    \"v1\"\n}\n");
    assert!(!f.home.exists() || fs::read_dir(&f.home).unwrap().next().is_none(), "fmt should not touch slots");
}

#[test]
fn lockfile_changes_are_written_back() {
    let f = Fixture::new();
    f.build(&f.main);
    git(&f.main, &["add", "Cargo.lock"]);
    git(&f.main, &["commit", "-qm", "lock"]);
    // New path dependency: cargo rewrites Cargo.lock during the slot build.
    write(f.main.join("dep/Cargo.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(f.main.join("dep/src/lib.rs"), "");
    let toml = fs::read_to_string(f.main.join("Cargo.toml")).unwrap();
    write(f.main.join("Cargo.toml"), &format!("{toml}\n[dependencies]\ndep = {{ path = \"dep\" }}\n"));
    let err = f.build(&f.main);
    assert!(err.contains("updated Cargo.lock in the worktree"), "{err}");
    assert!(fs::read_to_string(f.main.join("Cargo.lock")).unwrap().contains("name = \"dep\""));
    assert!(!f.build(&f.main).contains("updated Cargo.lock"), "no write-back when nothing changed");
}

#[test]
fn ignored_lockfile_is_kept() {
    let f = Fixture::new();
    write(f.main.join(".gitignore"), "/target\n/Cargo.lock\n");
    git(&f.main, &["commit", "-qam", "ignore lock"]);
    // Plain cargo would create the lockfile in the worktree; so do we.
    f.build(&f.main);
    let lock = f.main.join("Cargo.lock");
    assert!(lock.is_file());
    let before = fs::metadata(&lock).unwrap().modified().unwrap();
    let err = f.build(&f.main);
    assert!(err.contains("0 changed, 0 removed"), "{err}");
    assert!(f.slot_dir(1).join("src/Cargo.lock").is_file());
    assert_eq!(fs::metadata(&lock).unwrap().modified().unwrap(), before);
}

#[test]
fn include_env_mirrors_extra_ignored_files() {
    let f = Fixture::new();
    write(f.main.join("secret.env"), "s");
    let out = f.cmd(&["build"], &f.main).env("CARGO_SLOT_INCLUDE", "secret.env").output().unwrap();
    assert!(out.status.success());
    assert!(f.slot_dir(1).join("src/secret.env").is_file());
}

#[test]
fn sibling_path_deps_and_parent_configs() {
    let f = Fixture::new();
    write(f.work.join("other/Cargo.toml"), "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(f.work.join("other/src/lib.rs"), "pub fn x() -> &'static str { \"other\" }\n");
    write(f.work.join(".cargo/config.toml"), "[env]\nFROM_PARENT = \"parent-config\"\n");
    let toml = fs::read_to_string(f.main.join("Cargo.toml")).unwrap();
    write(f.main.join("Cargo.toml"), &format!("{toml}\n[dependencies]\nother = {{ path = \"../other\" }}\n"));
    write(f.main.join("src/main.rs"), "fn main() { println!(\"{} {}\", other::x(), env!(\"FROM_PARENT\")); }\n");
    let out = f.slot(&["run", "-q"], &f.main);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "other parent-config");
    // The sibling builds from its real location: edits there are picked up.
    write(f.work.join("other/src/lib.rs"), "pub fn x() -> &'static str { \"edited\" }\n");
    let out = f.slot(&["run", "-q"], &f.main);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "edited parent-config");
}

#[test]
fn deep_path_deps_fail_clearly() {
    let f = Fixture::new();
    let toml = fs::read_to_string(f.main.join("Cargo.toml")).unwrap();
    write(f.main.join("Cargo.toml"), &format!("{toml}\n[dependencies]\nx = {{ path = \"../../x\" }}\n"));
    let out = f.slot_raw(&["build"], &f.main);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("more than one level above the repo"));
}

#[test]
fn slot_git_is_isolated_from_worktree() {
    let f = Fixture::new();
    let head = git_out(&f.main, &["rev-parse", "HEAD"]);
    write(f.main.join("src/util.rs"), "pub fn msg() -> &'static str { \"dirty\" }\n");
    f.build(&f.main);
    let src = f.slot_dir(1).join("src");
    let index = fs::read(f.main.join(".git/index")).unwrap();
    let status = git_out(&f.main, &["status", "--porcelain"]);

    // The slot's git sees the same commit and dirty state.
    assert_eq!(git_out(&src, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_out(&src, &["status", "--porcelain"]), status);
    // What a build script does to git stays in the slot.
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "from a build script"]);
    assert_eq!(fs::read(f.main.join(".git/index")).unwrap(), index, "worktree index changed");
    assert_eq!(git_out(&f.main, &["rev-parse", "HEAD"]), head, "worktree branch moved");
    assert_eq!(git_out(&f.main, &["status", "--porcelain"]), status);

    // Next build resets the slot to the worktree's commit.
    f.build(&f.main);
    assert_eq!(git_out(&src, &["rev-parse", "HEAD"]), head);

    // GC removes the registration.
    f.slot(&["clean"], &f.main);
    assert!(!git_out(&f.main, &["worktree", "list"]).contains("slot-1"));
}

#[test]
fn toolchain_override() {
    if Command::new("rustup").arg("--version").output().is_err() {
        eprintln!("skipping: no rustup");
        return;
    }
    let f = Fixture::new();
    let out = f.slot(&["+stable", "run", "-q"], &f.main);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "v1");
    let out = f.slot(&["+stable", "-v", "build"], &f.main);
    assert!(String::from_utf8_lossy(&out.stderr).contains("synced"), "should build in a slot");
}

#[test]
fn gc_rejects_bad_args() {
    let f = Fixture::new();
    let out = f.slot_raw(&["gc", "--keep", "abc"], &f.main);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("expects a number"));
    assert!(!f.slot_raw(&["gc", "--bogus"], &f.main).status.success());
}
