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
import tempfile
from urllib.request import Request, urlopen

from PIL import Image


FIXTURE_REVISION = "2c81797da2dc5760fbc5944818d59b436c0b5fe8"
ORIGINAL_IDS = [
    "7673784e-db4b-43a1-8d55-1bb9fc1e284f",
    "4f616706-ec97-4923-bb1e-11a69fbaa1f8",
    "bd8fa327-dd41-4737-8f19-2cf5eb1f7cdd",
    "8ee443cc-e17a-493b-9c93-1f9e141a30e4",
]


SECRET_LAIR_EXAMPLES = (
    {
        "id": "27740ea5-79c8-420f-bc49-6d5eac58dac5",
        "name": "Lightning Bolt", "collector_number": "675", "revision": "1783924002",
        "sample_id": "secret-lair-hadoken", "title": "Secret Lair: Hadoken",
        "description": "A public Secret Lair Lightning Bolt scan with the alternate title Hadoken. Follow the literal title reading and printed SLD 675 collector bar into the native candidate search.",
        "png_sha256": "aa0061b2e3c47d2f4a2bd35b33e558868c6c9b7d80a43ab1d8417e67e86aebfc",
        "jpg_sha256": "8e58a7fc9e4f2a6d999713fbe9ed265dac9245d6889ceaff9422bf1dc035fb52",
    },
    {
        "id": "68357502-cf23-4bf6-ab4e-371428e540d0",
        "name": "Swords to Plowshares", "collector_number": "364", "revision": "1783924634",
        "sample_id": "secret-lair-mschf", "title": "Secret Lair: MSCHF gold lettering",
        "description": "A public MSCHF Swords to Plowshares scan with stylized gold lettering. Inspect the uncorrected OCR and the printed SLD 364 collector bar rather than assuming a conventional title.",
        "png_sha256": "87b6c3a5b9a67ae6409d98a95df9927fca44d6dee8839952441af8eed7997294",
        "jpg_sha256": "aaa08fd44b3f648aef34e41fa5ada11e53870f02f0c14a5470bca9d1b97d8221",
    },
    {
        "id": "c71446bd-08f2-41fe-ae44-db44814c8afb",
        "name": "Plains", "collector_number": "415", "revision": "1783924015",
        "sample_id": "secret-lair-textless", "title": "Secret Lair: a title at the bottom",
        "description": "A public textless Secret Lair Plains scan with its name near the bottom and no conventional top title. The native run retains that failed title reading alongside the actual SLD 415 collector-bar evidence.",
        "png_sha256": "f98774279a9cca1665bf10917ea87d95317decbd65a0831793d7fd76cf606e40",
        "jpg_sha256": "e0dd8e38c45695e2effbe82ac51a2704c0e61fc9b3d6193f30f7e0f6e72da958",
    },
)
CATALOG_SCOPE = (
    "31 curated public printings: four scene identities, early same-art reprints, "
    "twelve unrelated cards, and three Secret Lair examples. "
    "This is not a complete Magic catalog."
)


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


def public_image_url(example, extension):
    """Return the pinned public PNG or normal JPEG source for one allowlisted card."""
    identifier = example["id"]
    variant = "png" if extension == "png" else "normal"
    return (f"https://cards.scryfall.io/{variant}/front/{identifier[0]}/{identifier[1]}/"
            f"{identifier}.{extension}?{example['revision']}")


