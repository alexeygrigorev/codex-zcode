# Codex ZCode

This repository is a lightweight distribution of Codex CLI with a native
ZCode integration. It adds two tools to Codex's normal tool loop:

- `zcode_prompt`: run one task through ZCode's non-Electron headless agent.
- `zai_search`: search the web through Z.AI Search Prime.

The integration is intentionally native to Codex. ZCode and Z.AI are invoked as
tools from Codex's regular agent loop, rather than replacing Codex with a
separate UI or wrapping it behind an HTTP compatibility layer.

## What You Need

The release binary contains Codex with the integration. It does **not** contain
ZCode's runtime or credentials.

You need:

1. The `codex-zcode` release binary from GitHub Releases.
2. ZCode Desktop 3.9.2 or newer, which provides:
   `/opt/ZCode/resources/glm/zcode.cjs`
3. A running local Z.AI proxy, normally on:
   `http://127.0.0.1:18765/zai/v1/responses`
4. ZCode CLI credentials, normally under `~/.zcode`.

ZCode's headless runtime is part of the ZCode Desktop package. It is not bundled
with this project.

## Download Release

Download the matching binary from:

https://github.com/alexeygrigorev/codex-zcode/releases/tag/zcode-v0.1.0

Linux AMD64 example:

```bash
curl -fL \
  https://github.com/alexeygrigorev/codex-zcode/releases/download/zcode-v0.1.0/codex-zcode-linux-amd64 \
  -o ~/.local/bin/codex-zcode
chmod +x ~/.local/bin/codex-zcode
```

Linux ARM64:

```bash
curl -fL \
  https://github.com/alexeygrigorev/codex-zcode/releases/download/zcode-v0.1.0/codex-zcode-linux-arm64 \
  -o ~/.local/bin/codex-zcode
chmod +x ~/.local/bin/codex-zcode
```

Verify the downloaded file against `SHA256SUMS` from the same release.

## Quick Build

Use this for local development and testing. It uses an optimized enough, but
fast local profile and does not create a release.

```bash
cd codex-rs
cargo build --profile dev-small -p codex-cli --bin codex
./target/dev-small/codex
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
- `ZAI_SEARCH_URL=http://127.0.0.1:18765/zai/v1/responses`

ZCode sessions are independent by default. Pass a prior `session_id` to
`zcode_prompt` explicitly when you want to resume one.

## Upstream Synchronization

Upstream's `README.md` is intentionally unchanged. This project documents itself
in `ABOUT.md` so upstream README changes can be merged without conflict.
