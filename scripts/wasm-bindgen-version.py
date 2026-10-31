#!/usr/bin/env python3
"""Print the wasm-bindgen crate version resolved in Cargo.lock."""

import argparse
from pathlib import Path
import tomllib


def locked_version(lockfile):
    """Require one unambiguous wasm-bindgen version in the supplied lockfile."""
    with lockfile.open("rb") as source:
        lock = tomllib.load(source)
    versions = {
        package["version"]
        for package in lock.get("package", [])
        if package["name"] == "wasm-bindgen"
    }
    if len(versions) != 1:
        raise ValueError("expected exactly one wasm-bindgen version in Cargo.lock")
    return versions.pop()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "lockfile", nargs="?", type=Path,
        default=Path(__file__).resolve().parent.parent / "Cargo.lock",
    )
    arguments = parser.parse_args()
    try:
        print(locked_version(arguments.lockfile))
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"Cannot determine wasm-bindgen CLI version: {error}\n")
