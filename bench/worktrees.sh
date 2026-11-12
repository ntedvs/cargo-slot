#!/usr/bin/env bash
# Compare plain per-worktree `cargo build` with `cargo slot build` when
# several worktrees of one repo build in turn, like parallel agents do.
#
# Usage: bench/worktrees.sh <git repo> [worktrees=3] [rounds=2] [edit-file]
#
# Each worktree starts at a different recent commit. After the first build
# of each, every round edits one file per worktree and rebuilds them all.
# Round 1 is reported apart from later rounds: it includes one-time costs
# such as the first rebuild in a freshly cloned slot.
#
# Reports CPU time (user+sys, includes rustc) and disk. "disk" is du
# (counts copy-on-write clones in full); "phys" is the change in used space
# on the volume, which counts shared blocks once but picks up noise from
# anything else writing to the disk.
#
# Env:
#   BENCH_DIR   where worktrees and slots go (default: a temp dir). Put it on
#               the filesystem you want to measure, e.g. btrfs for reflinks.
#   BENCH_TSV   if set, also append one tab-separated line per strategy:
#               name first_wall first_cpu round1_cpu later_cpu disk_mb phys_mb
set -euo pipefail

REPO=$(cd "$1" && pwd)
N=${2:-3}
ROUNDS=${3:-2}
EDIT=${4:-}
SLOT=${CARGO_SLOT_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-slot}
WORK=$(mktemp -d "${BENCH_DIR:-${TMPDIR:-/tmp}}/bench.XXXXXX")
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
  git -C "$REPO" worktree prune
  trash "$WORK" 2>/dev/null || rm -rf "$WORK"
}
trap cleanup EXIT

# Runs a build, appends "wall cpu" to $WORK/times. Uses bash's `time`, which
# includes waited-for children (cargo, rustc), so no /usr/bin/time needed.
timed() {
  local t
  if ! t=$( { TIMEFORMAT='%R %U %S'; time "$@" >/dev/null 2>"$WORK/err"; } 2>&1 ); then
    cat "$WORK/err" >&2
    exit 1
  fi
  echo "$t" | awk '{printf "%s %.2f\n", $1, $2 + $3}' >>"$WORK/times"
}

edit() { # $1 = worktree, $2 = round
  local f=${EDIT:-$(git -C "$1" ls-files '*.rs' | head -1)}
  echo "// bench edit $2" >>"$1/$f"
}

used_kb() { sync; df -Pk "$WORK" | awk 'NR==2{print $3}'; }

run() { # $1 = strategy name, rest = build command
  local name=$1; shift
  export CARGO_SLOT_HOME=$WORK/cache-$name
  local used0; used0=$(used_kb)
  setup "$name"

  : >"$WORK/times"
  for i in $(seq 1 "$N"); do (cd "$WORK/$name-$i" && timed "$@"); done
  local first_wall first_cpu
  read -r first_wall first_cpu < <(awk '{w+=$1; c+=$2} END{printf "%.1f %.1f\n", w, c}' "$WORK/times")

  : >"$WORK/times"
  for i in $(seq 1 "$N"); do edit "$WORK/$name-$i" 1; (cd "$WORK/$name-$i" && timed "$@"); done
  local round1; round1=$(awk '{c+=$2} END{printf "%.1f", c}' "$WORK/times")

  : >"$WORK/times"
  for r in $(seq 2 "$ROUNDS"); do
    for i in $(seq 1 "$N"); do edit "$WORK/$name-$i" "$r"; (cd "$WORK/$name-$i" && timed "$@"); done
  done
  # CPU per round, averaged over rounds 2..ROUNDS.
  local later; later=$(awk -v n=$((ROUNDS - 1)) '{c+=$2} END{printf "%.1f", n ? c/n : 0}' "$WORK/times")

  local disk
  if [ "$name" = plain ]; then
    disk=$(du -sk "$WORK"/plain-*/target | awk '{s+=$1} END{printf "%.0f", s/1024}')
  else
    disk=$(du -sk "$CARGO_SLOT_HOME" | awk '{printf "%.0f", $1/1024}')
  fi
  local phys; phys=$(( ($(used_kb) - used0) / 1024 ))

  printf "%-7s first builds: %6ss wall %7ss cpu | edit round 1: %6ss cpu | later rounds: %6ss cpu/round | disk %5s MB  phys %5s MB\n" \
    "$name" "$first_wall" "$first_cpu" "$round1" "$later" "$disk" "$phys"
  if [ -n "${BENCH_TSV:-}" ]; then
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$first_wall" "$first_cpu" "$round1" "$later" "$disk" "$phys" >>"$BENCH_TSV"
  fi
}

echo "repo: $REPO  worktrees: $N  rounds: $ROUNDS  dir: $WORK"
run plain cargo build
CARGO_SLOT_MAX=1 run slot-1 "$SLOT" build
CARGO_SLOT_MAX=$N run slot-n "$SLOT" build
