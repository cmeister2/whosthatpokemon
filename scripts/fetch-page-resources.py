#!/usr/bin/env python3
"""Prepare verified public references and the native title OCR model outside Pages.

Only the explicit Scryfall references in the public page manifest are fetched.
The model and English configuration are pinned independently of upstream defaults.
No images are uploaded. Cache and enrollment catalogue stay outside source. The
camera page packager explicitly copies these verified OCR assets into deployment;
model weights and generated indexes are never checked into the repository.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import tempfile
import time
from urllib.parse import urlparse
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[1]
MAX_GALLERY_REFERENCES = 128
MODEL_REVISION = "db2911eca9fdcd0ccbd93135c516133fefd7d3ad"
MODEL_ASSETS = (
    {
        "path": "models/ppocr-v6-small.onnx",
        "url": "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/"
               "b8f84f0b80c529de40b4fbb3544b84fa7233a513/inference.onnx",
        "sha256": "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634",
        "bytes": 21_159_378,
    },
    {
        "path": "models/paddle-v6/en.json",
        "url": "https://raw.githubusercontent.com/cmeister2/ocracle/"
               f"{MODEL_REVISION}/models/paddle-v6/en.json",
        "sha256": "1fb370edad3b02a0303642cada8937d068df18adec352fd4d055555a5601c68e",
        "bytes": 56_947,
    },
)
CARD_FIELDS = (
    "game", "id", "name", "oracle_id", "set", "collector_number", "language",
    "face_index", "face_name", "illustration_id", "source", "oracle_text",
    "printed_text", "printed_name", "type_line", "layout",
)
USER_AGENT = (
    "whosthatpokemon-public-gallery/0.0.0 "
    "(+https://github.com/cmeister2/whosthatpokemon)"
)


def sha256(path):
    """Hash a file without retaining its bytes in memory."""
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def valid(path, expected, expected_bytes=None):
    """Only reuse a complete cache entry with the pinned content digest."""
    return (path.is_file()
            and (expected_bytes is None or path.stat().st_size == expected_bytes)
            and sha256(path) == expected)


def atomic_write(destination, writer):
    """Publish one complete file without exposing a partial cache entry."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        dir=destination.parent, prefix=f".{destination.name}.", suffix=".part", delete=False
    ) as output:
        temporary = Path(output.name)
        try:
            writer(output)
            output.flush()
            os.fsync(output.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    try:
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


class Fetcher:
    """Fetch sequentially, spacing public image requests and checking all bytes."""

    def __init__(self, offline):
        self.offline = offline
        self.last_image_request = None

    def prepare(self, url, destination, expected, expected_bytes=None, local=None):
        """Reuse, copy or download a pinned asset; preserve old files on failure."""
        if valid(destination, expected, expected_bytes):
            print(f"Verified {destination}")
            return
        if local is not None and valid(local, expected, expected_bytes):
            def copy(output):
                with local.open("rb") as source:
                    shutil.copyfileobj(source, output)
                output.flush()
                if sha256(Path(output.name)) != expected:
                    raise ValueError(f"Local source changed while copying: {local}")
                if expected_bytes is not None and output.tell() != expected_bytes:
                    raise ValueError(f"Local source byte count changed while copying: {local}")
            atomic_write(destination, copy)
            print(f"Copied verified public asset {destination}")
            return
        if self.offline:
            raise ValueError(f"No verified local asset is available: {destination}")
        if urlparse(url).hostname == "cards.scryfall.io":
            if self.last_image_request is not None:
                time.sleep(max(0.0, .55 - (time.monotonic() - self.last_image_request)))
            self.last_image_request = time.monotonic()

        def download(output):
            request = Request(url, headers={"User-Agent": USER_AGENT, "Accept": "*/*"})
            digest = hashlib.sha256()
            size = 0
            # The expected graph size bounds model downloads; public PNGs are bounded too.
            maximum = expected_bytes if expected_bytes is not None else 16 * 1024 * 1024
            with urlopen(request, timeout=120) as response:
                while chunk := response.read(1024 * 1024):
                    size += len(chunk)
                    if size > maximum:
                        raise ValueError(f"Downloaded asset exceeds its size limit: {url}")
                    digest.update(chunk)
                    output.write(chunk)
            if digest.hexdigest() != expected:
                raise ValueError(f"SHA-256 mismatch for {url}: {digest.hexdigest()}")
            if expected_bytes is not None and size != expected_bytes:
                raise ValueError(f"Unexpected byte count for {url}: {size}")

        atomic_write(destination, download)
        print(f"Downloaded and verified {destination}")


def local_references(directory):
    """Find only explicitly named public references in an optional local corpus."""
    if directory is None:
        return {}
    directory = directory.resolve()
    manifest = directory / "manifest.json"
    if not manifest.exists():
        return {}
    document = json.loads(manifest.read_text(encoding="utf-8"))
    found = {}
    for card in document.get("cards", []):
        if not isinstance(card.get("file"), str):
            continue
        source = (directory / card["file"]).resolve()
        if not source.is_relative_to(directory):
            raise ValueError("Local public reference path escapes its data directory")
        found[(card["id"], card.get("face_index", 0))] = source
    return found


def main():
    """Prepare only reproducible public gallery inputs, without running inference."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True,
                        help="Build cache outside docs (for example RUNNER_TEMP/page-resources)")
    parser.add_argument("--manifest", type=Path, default=ROOT / "docs/assets/samples.json")
    parser.add_argument("--local-images", type=Path,
                        help="Optional public PNG corpus, with IDs as filenames or a manifest.json")
    parser.add_argument("--local-model-root", type=Path,
                        help="Optional ocracle checkout/cache containing the pinned models/ files")
    parser.add_argument("--offline", action="store_true",
                        help="Require verified cache entries or supplied local copies; never fetch")
    args = parser.parse_args()
    cache = args.cache.resolve()
    if cache.is_relative_to(ROOT / "docs"):
        parser.error("--cache must be outside the published docs directory")
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    references = manifest["references"]
    if (not isinstance(references, list) or not 1 <= len(references) <= MAX_GALLERY_REFERENCES
            or type(manifest.get("catalog_count")) is not int
            or manifest["catalog_count"] != len(references)):
        raise ValueError(f"The public gallery requires 1–{MAX_GALLERY_REFERENCES} explicitly listed reference printings with a matching catalog_count")
    seen = set()
    for card in references:
        identifier = card["id"]
        if not isinstance(identifier, str) or not re.fullmatch(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", identifier
        ):
            raise ValueError("Public references require canonical Scryfall IDs")
        if (card.get("game") != "mtg" or card.get("face_index", 0) != 0
                or card.get("language") != "en" or card.get("layout") != "normal"):
            raise ValueError("The curated gallery expects English single-faced Magic references")
        key = (identifier, card.get("face_index", 0))
        if key in seen:
            raise ValueError("Duplicate public reference face")
        seen.add(key)
        url = urlparse(card["image_source"])
        if (url.scheme != "https" or url.netloc != "cards.scryfall.io"
                or not url.path.startswith("/png/front/")
                or not url.path.endswith(f"/{identifier}.png")):
            raise ValueError("Only pinned public Scryfall PNG sources may be fetched")
        if not re.fullmatch(r"[0-9a-f]{64}", card["image_sha256"]):
            raise ValueError("Every public reference requires a SHA-256 digest")
    local = local_references(args.local_images)
    fetcher = Fetcher(args.offline)
    enrollment = []
    for card in references:
        destination = cache / "references" / f"{card['id']}.png"
        source = local.get((card["id"], card.get("face_index", 0)))
        if source is None and args.local_images is not None:
            source = args.local_images / f"{card['id']}.png"
        fetcher.prepare(card["image_source"], destination, card["image_sha256"], local=source)
        enrollment.append({**{key: card.get(key) for key in CARD_FIELDS}, "file": str(destination)})
    model_root = cache / "ocracle"
    for asset in MODEL_ASSETS:
        local_model = args.local_model_root / asset["path"] if args.local_model_root else None
        fetcher.prepare(asset["url"], model_root / asset["path"], asset["sha256"],
                        asset["bytes"], local_model)
    catalogue = json.dumps({"cards": enrollment}, ensure_ascii=False, indent=2) + "\n"
    atomic_write(cache / "catalog.json", lambda output: output.write(catalogue.encode("utf-8")))
    print(f"Ready: {len(enrollment)} public references; catalogue {cache / 'catalog.json'}")
    print(f"Native title OCR model root: {model_root}")


if __name__ == "__main__":
    main()
