#!/usr/bin/env python3
"""Package the actual OCRacle WASM camera page beside the recorded native gallery.

Weights, the generated index and generated manifest are deployment artifacts only.
The complete English paper metadata snapshot is packaged without acquiring its
reference images. Only OCR-selected canonical images are acquired during scanning.
Models and metadata have a deployment length and SHA-256 pin.
"""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def descriptor(path, site):
    """Describe one actual deployment resource using its bytes and page-relative path."""
    content = path.read_bytes()
    return {"path": str(path.relative_to(site)), "bytes": len(content),
            "sha256": hashlib.sha256(content).hexdigest()}


def build(args):
    """Package the complete English metadata and reusable, pinned OCR resources."""
    output, cache, engine = args.output.resolve(), args.cache.resolve(), args.engine.resolve()
    if output == ROOT / "docs" or output.is_relative_to(ROOT / "docs"):
        raise ValueError("Generated camera resources must be outside docs")
    if not (output / "index.html").is_file():
        raise ValueError("Build the recorded native gallery into OUTPUT first")
    if (output / "camera.html").exists() or (output / "assets/camera").exists():
        raise ValueError("Camera deployment paths must not already exist")
    expected_engine = ("whosthatpokemon.js", "whosthatpokemon_bg.wasm")
    if any(not (engine / name).is_file() for name in expected_engine):
        raise ValueError("Build a real camera WASM engine with scripts/build-camera.sh")
    public = json.loads((ROOT / "docs/assets/samples.json").read_text())
    metadata = args.metadata.resolve()
    content = metadata.read_bytes()
    snapshot = json.loads(gzip.decompress(content))
    coverage = snapshot["catalogue"]
    if snapshot["schema_version"] != 1 or coverage["faces"] != len(snapshot["cards"]):
        raise ValueError("Full camera metadata has an inconsistent physical-face count")
    if not coverage.get("paper_only") or coverage.get("languages") != ["en"]:
        raise ValueError("The camera deployment requires the complete English paper catalogue")
    assets = output / "assets/camera"
    assets.mkdir()
    metadata_path = assets / "catalogue.json.gz"
    shutil.copyfile(metadata, metadata_path)
    model_assets = runpy.run_path(str(ROOT / "scripts/fetch-page-resources.py"))["MODEL_ASSETS"]
    for pinned, name in zip(model_assets, ("model.onnx", "config.json"), strict=True):
        source = cache / "ocracle" / pinned["path"]
        data = source.read_bytes()
        if len(data) != pinned["bytes"] or hashlib.sha256(data).hexdigest() != pinned["sha256"]:
            raise ValueError("Camera OCR assets must match the native gallery's pinned weights/config")
        shutil.copyfile(source, assets / name)
    for name in ("camera.html", "camera.css", "camera.js", "camera-worker.js",
                 "camera-model-license.txt", "camera-model-notice.txt"):
        shutil.copyfile(ROOT / "docs" / name, output / name)
    shutil.copytree(engine, output / "camera-engine")
    model = descriptor(assets / "model.onnx", output)
    config = descriptor(assets / "config.json", output)
    model.update({"license": "Apache-2.0", "license_path": "camera-model-license.txt",
                  "notice_path": "camera-model-notice.txt", "config_path": config["path"], "config_sha256": config["sha256"],
                  "config_bytes": config["bytes"]})
    manifest = {
        "schema_version": 1,
        "pipeline": "ocracle RTen + shared native title/footer, orientation and candidate evidence code",
        "source_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "catalogue": {**coverage, "label": coverage["scope"],
                      "metadata": {**descriptor(metadata_path, output), "compression": "gzip"}},
        "model": model,
        "max_pending_frames": 1,
        "max_frame_edge": 1280,
        "max_detected_cards": 4,
        "samples": [{**sample, **({"camera_note":
                     "This rendered scene includes a digital-only Black Lotus printing. "
                     "The camera’s paper catalogue can leave that artwork unresolved."}
                     if sample["id"] == "four-cards" else {})}
                    for sample in public["samples"]],
        "references": public["references"],
    }
    (assets / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    print(f"Packaged real browser OCR for {coverage['faces']:,} English paper faces; "
          f"model {model['bytes']:,} bytes; metadata {len(content):,} compressed bytes")


def main():
    """Accept external deployment inputs without modifying checked-in docs or weights."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--engine", type=Path, required=True)
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    build(parser.parse_args())


if __name__ == "__main__":
    main()
