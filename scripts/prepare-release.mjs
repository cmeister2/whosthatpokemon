import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Inject the build version into the crate's manifest and lockfile. */
export async function prepareRelease(version, directory = process.cwd()) {
  if (
    typeof version !== "string" ||
    version.trim() !== version ||
    !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version ?? "") ||
    version === "0.0.0"
  ) {
    throw new Error("Supply a plain SemVer version other than 0.0.0.");
  }

  const manifestPath = resolve(directory, "Cargo.toml");
  const lockPath = resolve(directory, "Cargo.lock");
  const [manifest, lockfile] = await Promise.all([
    readFile(manifestPath, "utf8"),
    readFile(lockPath, "utf8"),
  ]);
  const manifestPattern = /(\[package\][\s\S]*?^version\s*=\s*")([^"]+)(")/m;
  const lockPattern = /^(\[\[package\]\]\r?\nname = "whosthatpokemon"\r?\nversion = ")([^"]+)(")/m;
  const manifestVersion = manifest.match(manifestPattern)?.[2];
  const lockVersion = lockfile.match(lockPattern)?.[2];

  // Validate both files before changing either; dependency versions stay untouched.
  if (manifestVersion !== lockVersion || !manifestVersion) {
    throw new Error("The crate version must exist and match in both Cargo files.");
  }
  if (manifestVersion === version) return;
  if (manifestVersion !== "0.0.0") {
    throw new Error("Expected the 0.0.0 sentinel in both Cargo files.");
  }

  await writeFile(manifestPath, manifest.replace(manifestPattern, `$1${version}$3`));
  await writeFile(lockPath, lockfile.replace(lockPattern, `$1${version}$3`));
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  try {
    if (process.argv.length !== 3) {
      throw new Error("Usage: node scripts/prepare-release.mjs <version>");
    }
    await prepareRelease(process.argv[2]);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
