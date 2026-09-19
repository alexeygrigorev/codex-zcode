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
2. ZCode Desktop 3.11.2 or newer, which provides:
   `/opt/ZCode/resources/glm/zcode.cjs`
3. ZCode CLI credentials, normally under `~/.zcode/cli/config.json`.

ZCode's headless runtime is part of the ZCode Desktop package. It is not bundled
with this project.

## Download Release

Download the matching binary from the latest GitHub Release, for example:

https://github.com/alexeygrigorev/codex-zcode/releases/tag/zcode-3.11.2-codex-0.153.4

Linux AMD64 example (replace the tag with the release you want):

```bash
curl -fL \
  https://github.com/alexeygrigorev/codex-zcode/releases/download/zcode-3.11.2-codex-0.153.4/zcodex-linux-amd64 \
  -o ~/.local/bin/zcodex
chmod +x ~/.local/bin/zcodex
```

Linux ARM64:

```bash
curl -fL \
  https://github.com/alexeygrigorev/codex-zcode/releases/download/zcode-3.11.2-codex-0.153.4/zcodex-linux-arm64 \
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
git tag zcode-3.11.2-codex-0.153.4
git push origin zcode-3.11.2-codex-0.153.4
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
  model control, native subagent spawn);
  `node --test tests/zcodex-integration.test.mjs` from the repo
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

## Troubleshooting

### `无法定位 CLI ZCode Built-in Provider Config` (ZCode 3.14+)

Symptom when starting a new session:

```text
stream disconnected before completion: ZCode exited unsuccessfully (exit status: 1);
stderr: 无法定位 CLI ZCode Built-in Provider Config：
/opt/ZCode/resources/glm/provider/zcode-builtin.json, /config/provider/zcode-builtin.json
```

Cause: ZCode Desktop 3.14+ ships the file at
`/opt/ZCode/resources/config/provider/zcode-builtin.json`, but the headless
runtime looks under `/opt/ZCode/resources/glm/provider/` (plus a bad
`/config/provider/` fallback). The install was tested against 3.11.2.

Fix: copy the shipped file to both expected locations, then verify headless:

```bash
ls -l /opt/ZCode/resources/config/provider/zcode-builtin.json
sudo mkdir -p /opt/ZCode/resources/glm/provider /config/provider
sudo cp /opt/ZCode/resources/config/provider/zcode-builtin.json \
  /opt/ZCode/resources/glm/provider/zcode-builtin.json
sudo cp /opt/ZCode/resources/config/provider/zcode-builtin.json \
  /config/provider/zcode-builtin.json
node /opt/ZCode/resources/glm/zcode.cjs --prompt hi --json --mode yolo --cwd /tmp
```

For a self-contained bundle (`scripts/build-zcode-bundle.sh`), the same drift
applies under `zcode/`: copy `zcode/config/provider/zcode-builtin.json` to
`zcode/glm/provider/zcode-builtin.json` inside the unpacked bundle.
Re-apply after every ZCode Desktop update until upstream fixes the lookup.

## Upstream Synchronization

Upstream's `README.md` is intentionally unchanged. This project documents itself
in `ABOUT.md` so upstream README changes can merge without conflict.

Sync is one-way: upstream (`openai/codex`) flows into this fork, never back.
The fork tracks upstream with a remote (already configured in fresh clones;
add once otherwise):

```bash
git remote add upstream https://github.com/openai/codex.git
```

Sync process (run from `main` with a clean tree):

1. `git fetch upstream main` (add `--tags` when you need release versions).
2. Find the fork point and check drift:
   ```bash
   fork_point="$(git merge-base HEAD upstream/main)"
   git log --oneline "$fork_point"..upstream/main | wc -l
   ```
3. Preview collisions with fork-touched files:
   ```bash
   comm -12 <(git diff "$fork_point"..upstream/main --name-only | sort) \
     <(git diff "$fork_point"..HEAD --name-only | sort)
   ```
4. `git merge upstream/main --no-commit`.
5. Triage conflicts. Known recurring resolutions:
   - `codex-rs/Cargo.toml` reqwest/sentry: keep the fork's rustls-only
     (`default-features = false`) options, take upstream version bumps.
   - `codex-rs/Cargo.lock`: take upstream's (`git checkout --theirs`),
     then rebuild once so Cargo re-registers the `codex-zcode` member
     and fetches any new upstream dependencies.
   - Fork behavior additions (Zcode wire checks, model presets): rebase
     them onto renamed upstream symbols (e.g. new function parameters).
   - Upstream API changes (e.g. lifetime-parameterized `ToolCall`): migrate
     `codex-rs/ext/zcode` following an upstream in-tree extension such as
     `ext/web-search`.
   - New upstream `.github/workflows/*` files arrive live; move them to
     `.github/workflows.disabled/` (renamed `.disabled`) to keep the
     no-upstream-CI invariant. Edits to disabled workflows merge into the
     renamed copies harmlessly.
6. Rebuild: `cargo build --profile dev-small -p codex-cli --bin zcodex`
   (drop `--offline` if upstream added dependencies missing from the cache),
   then verify `--locked` builds for release CI.
7. Compile-check tests: `cargo check --profile dev-small --tests
   -p codex-core -p codex-zcode` (plus any crate with merge fallout).
8. Smoke test: `zcodex --version` and `zcodex "hello" < /dev/null`
   (expect `stdin is not a terminal`, same as stock `zcodex`).
9. Commit the merge. Pitfall: fixes you make after resolving conflicts are
   unstaged worktree changes — run `git status` and stage everything you
   intend (`git add` the post-merge fixes too) before committing, otherwise
   the merge commit captures the pre-fix state. Prefer committing the merge
   first, then post-merge fixes as a separate commit.
10. Push to origin when ready: `git push origin main`.

Release checklist after a sync: confirm the ZCode stable runtime
(`scripts/download-zcode-release.sh` prints the manifest version; compare
with the installed deb via `--verify-installed`), run the end-to-end suite
(`node --test tests/zcodex-integration.test.mjs`), then cut a release tag
following [Versioning](#versioning).

## Versioning

Releases are tagged `zcode-<ZCODE>-codex-<CODEX>` combining both sides,
for example `zcode-3.11.2-codex-0.153.4`:

- `<ZCODE>` is the ZCode Desktop stable version the release was tested
  against (`scripts/download-zcode-release.sh` prints it; the bundle name
  embeds it too).
- `<CODEX>` is the latest upstream Codex release (`rust-vX.Y.Z` tag)
  contained in the merge.

The `VERSION` file in each GitHub Release holds `<ZCODE>-codex-<CODEX>`
(the tag minus the leading `zcode-`). The workflow
(`.github/workflows/zcode-release.yml`) triggers on `zcode-*-codex-*`.

`Cargo.toml` workspace version intentionally stays `0.0.0`: that marks
every build as a source build, which keeps the upstream self-update
checks permanently disabled. Do not stamp the combined version into
Cargo — a parseable version would re-enable update prompts pointing at
OpenAI releases.