def extra_public_image(data, example, extension, fetch_missing, supplied=None):
    """Require or explicitly fetch a checksum-pinned image from the fixed allowlist."""
    expected = example[f"{extension}_sha256"]
    relative = Path("page-public-extras") / f"{example['id']}.{extension}"
    candidates = [data / relative, data / f"{example['id']}.{extension}"]
    if supplied is not None:
        candidates.insert(0, (data / supplied).resolve())
    for source in candidates:
        if not source.resolve().is_relative_to(data):
            raise ValueError("Secret Lair reference escapes the explicit public data directory")
        if source.is_file():
            if hashlib.sha256(source.read_bytes()).hexdigest() != expected:
                raise ValueError("Pinned public Secret Lair image checksum changed: " + str(source))
            return source
    if not fetch_missing:
        raise ValueError(f"Missing public {extension.upper()} for {example['id']}; supply it under {relative} or rerun with --fetch-missing")
    destination = data / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    time.sleep(0.55)
    request = Request(public_image_url(example, extension), headers={
        "User-Agent": "whosthatpokemon-public-page/0.0.0 (+https://github.com/cmeister2/whosthatpokemon)",
        "Accept": "image/*",
    })
    with urlopen(request, timeout=45) as response:
        content = response.read(16 * 1024 * 1024 + 1)
    if len(content) > 16 * 1024 * 1024 or hashlib.sha256(content).hexdigest() != expected:
        raise ValueError("Downloaded public Secret Lair image does not match its pin")
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(content)
            output.flush()
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)
    return destination


def secret_lair_sample(example, prefix):
    """Describe one native-only public input without a legacy visual report path."""
    return {
        "id": example["sample_id"], "title": example["title"],
        "description": example["description"],
        "image": f"{prefix}/inputs/{example['sample_id']}.jpg",
        "cropped": True, "kind": "canonical_scan", "layout": "modern",
        "provenance": {
            "source": f"https://scryfall.com/card/sld/{example['collector_number']}/{example['name'].lower().replace(' ', '-')}?utm_source=api",
            "image_source": public_image_url(example, "jpg"),
            "sha256": example["jpg_sha256"], "expected_card_count": 1,
        },
    }


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
    extra_ids = {example["id"] for example in SECRET_LAIR_EXAMPLES}
    base_cards = [card for card in public_cards if card["id"] not in extra_ids]
    for name in ("Lightning Bolt", "Counterspell", "Sol Ring"):
        reprints = [card for card in base_cards if card["name"] == name and card.get("partition") == "reprint-challenge"]
        selected.extend(reprints[:4])
    unrelated = [card for card in base_cards if card.get("partition") in ("train", "validation", "test") and "//" not in card["name"]]
    selected.extend(unrelated[:12])
    if len(selected) != 28 or len({card["id"] for card in selected}) != 28:
        raise ValueError("Expected exactly 28 distinct base public reference images")
    extra_inputs = {}
    for example in SECRET_LAIR_EXAMPLES:
        supplied = by_id.get(example["id"], {}).get("file")
        source = extra_public_image(data, example, "png", args.fetch_missing, supplied)
        extra_inputs[example["id"]] = extra_public_image(data, example, "jpg", args.fetch_missing)
        selected.append({"id": example["id"], "name": example["name"], "set": "sld",
                         "file": str(source.relative_to(data)), "sha256": example["png_sha256"],
                         "image_url": public_image_url(example, "png"),
                         "source": secret_lair_sample(example, prefix)["provenance"]["source"]})
    if len({card["id"] for card in selected}) != len(selected):
        raise ValueError("Public Secret Lair examples must add distinct reference printings")
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
    for example in SECRET_LAIR_EXAMPLES:
        destination = output / "inputs" / f"{example['sample_id']}.jpg"
        shutil.copyfile(extra_inputs[example["id"]], destination)
        samples.append(secret_lair_sample(example, prefix))
    work = data / "page-assets-work"
    catalog = work / "catalog.json"
    write_json(catalog, {"cards": enrollment})
    page_manifest = output / "samples.json"
    write_json(page_manifest, {"catalog_count": len(references), "index": f"{prefix}/catalog.index.json",
                              "catalog_scope": CATALOG_SCOPE,
                              "fixture_revision": FIXTURE_REVISION, "samples": samples, "references": references})
    if not args.skip_reports:
        subprocess.run(["cargo", "run", "--locked", "--release", "--features", "web", "--example", "page_assets", "--",
                        str(catalog), str(output / "catalog.index.json"), str(page_manifest)], check=True)
    print(f"Prepared {len(references)} public references and {len(samples)} samples in {output}")


if __name__ == "__main__":
    main()
