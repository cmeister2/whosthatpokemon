#!/usr/bin/env python3
"""Build a static walkthrough by invoking the production native identification CLI.

Only public inputs allowlisted in docs/assets/samples.json are processed. Native
JSON and PNG traces are generated into a deployment directory, never checked in.
The browser only displays those results; it performs no OCR or matching.
"""

import argparse
import hashlib
import json
import platform
from pathlib import Path
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def read_json(path):
    """Read one UTF-8 JSON document."""
    return json.loads(path.read_text(encoding="utf-8"))


def checked_public_path(site, relative):
    """Resolve an allowlisted asset within the explicit public site tree."""
    path = (site / relative).resolve()
    if not path.is_relative_to(site.resolve()):
        raise ValueError(f"Public asset escapes the site: {relative}")
    return path


def run_json(command):
    """Run the actual native command and parse its unmodified JSON response."""
    return json.loads(subprocess.check_output([str(value) for value in command], text=True))


def cpu_description():
    """Record the generation machine so timings are not mistaken for viewer speed."""
    cpu_info = Path("/proc/cpuinfo")
    if cpu_info.exists():
        for line in cpu_info.read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    return platform.processor() or platform.machine()


def build(args):
    """Build a native shard index, execute identification, and package the gallery."""
    source = ROOT / "docs"
    manifest = read_json(source / "assets/samples.json")
    output = args.output.resolve()
    cache = args.cache.resolve()
    binary = args.binary.resolve()
    model_root = args.model_root.resolve() if args.model_root else cache / "ocracle"
    if output == source or output.is_relative_to(source):
        raise ValueError("Use a deployment directory outside docs; generated traces are not source")
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise ValueError("The deployment directory must be empty")
    cache.mkdir(parents=True, exist_ok=True)
    for filename in ("index.html", "styles.css", "app.js"):
        shutil.copyfile(source / filename, output / filename)
    (output / "assets").mkdir()
    for directory in ("inputs", "references"):
        shutil.copytree(source / "assets" / directory, output / "assets" / directory)
    shutil.copyfile(source / "assets/samples.json", output / "assets/samples.json")

    with tempfile.TemporaryDirectory(prefix="native-gallery-", dir=cache) as temporary:
        work = Path(temporary)
        index = args.index.resolve() if args.index else work / "index"
        if not args.index:
            subprocess.run([
                str(binary), "mtg", "index", "--catalog", str(cache / "catalog.json"),
                "--images", str(cache), "--output", str(index),
                "--shard-size", "256", "--workers", "4",
            ], check=True, stdout=subprocess.DEVNULL)
        faces = read_json(index / "manifest.json")["references"]
        gallery = {
            "schema_version": 1,
            "engine": {
                "profile": "release", "ocr_backend": "ocracle", "ocr_threads": 4,
                "command": "whosthatpokemon mtg identify",
                "source_revision": subprocess.check_output(
                    ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "source_dirty": bool(subprocess.check_output(
                    ["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, text=True)),
                "generation_host": {"cpu": cpu_description(), "os": platform.system(),
                                    "architecture": platform.machine()},
                "catalogue": {"faces": faces, "label": manifest["catalog_scope"] if not args.index
                              else f"Native enrolled visual catalogue: {faces:,} physical faces."},
                "timing_note": "Native release-mode wall time on the generation host; not browser performance. "
                               "Model/index loading is separate. PNG export and page rendering are excluded.",
            },
            "references": manifest["references"],
            "samples": [],
        }
        for sample in manifest["samples"]:
            image = checked_public_path(source, sample["image"])
            digest = hashlib.sha256(image.read_bytes()).hexdigest()
            if digest != sample["provenance"]["sha256"]:
                raise ValueError(f"Public input hash changed: {sample['id']}")
            trace = work / sample["id"]
            command = [binary, "mtg", "identify", "--index", index,
                       "--model-root", model_root, "--layout", sample["layout"],
                       "--trace-dir", trace]
            if sample["cropped"]:
                command.append("--cropped")
            command.append(image)
            native = run_json(command)
            if native["build"]["profile"] != "release":
                raise ValueError("Native gallery timings require a release binary")
            gallery["engine"]["crate_version"] = native["build"]["crate_version"]
            result = native["images"][0]["result"]
            expected = sample["provenance"]["expected_card_count"]
            if len(result["cards"]) != expected:
                raise ValueError(f"{sample['id']}: expected {expected} detections, got {len(result['cards'])}")
            trace_url = f"assets/native/{sample['id']}"
            destination = output / trace_url
            destination.parent.mkdir(exist_ok=True)
            shutil.copytree(trace, destination)
            for card_number, card in enumerate(result["cards"]):
                prefix = f"{trace_url}/image-0-card-{card_number}"
                card["crop_image"] = f"{prefix}-crop.png"
                for attempt_number, attempt in enumerate(card["attempts"]):
                    attempt["ocr_image"] = f"{prefix}-attempt-{attempt_number}-title.png"
                    attempt["oriented_image"] = f"{prefix}-attempt-{attempt_number}-oriented.png"
            gallery["samples"].append({**sample, "result": result,
                                        "initialization_ms": native["initialization_ms"],
                                        "title_source": native["title_source"]})
            decisions = [card["attempts"][card["selected_attempt"]]["identification"]["identity"]
                         for card in result["cards"]]
            labels = [decision["card"]["name"] if decision["card"] else decision["status"]
                      for decision in decisions]
            print(f"{sample['id']}: {', '.join(labels) or 'no cards'}; {result['total_ms']:.1f} ms")
        gallery["engine"]["initialization_ms"] = gallery["samples"][0]["initialization_ms"]
        (output / "assets/native-gallery.json").write_text(
            json.dumps(gallery, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return gallery


def main():
    """Read deployment paths; all downloaded/generated resources stay outside source."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="Release whosthatpokemon binary with ocr enabled")
    parser.add_argument("--cache", type=Path, required=True, help="Public resource cache from fetch-page-resources.py")
    parser.add_argument("--model-root", type=Path, help="Existing ocracle model root; defaults to CACHE/ocracle")
    parser.add_argument("--index", type=Path, help="Existing native shard index; defaults to the 28 public references")
    parser.add_argument("--output", type=Path, required=True, help="Empty deployment directory outside docs")
    build(parser.parse_args())


if __name__ == "__main__":
    main()
