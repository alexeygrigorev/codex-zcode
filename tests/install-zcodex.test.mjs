import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const installer = fileURLToPath(new URL("../scripts/install-zcodex.sh", import.meta.url));
function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "zcodex-install-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const binary = join(root, "candidate");
  const source = '#!/bin/sh\n[ "$1" = --version ] || exit 9\necho "zcodex test-local"\n';
  writeFileSync(binary, source, { mode: 0o755 });
  const installDir = join(root, "runtime");
  const run = (...args) => spawnSync("bash", [installer, ...args], {
    env: { ...process.env, HOME: join(root, "home"), TMPDIR: root }, encoding: "utf8",
  });
  const install = () => run("--binary", binary, "--install-dir", installDir);
  return { root, binary, source, installDir, run, install };
}

test("local install validates and atomically replaces only the runtime executable", (t) => {
  const f = fixture(t);
  assert.equal(f.install().status, 0);
  const installed = join(f.installDir, "zcodex");
  assert.equal(readFileSync(installed, "utf8"), f.source);
  assert.ok(lstatSync(installed).mode & 0o111);
  assert.equal(spawnSync(installed, ["--version"], { encoding: "utf8" }).stdout, "zcodex test-local\n");
  assert.equal(existsSync(join(f.root, "home")), false);
  assert.deepEqual(readdirSync(f.installDir), ["zcodex"]);
  writeFileSync(f.binary, f.source.replace("test-local", "test-updated"));
  assert.equal(f.install().status, 0);
  assert.match(readFileSync(installed, "utf8"), /test-updated/);
});

test("failed validation preserves the installed binary and cleans staging", (t) => {
  const f = fixture(t);
  assert.equal(f.install().status, 0);
  writeFileSync(f.binary, "#!/bin/sh\nexit 17\n");
  assert.equal(f.install().status, 17);
  assert.equal(readFileSync(join(f.installDir, "zcodex"), "utf8"), f.source);
  assert.deepEqual(readdirSync(f.installDir), ["zcodex"]);
  chmodSync(f.binary, 0o644);
  assert.equal(f.install().status, 1);
  assert.equal(readFileSync(join(f.installDir, "zcodex"), "utf8"), f.source);
});

test("local mode refuses symlink destinations and incomplete options", (t) => {
  const f = fixture(t);
  assert.equal(f.run("--binary", f.binary).status, 1);
  assert.equal(f.run("--install-dir", f.installDir).status, 1);
  assert.equal(f.run("--binary").status, 1);
  assert.equal(f.run("--unknown").status, 1);
  assert.equal(existsSync(f.installDir), false);
  assert.equal(f.install().status, 0);
  rmSync(join(f.installDir, "zcodex"));
  symlinkSync(f.binary, join(f.installDir, "zcodex"));
  assert.equal(f.install().status, 1);
  assert.ok(lstatSync(join(f.installDir, "zcodex")).isSymbolicLink());
  assert.equal(readFileSync(f.binary, "utf8"), f.source);
  rmSync(join(f.installDir, "zcodex"));
  mkdirSync(join(f.installDir, "zcodex"));
  assert.equal(f.install().status, 1);
  assert.ok(lstatSync(join(f.installDir, "zcodex")).isDirectory());
});
