#!/usr/bin/env bash
# Build a self-contained zcodex bundle:
#   zcodex binary (built from this fork) + the official ZCode headless runtime
#   (extracted from the SHA-512-verified desktop deb) + launcher/installer.
#
# The result is a single tar.gz that runs on any linux-x64 devbox with node
# >= 18 on PATH; no ZCode Desktop install required.
#
# Usage: scripts/build-zcode-bundle.sh [--out DIR] [--skip-build] [--keep-strip]
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/build-zcode-bundle.sh [options]

Options:
  --out DIR     Output directory for the tarball (default: dist)
  --skip-build  Reuse the existing codex-rs/target/dev-small/zcodex binary
  -h, --help    Show this help
EOF
}

out_dir="dist"
skip_build=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) out_dir="${2:?missing value}"; shift 2 ;;
    --skip-build) skip_build=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
mkdir -p "$out_dir"

# ---------------------------------------------------------------- binary ----
# Release profile: same as the GitHub release workflow (opt + thin LTO). The
# dev-small binary is opt-level 0 and ~400 MB; not suitable for distribution.
binary="codex-rs/target/release/zcodex"
if [[ "$skip_build" == false ]]; then
  echo "==> Building zcodex (cargo, release profile)"
  build_env=()
  if [[ -d "${HOME}/.local/cc-bin" ]]; then
    build_env=("PATH=${HOME}/.local/cc-bin:$PATH")
  fi
  (cd codex-rs && env "${build_env[@]}" cargo build --release -p codex-cli --bin zcodex)
fi
[[ -f "$binary" ]] || { echo "binary missing: $binary" >&2; exit 1; }

# --------------------------------------------------------------- runtime ----
echo "==> Downloading official ZCode deb (SHA-512 verified)"
deb_out="$out_dir/zcode-deb"
pkg="$out_dir/zcode-deb-pkg"
"$root/scripts/download-zcode-release.sh" --out "$deb_out" --unpack "$pkg" >/dev/null
zcode_version="$(cat "$deb_out/zcode-version.txt")"
resources="$pkg/opt/ZCode/resources"

fork_version="$(git -C "$root" describe --tags --always --dirty 2>/dev/null || echo dev)"
bundle_name="zcodex-bundle-${fork_version}-zcode${zcode_version}-linux-x64"
stage="$out_dir/$bundle_name"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/zcode" "$stage/scripts"

echo "==> Assembling $bundle_name"
cp "$binary" "$stage/bin/zcodex.bin"
chmod +x "$stage/bin/zcodex.bin"
cp "$root/scripts/ensure-zcode-cli-config.sh" "$stage/scripts/"

# Headless runtime pieces from the official deb. app.asar (the desktop
# Electron app) is deliberately excluded.
cp -r "$resources/glm" "$stage/zcode/glm"
cp -r "$resources/tools" "$stage/zcode/tools"
cp -r "$resources/model-providers" "$stage/zcode/model-providers"
cp -r "$resources/config" "$stage/zcode/config"

cat > "$stage/bin/zcodex" <<'WRAPPER'
#!/usr/bin/env bash
# zcodex bundle launcher. Prefers a desktop-installed runtime (kept current by
# ZCode Desktop updates) and falls back to the runtime shipped in this bundle.
set -euo pipefail
here="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
if [[ -z "${ZCODE_CJS:-}" ]]; then
  if [[ -f /opt/ZCode/resources/glm/zcode.cjs ]]; then
    ZCODE_CJS=/opt/ZCode/resources/glm/zcode.cjs
  else
    ZCODE_CJS="$here/zcode/glm/zcode.cjs"
  fi
fi
export ZCODE_CJS
exec "$here/bin/zcodex.bin" "$@"
WRAPPER
chmod +x "$stage/bin/zcodex"

cat > "$stage/install.sh" <<'INSTALLER'
#!/usr/bin/env bash
# Install the bundle for the current user: symlink the launcher onto PATH and
# bootstrap ~/.zcodex config. Idempotent.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$HOME/.local/bin"
ln -sfn "$here/bin/zcodex" "$HOME/.local/bin/zcodex"

if [[ ! -f "$HOME/.zcodex/config.toml" ]]; then
  mkdir -p "$HOME/.zcodex"
  cat > "$HOME/.zcodex/config.toml" <<'EOF'
model = "glm-5.3-flash"
model_reasoning_effort = "max"
model_provider = "zcode"

[model_providers.zcode]
name = "ZCode"
base_url = ""
wire_api = "zcode"
EOF
  echo "Wrote $HOME/.zcodex/config.toml"
fi

"$here/scripts/ensure-zcode-cli-config.sh" || true

case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "Note: add ~/.local/bin to your PATH to run zcodex directly." ;;
esac

echo "Installed. Try: zcodex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check 'Reply with: OK'"
INSTALLER
chmod +x "$stage/install.sh"

cat > "$stage/BUNDLE.md" <<DOCS
# zcodex bundle ${fork_version} (ZCode runtime ${zcode_version})

Self-contained zcodex: the Codex-fork CLI plus the official ZCode headless
runtime extracted from the SHA-512-verified desktop deb.

Requires: linux-x64, bash, node >= 18 on PATH.

## Use in place

    ./install.sh                 # symlink ~/.local/bin/zcodex + bootstrap config
    zcodex                       # interactive TUI (in any project directory)
    zcodex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check 'PING'

## Auth

The runtime reads the ZCode coding-plan OAuth credential from
~/.zcode/cli/config.json. scripts/ensure-zcode-cli-config.sh rebuilds it from
a logged-in desktop configuration (~/.zcode/v2/config.json); on a fresh devbox
copy ~/.zcode from a machine where ZCode Desktop is logged in.

## Runtime selection

bin/zcodex prefers /opt/ZCode/resources/glm/zcode.cjs when ZCode Desktop is
installed (so desktop updates are picked up automatically) and otherwise uses
the bundled zcode/glm/zcode.cjs. Override either with ZCODE_CJS.
DOCS

echo "==> Packing"
tarball="$out_dir/$bundle_name.tar.gz"
tar -czf "$tarball" -C "$out_dir" "$bundle_name"
rm -rf "$stage"

echo "Bundle: $tarball ($(du -h "$tarball" | cut -f1))"
echo "Unpack anywhere, then: ./$bundle_name/install.sh"
