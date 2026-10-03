# FIX-WORKLOG — zc-dupexec-fix

Session: zc-dupexec-fix (ZCode zcodex executor for claude-principal)

## `aplexer whoami --json` (recorded 2026-10-03)

```json
{
  "schema_version": 1,
  "id": "302e9424-c499-4119-916d-aa330614e451",
  "workspace": "/home/alexey/git/codex-zcode-wt-dupexec",
  "tag": "zc-dupexec-fix",
  "engine": "shell",
  "parent_session": "b3a92dd0-a17e-4a62-940f-eb3b829393f6",
  "command": [
    "bash",
    "-lc",
    "cd /home/alexey/git/codex-zcode-wt-dupexec && timeout 120m zcodex exec --skip-git-repo-check \"$(cat /home/alexey/git/cloudflare-agent-git/.local/claude/ZC-DUPEXEC.md)\" > /home/alexey/git/cloudflare-agent-git/.local/claude/zc-dupexec.log 2>&1; echo EXIT=$? >> /home/alexey/git/cloudflare-agent-git/.local/claude/zc-dupexec.log"
  ],
  "cwd": "/home/alexey/git/cloudflare-agent-git",
  "limits": { "memory_bytes": 6291456000 },
  "created_at_ms": 1791033350305,
  "worker_pid": 2140054,
  "workload_pid": 2140067
}
```

(Truncated to the identity-relevant fields; full output available from `aplexer whoami --json`.)

## Task

USER DIRECTION (verbatim): "if double execution still exists solve it via zcodex. zcode is open source btw"

- STEP 1: verify installed binary (`~/.local/lib/zcodex/zcodex`) for fix commit bf9d7ed22a; reproduce duplicates on installed binary (5 runs × 3 prompt shapes, cold vs warm).
- STEP 2: fix on branch `diag/zcode-duplicate-exec` (inner mode must not execute tools; outer Codex runtime is sole executor; outer idempotent on retried identical call_id). Test that fails on old behaviour. Build only through build guard; candidate to `.local/candidate/` + sha256. Do NOT touch `~/.local/lib/zcodex`.
- STEP 3: rerun STEP 1 repro against candidate; report before/after counts; commit; aplexer message to claude-principal.

## Progress log

- 2026-10-03: session started; whoami recorded; work scope declared (edit: `.local`, `codex-rs/core`); no unread peer mail; read `.local/DIAGNOSIS.md`.

## STEP 1 — installed binary verification

- Installed binary: `/home/alexey/.local/lib/zcodex/zcodex`, ELF, built **2026-09-26 21:41** (stripped).
- Fix commit `bf9d7ed22a` is dated **2026-10-03 04:45** → the installed binary predates the fix.
- Literal cluster check: the installed binary's cold-spawn arg literal run reads
  `stream-json--mode--cwd--output-format` (no `build` value); the worktree's `target/debug/zcodex`
  (built 2026-10-03 06:18) reads `stream-json--modebuild--cwd--output-format` → contains the fix.
  The three `yolo` hits in the installed binary are TUI/approval-policy display strings, not the spawn value.

## STEP 1 — repro on the INSTALLED binary (scratch /tmp git dirs, `zcodex exec -s danger-full-access`)

Note: without `-s danger-full-access` the OUTER sandbox fails on this host
(bubblewrap: "app-server socket directory has an unsupported host mount at /data") and only the
inner yolo copy runs — the first smoke run showed exactly that (count=1, inner-only).

Raw counts (lines per nonce, 5 runs each):

| shape | path | run1 | run2 | run3 | run4 | run5 | expected |
|-------|------|------|------|------|------|------|----------|
| a: one command | cold | 2 | 2 | 2 | 2 | 2 | 1 |
| b: two sequential | cold | 2+2 | 2+2 | 2+2 | 2+2 | 2+2 | 1+1 |
| c: fail+retry | cold | 2+2 | 2+2 | 2+2 | 2+2 | 2+2 | 1+1 |
| a: one command | warm | 1 | 1 | 1 | 1 | 1 | 1 |
| b: two sequential | warm | 1+1 | 1+1 | 1+1 | 1+1 | 1+1 | 1+1 |
| c: fail+retry | warm | 1+1 | 1+1 | 1+1 | 1+1 | 1+1 | 1+1 |

