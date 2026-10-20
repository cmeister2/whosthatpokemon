#!/usr/bin/env python3
"""Export the complete English paper image plan as small browser lookup metadata.

The production CLI owns Scryfall bulk import, paper/language filtering and physical
face selection. This script consumes its prepared JSONL contract; it downloads no
reference images and writes no models or generated catalogue into the repository.
The browser fetches canonical reference images only for its OCR proposals.
"""

import argparse
from datetime import datetime
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import urlsplit
import uuid


ROOT = Path(__file__).resolve().parents[1]
MAX_PLAN_LINE_BYTES = 2 * 1024 * 1024
METADATA_FIELDS = (
    "game", "id", "name", "oracle_id", "set", "collector_number", "language",
    "face_index", "face_name", "illustration_id", "printed_name", "layout",
)
REQUIRED_TEXT_FIELDS = ("game", "id", "name", "set", "collector_number", "language")
UUID_FIELDS = ("id", "oracle_id", "illustration_id")
PRINTING_FIELDS = ("name", "oracle_id", "set", "collector_number", "language", "layout")
SCOPE = (
    "All English paper Magic physical faces with available, non-placeholder public "
    "reference images in the prepared Scryfall snapshot. Title and collector-footer "
    "lookup covers this complete metadata catalogue; visual reference images are "
    "loaded and cached only when OCR proposes them."
)


def external_path(path, label):
    """Keep generated catalogues and caches outside the checked-out repository."""
    path = Path(path).resolve()
    if path == ROOT or path.is_relative_to(ROOT):
        raise ValueError(f"{label} must be outside the repository")
    return path


def run_report(binary, arguments):
    """Read the existing CLI's machine-readable report instead of reimporting bulk."""
    completed = subprocess.run(
        [str(binary), "mtg", *arguments], check=True, stdout=subprocess.PIPE,
        text=True, encoding="utf-8",
    )
    report = json.loads(completed.stdout)
    if not isinstance(report, dict):
        raise ValueError("The production CLI did not return a JSON report object")
    return report


def prepare_plan(binary, cache):
    """Use default-cards metadata and the production English paper-face policy."""
    bulk = run_report(binary, ["bulk", "--kind", "default-cards", "--output", str(cache / "bulk")])
    if bulk.get("kind") != "default_cards" or not isinstance(bulk.get("catalog"), str):
        raise ValueError("The bulk command did not return a default_cards catalogue")
    prepared = run_report(binary, [
        "prepare", "--catalog", bulk["catalog"], "--output", str(cache / "prepared"),
        "--image-language", "en",
    ])
    if not isinstance(prepared.get("image_plan"), str):
        raise ValueError("The prepare command did not return its image_plan path")
    return Path(prepared["image_plan"]).resolve(), prepared, bulk


def read_object(path):
    """Read one small optional production report, rejecting malformed sidecars."""
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"Expected a JSON object in {path.name}")
    return value


def plan_reports(plan):
    """Recover public snapshot provenance from adjacent CLI reports when available."""
    coverage_path = plan.parent / "coverage.json"
    if not coverage_path.is_file():
        return None, None
    prepared = read_object(coverage_path)
    if not isinstance(prepared.get("image_plan"), str):
        raise ValueError("The preparation report has no image_plan")
    if Path(prepared["image_plan"]).resolve() != plan:
        raise ValueError("The preparation report describes a different image plan")
    source = prepared.get("source")
    if not isinstance(source, str):
        return prepared, None
    source = Path(source).resolve()
    for path in sorted(source.parent.glob("*-download.json")):
        bulk = read_object(path)
        if isinstance(bulk.get("catalog"), str) and Path(bulk["catalog"]).resolve() == source:
            return prepared, bulk
    return prepared, None


