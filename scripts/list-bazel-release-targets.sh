#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Keep this list focused on first-party Rust targets whose compile surface can
# differ when `cfg(not(debug_assertions))` becomes active.
#
# The normal test job covers the Wine smoke test; omit its downloaded runtime
# and cross-compile from this build-only release sweep.
printf '%s\n' \
  "//codex-rs/..." \
  "-//codex-rs/core/tests/remote_env_windows:smoke-test" \
