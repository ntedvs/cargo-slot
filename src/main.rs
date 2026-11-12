#[cfg(not(unix))]
compile_error!("cargo-slot supports Unix (macOS, Linux) only");

mod cli;
mod external;
mod git;
mod rewrite;
mod slot;
mod sync;
mod term;

use anyhow::{Context, Result, bail};
use cli::Mode;
use std::ffi::OsString;
use std::io::{IsTerminal, Read};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::Instant;

const HELP: &str = "\
cargo-slot: share incremental Rust builds across git worktrees

Usage:
  cargo slot <cargo args...>   Run cargo in a shared build slot
  cargo slot status            List slots for this repo
  cargo slot gc [--keep N]     Delete idle slots beyond the N most recent
  cargo slot clean             Delete all idle slots for this repo

Each repo gets a small pool of slots. A slot is a mirror of a worktree at a
fixed path, plus its own target dir. Before building, your worktree is synced
into a slot, copying only changed files. Cargo then sees the same paths and
mtimes as last time and rebuilds only what actually changed.

Commands that edit source files (fmt, fix, clippy --fix, add, remove,
update, ...) run as plain cargo in the worktree instead. Lockfile changes
cargo makes during a slot build are copied back to the worktree.

Env:
  CARGO_SLOT_MAX      Max slots per repo (default 4)
  CARGO_SLOT_HOME     Where slots live (default ~/.cache/cargo-slot)
  CARGO_SLOT_INCLUDE  Extra gitignored files to mirror, comma-separated
                      git globs, e.g. `.env,config/*.local.toml`
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("cargo-slot: {e:#}");
            ExitCode::from(101)
        }
    }
}

fn run() -> Result<u8> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "slot") {
        args.remove(0);
    }
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{HELP}");
            Ok(0)
        }
        Some("status") => status().map(|_| 0),
        Some("gc") => {
            let keep = match rest.as_slice() {
                [] => max_slots(),
                ["--keep", n] => n.parse().with_context(|| format!("--keep expects a number, got {n:?}"))?,
                _ => bail!("usage: cargo slot gc [--keep N]"),
            };
            gc(keep as usize).map(|_| 0)
        }
        Some("clean") => {
            if !rest.is_empty() {
                bail!("usage: cargo slot clean");
            }
            gc(0).map(|_| 0)
        }
        _ => cargo(&args),
    }
}

