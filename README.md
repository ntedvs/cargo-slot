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

1. Pick the idle slot that needs the least work (same worktree first, then
   fewest files changed between commits).
2. Sync your worktree into it. Only files whose content changed are written.
   Everything else keeps its mtime, so cargo's fingerprints stay fresh.
3. Run cargo there. Paths in output are rewritten back to your worktree so
   errors are still clickable.

So switching worktrees costs only what actually differs. If every slot is busy,
a new one is made as a copy-on-write clone of the closest slot (APFS clonefile,
or reflinks on btrfs/XFS), so dependencies are reused without using more disk.
At the max, it waits for a slot.

Disk usage is bounded by the slot count, not the worktree count.

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
- Builds and tests run in the slot. Anything they write to the source tree
  (e.g. snapshot updates) lands in the slot, not your worktree.
- Artifacts live in the slot's target dir, not `./target`. `cargo slot run`
  handles this; for other uses, read the paths cargo prints.
- `cargo slot run` holds its slot until the program exits.
- On filesystems without reflinks, cloning a slot is a full copy.
