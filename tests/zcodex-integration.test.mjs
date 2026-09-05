import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFile, readdir } from "node:fs/promises";
import { accessSync, constants } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import test from "node:test";

const repoRoot = path.resolve(import.meta.dirname, "..");
const zcodexPath = process.env.ZCODEX_PATH || path.join(os.homedir(), ".local/bin/zcodex");
const zcodexConfigPath = process.env.ZCODEX_CONFIG_PATH || path.join(os.homedir(), ".zcodex/config.toml");
const zcodeRuntimePath =
  process.env.ZCODE_CJS || "/opt/ZCode/resources/glm/zcode.cjs";
const expectedProvider = "zcode";
const expectedWireApi = "zcode";

function assertExecutable(filePath) {
  accessSync(filePath, constants.F_OK | constants.X_OK);
}

function parseJsonLines(stdout) {
  return stdout.split(/\r?\n/).flatMap((line) => {
    if (!line.trim()) {
      return [];
    }

    try {
      return [JSON.parse(line)];
    } catch {
      // Nested processes may emit plain text alongside the JSON stream.
      return [];
    }
  });
}

function runProcess(command, args, { timeoutMs, cwd = repoRoot, env = process.env } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";
    let settled = false;

    const settle = (callback, value) => {
      if (settled) {
        return;
      }

      settled = true;
      clearTimeout(timeoutId);
      callback(value);
    };

    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });

    const timeoutId = setTimeout(() => {
      child.kill("SIGKILL");
      settle(
        reject,
        new Error(
          `Command timed out after ${timeoutMs}ms: ${command} ${args.join(" ")}\n` +
            `stdout:\n${stdout}\nstderr:\n${stderr}`,
        ),
      );
    }, timeoutMs);

    child.on("error", (error) => {
      settle(reject, error);
    });
    child.on("close", (code, signal) => {
      settle(resolve, { code, signal, stdout, stderr });
    });
  });
}

function environmentWithoutCodexHome() {
  const env = { ...process.env };
  delete env.CODEX_HOME;
  return env;
}

function isolatedZcodeHomeCandidates() {
  // zcodex reads the UID env var and falls back to whoami::username; mirror
  // both spellings plus the numeric uid so the test works across environments.
  const uid = process.env.UID || String(process.getuid());
  const username = process.env.USER || process.env.LOGNAME || os.userInfo().username;
  const base = "/tmp";
  return [
    path.join(base, `zcodex-zcode-home-${uid}`),
    path.join(base, `zcodex-zcode-home-${username}`),
  ];
}

async function findIsolatedZcodeHome() {
  const candidates = isolatedZcodeHomeCandidates();
  for (const candidate of candidates) {
    try {
      const configPath = path.join(candidate, ".zcode/cli/config.json");
      accessSync(configPath, constants.F_OK);
      return candidate;
    } catch {
      // try the next candidate
    }
  }
  throw new Error(`No prepared zcodex ZCode HOME found; tried:
${candidates.join("\n")}`);
}

async function latestRolloutItems(cwd) {
  const sessionsRoot = path.join(os.homedir(), ".zcodex/sessions");
  const candidates = [];

  async function walk(current) {
    const entries = await readdir(current, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      const child = path.join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(child);
      } else if (entry.isFile() && entry.name.endsWith(".jsonl")) {
        const stats = await import("node:fs/promises").then((fs) => fs.stat(child));
        candidates.push({ child, mtimeMs: stats.mtimeMs });
      }
    }
  }

  await walk(sessionsRoot);
  candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);

  for (const candidate of candidates) {
    const content = await readFile(candidate.child, "utf8");
    const firstLine = content.split(/\r?\n/, 1)[0];
    let meta;
    try {
      meta = JSON.parse(firstLine);
    } catch {
      continue;
    }
    if (meta?.type === "session_meta" && meta.payload?.cwd === cwd) {
      return content
        .split(/\r?\n/)
        .flatMap((line) => {
          if (!line.trim()) {
            return [];
          }
          try {
            const parsed = JSON.parse(line);
            return parsed.type === "response_item" ? [parsed.payload] : [];
          } catch {
            return [];
          }
        });
    }
  }

  return [];
}

async function runDirectZCode(prompt, timeoutMs) {
  return runProcess(
    process.execPath,
    [
      zcodeRuntimePath,
      "--prompt",
      prompt,
      "--output-format",
      "stream-json",
      "--mode",
      "yolo",
      "--cwd",
      repoRoot,
    ],
    { timeoutMs },
  );
}

