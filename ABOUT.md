# Codex ZCode

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

## Environment

ZCode runtime discovery:

- `ZCODE_CJS=/opt/ZCode/resources/glm/zcode.cjs`

Optional overrides:

- `ZCODE_NODE=/path/to/node`

Configure `~/.zcodex/config.toml`:

```toml
model = "GLM-5.2"
model_provider = "zcode"

[model_providers.zcode]
name = "ZCode"
base_url = ""
wire_api = "zcode"
```

## Upstream Synchronization

Upstream's `README.md` is intentionally unchanged. This project documents itself
in `ABOUT.md` so upstream README changes can be merged without conflict.
