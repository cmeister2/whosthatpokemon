/** Release the Rust crate with plain SemVer tags and generated GitHub notes. */
export default {
  branches: ["main", "master"],
  repositoryUrl: "https://github.com/cmeister2/whosthatpokemon.git",
  tagFormat: "${version}",
  plugins: [
    "@semantic-release/commit-analyzer",
    ["@semantic-release/release-notes-generator", { preset: "conventionalcommits" }],
    [
      "@semantic-release/exec",
      {
        verifyConditionsCmd:
          'node -e "if (!process.env.CARGO_REGISTRY_TOKEN?.trim()) { console.error(\'Set the CARGO_REGISTRY_TOKEN repository secret before releasing.\'); process.exit(1); }"',
        prepareCmd: "node scripts/prepare-release.mjs ${nextRelease.version}",
        publishCmd: "cargo publish --locked --allow-dirty",
      },
    ],
    [
      "@semantic-release/github",
      {
        successComment: false,
        failComment: false,
        failTitle: false,
        releasedLabels: false,
      },
    ],
  ],
};
