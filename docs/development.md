# Development

Run these commands from the repository root.

## Checks

The crate uses Rust edition 2024 and requires Rust 1.94.0 or newer. Run the CI
checks on stable and the declared MSRV:

```sh
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings

cargo +1.94.0 check --locked --all-targets --all-features
cargo +1.94.0 test --locked --all-targets --all-features
cargo +1.94.0 test --locked --doc --all-features
cargo +1.94.0 fmt --all -- --check
cargo +1.94.0 clippy --locked --all-targets --all-features -- -D warnings
```

These compile and test the matching backends and OCR adapter without downloading
models or loading an external inference runtime. Public and private Rust items require documentation, including
actual failure/panic conditions where applicable. CI separately runs
`cargo msrv verify -- cargo check --locked --all-targets --all-features` and
`cargo llvm-cov --all-features --workspace --locked`. Install those tools using
the stable toolchain; coverage requires `llvm-tools-preview`.

The `Success` CI job aggregates the required checks and gates releases.

Release tooling requires Node.js 24.15.0+ (or Node 22.22.2+) and is private npm
tooling for this Rust crate:

```sh
npm ci
npm test
```

The regression exercises version preparation and notes rendering through
semantic-release's actual dependency graph. Preserve the pinned Conventional
Commits preset and release-notes-generator override together with the lockfile.
Renovate extends `github>cmeister2/renovate`.

## Pipeline gallery and browser camera

Pages first records the actual native OCRacle pipeline, then packages the real
OCRacle WASM camera engine and full English paper metadata snapshot. Models,
generated indexes, reports and WASM camera artifacts stay outside the source tree:

```sh
python3 scripts/fetch-page-resources.py --cache /tmp/page-resources
cargo build --locked --release --features ocr --bin whosthatpokemon
python3 scripts/build-native-gallery.py --binary target/release/whosthatpokemon --cache /tmp/page-resources --output /tmp/pages
python3 scripts/check-native-gallery.py /tmp/pages
scripts/build-camera.sh /tmp/camera-engine
python3 scripts/build-full-camera-catalogue.py --binary target/release/whosthatpokemon --cache /tmp/camera-catalogue-cache --output /tmp/catalogue.json.gz
python3 scripts/build-camera-page.py --metadata /tmp/catalogue.json.gz --cache /tmp/page-resources --engine /tmp/camera-engine --output /tmp/pages
cargo run --locked --release --features web,ocr --example camera_parity -- /tmp/pages /tmp/camera-parity
node scripts/check-camera.mjs /tmp/pages /tmp/camera-parity
python3 -m http.server 8000 --directory /tmp/pages
```

The camera build requires `wasm32-unknown-unknown` and `wasm-bindgen-cli 0.2.129`.
It enables portable WASM SIMD and runs on one worker without shared-memory headers.
The parity check executes the actual deployed OCR weights and compares native
and WASM results on identical public pixels, including Secret Lair and inverted
inputs. Its temporary RGBA/reference bytes and reports are outside the deployment.

Camera frames stay local. The browser acquires only candidate-selected canonical
reference images from Scryfall; metadata snapshot coverage and source date are
visible on the page. Cold acquisition/preparation and warm query costs are shown
separately. [Card art](assets/README.md) and the [camera model](camera-model-notice.txt)
have separate attribution and licences.

The older checked-in `docs/engine` bindings remain available for existing browser
integrations. Rebuild those using `scripts/build-browser.sh` and verify with
`node scripts/check-browser.mjs`; they do not supply the camera page's OCR engine.

## Releases

The extractor uses the published `magicextract` registry dependency. Before a
release tag is created, preparation injects the release version into both Cargo
files and runs `cargo publish --dry-run --locked --allow-dirty` to verify package
contents, registry dependencies and compilation. Actual registry upload follows
tag creation; credentials or service availability can still affect that step.

The configured repository is `https://github.com/cmeister2/whosthatpokemon`.
Before publishing, add a **`CARGO_REGISTRY_TOKEN`** repository Actions secret
authorized to publish this crate. The release workflow uses `GITHUB_TOKEN` for
tags and GitHub releases. After all checks pass, only pushes to `main` or `master`
run semantic-release and publish to crates.io when that secret is configured.
Without credentials CI still checks the code and builds a fallback version.
Complete history/tags are fetched; tags are plain SemVer such as `1.2.3`.

Preserve meaningful Conventional Commits when merging: `fix:`/`perf:` request
patch releases, `feat:` requests minor, and a `BREAKING CHANGE:` footer requests
major. The analyzer uses its default preset; use the footer for breaking changes.
Ordinary `docs:`, `test:`, `ci:`, and `chore:` commits alone do not release.

Keep the checked-in version at **`0.0.0`** in both `Cargo.toml` and this crate's
`Cargo.lock` entry. Preparation injects a release version into the build checkout:

```sh
node scripts/prepare-release.mjs 1.2.3
```

When no release is selected, CI builds with `0.0.<github.run_id>` without
publishing that fallback. Do not commit injected versions.

[Documentation](README.md)
