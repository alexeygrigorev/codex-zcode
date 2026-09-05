# Codex ZCode
model_reasoning_effort = "max"

This repository is a fork of Codex CLI that uses ZCode as its native model
backend. The executable is `zcodex`, and its configuration lives in
`~/.zcodex`, so it does not conflict with ordinary Codex or `~/.codex`.

## Architecture

```text
zcodex TUI / exec (terminal UI, approval flow, sandbox)
    | ModelClient WireApi::Zcode
    v
node zcode.cjs --prompt <text> --json --mode yolo --cwd <dir>
    |
    v
ZCode headless agent (model, web search, internal tools)
```

There is no HTTP proxy and no tool indirection. `WireApi::Zcode` spawns the
ZCode headless CLI for each model turn and maps its JSON result into Codex's
normal response stream.

## What You Need

The release binary contains Codex with the integration. It does **not** contain
ZCode's runtime or credentials.

You need:

1. The `zcodex` release binary from GitHub Releases.
2. ZCode Desktop 3.9.2 or newer, which provides:
   `/opt/ZCode/resources/glm/zcode.cjs`
3. ZCode CLI credentials, normally under `~/.zcode/cli/config.json`.

ZCode's headless runtime is part of the ZCode Desktop package. It is not bundled
with this project.

## Download Release

Download the matching binary from:

https://github.com/alexeygrigorev/codex-zcode/releases/tag/zcode-v0.1.0

Linux AMD64 example:

```bash
curl -fL \
  https://github.com/alexeygrigorev/codex-zcode/releases/download/zcode-v0.1.0/zcodex-linux-amd64 \
  -o ~/.local/bin/zcodex
chmod +x ~/.local/bin/zcodex
```

Linux ARM64:

```bash
curl -fL \
  https://github.com/alexeygrigorev/codex-zcode/releases/download/zcode-v0.1.0/zcodex-linux-arm64 \
  -o ~/.local/bin/zcodex
chmod +x ~/.local/bin/zcodex
```

Verify the downloaded file against `SHA256SUMS` from the same release.

## Quick Build

Use this for local development and testing. It uses an optimized enough, but
fast local profile and does not create a release.

```bash
cd codex-rs
cargo build --profile dev-small -p codex-cli --bin zcodex
./target/dev-small/zcodex
```

Typical timings:

- no change: 1-2 seconds
- Zcode extension change: 15-25 seconds
- broader CLI change: 40-60 seconds

## Release Build

Use GitHub Actions for distributable release binaries.

```bash
git tag zcode-v0.2.0
git push origin zcode-v0.2.0
```

The workflow checks the Zcode extension, builds Linux AMD64 and ARM64 release
binaries, verifies them, and publishes a GitHub Release with `SHA256SUMS`.
Typical CI wall time is 40-45 minutes.

## Self-Contained Bundle

For a remote devbox, build one executable package that carries both the
release-built `zcodex` binary and the official ZCode headless runtime
(extracted from the SHA-512-verified desktop deb — the setup that keeps
coding-plan OAuth discounts):

```bash
scripts/build-zcode-bundle.sh
```

The result under `dist/` is a tar.gz you can unpack anywhere (linux-x64,
bash, node >= 18; no ZCode Desktop required) and install with
`./install.sh`. The bundled launcher prefers a `/opt` desktop-installed
runtime when present and falls back to the bundled copy; override with
`ZCODE_CJS`. Rebuild the bundle to pick up a newer ZCode release.

Supporting scripts:

- `scripts/download-zcode-release.sh` — fetch and checksum-verify the
  official deb; `--verify-installed` compares it against `/opt`
- `scripts/ensure-zcode-cli-config.sh` — rebuild `~/.zcode/cli/config.json`
  from the desktop OAuth configuration after Desktop updates migrate it away
- `scripts/install-zcodex.sh` — binary-only install from GitHub Releases
- `tests/zcodex-integration.test.mjs` — end-to-end suite (exec, tool loop,
  model control, native subagent spawn); `node --test tests/` from the repo
  root on a machine with credentials

## Environment

ZCode runtime discovery:

- `ZCODE_CJS=/opt/ZCode/resources/glm/zcode.cjs`

Optional overrides:

- `ZCODE_NODE=/path/to/node`

Configure `~/.zcodex/config.toml`:

```toml
model = "glm-5.3-flash"
model_provider = "zcode"

[model_providers.zcode]
name = "ZCode"
base_url = ""
wire_api = "zcode"
```

## Upstream Synchronization

Upstream's `README.md` is intentionally unchanged. This project documents itself
in `ABOUT.md` so upstream README changes can be merged without conflict.
