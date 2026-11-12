# cargo-slot

Share incremental Rust builds across git worktrees.

```sh
cargo install --path .
cd any-worktree
cargo slot build        # or test, clippy, run, nextest...
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
   errors are still clickable.

Disk usage is bounded by the slot count, not the worktree count.

## Benchmark

`bench/worktrees.sh <repo> [worktrees] [rounds] [edit-file]` checks out
worktrees at different recent commits, builds each, then for each round
edits one file per worktree and rebuilds all of them in turn.

ripgrep (60 crates), 5 worktrees, 3 rounds, editing
`crates/searcher/src/lib.rs`, debug builds, M-series Mac, APFS:

| | first builds (CPU) | edit rounds (CPU) | disk (physical) |
|---|---|---|---|
| plain `cargo build` | 124s | 25s | 1586 MB |
| `cargo slot`, `CARGO_SLOT_MAX=1` | 45s | 63s | 782 MB |
| `cargo slot`, `CARGO_SLOT_MAX=5` | 55s | 33s | 1071 MB |

One slot per worktree (the default up to the max) cuts first-build CPU by
about 55% and disk by about 30%. Edits cost about the same as plain cargo
once warm; the extra in the table is a one-time slowdown on the first
rebuild after a clone. A single shared slot is cheapest on disk but
thrashes when worktrees sit at different commits. Physical disk comes from
the change in used space on the volume, so it's approximate. `du` counts
clones in full and shows 2694 MB for the 5-slot case.

## Commands

```
cargo slot <cargo args>   run cargo in a slot
cargo slot status         list slots
cargo slot gc [--keep N]  delete idle slots beyond the N most recent
cargo slot clean          delete all idle slots
```

Env: `CARGO_SLOT_MAX` (slot count), `CARGO_SLOT_HOME` (cache location).
Cargo runs with `CARGO_SLOT_WORKTREE` set to the real worktree root.

## Caveats

- The files mirrored are what `git ls-files --cached --others --exclude-standard`
  lists. Ignored files (like `.env`) and submodule contents are not mirrored.
  Only regular files and symlinks are copied; FIFOs, sockets and devices are
  skipped. Non-UTF-8 paths are an error.
- Builds and tests run in the slot. Anything they write to the source tree
  (e.g. snapshot updates) lands in the slot, not your worktree, and is
  deleted on the next sync.
- Artifacts live in the slot's target dir, not `./target`. `cargo slot run`
  handles this; for other uses, read the paths cargo prints.
- `cargo slot run` holds its slot until the program exits.
- On filesystems without reflinks, cloning a slot is a full copy.
