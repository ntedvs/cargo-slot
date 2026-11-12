#!/usr/bin/env bash
# Compare plain per-worktree `cargo build` with `cargo slot build` when
# several worktrees of one repo build in turn, like parallel agents do.
#
# Usage: bench/worktrees.sh <git repo> [worktrees=3] [rounds=2] [edit-file]
#
# Each worktree starts at a different recent commit. After the first build
# of each, every round edits one file per worktree and rebuilds them all.
# Reports wall time, CPU time (user+sys, includes rustc) and disk. "disk" is
# du (counts copy-on-write clones in full); "phys" is the change in used
# space on the volume, which counts shared blocks once but picks up noise
# from anything else writing to the disk.
set -euo pipefail

REPO=$(cd "$1" && pwd)
N=${2:-3}
ROUNDS=${3:-2}
EDIT=${4:-}
SLOT=${CARGO_SLOT_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-slot}
WORK=$(mktemp -d)
unset CARGO_TARGET_DIR

setup() { # $1 = prefix
  for i in $(seq 1 "$N"); do
    git -C "$REPO" worktree add -q --detach "$WORK/$1-$i" "HEAD~$(( (i - 1) * 3 ))"
  done
}

cleanup() {
  for d in "$WORK"/plain-* "$WORK"/slot-1-* "$WORK"/slot-n-*; do
    [ -d "$d" ] && git -C "$REPO" worktree remove --force "$d" 2>/dev/null || true
  done
  trash "$WORK" 2>/dev/null || rm -rf "$WORK"
}
trap cleanup EXIT

# Runs a build, appends "real cpu" to $WORK/times.
timed() {
  /usr/bin/time -p "$@" >/dev/null 2>"$WORK/err" || { cat "$WORK/err"; exit 1; }
  awk '/^real/{r=$2} /^user/{u=$2} /^sys/{s=$2} END{printf "%s %.2f\n", r, u+s}' "$WORK/err" >>"$WORK/times"
}

edit() { # $1 = worktree, $2 = round
  local f=${EDIT:-$(git -C "$1" ls-files '*.rs' | head -1)}
  echo "// bench edit $2" >>"$1/$f"
}

run() { # $1 = strategy name, rest = build command
  local name=$1; shift
  export CARGO_SLOT_HOME=$WORK/cache-$name
  sync; local used0=$(df -k "$WORK" | awk 'NR==2{print $3}')
  : >"$WORK/times"
  setup "$name"
  for i in $(seq 1 "$N"); do (cd "$WORK/$name-$i" && timed "$@"); done
  local first=$(awk '{w+=$1; c+=$2} END{printf "%.1f %.1f", w, c}' "$WORK/times")
  : >"$WORK/times"
  for r in $(seq 1 "$ROUNDS"); do
    for i in $(seq 1 "$N"); do edit "$WORK/$name-$i" "$r"; (cd "$WORK/$name-$i" && timed "$@"); done
  done
  local rounds=$(awk '{w+=$1; c+=$2} END{printf "%.1f %.1f", w, c}' "$WORK/times")
  local disk
  if [ "$name" = plain ]; then
    disk=$(du -sk "$WORK"/plain-*/target | awk '{s+=$1} END{printf "%.0f", s/1024}')
  else
    disk=$(du -sk "$CARGO_SLOT_HOME" | awk '{printf "%.0f", $1/1024}')
  fi
  sync; local phys=$(df -k "$WORK" | awk -v u0="$used0" 'NR==2{printf "%.0f", ($3-u0)/1024}')
  printf "%-7s first builds: %6ss wall %7ss cpu | edit rounds: %6ss wall %7ss cpu | disk %5s MB  phys %5s MB\n" \
    "$name" ${first% *} ${first#* } ${rounds% *} ${rounds#* } "$disk" "$phys"
}

echo "repo: $REPO  worktrees: $N  rounds: $ROUNDS"
run plain cargo build
CARGO_SLOT_MAX=1 run slot-1 "$SLOT" build
CARGO_SLOT_MAX=$N run slot-n "$SLOT" build
