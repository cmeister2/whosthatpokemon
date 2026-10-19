#!/usr/bin/env python3
"""Prepare the public demo corpus from an explicit public fixture/data checkout.

Requires Pillow. Run with --fetch-missing to explicitly retrieve missing Scryfall
metadata for the selected public image IDs. Images are never uploaded. Raw
metadata and the temporary enrollment manifest stay under the supplied data
directory; only selected public assets are copied into the page directory.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time
from urllib.request import Request, urlopen

from PIL import Image


FIXTURE_REVISION = "2c81797da2dc5760fbc5944818d59b436c0b5fe8"
ORIGINAL_IDS = [
    "7673784e-db4b-43a1-8d55-1bb9fc1e284f",
    "4f616706-ec97-4923-bb1e-11a69fbaa1f8",
    "bd8fa327-dd41-4737-8f19-2cf5eb1f7cdd",
    "8ee443cc-e17a-493b-9c93-1f9e141a30e4",
]


def read_json(path):
    """Read one UTF-8 JSON document."""
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path, value):
    """Write reproducible, readable UTF-8 metadata."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def metadata_by_id(data, selected, fetch_missing):
    """Reuse public metadata and optionally fetch only allowlisted public IDs."""
    cache = data / "page-metadata.json"
    documents = list(data.glob("search-*.json"))
    if cache.exists():
        documents.append(cache)
    found = {}
    for path in documents:
        document = read_json(path)
        for card in document.get("data", []):
            found[card["id"]] = card
    missing = [card["id"] for card in selected if card["id"] not in found]
    if missing and not fetch_missing:
        raise ValueError("Missing public metadata; rerun with --fetch-missing: " + ", ".join(missing))
    for offset in range(0, len(missing), 75):
        if offset:
            time.sleep(0.55)
        request = Request(
            "https://api.scryfall.com/cards/collection",
            data=json.dumps({"identifiers": [{"id": value} for value in missing[offset:offset + 75]]}).encode(),
            headers={
                "User-Agent": "whosthatpokemon-public-page/0.0.0 (+https://github.com/cmeister2/whosthatpokemon)",
                "Accept": "application/json",
                "Content-Type": "application/json",
            },
            method="POST",
        )
        with urlopen(request, timeout=45) as response:
            result = json.load(response)
        if result.get("not_found"):
            raise ValueError("Scryfall could not resolve selected public image IDs")
        for card in result["data"]:
            if card["id"] not in missing:
                raise ValueError("Unexpected ID in Scryfall collection response")
            found[card["id"]] = card
    chosen = {card["id"]: found[card["id"]] for card in selected}
    write_json(cache, {"data": list(chosen.values())})
    return chosen