function containsDeep(value, needle) {
  if (typeof value === "string") {
    return value.includes(needle);
  }

  if (Array.isArray(value)) {
    return value.some((item) => containsDeep(item, needle));
  }

  if (value && typeof value === "object") {
    return Object.values(value).some((item) => containsDeep(item, needle));
  }

  return false;
}

function reportsModel(event, modelId) {
  if (!event || typeof event !== "object") {
    return false;
  }

  const payload = event.payload ?? {};
  return (
    payload.modelId === modelId ||
    payload.modelRef?.modelId === modelId ||
    payload.model?.modelId === modelId ||
    payload.model === modelId
  );
}

function assertExitZero(description, result) {
  assert.equal(
    result.code,
    0,
    `${description} exited ${result.code ?? "unknown"}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
  );
}

test("binary and environment", async () => {
  assertExecutable(zcodexPath);

  const result = await runProcess(zcodexPath, ["--version"], { timeoutMs: 15_000 });
  assertExitZero("zcodex --version", result);
  assert.match(result.stdout, /codex-cli/i);
});

test("config sanity", async () => {
  accessSync(zcodexConfigPath, constants.F_OK);
  const config = await readFile(zcodexConfigPath, "utf8");
  const providerPattern = new RegExp(`^\\s*model_provider\\s*=\\s*"${expectedProvider}"\\s*$`, "m");
  const wireApiPattern = new RegExp(`^\\s*wire_api\\s*=\\s*"${expectedWireApi}"\\s*$`, "m");
  assert.match(config, providerPattern);
  assert.match(config, wireApiPattern);
});

test("ZCode runtime present", () => {
  assertExecutable(zcodeRuntimePath);
});

// Expected runtime: usually a few seconds; capped at 120 seconds because the
// shared coding-plan rate limit can trigger 429 retry backoff.
test("direct ZCode stream sanity", async () => {
  const result = await runDirectZCode("Reply with exactly: PING", 120_000);
  assertExitZero("direct ZCode stream", result);

  const events = parseJsonLines(result.stdout);
  assert.ok(
    events.some((event) => event.type === "model.streaming"),
    `No model.streaming event in stdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
  );

  const finalEvent = events.at(-1);
  assert.equal(finalEvent?.type, "result");
  assert.ok(
    containsDeep(finalEvent.response, "PING"),
    `Final result response did not contain PING: ${JSON.stringify(finalEvent)}`,
  );
});

// Expected runtime: 10-180 seconds depending on model latency and 429 retry backoff.
test("zcodex exec end-to-end", async () => {
  const result = await runProcess(
    zcodexPath,
    [
      "exec",
      "--dangerously-bypass-approvals-and-sandbox",
      "--skip-git-repo-check",
      "Reply with exactly: ZCODEX_OK",
    ],
    { timeoutMs: 180_000, env: environmentWithoutCodexHome() },
  );

  assertExitZero("zcodex exec", result);
  assert.ok(
    result.stdout.includes("ZCODEX_OK"),
    `stdout did not contain ZCODEX_OK:\n${result.stdout}\nstderr:\n${result.stderr}`,
  );
});

