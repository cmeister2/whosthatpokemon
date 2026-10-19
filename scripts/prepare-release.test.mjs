import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { prepareRelease } from "./prepare-release.mjs";

/** Create isolated Cargo files, including a dependency with the sentinel version. */
async function fixture(t, lockVersion = "0.0.0") {
  const directory = await mkdtemp(join(tmpdir(), "whosthatpokemon-release-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  await writeFile(join(directory, "Cargo.toml"), '[package]\nname = "whosthatpokemon"\nversion = "0.0.0"\n');
  await writeFile(join(directory, "Cargo.lock"), `version = 4\n\n[[package]]\nname = "dependency"\nversion = "0.0.0"\n\n[[package]]\nname = "whosthatpokemon"\nversion = "${lockVersion}"\n`);
  return directory;
}

test("release preparation synchronizes the crate without changing dependencies", async (t) => {
  const directory = await fixture(t);
  await prepareRelease("1.2.3", directory);
  assert.match(await readFile(join(directory, "Cargo.toml"), "utf8"), /version = "1\.2\.3"/);
  const lockfile = await readFile(join(directory, "Cargo.lock"), "utf8");
  assert.match(lockfile, /name = "whosthatpokemon"\nversion = "1\.2\.3"/);
  assert.match(lockfile, /name = "dependency"\nversion = "0\.0\.0"/);
});

test("fallback builds can use the CI run ID and repeat preparation", async (t) => {
  const directory = await fixture(t);
  await prepareRelease("0.0.123456789", directory);
  await prepareRelease("0.0.123456789", directory);
  assert.match(await readFile(join(directory, "Cargo.toml"), "utf8"), /version = "0\.0\.123456789"/);
});

test("mismatched Cargo files fail before either file is written", async (t) => {
  const directory = await fixture(t, "1.0.0");
  const manifest = await readFile(join(directory, "Cargo.toml"), "utf8");
  await assert.rejects(prepareRelease("1.2.3", directory), /must exist and match/);
  assert.equal(await readFile(join(directory, "Cargo.toml"), "utf8"), manifest);
  assert.match(await readFile(join(directory, "Cargo.lock"), "utf8"), /name = "whosthatpokemon"\nversion = "1\.0\.0"/);
});

test("invalid release versions leave both files untouched", async (t) => {
  const directory = await fixture(t);
  for (const version of [undefined, "0.0.0", "v1.2.3", "1.2", "01.2.3", "1.2.3\n", "1.2.3; echo nope"]) {
    await assert.rejects(prepareRelease(version, directory), /plain SemVer/);
  }
  assert.match(await readFile(join(directory, "Cargo.toml"), "utf8"), /version = "0\.0\.0"/);
  assert.match(await readFile(join(directory, "Cargo.lock"), "utf8"), /name = "whosthatpokemon"\nversion = "0\.0\.0"/);
});

test("preparation cannot overwrite an existing different release version", async (t) => {
  const directory = await fixture(t);
  await prepareRelease("1.2.3", directory);
  await assert.rejects(prepareRelease("2.0.0", directory), /0\.0\.0 sentinel/);
});