def main():
    """Build page inputs, thumbnails, enrollment metadata and native reports."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", type=Path, required=True, help="Public magicextract repository checkout")
    parser.add_argument("--data", type=Path, required=True, help="Public canonical images and manifest.json")
    parser.add_argument("--output", type=Path, default=Path("docs/assets"))
    parser.add_argument("--fetch-missing", action="store_true")
    parser.add_argument("--skip-reports", action="store_true", help="Prepare inputs without invoking Cargo")
    args = parser.parse_args()
    fixtures, data, output = args.fixtures.resolve(), args.data.resolve(), args.output.resolve()
    prefix = output.name
    output.mkdir(parents=True, exist_ok=True)
    for folder in ("inputs", "references", "reports"):
        (output / folder).mkdir(exist_ok=True)
    manifest = read_json(data / "manifest.json")
    public_cards = manifest["cards"]
    by_id = {card["id"]: card for card in public_cards}
    selected = [by_id[identifier] for identifier in ORIGINAL_IDS]
    for name in ("Lightning Bolt", "Counterspell", "Sol Ring"):
        reprints = [card for card in public_cards if card["name"] == name and card.get("partition") == "reprint-challenge"]
        selected.extend(reprints[:4])
    unrelated = [card for card in public_cards if card.get("partition") in ("train", "validation", "test") and "//" not in card["name"]]
    selected.extend(unrelated[:12])
    if len(selected) != 28 or len({card["id"] for card in selected}) != 28:
        raise ValueError("Expected exactly 28 distinct public reference images")
    metadata = metadata_by_id(data, selected, args.fetch_missing)
    references, enrollment = [], []
    for public in selected:
        identifier = public["id"]
        source = (data / public["file"]).resolve()
        if not source.is_relative_to(data):
            raise ValueError("Reference file escapes the explicit public data directory")
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        if digest != public["sha256"]:
            raise ValueError("Public reference image checksum changed: " + identifier)
        full = metadata[identifier]
        if full.get("layout") != "normal":
            raise ValueError("This curated page manifest expects ordinary single-faced references")
        card = {
            "game": "mtg", "id": identifier, "name": full["name"],
            "oracle_id": full.get("oracle_id"), "set": full.get("set"),
            "collector_number": full.get("collector_number"), "language": full.get("lang"),
            "face_index": 0, "face_name": None, "illustration_id": full.get("illustration_id"),
            "source": full.get("scryfall_uri", public["source"]),
            "oracle_text": full.get("oracle_text"), "printed_text": full.get("printed_text"),
            "printed_name": full.get("printed_name"), "type_line": full.get("type_line"),
            "layout": full.get("layout"),
        }
        thumbnail = output / "references" / f"{identifier}.webp"
        with Image.open(source) as image:
            image = image.convert("RGB")
            image.thumbnail((300, 420), Image.Resampling.LANCZOS)
            image.save(thumbnail, "WEBP", quality=88, method=6)
        references.append({**card, "image": f"{prefix}/references/" + thumbnail.name,
                           "artist": full.get("artist", public.get("artist")),
                           "image_source": public["image_url"], "image_sha256": digest})
        enrollment.append({**card, "file": str(source)})
    fixture_manifest = read_json(fixtures / "tests/fixtures/scenes/manifest.json")
    samples = []
    definitions = [
        ("perspective", "One card, at an angle", "A rendered public fixture: detect Lightning Bolt's outline, correct its perspective, and compare the resulting crop.", "baseline-lightning-bolt-perspective-1"),
        ("four-cards", "Four cards in one scene", "A rendered public fixture with Lightning Bolt, Counterspell, Black Lotus and Sol Ring. Select each detected outline to inspect its evidence.", "baseline-four-cards"),
        ("empty", "An empty scene", "A rendered negative control. A picture with no detected card should produce no recognition result.", "negative-empty-table"),
    ]
    for identifier, title, description, fixture_id in definitions:
        scene = next(scene for scene in fixture_manifest["scenes"] if scene["id"] == fixture_id)
        relative = "tests/fixtures/scenes/" + scene["file"]
        source, destination = fixtures / relative, output / "inputs" / f"{identifier}.jpg"
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        expected = fixture_manifest["sha256"][scene["file"]]
        if digest != expected:
            raise ValueError("Public fixture image checksum changed: " + relative)
        shutil.copyfile(source, destination)
        samples.append({"id": identifier, "title": title, "description": description,
                        "image": f"{prefix}/inputs/" + destination.name,
                        "report": f"{prefix}/reports/{identifier}.json", "cropped": False,
                        "kind": "rendered_fixture", "layout": "modern",
                        "provenance": {"source": f"https://github.com/cmeister2/magicextract/blob/{FIXTURE_REVISION}/{relative}",
                                       "sha256": digest,
                                       "expected_card_count": len(scene["cards"])}})
    classic = next(card for card in selected if card["name"] == "Lightning Bolt" and card.get("set") == "lea")
    source = data / classic["file"]
    destination = output / "inputs" / "classic-bolt.png"
    shutil.copyfile(source, destination)
    samples.append({"id": "classic-bolt", "title": "An early printing, without a collector number",
                    "description": "A public canonical Alpha Lightning Bolt scan, already cropped. Its catalog number 161 is not printed on the card; similar early printings share its artwork.",
                    "image": f"{prefix}/inputs/classic-bolt.png", "report": f"{prefix}/reports/classic-bolt.json",
                    "cropped": True, "kind": "canonical_scan", "layout": "classic",
                    "provenance": {"source": classic["source"], "sha256": classic["sha256"], "expected_card_count": 1}})
    work = data / "page-assets-work"
    catalog = work / "catalog.json"
    write_json(catalog, {"cards": enrollment})
    page_manifest = output / "samples.json"
    write_json(page_manifest, {"catalog_count": len(references), "index": f"{prefix}/catalog.index.json",
                              "catalog_scope": "28 curated public printings: four scene identities, early same-art reprints, and twelve unrelated cards. This is not a complete Magic catalog.",
                              "fixture_revision": FIXTURE_REVISION, "samples": samples, "references": references})
    if not args.skip_reports:
        subprocess.run(["cargo", "run", "--locked", "--release", "--features", "web", "--example", "page_assets", "--",
                        str(catalog), str(output / "catalog.index.json"), str(page_manifest)], check=True)
    print(f"Prepared {len(references)} public references and {len(samples)} samples in {output}")


if __name__ == "__main__":
    main()