fn max_slots() -> u32 {
    std::env::var("CARGO_SLOT_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(4)
}

fn cargo(args: &[String]) -> Result<u8> {
    let parsed = cli::parse(args);
    let quiet = args.iter().any(|a| a == "-q" || a == "--quiet" || a.starts_with("--message-format"));
    // With `+toolchain`, go through the rustup proxy on PATH; `$CARGO` is a
    // concrete toolchain's cargo and doesn't understand `+`.
    let cargo_bin = match (parsed.toolchain, std::env::var_os("CARGO")) {
        (None, Some(c)) => c,
        _ => OsString::from("cargo"),
    };

    if parsed.mode == Mode::Worktree {
        if let (Some(sub), false) = (parsed.subcommand, quiet) {
            eprintln!("cargo-slot: `{sub}` edits the worktree, running plain cargo there");
        }
        return Ok(code(Command::new(cargo_bin).args(args).status()?));
    }

    let repo = git::Repo::discover()?;
    let store = slot::Store::open(&repo)?;
    let slot = store.acquire(&repo, max_slots())?;

    let started = Instant::now();
    let mut info = slot.info();
    let mut manifest = slot.manifest();
    let same_origin = info.origin.as_deref() == Some(repo.root.as_path());
    let files = repo.files()?;
    let siblings = external::sibling_deps(&repo.root, &files)?;
    let stats = sync::sync(&repo.root, &files, &slot.src(), &mut manifest, same_origin)?;
    if let Some(parent) = repo.root.parent() {
        slot.link_siblings(&siblings, parent)?;
    }
    info.git_admin = repo.link_slot(&slot.src(), &slot.git_name())?;
    info.origin = Some(repo.root.clone());
    info.head = repo.head.clone();
    info.last_used = slot::now();
    slot.save(&info, &manifest)?;

    if !quiet {
        eprintln!(
            "cargo-slot: slot {} synced {} files ({} changed, {} removed) in {:.0?}",
            slot.id,
            stats.files,
            stats.copied,
            stats.removed,
            started.elapsed()
        );
    }

    // Cargo configs above the repo aren't above the slot; pass them along.
    let mut full: Vec<OsString> = Vec::new();
    let mut user = args;
    if parsed.toolchain.is_some() {
        full.push(args[0].clone().into());
        user = &args[1..];
    }
    for cfg in external::parent_configs(&repo.root, &slot.src()) {
        if cfg.extension().is_some_and(|e| e == "toml") {
            full.push("--config".into());
            full.push(cfg.into_os_string());
        } else {
            eprintln!("cargo-slot: warning: ignoring {}; rename it to config.toml to use it from a slot", cfg.display());
        }
    }
    full.extend(user.iter().map(OsString::from));

    let mut cmd = Command::new(cargo_bin);
    cmd.args(&full)
        .current_dir(slot.src().join(&repo.cwd_rel))
        .env("CARGO_TARGET_DIR", slot.target())
        .env("CARGO_SLOT_WORKTREE", &repo.root);

    let status = if parsed.mode == Mode::Interactive {
        // `cargo run` hands the terminal to the user's program untouched.
        cmd.status()?
    } else {
        let (out_stdio, out_pty) = term::output(std::io::stdout().is_terminal())?;
        let (err_stdio, err_pty) = term::output(std::io::stderr().is_terminal())?;
        cmd.stdout(out_stdio).stderr(err_stdio);
        let mut child = cmd.spawn()?;
        // Close our copies of the pty child ends, or reads never see EOF.
        drop(cmd);
        let out: Box<dyn Read + Send> = match out_pty {
            Some(f) => Box::new(f),
            None => Box::new(child.stdout.take().unwrap()),
        };
        let err: Box<dyn Read + Send> = match err_pty {
            Some(f) => Box::new(f),
            None => Box::new(child.stderr.take().unwrap()),
        };
        let from = slot.src().to_string_lossy().into_owned().into_bytes();
        let to = repo.root.to_string_lossy().into_owned().into_bytes();
        std::thread::scope(|s| {
            s.spawn(|| rewrite::pipe(out, std::io::stdout().lock(), &from, &to));
            s.spawn(|| rewrite::pipe(err, std::io::stderr().lock(), &from, &to));
        });
        child.wait()?
    };

    for (rel, written) in sync::write_back_locks(&repo.root, &slot.src(), &repo.cwd_rel, &manifest)? {
        if written {
            if !quiet {
                eprintln!("cargo-slot: updated {rel} in the worktree");
            }
        } else {
            eprintln!("cargo-slot: warning: {rel} changed in both the worktree and the build; kept the worktree's copy");
        }
    }
    Ok(code(status))
}

fn code(status: ExitStatus) -> u8 {
    status.code().map_or(101, |c| c.clamp(0, 255) as u8)
}

fn status() -> Result<()> {
    let repo = git::Repo::discover()?;
    let store = slot::Store::open(&repo)?;
    println!("{}", store.dir.display());
    let now = slot::now();
    for s in store.status()? {
        let origin = s.info.origin.map_or("-".into(), |p| p.display().to_string());
        let head = s.info.head.get(..8).unwrap_or("-");
        let ago = if s.info.last_used == 0 { "never".into() } else { fmt_ago(now.saturating_sub(s.info.last_used)) };
        println!("  slot {}  {:<4}  {head:<8}  {ago:>6}  {origin}", s.id, if s.busy { "busy" } else { "idle" });
    }
    Ok(())
}

fn gc(keep: usize) -> Result<()> {
    let repo = git::Repo::discover()?;
    let store = slot::Store::open(&repo)?;
    for (id, removed) in store.gc(keep)? {
        if removed {
            println!("removed slot {id}");
        } else {
            println!("slot {id} is busy, skipped");
        }
    }
    Ok(())
}

fn fmt_ago(s: u64) -> String {
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}
