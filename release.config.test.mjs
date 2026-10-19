import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";
import { pathToFileURL } from "node:url";

import releaseConfig from "./release.config.js";

// Resolve the generator from semantic-release's dependency graph, including overrides.
const require = createRequire(import.meta.url);
const releaseRequire = createRequire(require.resolve("semantic-release"));
const generatorUrl = pathToFileURL(
  releaseRequire.resolve("@semantic-release/release-notes-generator"),
);
const { generateNotes } = await import(generatorUrl.href);

test("release notes render the configured preset through semantic-release", async () => {
  const notesPlugin = releaseConfig.plugins.find(
    (plugin) =>
      Array.isArray(plugin) &&
      plugin[0] === "@semantic-release/release-notes-generator",
  );
  assert.ok(notesPlugin, "release configuration must select a notes generator");

  const repository = "https://github.com/cmeister2/whosthatpokemon";
  const featureHash = "a".repeat(40);
  const fixHash = "b".repeat(40);
  const breakingHash = "c".repeat(40);
  const notes = await generateNotes(notesPlugin[1], {
    cwd: process.cwd(),
    options: { repositoryUrl: releaseConfig.repositoryUrl },
    logger: { log() {} },
    lastRelease: { version: "1.0.0", gitTag: "1.0.0" },
    nextRelease: { version: "2.0.0", gitTag: "2.0.0" },
    commits: [
      { hash: featureHash, message: "feat: add silhouette rounds" },
      {
        hash: fixHash,
        message: "fix: accept case-insensitive guesses\n\nCloses #42",
      },
      {
        hash: breakingHash,
        message:
          "feat: change the game interface\n\nBREAKING CHANGE: guesses now use named rounds.",
      },
    ],
  });

  assert.match(notes, /### Features/);
  assert.match(notes, /### Bug Fixes/);
  assert.match(notes, /### [^\n]*BREAKING CHANGES/);
  assert.match(notes, /guesses now use named rounds\./);
  assert.match(notes, /add silhouette rounds/);
  assert.match(notes, /accept case-insensitive guesses/);
  assert.match(notes, /change the game interface/);
  assert.ok(notes.includes(`${repository}/compare/1.0.0...2.0.0`));
  assert.ok(notes.includes(`${repository}/issues/42`));
  for (const hash of [featureHash, fixHash, breakingHash]) {
    assert.ok(notes.includes(`${repository}/commit/${hash}`));
  }
});
