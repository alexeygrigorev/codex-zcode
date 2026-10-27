#!/usr/bin/env bash
set -euo pipefail

# Local installation deliberately skips release downloads and configuration.
local_binary=""
local_install_dir=""
while (($#)); do
    case "$1" in
        --binary | --install-dir)
            if (($# < 2)) || [[ -z "$2" ]]; then
                echo "$1 requires a path" >&2
                exit 1
            fi
            case "$1" in
                --binary) local_binary="$2" ;;
                --install-dir) local_install_dir="$2" ;;
            esac
            shift 2
            ;;
        --help)
            echo "Usage: $0 [--binary PATH --install-dir DIR]"
            echo "Local mode atomically installs an executable without accessing configuration."
            exit 0
            ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done
if [[ -n "$local_binary" || -n "$local_install_dir" ]]; then
    if [[ -z "$local_binary" || -z "$local_install_dir" ]]; then
        echo "Local installation requires both --binary and --install-dir" >&2
        exit 1
    fi
    if [[ ! -f "$local_binary" || ! -x "$local_binary" ]]; then
        echo "Local binary must be a regular executable file" >&2
        exit 1
    fi
    destination="$local_install_dir/zcodex"
    if [[ -L "$destination" || ( -e "$destination" && ! -f "$destination" ) ]]; then
        echo "Refusing to replace a non-regular destination: $destination" >&2
        exit 1
    fi
    mkdir -p "$local_install_dir"
    staged_binary="$(mktemp "$local_install_dir/.zcodex-install.XXXXXX")"
    trap 'rm -f "$staged_binary"' EXIT
    cp "$local_binary" "$staged_binary"
    chmod 755 "$staged_binary"
    "$staged_binary" --version
    mv -f "$staged_binary" "$destination"
    echo "Installed local zcodex at $destination"
    exit 0
fi

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