def public_image_url(value, printing):
    """Accept only canonical public Scryfall CDN images for the matching printing."""
    if not isinstance(value, str) or len(value) > 2048 or any(c.isspace() for c in value):
        raise ValueError(f"Invalid image_url for {printing}")
    url = urlsplit(value)
    if (url.scheme != "https" or url.netloc != "cards.scryfall.io" or url.fragment
            or url.username or url.password or ".." in url.path.split("/")):
        raise ValueError(f"Reference image for {printing} must use https://cards.scryfall.io")
    if url.path.rsplit("/", 1)[-1] not in {
        f"{printing}.jpg", f"{printing}.jpeg", f"{printing}.png", f"{printing}.webp",
    }:
        raise ValueError(f"Reference image filename does not match printing {printing}")
    return value


def compact_card(row, line_number):
    """Validate the prepared physical face and discard unrelated bulk/card text."""
    if not isinstance(row, dict):
        raise ValueError(f"Image-plan line {line_number} must be an object")
    card = {key: row.get(key) for key in METADATA_FIELDS}
    for key in REQUIRED_TEXT_FIELDS:
        value = card[key]
        if not isinstance(value, str) or not value.strip() or len(value) > 1024:
            raise ValueError(f"Image-plan line {line_number} has invalid {key}")
    if card["game"] != "mtg" or card["language"] != "en":
        raise ValueError(f"Image-plan line {line_number} must describe English Magic")
    for key in METADATA_FIELDS:
        if key != "face_index" and card[key] is not None and not isinstance(card[key], str):
            raise ValueError(f"Image-plan line {line_number} has non-text {key}")
    for key in UUID_FIELDS:
        value = card[key]
        if value is not None:
            try:
                valid = str(uuid.UUID(value)) == value
            except ValueError:
                valid = False
            if not valid:
                raise ValueError(f"Image-plan line {line_number} has invalid {key} UUID")
    face = card["face_index"]
    if isinstance(face, bool) or not isinstance(face, int) or not 0 <= face <= 0xFFFF_FFFF:
        raise ValueError(f"Image-plan line {line_number} has invalid face_index")
    # Production prepare already filters these. Explicit contradictory input must
    # never broaden the deployed English paper scope through --images-plan.
    games = row.get("games")
    if row.get("digital") is True or (games is not None and (
            not isinstance(games, list) or "paper" not in games)):
        raise ValueError(f"Image-plan line {line_number} is not a paper face")
    if row.get("image_status") in {"placeholder", "missing"}:
        raise ValueError(f"Image-plan line {line_number} has no usable reference image")
    card["image_url"] = public_image_url(row.get("image_url"), card["id"])
    return card


def read_cards(plan):
    """Read the CLI JSONL contract once, validating physical and printing keys."""
    cards = []
    faces = set()
    printings = {}
    digest = hashlib.sha256()
    byte_count = 0
    with plan.open("rb") as stream:
        line_number = 0
        while line := stream.readline(MAX_PLAN_LINE_BYTES + 1):
            line_number += 1
            if len(line) > MAX_PLAN_LINE_BYTES:
                raise ValueError(f"Image-plan line {line_number} exceeds the size limit")
            digest.update(line)
            byte_count += len(line)
            if not line.strip():
                raise ValueError(f"Image-plan line {line_number} is empty")
            card = compact_card(json.loads(line), line_number)
            key = (card["game"], card["id"], card["face_index"])
            if key in faces:
                raise ValueError(f"Duplicate physical face {card['id']}:{card['face_index']}")
            faces.add(key)
            printing = tuple(card[field] for field in PRINTING_FIELDS)
            old = printings.setdefault(card["id"], printing)
            if old != printing:
                raise ValueError(f"Inconsistent printing metadata for {card['id']}")
            cards.append(card)
    if not cards:
        raise ValueError("The English paper image plan is empty")
    cards.sort(key=lambda card: (card["game"], card["id"], card["face_index"]))
    return cards, {"image_plan_sha256": digest.hexdigest(), "image_plan_bytes": byte_count}


