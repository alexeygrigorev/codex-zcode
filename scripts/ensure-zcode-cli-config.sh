#!/usr/bin/env bash
# Restore ~/.zcode/cli/config.json from the desktop OAuth configuration.
#
# Headless ZCode (and therefore zcodex) reads the legacy
# ~/.zcode/cli/config.json for model credentials. ZCode Desktop updates have
# been observed to migrate that file away, which breaks every headless run
# with "Model config is missing". The active OAuth credentials still live in
# ~/.zcode/v2/config.json under provider builtin:zai-coding-plan; this script
# rebuilds the legacy file from there.
#
# Usage: scripts/ensure-zcode-cli-config.sh [model]
#   model defaults to zai/glm-5.3-flash; pass e.g. zai/GLM-5.3 to override.
set -euo pipefail

CLI_DIR="${HOME}/.zcode/cli"
CLI_CONFIG="${CLI_DIR}/config.json"
V2_CONFIG="${HOME}/.zcode/v2/config.json"
MODEL="${1:-zai/glm-5.3-flash}"

if [[ -f "${CLI_CONFIG}" ]]; then
    echo "OK: ${CLI_CONFIG} already exists"
    exit 0
fi

if [[ ! -f "${V2_CONFIG}" ]]; then
    echo "Error: ${V2_CONFIG} not found. Log in to ZCode Desktop once so the" >&2
    echo "OAuth configuration exists, then re-run this script." >&2
    exit 1
fi

mkdir -p "${CLI_DIR}"

MODEL="${MODEL}" V2_CONFIG="${V2_CONFIG}" CLI_CONFIG="${CLI_CONFIG}" python3 - <<'PY'
import json
import os

with open(os.environ["V2_CONFIG"]) as f:
    v2 = json.load(f)

provider = (v2.get("provider") or {}).get("builtin:zai-coding-plan")
options = (provider or {}).get("options") or {}
api_key = options.get("apiKey")
base_url = options.get("baseURL")
if not api_key or not base_url:
    raise SystemExit(
        "Error: builtin:zai-coding-plan in the desktop config has no usable "
        "apiKey/baseURL. Log in to ZCode Desktop and re-run this script."
    )

config = {
    "provider": {
        "zai": {
            "kind": "anthropic",
            "name": "Z.AI",
            "options": {"apiKey": api_key, "baseURL": base_url},
            "models": {
                os.environ["MODEL"].split("/", 1)[1]: {"name": os.environ["MODEL"].split("/", 1)[1]},
            },
        }
    },
    "model": os.environ["MODEL"],
}

fd = os.open(os.environ["CLI_CONFIG"], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(config, f, indent=2)
PY

echo "Restored ${CLI_CONFIG} (model ${MODEL}) from the desktop OAuth config"
