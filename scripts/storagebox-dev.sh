#!/usr/bin/env bash
# Linux development process: local Git objects, SSHFS worktrees and build output.
set -euo pipefail

usage() {
  echo 'Usage: scripts/storagebox-dev.sh worktree NAME [REF] | run COMMAND [ARG...] | paths'
}
fail() { echo "storagebox-dev: $*" >&2; exit 1; }

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  worktree|run|paths) action="$1"; shift ;;
  *) usage >&2; exit 2 ;;
esac
mount_root="${ZCODE_STORAGEBOX_ROOT:-$HOME/storagebox}"
[[ "$mount_root" = /* ]] || fail 'mount root must be absolute'
mount_root="$(timeout 10 realpath -m "$mount_root")" || fail 'mount path resolution timed out'
[[ "$(findmnt -rn -M "$mount_root" -o FSTYPE)" = fuse.sshfs ]] || fail 'expected an existing SSHFS mount; refusing local fallback'
# Bound probing of a disconnected mount before creating anything.
timeout 10 stat "$mount_root/." >/dev/null || fail 'remote mount is unavailable'

repo="$(git -C "$(dirname "${BASH_SOURCE[0]}")/.." rev-parse --show-toplevel)"
name="${ZCODE_DEV_TASK:-$(basename "$repo")}"
[[ "$name" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ ]] || fail 'task name must be a simple path component'
worktree_root="$mount_root/worktrees/codex-zcode"
export CARGO_TARGET_DIR="$mount_root/targets/codex-zcode/$name"
export TMPDIR="$mount_root/tmp/codex-zcode/$name"
export ZCODE_BUNDLE_OUT_DIR="$mount_root/build-output/codex-zcode/$name"
# Reject local overrides instead of silently accepting a build on root disk.
[[ -z "${CARGO_BUILD_TARGET_DIR:-}" || "$CARGO_BUILD_TARGET_DIR" = "$CARGO_TARGET_DIR" ]] || fail 'conflicting CARGO_BUILD_TARGET_DIR'
[[ -z "${CARGO_BUILD_BUILD_DIR:-}" ]] || fail 'unset CARGO_BUILD_BUILD_DIR to keep intermediate output on remote target'
export CARGO_BUILD_TARGET_DIR="$CARGO_TARGET_DIR"

for remote_path in "$worktree_root" "$CARGO_TARGET_DIR" "$TMPDIR" "$ZCODE_BUNDLE_OUT_DIR"; do
  resolved_path="$(timeout 10 realpath -m "$remote_path")" || fail 'remote path resolution timed out'
  [[ "$resolved_path" = "$mount_root/"* ]] || fail 'remote path escapes mount (possibly through a symlink)'
done

case "$action" in
  paths)
    printf 'worktrees=%s\nCARGO_TARGET_DIR=%s\nTMPDIR=%s\nZCODE_BUNDLE_OUT_DIR=%s\n' "$worktree_root" "$CARGO_TARGET_DIR" "$TMPDIR" "$ZCODE_BUNDLE_OUT_DIR"
    ;;
  worktree)
    [[ $# -ge 1 && $# -le 2 ]] || { usage >&2; exit 2; }
    [[ "$1" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ ]] || fail 'worktree name must be a simple path component'
    mkdir -p "$worktree_root"
    # Git's object database stays in the local main clone. No existing tree moves.
    git -C "$repo" worktree add -b "dev/$1" "$worktree_root/$1" "${2:-HEAD}"
    ;;
  run)
    [[ $# -gt 0 ]] || { usage >&2; exit 2; }
    # Keep the lock local: SSHFS advisory locks are not a concurrency gate.
    lock_dir="$HOME/.local/state/codex-zcode-dev/locks"
    mkdir -p "$lock_dir"
    exec 9>"$lock_dir/$name.lock"
    flock -n 9 || fail 'another command owns this task target/output; choose a distinct task'
    mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR" "$ZCODE_BUNDLE_OUT_DIR"
    exec "$@"
    ;;
esac