def provenance(plan_identity, prepared, bulk, faces):
    """Publish source age and exclusions without leaking local filesystem paths."""
    result = {"kind": "prepared_scryfall_image_plan", **plan_identity, "snapshot_known": False}
    if prepared is not None:
        coverage = prepared.get("coverage")
        if not isinstance(coverage, dict) or coverage.get("image_language") != "en":
            raise ValueError("The preparation report must select English images")
        if coverage.get("selected_faces") != faces:
            raise ValueError("The preparation report face count does not match its image plan")
        result["preparation_coverage"] = {
            key: coverage[key] for key in (
                "metadata_printings", "metadata_faces", "selected_printings", "selected_faces",
                "selected_oracle_identities", "excluded_language_faces", "excluded_nonpaper_faces",
                "missing_image_faces", "placeholder_image_faces",
            ) if key in coverage
        }
    if bulk is not None:
        kind, updated, uri = (bulk.get(key) for key in ("kind", "updated_at", "download_uri"))
        if kind not in {"default_cards", "all_cards"} or not isinstance(updated, str):
            raise ValueError("Unsupported or undated Scryfall bulk snapshot")
        try:
            timestamp = datetime.fromisoformat(updated.replace("Z", "+00:00"))
        except ValueError as error:
            raise ValueError("Invalid Scryfall bulk snapshot updated_at") from error
        if timestamp.tzinfo is None:
            raise ValueError("Scryfall bulk snapshot updated_at needs a timezone")
        url = urlsplit(uri) if isinstance(uri, str) else None
        if url is None or url.scheme != "https" or url.netloc not in {
                "data.scryfall.io", "data.scryfall.com"} or url.fragment:
            raise ValueError("Invalid public Scryfall bulk snapshot URL")
        result.update({"snapshot_known": True, "bulk_kind": kind,
                       "updated_at": updated, "download_uri": uri})
    return result


def export_catalogue(plan, output, prepared=None, bulk=None):
    """Atomically serialize stable compact JSON and a reproducible gzip wrapper."""
    cards, plan_identity = read_cards(plan)
    catalogue = {
        "scope": SCOPE, "faces": len(cards), "printings": len({card["id"] for card in cards}),
        "identities": len({(card["oracle_id"] is not None, card["oracle_id"] or card["id"])
                           for card in cards}),
        "oracle_identities": len({card["oracle_id"] for card in cards if card["oracle_id"]}),
        "languages": ["en"], "paper_only": True,
        "source": provenance(plan_identity, prepared, bulk, len(cards)),
    }
    document = {"schema_version": 1, "catalogue": catalogue, "cards": cards}
    output.parent.mkdir(parents=True, exist_ok=True)
    pending = None
    try:
        with tempfile.NamedTemporaryFile(dir=output.parent, prefix=f".{output.name}.", delete=False) as stream:
            pending = Path(stream.name)
            with gzip.GzipFile(filename="", mode="wb", fileobj=stream, compresslevel=6, mtime=0) as compressed:
                with io.TextIOWrapper(compressed, encoding="utf-8", newline="\n") as text:
                    json.dump(document, text, ensure_ascii=False, separators=(",", ":"), allow_nan=False)
                    text.write("\n")
        os.replace(pending, output)
    finally:
        if pending is not None:
            pending.unlink(missing_ok=True)
    encoded = output.read_bytes()
    return {"schema_version": 1, "output": str(output), "bytes": len(encoded),
            "sha256": hashlib.sha256(encoded).hexdigest(), "catalogue": catalogue}


def main():
    """Build a fresh production plan or validate/export an existing offline plan."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/whosthatpokemon")
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--images-plan", type=Path,
                        help="Existing production prepare images.jsonl; avoids bulk download/import")
    parser.add_argument("--output", type=Path, required=True, help="External .json.gz deployment artifact")
    args = parser.parse_args()
    cache = external_path(args.cache, "Cache")
    output = external_path(args.output, "Catalogue output")
    if args.images_plan is None:
        cache.mkdir(parents=True, exist_ok=True)
        plan, prepared, bulk = prepare_plan(args.binary.resolve(), cache)
    else:
        plan = args.images_plan.resolve()
        prepared, bulk = plan_reports(plan)
    print(json.dumps(export_catalogue(plan, output, prepared, bulk), ensure_ascii=False))


if __name__ == "__main__":
    main()
