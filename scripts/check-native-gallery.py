#!/usr/bin/env python3
"""Check native gallery integration, including known OCR and orientation regressions."""

import argparse
import hashlib
import json
import math
from pathlib import Path
import struct


ACCEPTED = {"matched", "verified_shortlist"}


def require(condition, message):
    """Fail the build with an actionable validation message."""
    if not condition:
        raise ValueError(message)


def png_size(root, relative):
    """Validate a local native trace asset and read its PNG dimensions."""
    path = (root / relative).resolve()
    require(path.is_relative_to(root), f"Trace path escapes deployment: {relative}")
    header = path.read_bytes()[:24]
    require(header[:8] == b"\x89PNG\r\n\x1a\n", f"Not a PNG trace: {relative}")
    return list(struct.unpack(">II", header[16:24]))


def milliseconds(value, label):
    """Require a real, finite, nonnegative measured native duration."""
    require(isinstance(value, (float, int)) and math.isfinite(value) and value >= 0,
            f"Invalid measured timing: {label}")


def check(root):
    """Validate generated native output without predicting OCR from reference names."""
    root = root.resolve()
    gallery = json.loads((root / "assets/native-gallery.json").read_text())
    require(gallery["schema_version"] == 1, "Unsupported native gallery schema")
    require(gallery["engine"]["profile"] == "release", "Use the release native pipeline")
    require(gallery["engine"]["ocr_backend"] == "ocracle", "Gallery must use production ocracle")
    samples = {sample["id"]: sample for sample in gallery["samples"]}
    require(set(samples) == {"perspective", "four-cards", "empty", "classic-bolt"},
            "Missing public gallery examples")
    for sample in samples.values():
        image_bytes = (root / sample["image"]).read_bytes()
        require(hashlib.sha256(image_bytes).hexdigest() == sample["provenance"]["sha256"],
                f"Changed public input: {sample['id']}")
        result = sample["result"]
        require(len(result["cards"]) == sample["provenance"]["expected_card_count"],
                f"Unexpected detections: {sample['id']}")
        for field in ("decode_ms", "extraction_ms", "total_ms"):
            milliseconds(result[field], f"{sample['id']}.{field}")
        for value in sample["initialization_ms"].values():
            milliseconds(value, "model/index setup")
        for card in result["cards"]:
            require(0 <= card["selected_attempt"] < len(card["attempts"]), "Invalid selected attempt")
            crop_size = png_size(root, card["crop_image"])
            for attempt in card["attempts"]:
                ocr = attempt["ocr"]
                require(ocr["rotation_degrees"] in (0, 180), "Unexpected title orientation")
                require(png_size(root, attempt["oriented_image"]) == crop_size, "Changed crop size")
                require(png_size(root, attempt["ocr_image"]) == ocr["crop_bounds"][2:],
                        "Title PNG differs from actual OCR rectangle")
                require(ocr["reading"]["model"]["sha256"], "Missing actual model provenance")
                for field in ("ocr_ms", "matching_ms"):
                    milliseconds(attempt[field], field)
                identification = attempt["identification"]
                require(identification["observations"]["title"] == ocr["reading"]["text"],
                        "Catalogue metadata must not replace literal OCR")
                for value in identification["timings"].values():
                    milliseconds(value, "native identification stage")
                visual = identification["visual"]
                require(visual["descriptors_scored"] <= 256 and visual["geometry_attempted"] <= 8,
                        "Gallery bypassed bounded candidate matching")
                if identification["identity"]["status"] in ACCEPTED:
                    require(visual["geometry_verified_count"] > 0,
                            "Accepted a card without actual image verification")
    require(not samples["empty"]["result"]["cards"], "Empty scene produced a card")
    scene_cards = samples["four-cards"]["result"]["cards"]
    decisions = [card["attempts"][card["selected_attempt"]]["identification"]["identity"]
                 for card in scene_cards]
    names = {decision["card"]["name"] for decision in decisions
             if decision["card"] and decision["status"] in ACCEPTED}
    require(names == {"Counterspell", "Lightning Bolt", "Sol Ring", "Black Lotus"},
            f"Four-card native recognition regressed: {sorted(names)}")
    classic = samples["classic-bolt"]["result"]["cards"][0]
    selected = classic["attempts"][classic["selected_attempt"]]
    identity = selected["identification"]["identity"]
    require(identity["status"] in ACCEPTED and identity["card"]["name"] == "Lightning Bolt",
            "Classic Christopher Rush Lightning Bolt was not identified")
    require(selected["ocr"]["rotation_degrees"] == 0 and not classic["orientation_conflict"],
            "Upright Christopher Rush Lightning Bolt was flipped")
    require((root / classic["crop_image"]).read_bytes() ==
            (root / selected["oriented_image"]).read_bytes(), "Gallery flipped an upright native crop")
    print("Native gallery: public inputs, all four identities, upright classic Bolt, OCR provenance and stage timings pass")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("site", type=Path)
    check(parser.parse_args().site)
