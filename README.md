# cargo-slot

Share incremental Rust builds across git worktrees. macOS and Linux only.

```sh
cargo install --path .
cd any-worktree
cargo slot build        # or test, check, clippy, run, nextest...
```

## The problem

Running agents in parallel worktrees means every worktree builds from scratch:

- **Separate `target/` per worktree**: N full builds, N copies on disk.
- **Shared `CARGO_TARGET_DIR`**: one build at a time (cargo's lock), and
  workspace crates still rebuild every time you switch worktrees because
  their absolute paths differ.
- **sccache**: skips incremental crates and workspace members, which is
  exactly the code that changes.

## How it works

Each repo gets a small pool of **slots** (default 4) in `~/.cache/cargo-slot`.
A slot is a mirror of a worktree at a fixed path, plus its own target dir.

On `cargo slot <args>`:

1. Pick a slot: this worktree's own idle slot if it has one. Otherwise,
   below `CARGO_SLOT_MAX`, a new slot cloned copy-on-write (APFS clonefile,
   or reflinks on btrfs/XFS) from the closest slot, so dependencies are
   reused without duplicating disk. At the max, the closest idle slot
   (fewest files changed between commits), or wait for one.
2. Sync your worktree into it. The slot ends up holding exactly the files
   git lists, with matching contents and permissions. Only changed files
   are written; everything else keeps its mtime, so cargo's fingerprints
   stay fresh. Files a build created in the slot are removed.
3. Run cargo there. Paths in output are rewritten back to your worktree so
   errors are still clickable. When output goes to a terminal, cargo gets a
   pseudo-terminal, so colors and progress bars still work.
4. Copy any `Cargo.lock` that cargo created or changed back to the worktree.

Disk usage is bounded by the slot count, not the worktree count.

### What runs where

Commands that edit source files run as **plain cargo in the worktree**, since
their edits would otherwise land in the mirror and be thrown away: `fmt`,
`fix`, `clippy --fix`, `add`, `remove`, `update`, `generate-lockfile`,
`vendor`, `insta`, `new`, `init`, plus registry commands like `publish` and
`install`. These get no build sharing.

Everything else, including third-party subcommands, runs in a slot. `run`
gets your terminal directly; other commands go through the path rewriter.
`+toolchain` works: `cargo slot +nightly test`.

### What the slot sees

- Files `git ls-files --cached --others --exclude-standard` lists.
- `Cargo.lock` next to any `Cargo.toml`, even if gitignored.
- Other gitignored files only if listed in `CARGO_SLOT_INCLUDE`, a
  comma-separated list of git globs, e.g. `CARGO_SLOT_INCLUDE=.env,*.local.toml`.
- Path dependencies one level outside the repo (`path = "../other"`) are
  linked into the slot and built from their real location. Deeper escapes
  (`../../x`) are an error.
- `.cargo/config.toml` files in directories above the repo are passed with
  `--config`. That gives them higher priority than the repo's own config,
  which is the reverse of normal cargo. Legacy `.cargo/config` files without
  the extension are skipped with a warning.
- Git: each slot is registered as its own linked worktree (it shows in
  `git worktree list` as `cargo-slot-*`) with a detached HEAD at your
  worktree's commit and its own index. Build scripts that run git see the
  right commit and dirty state, and whatever they do to git stays out of
  your index and branch. Branch names are not visible (detached HEAD).

## Benchmark

`bench/worktrees.sh <repo> [worktrees] [rounds] [edit-file]` checks out
worktrees at different recent commits and builds each. Then each round
edits one file per worktree and rebuilds all of them in turn.

A [weekly CI job](.github/workflows/bench.yml) runs it on Linux with btrfs
and commits the latest numbers to [bench/RESULTS.md](bench/RESULTS.md), with
history in [bench/history.tsv](bench/history.tsv). Run the same thing
locally with `bench/ci.sh`. The table below is a one-off run on a Mac.

ripgrep (60 crates), 5 worktrees, 4 rounds, editing
`crates/searcher/src/lib.rs`, debug builds, M-series Mac, APFS. CPU is
user+sys including rustc, from one run.

| | first builds | edit round 1 | later rounds (each) | disk (physical) |
|---|---|---|---|---|
| plain `cargo build` | 122s | 8.4s | 8.3s | 1563 MB |
| `cargo slot`, `CARGO_SLOT_MAX=1` | 45s | 21s | 21s | 870 MB |
| `cargo slot`, `CARGO_SLOT_MAX=5` | 56s | 14s | 8.9s | 1063 MB |

- With a slot per worktree, first builds use about 55% less CPU and disk is
  about 30% lower. The first edit round costs more (a one-time slowdown on
  the first rebuild in a cloned slot). After that, edits cost about 7% more
  than plain cargo, which is the sync overhead.
- A single shared slot is cheapest on disk but thrashes when worktrees sit
  at different commits.
- Physical disk is the change in used space on the volume, so it's
  approximate. `du` counts clones in full and shows 2694 MB for the 5-slot
  case.

## Commands

```
cargo slot <cargo args>   run cargo in a slot (or the worktree, see above)
cargo slot status         list slots
cargo slot gc [--keep N]  delete idle slots beyond the N most recent
cargo slot clean          delete all idle slots
```

Env: `CARGO_SLOT_MAX` (slot count), `CARGO_SLOT_HOME` (cache location),
`CARGO_SLOT_INCLUDE` (see above). Cargo runs with `CARGO_SLOT_WORKTREE` set
to the real worktree root.

## Caveats

- Builds and tests run in the slot. Anything they write to the source tree
  (snapshot updates, generated files) lands in the slot, not your worktree,
  and is deleted on the next sync. Only `Cargo.lock` is copied back.
- Artifacts live in the slot's target dir, not `./target`. `cargo slot run`
  handles this; for other uses, read the paths cargo prints.
- `cargo slot run` holds its slot until the program exits.
- FIFOs, sockets and devices are not mirrored. Non-UTF-8 paths are an error.
- On filesystems without reflinks, cloning a slot is a full copy.
- Tested on macOS (APFS) and Linux (overlayfs, and btrfs with reflinks).
