#!/usr/bin/env bash
# Download a ZCode desktop release from the official update manifest,
# verify its publisher-supplied SHA-512 digest, and optionally unpack or
# install it.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/download-zcode-release.sh [options]

Options:
  --asset deb|rpm|appimage  Asset type to download (default: deb)
  --out DIR                 Download directory (default: dist)
  --channel stable|preview  Release channel (default: stable)
  --endpoint URL            Manifest endpoint origin (default: https://zcode.z.ai)
  --unpack DIR              Extract a deb package into DIR
  --install                 Install the deb package with sudo dpkg -i
  --verify-installed        Compare the installed /opt runtime against the
                            freshly downloaded official deb (no sudo needed)
  -h, --help                Show this help
EOF
}

asset="deb"
channel="stable"
endpoint="https://zcode.z.ai"
out_dir="dist"
unpack_dir=""
install_package=false
verify_installed=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --asset) asset="${2:?missing value}"; shift 2 ;;
    --out) out_dir="${2:?missing value}"; shift 2 ;;
    --channel) channel="${2:?missing value}"; shift 2 ;;
    --endpoint) endpoint="${2:?missing value}"; shift 2 ;;
    --unpack) unpack_dir="${2:?missing value}"; shift 2 ;;
    --install) install_package=true; shift ;;
    --verify-installed) verify_installed=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "${asset,,}" in
  deb|rpm|appimage) asset="${asset,,}" ;;
  *) echo "--asset must be deb, rpm, or appimage" >&2; exit 2 ;;
esac
case "${channel,,}" in
  stable) channel_id=1 ;;
  preview) channel_id=3 ;;
  *) echo "--channel must be stable or preview" >&2; exit 2 ;;
esac

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform="linux-x86_64"; asset_platform="linux-x64" ;;
  Linux-aarch64) platform="linux-aarch64"; asset_platform="linux-arm64" ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

manifest_url="$endpoint/api/v1/releases/electron/manifest?platform=$platform&channel=$channel_id"
mkdir -p "$out_dir"
manifest_path="$out_dir/zcode-$platform-$channel.manifest.yml"

echo "Fetching $manifest_url"
curl --fail --location --silent --show-error "$manifest_url" -o "$manifest_path"

version="$(awk '$1 == "version:" { print $2; exit }' "$manifest_path")"
[[ -n "$version" ]] || { echo "no version found in manifest" >&2; exit 1; }

declared_size=""
expected_b64=""
asset_url=""
entry="$(python3 - "$manifest_path" "$asset" <<'PY'
import sys, yaml

path, asset = sys.argv[1:]
with open(path, encoding="utf-8") as f:
    manifest = yaml.safe_load(f)

suffix = f".{asset}"
match = next((x for x in manifest.get("files", []) if str(x.get("url", "")).endswith(suffix)), None)
if not match:
    raise SystemExit(1)

for key in ("url", "sha512", "size"):
    value = str(match.get(key, ""))
    if not value:
        raise SystemExit(f"missing {key}")
    print(value)
PY
)"
while IFS= read -r line; do
  case "${entries_count:-0}" in
    0) asset_url="$line" ;;
    1) expected_b64="$line" ;;
    2) declared_size="$line" ;;
  esac
  entries_count=$(( ${entries_count:-0} + 1 ))
done <<< "$entry"

[[ -n "$asset_url" && -n "$expected_b64" ]] || {
  echo "no $asset asset for $version in manifest" >&2
  exit 1
}

filename="${asset_url##*/}"
download_path="$out_dir/$filename"

echo "Downloading $asset_url"
curl --fail --location --silent --show-error "$asset_url" -o "$download_path"

actual_size="$(stat -c '%s' "$download_path")"
if [[ -n "$declared_size" && "$actual_size" != "$declared_size" ]]; then
  echo "size mismatch: expected $declared_size, got $actual_size" >&2
  exit 1
fi

actual_hex="$(sha512sum "$download_path" | cut -d' ' -f1)"
expected_hex="$(printf %s "$expected_b64" | base64 -d | od -An -v -tx1 | tr -d ' \n')"
echo "Verified SHA-512 $actual_hex"
[[ "$actual_hex" == "$expected_hex" ]] || {
  echo "checksum mismatch for $download_path" >&2
  exit 1
}

printf '%s\n' "$version" > "$out_dir/zcode-version.txt"
printf '%s\n' "$download_path"

if [[ "$verify_installed" == true ]]; then
  [[ "$asset" == "deb" ]] || { echo "--verify-installed requires --asset deb" >&2; exit 2; }
  if [[ -z "$unpack_dir" ]]; then
    unpack_dir="$(mktemp -d)"
    verify_cleanup=true
  fi
fi

if [[ -n "$unpack_dir" ]]; then
  [[ "$asset" == "deb" ]] || { echo "--unpack requires --asset deb" >&2; exit 2; }
  mkdir -p "$unpack_dir"
  dpkg-deb -x "$download_path" "$unpack_dir"
fi

if [[ "$install_package" == true ]]; then
  [[ "$asset" == "deb" ]] || { echo "--install requires --asset deb" >&2; exit 2; }
  sudo dpkg -i "$download_path"
fi

if [[ "$verify_installed" == true ]]; then
  installed_runtime="/opt/ZCode/resources/glm/zcode.cjs"
  bundled_runtime="$unpack_dir/opt/ZCode/resources/glm/zcode.cjs"
  if [[ ! -f "$installed_runtime" || ! -f "$bundled_runtime" ]]; then
    echo "runtime file missing (installed: $installed_runtime, bundled: $bundled_runtime)" >&2
    exit 1
  fi
  installed_sum="$(sha256sum "$installed_runtime" | cut -d' ' -f1)"
  bundled_sum="$(sha256sum "$bundled_runtime" | cut -d' ' -f1)"
  echo "installed zcode.cjs: $installed_sum"
  echo "official  zcode.cjs: $bundled_sum"
  if [[ "$installed_sum" == "$bundled_sum" ]]; then
    echo "OK: installed runtime matches official ZCode $version deb"
    [[ "${verify_cleanup:-false}" == true ]] && rm -rf "$unpack_dir"
  else
    echo "MISMATCH: installed runtime differs from official ZCode $version deb" >&2
    echo "unofficial copy kept at $unpack_dir for comparison" >&2
    exit 1
  fi
fi
