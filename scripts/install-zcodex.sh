#!/usr/bin/env bash
set -euo pipefail

REPO_URL="https://github.com/alexeygrigorev/codex-zcode"
INSTALL_DIR="${HOME}/.local/bin"
CONFIG_DIR="${HOME}/.zcodex"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64) asset="zcodex-linux-amd64.tar.zst" ;;
    aarch64 | arm64) asset="zcodex-linux-arm64.tar.zst" ;;
    *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

mkdir -p "$INSTALL_DIR" "$CONFIG_DIR"
echo "Downloading $asset..."
# Download and extract into a temp dir first so a failed download never
# leaves a broken binary at the install location. Requires tar with zstd
# support (the zstd package) and curl.
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
curl -fL "$REPO_URL/releases/latest/download/$asset" -o "$tmpdir/$asset"
tar --zstd -xf "$tmpdir/$asset" -C "$tmpdir"
chmod +x "$tmpdir/${asset%.tar.zst}"
mv "$tmpdir/${asset%.tar.zst}" "$INSTALL_DIR/zcodex"

if [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
    echo "Writing default config to $CONFIG_DIR/config.toml..."
    cat > "$CONFIG_DIR/config.toml" <<'EOF'
model = "glm-5.3-flash"
model_reasoning_effort = "max"
model_provider = "zcode"

[model_providers.zcode]
name = "ZCode"
base_url = ""
wire_api = "zcode"
EOF
fi

echo "Installed zcodex at $INSTALL_DIR/zcodex"
echo "Configuration at $CONFIG_DIR/config.toml"
echo "Make sure $INSTALL_DIR is in your PATH."