// Expected runtime: 30-300 seconds because the model must issue and process a
// shell tool call; shared rate limits can add 429 retry backoff to both turns.
test("tool loop", async () => {
  const isolatedCwd = await import("node:fs/promises").then(async (fs) => {
    const dir = path.join(os.tmpdir(), `zcodex-tool-loop-${process.pid}-${Date.now()}`);
    await fs.mkdir(dir, { recursive: true });
    return dir;
  });

  try {
    const result = await runProcess(
      zcodexPath,
      [
        "exec",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "Use the shell to run exactly: echo ZCODE_TOOL_OK. Then report the exact output.",
      ],
      { timeoutMs: 300_000, cwd: isolatedCwd, env: environmentWithoutCodexHome() },
    );

    assertExitZero("zcodex shell tool loop", result);
    assert.ok(
      result.stdout.includes("ZCODE_TOOL_OK"),
      `stdout did not contain ZCODE_TOOL_OK:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );

    const items = await latestRolloutItems(isolatedCwd);
    const calls = items.filter((item) => item.type === "function_call");
    const outputs = items.filter((item) => item.type === "function_call_output");
    assert.ok(calls.length >= 1, "transcript did not contain a function_call item");
    assert.equal(
      calls.length,
      outputs.length,
      `function_call count (${calls.length}) did not match function_call_output count (${outputs.length})`,
    );
    const markerCalls = calls.filter((item) => String(item.arguments).includes("ZCODE_TOOL_OK"));
    assert.ok(
      markerCalls.length >= 1,
      "no function_call arguments contained the marker command",
    );
    assert.ok(
      markerCalls.length <= 3,
      `tool call repeated too many times (${markerCalls.length}); possible continuation loop`,
    );
  } finally {
    await import("node:fs/promises").then((fs) => fs.rm(isolatedCwd, { recursive: true, force: true }));
  }
});
// Expected runtime: usually a few seconds; capped at 120 seconds for 429 retry backoff.
test("model control", async () => {
  const config = await readFile(zcodexConfigPath, "utf8");
  const model = config.match(/^\s*model\s*=\s*"([^"]+)"\s*$/m)?.[1];
  assert.ok(model, `Could not find the model slug in:\n${config}`);

  const preparedHome = await findIsolatedZcodeHome();
  const preparedConfig = path.join(preparedHome, ".zcode/cli/config.json");
  accessSync(preparedConfig, constants.F_OK);
  const prepared = JSON.parse(await readFile(preparedConfig, "utf8"));
  assert.equal(
    prepared.model,
    `zai/${model}`,
    `prepared ZCode config model is not zai/${model}: ${JSON.stringify(prepared.model)}`,
  );
  assert.ok(
    Boolean(prepared.provider?.zai?.options?.apiKey),
    "prepared ZCode config lost the provider apiKey",
  );

  const result = await runProcess(
    process.execPath,
    [
      zcodeRuntimePath,
      "--prompt",
      "hi",
      "--output-format",
      "stream-json",
      "--mode",
      "yolo",
      "--cwd",
      repoRoot,
    ],
    {
      timeoutMs: 120_000,
      env: { ...process.env, HOME: await findIsolatedZcodeHome() },
    },
  );
  assertExitZero("direct ZCode model-control stream", result);

  const events = parseJsonLines(result.stdout);
  assert.ok(
    events.some((event) => reportsModel(event, model)),
    `No streaming or session event reported modelId ${model}.\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
  );
});

// Expected runtime: 60-480 seconds. Spawns a real Codex child thread through
// ZCode's Agent tool passthrough and asserts the child session recorded the
// task's marker answer. Each model turn is a full ZCode one-shot, and shared
// coding-plan rate limits (429 backoff) plus subagent wait cycles can stretch
// the round trip well past five minutes.
test("native subagent spawn", async () => {
  const isolatedCwd = await import("node:fs/promises").then(async (fs) => {
    const dir = path.join(os.tmpdir(), `zcodex-subagent-${process.pid}-${Date.now()}`);
    await fs.mkdir(dir, { recursive: true });
    return dir;
  });
  const marker = `CHILD_OK_${process.pid}`;
  const markerSlug = marker.toLowerCase().replace(/_/g, "");

  try {
    const result = await runProcess(
      zcodexPath,
      [
        "exec",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        `Spawn a subagent with task_name probe_child_${process.pid} and message: Reply with exactly: ${marker}. Wait for its final answer and tell me.`,
      ],
      { timeoutMs: 480_000, cwd: isolatedCwd, env: environmentWithoutCodexHome() },
    );

    assertExitZero("zcodex native subagent spawn", result);
    assert.ok(
      result.stdout.includes(marker),
      `stdout did not contain ${marker}:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );

    const sessionsRoot = path.join(os.homedir(), ".zcodex/sessions");
    const childFiles = [];
    async function walk(current) {
      const entries = await readdir(current, { withFileTypes: true }).catch(() => []);
      for (const entry of entries) {
        const child = path.join(current, entry.name);
        if (entry.isDirectory()) {
          await walk(child);
        } else if (entry.isFile() && entry.name.endsWith(".jsonl")) {
          childFiles.push(child);
        }
      }
    }
    await walk(sessionsRoot);

    const fs = await import("node:fs/promises");
    let foundChild = false;
    for (const file of childFiles) {
      const stats = await fs.stat(file);
      if (Date.now() - stats.mtimeMs > 15 * 60 * 1000) {
        continue;
      }
      const content = await readFile(file, "utf8");
      const firstLine = content.split(/\r?\n/, 1)[0];
      let meta;
      try {
        meta = JSON.parse(firstLine);
      } catch {
        continue;
      }
      const payload = meta?.payload ?? {};
      const cwd = payload.cwd ?? "";
      const source = payload.thread_source ?? payload.source ?? "";
      if (cwd !== isolatedCwd || source !== "subagent") {
        continue;
      }
      if (content.includes(marker)) {
        foundChild = true;
        break;
      }
    }
    assert.ok(
      foundChild,
      `no child subagent session containing ${marker} was found under ${sessionsRoot}`,
    );
  } finally {
    await import("node:fs/promises").then((fs) => fs.rm(isolatedCwd, { recursive: true, force: true }));
  }
});