**Verdict: installed binary duplicates 100% of cold-wire tool executions; warm path (`ZCODE_WARM=1`) is clean.**
Raw logs: `/tmp/zc-dupexec-repro/{cold,warm}-{a,b,c}.log`, per-run `.out` files alongside.

## STEP 2 — fix work (branch diag/zcode-duplicate-exec)

- `bf9d7ed22a` already on HEAD: cold spawn `--mode build` + `zcode_inner_mode` suite test.
- Added (this session): outer-side idempotency in `stream_zcode` — mapped calls whose
  `call_id` (`zcode_tool_<toolCallId>`) already has a recorded output in the follow-up
  request input are dropped at all three emission sites (`tool_input_start`, `tool_call`,
  orphan `tool_call`); new `ZCODE_TOOL_CALL_ID_PREFIX` const single-sources the prefix.
- Tests: `zcode_cold_wire_executes_streamed_tool_call_once` (existing) and new
  `zcode_cold_wire_ignores_retried_duplicate_call_id` (stub re-emits the same
  `toolCallId` on the continuation; asserts probe still has exactly one line — fails
  pre-fix with two).
- Build constraint discovered: session cgroup caps memory at 6 GiB; linking debug
  test binaries OOM-kills rustc → builds run with `CARGO_PROFILE_DEV_DEBUG=0`
  and `--build-jobs 2`, scoped to the affected test target (`--test all`) under the guard.

### Test matrix (nextest, suite::zcode_inner_mode)

| client.rs source | single-call test | reemit-dup-call_id test |
|------------------|------------------|-------------------------|
| `bf9d7ed22a~1` (yolo, pre-fix) | FAIL | FAIL |
| `bf9d7ed22a` HEAD (build mode, no idempotency) | PASS | FAIL |
| branch (build mode + idempotency) | PASS | PASS |

All runs through `scripts/guard/build_guard.py` (GUARD_SUCCESS on green runs).

---

# FIX-WORKLOG — zc-dupexec-2 (continuation)

Session: zc-dupexec-2 (ZCode zcodex executor, warm path, for claude-principal).
Continues `zc-dupexec-fix`, which hit its `timeout 120m` (EXIT=124) mid-STEP-3.

## `a whoami --json` (recorded 2026-10-03 ~17:30)

```json
{
  "schema_version": 1,
  "id": "210d3775-18b9-48a6-8284-8635fac41582",
  "workspace": "/home/alexey/git/codex-zcode-wt-dupexec",
  "tag": "zc-dupexec-2",
  "engine": "shell",
  "parent_session": "b3a92dd0-a17e-4a62-940f-eb3b829393f6",
  "command": [
    "bash", "-lc",
    "cd /home/alexey/git/codex-zcode-wt-dupexec && ZCODE_WARM=1 timeout 60m zcodex exec --skip-git-repo-check \"$(cat /home/alexey/git/cloudflare-agent-git/.local/claude/ZC-DUP2.md)\" > /home/alexey/git/cloudflare-agent-git/.local/claude/zc-dup2.log 2>&1; echo \"RUN_EXIT=$?\" >> .../zc-dup2.log"
  ],
  "limits": { "memory_bytes": 1572864000 }
}
```

(Truncated to identity-relevant fields. Note: this session is capped at 1.5 GiB
and 60 min; ambient env has `ZCODE_WARM=1` — repro cold runs must unset it explicitly.)

## Handoff record — how zc-dupexec-fix ended

