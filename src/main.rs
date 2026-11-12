mod git;
mod rewrite;
mod slot;
mod sync;

use anyhow::Result;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::{Command, ExitCode, Stdio};
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
into the best idle slot, copying only changed files. Cargo then sees the same
paths and mtimes as last time and rebuilds only what actually changed, no
matter which worktree built last.

Env:
  CARGO_SLOT_MAX    Max slots per repo (default 4)
  CARGO_SLOT_HOME   Where slots live (default ~/.cache/cargo-slot)
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
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{HELP}");
            Ok(0)
        }
        Some("status") => status().map(|_| 0),
        Some("gc") => {
            let keep = match args.get(1).map(String::as_str) {
                Some("--keep") => args.get(2).and_then(|n| n.parse().ok()).unwrap_or_else(max_slots),
                _ => max_slots(),
            };
            gc(keep as usize).map(|_| 0)
        }
        Some("clean") => gc(0).map(|_| 0),
        _ => build(&args),
    }
}

fn max_slots() -> u32 {
    std::env::var("CARGO_SLOT_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(4)
}

fn build(args: &[String]) -> Result<u8> {
    let repo = git::Repo::discover()?;
    let store = slot::Store::open(&repo)?;
    let slot = store.acquire(&repo, max_slots())?;

    let started = Instant::now();
    let mut info = slot.info();
    let mut manifest = slot.manifest();
    let same_origin = info.origin.as_deref() == Some(repo.root.as_path());
    let files = repo.files()?;
    let stats = sync::sync(&repo.root, &files, &slot.src(), &mut manifest, same_origin)?;
    sync::write_git_pointer(&repo.git_dir, &slot.src())?;
    info.origin = Some(repo.root.clone());
    info.head = repo.head.clone();
    info.last_used = slot::now();
    slot.save(&info, &manifest)?;

    let quiet = args.iter().any(|a| a == "-q" || a == "--quiet" || a.starts_with("--message-format"));
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

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut cmd = Command::new(cargo);
    cmd.args(args)
        .current_dir(slot.src().join(&repo.cwd_rel))
        .env("CARGO_TARGET_DIR", slot.target())
        .env("CARGO_SLOT_WORKTREE", &repo.root);

    // Output is piped for path rewriting, so keep cargo's tty niceties.
    if std::io::stderr().is_terminal() {
        if std::env::var_os("CARGO_TERM_COLOR").is_none() {
            cmd.env("CARGO_TERM_COLOR", "always");
        }
        if std::env::var_os("CARGO_TERM_PROGRESS_WHEN").is_none() {
            cmd.env("CARGO_TERM_PROGRESS_WHEN", "always");
            if let Some((w, _)) = terminal_size::terminal_size_of(std::io::stderr()) {
                cmd.env("CARGO_TERM_PROGRESS_WIDTH", w.0.to_string());
            }
        }
    }

    // `cargo run` hands the terminal to the user's program, so don't pipe it.
    let interactive = args.first().is_some_and(|a| a == "run" || a == "r");
    let status = if interactive {
        cmd.status()?
    } else {
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let from = slot.src().to_string_lossy().into_owned().into_bytes();
        let to = repo.root.to_string_lossy().into_owned().into_bytes();
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        std::thread::scope(|s| {
            s.spawn(|| rewrite::pipe(out, std::io::stdout().lock(), &from, &to));
            s.spawn(|| rewrite::pipe(err, std::io::stderr().lock(), &from, &to));
        });
        child.wait()?
    };
    Ok(status.code().map_or(101, |c| c.clamp(0, 255) as u8))
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
        println!(
            "  slot {}  {:<4}  {head:<8}  {ago:>6}  {origin}",
            s.id,
            if s.busy { "busy" } else { "idle" },
        );
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
