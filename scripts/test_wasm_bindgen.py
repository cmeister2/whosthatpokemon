#!/usr/bin/env python3
"""Check that browser build tooling follows the resolved wasm-bindgen version."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = ("build-camera.sh", "build-browser.sh")
LOCKED_VERSION = "0.2.999"


class BindgenContracts(unittest.TestCase):
    """Exercise the actual build scripts at their cargo and bindgen boundaries."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wasm-bindgen-contract-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.repository = self.directory / "repository"
        scripts = self.repository / "scripts"
        scripts.mkdir(parents=True)
        for name in (*SCRIPTS, "wasm-bindgen-version.py"):
            shutil.copyfile(ROOT / "scripts" / name, scripts / name)
        self.lockfile = self.repository / "Cargo.lock"
        self.lockfile.write_text(
            f'[[package]]\nname = "wasm-bindgen"\nversion = "{LOCKED_VERSION}"\n'
            '[[package]]\nname = "wasm-bindgen-macro"\nversion = "0.2.998"\n',
            encoding="utf-8",
        )
        tools = self.directory / "tools"
        tools.mkdir()
        cargo = tools / "cargo"
        cargo.write_text(
            '#!/usr/bin/env bash\nset -euo pipefail\n'
            'touch "$WTP_TEST_CARGO_CALLED"\n'
            'mkdir -p "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release"\n'
            'printf wasm > "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/whosthatpokemon.wasm"\n',
            encoding="utf-8",
        )
        bindgen = tools / "bindgen"
        bindgen.write_text(
            '#!/usr/bin/env bash\nset -euo pipefail\n'
            'if [[ "${1:-}" == --version ]]; then\n'
            '  printf "wasm-bindgen %s\\n" "$WTP_TEST_BINDGEN_VERSION"\n'
            '  exit 0\n'
            'fi\n'
            'while [[ "$#" -gt 0 ]]; do\n'
            '  if [[ "$1" == --out-dir ]]; then output="$2"; shift 2; else shift; fi\n'
            'done\n'
            'printf bindings > "$output/whosthatpokemon.js"\n',
            encoding="utf-8",
        )
        cargo.chmod(0o755)
        bindgen.chmod(0o755)
        self.marker = self.directory / "cargo-called"
        self.environment = {
            **os.environ,
            "PATH": str(tools) + os.pathsep + os.environ["PATH"],
            "WASM_BINDGEN": str(bindgen),
            "CARGO_TARGET_DIR": str(self.directory / "target"),
            "WTP_TEST_CARGO_CALLED": str(self.marker),
            "WTP_TEST_BINDGEN_VERSION": LOCKED_VERSION,
        }

    def resolve(self):
        return subprocess.run(
            ["python3", str(self.repository / "scripts/wasm-bindgen-version.py")],
            capture_output=True, text=True, check=False,
        )

    def build(self, script):
        command = ["bash", str(self.repository / "scripts" / script)]
        if script == "build-camera.sh":
            command.append(str(self.directory / "camera"))
        return subprocess.run(
            command, cwd=self.directory, env=self.environment,
            capture_output=True, text=True, check=False,
        )

    def test_resolves_the_locked_crate_version(self):
        result = self.resolve()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), LOCKED_VERSION)

    def test_missing_crate_fails_closed(self):
        self.lockfile.write_text('version = 4\n', encoding="utf-8")
        result = self.resolve()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one", result.stderr)

    def test_ambiguous_versions_fail_closed(self):
        with self.lockfile.open("a", encoding="utf-8") as lock:
            lock.write('[[package]]\nname = "wasm-bindgen"\nversion = "0.2.998"\n')
        result = self.resolve()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one", result.stderr)

    def test_build_scripts_accept_the_locked_cli(self):
        for script in SCRIPTS:
            with self.subTest(script=script):
                result = self.build(script)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(self.marker.exists())
                output = (
                    self.directory / "camera" if script == "build-camera.sh"
                    else self.repository / "docs/engine"
                )
                self.assertEqual((output / "whosthatpokemon.js").read_text(), "bindings")

    def test_build_scripts_reject_mismatch_before_cargo(self):
        self.environment["WTP_TEST_BINDGEN_VERSION"] = "0.2.998"
        for script in SCRIPTS:
            with self.subTest(script=script):
                result = self.build(script)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(LOCKED_VERSION, result.stderr)
                self.assertIn("0.2.998", result.stderr)
                self.assertFalse(self.marker.exists())


if __name__ == "__main__":
    unittest.main()