- Its log tail (`.../claude/zc-dupexec.log`): candidate built (GUARD_SUCCESS,
  711 MB, staged + sha-verified, `--mode build` confirmed in the spawn cluster),
  then it ran the STEP 3 cold matrix against the candidate at 16:52–17:07 and
  found **the candidate still duplicates**. Its last words: the inner build-mode
  deny IS firing, but the denial narration ("No permission client configured for
  Bash") makes the inner model believe the call failed and **re-issue it on the
  continuation under a NEW `toolCallId`** — a denial-retry mechanism the
  call_id idempotency fix cannot catch (it only drops same-`call_id` re-emits).
  EXIT=124 killed it before it wrote any of this down.
- Its raw candidate cold counts (transcribed verbatim from
  `/tmp/zc-dupexec-repro/cand-cold-{a,b,c}.log`, run with its
  `run_one_candidate.sh`; counts are per nonce variant, expected 1 each):
  - shape a (one command): 2, 2, 2, 2, 2
  - shape b (two sequential): -one 2/2/3/2/2, -two 1/1/2/1/0 (run5: 2nd command
    never ran; run3: 3+2)
  - shape c (fail+retry): -fail 2/4/6/2/1, -retry 1/1/0/1/1; run3 hit rc=124
    (300 s timeout) inside the retry loop with 6 executions of the fail command;
    run5 was clean (1+1)
  - Evidence of mechanism in transcripts: 5× "No permission client configured"
    narrations in `cand-run-c-cold-3.out` (the 6× run).
- Disk/OOM history (honest): this worktree's session cgroup capped memory at
  6 GiB; linking debug test binaries OOM-killed rustc, so builds ran with
  `CARGO_PROFILE_DEV_DEBUG=0` and `--build-jobs 2`, scoped to affected targets,
  always via `scripts/guard/build_guard.py`. The shared `codex-rs/target` grew
  **52 → 79 GB today** (one full debug rebuild with debug=0 plus the test
  builds; the earlier zcy TUI session's test build alone added ~16 GiB against
  a then-stated 512 MiB cap). Host free disk is 69 GB. The coordinator has
  frozen Rust builds: **zc-dupexec-2 runs NO cargo/rustc builds at all.**

## Guarded test matrix (as already observed by zc-dupexec-fix)

nextest `suite::zcode_inner_mode`, via build guard:

| client.rs source | single-call test | reemit-dup-call_id test |
|------------------|------------------|-------------------------|
| `bf9d7ed22a~1` (yolo, pre-fix) | FAIL | FAIL |
| `bf9d7ed22a` (build mode, no idempotency) | PASS | FAIL |
| branch (build mode + call_id idempotency) | PASS | PASS |

## STEP 0 — fix committed (this session)

- Candidate sha verified before any use:
  `f10180c2fcd8d7fe61b36682c88cf7fa72c3340cf7d898533b580a511ae97787`
  = `.local/candidate/zcodex-fixture.sha256`. Binary NOT installed anywhere.
- Commit `7684481e10` on `diag/zcode-duplicate-exec`: `codex-rs/core/src/client.rs`
  + `codex-rs/core/tests/suite/zcode_inner_mode.rs`, as-is (no fmt/churn).
  `codex-rs/target` and `.local/` (incl. the 711 MB candidate) stay untracked.

## STEP 3 rerun (this session, fresh scratch /tmp/zc-dup2-repro)

- Same protocol as STEP 1 (`run_one.sh` shapes a/b/c, per-run nonce, probe line
  counts); cold = `env -u ZCODE_WARM` (ambient has it set — earlier scripts
  relied on ambient env); per-run `timeout 300`, `-s danger-full-access`.
- Control, installed binary `/home/alexey/.local/lib/zcodex/zcodex` (pre-fix,
  2026-09-26), shape a cold, 1 run: **nonce count 2** (expected 1) — bug still
  reproduces today under this session's conditions.
- Candidate cold matrix (3 shapes × 5 runs): see counts appended below when the
  batch finishes.
- Workspace note: peer `zcy-dupexec` [5613f3f9] is the **interactive zcodex TUI
  left over from this morning's diagnosis session** (workload pid 3836235 — the
  same pid DIAGNOSIS.md's ancestry probe showed; idle since 04:49). Left
  untouched; not the source of these counts.



