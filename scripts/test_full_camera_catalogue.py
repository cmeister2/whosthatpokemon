#!/usr/bin/env python3
"""Contract tests for full browser catalogue preparation and serialization.

Set WTP_TEST_BINARY to additionally exercise the real CLI paper/language/face
import policy. Tests and generated resources live in temporary directories.
"""

import gzip
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "full_camera_catalogue", Path(__file__).with_name("build-full-camera-catalogue.py"))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

PRINTING = "0000419b-0bba-4488-8f7a-6194544ce91e"
SECOND = "0000579f-7b35-4ed3-b44c-db2a538066fe"
ORACLE = "b34bb2dc-c1af-4d77-b0b3-a0fb342a5fc6"
ART = "fb2b1ca2-7440-48c2-81c8-84da0a45a626"


def card(printing=PRINTING, face=0, **changes):
    """Create a realistic prepared face with explicit fields and public imagery."""
    result = {
        "game": "mtg", "id": printing, "name": "Forest", "oracle_id": ORACLE,
        "set": "blb", "collector_number": "280", "language": "en", "face_index": face,
        "face_name": None, "illustration_id": ART, "printed_name": "Fôret",
        "layout": "normal", "source": "/private/source", "oracle_text": "Discard this rules text.",
        "printed_text": "Also discarded", "file": "/private/image.jpg",
        "image_url": f"https://cards.scryfall.io/normal/{'back' if face else 'front'}/0/0/{printing}.jpg?123",
    }
    result.update(changes)
    return result


class CatalogueTests(unittest.TestCase):
    """The deployed data contract is bounded, complete, public and repeatable."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="full-camera-contract-")
        self.directory = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def plan(self, rows):
        path = self.directory / "images.jsonl"
        path.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows), encoding="utf-8")
        return path

    def test_compact_serialization_retains_faces_aliases_and_counts(self):
        plan = self.plan([
            card(SECOND, name="Front // Back", face_name="Front", layout="transform"),
            card(SECOND, face=1, name="Front // Back", face_name="Back", layout="transform"),
            card(oracle_id=None),
        ])
        output = self.directory / "catalogue.json.gz"
        report = MODULE.export_catalogue(plan, output)
        raw = gzip.decompress(output.read_bytes())
        document = json.loads(raw)
        self.assertEqual(document["schema_version"], 1)
        catalogue = document["catalogue"]
        self.assertEqual((catalogue["faces"], catalogue["printings"], catalogue["identities"],
                          catalogue["oracle_identities"]), (3, 2, 2, 1))
        self.assertEqual(catalogue["languages"], ["en"])
        self.assertTrue(catalogue["paper_only"])
        self.assertFalse(catalogue["source"]["snapshot_known"])
        self.assertEqual(catalogue["source"]["image_plan_bytes"], plan.stat().st_size)
        self.assertEqual(len(catalogue["source"]["image_plan_sha256"]), 64)
        self.assertEqual(document["cards"][0]["printed_name"], "Fôret")
        self.assertEqual([face["face_index"] for face in document["cards"] if face["id"] == SECOND], [0, 1])
        self.assertNotIn(b"/private", raw)
        self.assertNotIn(b"oracle_text", raw)
        self.assertNotIn(b"printed_text", raw)
        self.assertNotIn(b"image_sha256", raw)
        self.assertEqual(report["bytes"], output.stat().st_size)

    def test_gzip_has_no_time_or_filename_and_is_reproducible(self):
        plan = self.plan([card()])
        outputs = [self.directory / name for name in ("first.json.gz", "second.json.gz")]
        reports = [MODULE.export_catalogue(plan, output) for output in outputs]
        encoded = outputs[0].read_bytes()
        self.assertEqual(encoded, outputs[1].read_bytes())
        self.assertEqual(encoded[4:8], b"\0\0\0\0")
        self.assertEqual(encoded[3] & 8, 0, "gzip must omit the destination filename")
        self.assertEqual(reports[0]["sha256"], reports[1]["sha256"])

    def test_optional_metadata_is_nullable_and_stable(self):
        row = card()
        for key in ("oracle_id", "face_name", "illustration_id", "printed_name", "layout"):
            row.pop(key)
        actual = MODULE.read_cards(self.plan([row]))[0][0]
        for key in ("oracle_id", "face_name", "illustration_id", "printed_name", "layout"):
            self.assertIsNone(actual[key])

    def test_duplicate_faces_and_inconsistent_printings_are_rejected(self):
        for rows in ([card(), card()], [card(), card(face=1, collector_number="281")]):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                MODULE.read_cards(self.plan(rows))

    def test_english_paper_and_real_image_scope_cannot_be_broadened(self):
        for changes in ({"language": "ja"}, {"game": "other"}, {"games": ["arena"]},
                        {"digital": True}, {"image_status": "placeholder"}, {"image_status": "missing"}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                MODULE.read_cards(self.plan([card(**changes)]))

    def test_invalid_metadata_and_face_values_are_rejected(self):
        for changes in ({"id": "../../private"}, {"oracle_id": "invalid"}, {"name": " "},
                        {"face_index": -1}, {"face_index": True}, {"face_index": "1"},
                        {"illustration_id": []}, {"printed_name": 7}, {"set": None}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                MODULE.read_cards(self.plan([card(**changes)]))

    def test_only_canonical_public_reference_urls_are_accepted(self):
        for url in (f"http://cards.scryfall.io/normal/{PRINTING}.jpg",
                    f"https://private.example/{PRINTING}.jpg",
                    f"https://cards.scryfall.io.evil.test/{PRINTING}.jpg",
                    f"https://user@cards.scryfall.io/{PRINTING}.jpg",
                    f"https://cards.scryfall.io:443/{PRINTING}.jpg",
                    f"https://cards.scryfall.io/{PRINTING}.jpg#private",
                    f"https://cards.scryfall.io/../{PRINTING}.jpg",
                    f"https://cards.scryfall.io/{SECOND}.jpg"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                MODULE.read_cards(self.plan([card(image_url=url)]))

    def test_empty_and_malformed_plans_are_rejected_without_output(self):
        plan = self.directory / "images.jsonl"
        output = self.directory / "catalogue.json.gz"
        for content in ("", "\n", "{}\n", "[]\n", "not json\n"):
            plan.write_text(content, encoding="utf-8")
            with self.subTest(content=content), self.assertRaises(ValueError):
                MODULE.export_catalogue(plan, output)
            self.assertFalse(output.exists())

    def test_snapshot_age_is_public_and_coverage_is_cross_checked(self):
        prepared = {"coverage": {"image_language": "en", "selected_faces": 1}}
        bulk = {"kind": "default_cards", "updated_at": "2026-10-19T09:18:31.916+00:00",
                "download_uri": "https://data.scryfall.io/default-cards/snapshot.jsonl.gz",
                "catalog": "/private/bulk.jsonl.gz"}
        output = self.directory / "catalogue.json.gz"
        MODULE.export_catalogue(self.plan([card()]), output, prepared, bulk)
        document = json.loads(gzip.decompress(output.read_bytes()))
        source = document["catalogue"]["source"]
        self.assertTrue(source["snapshot_known"])
        self.assertEqual(source["updated_at"], bulk["updated_at"])
        self.assertEqual(source["bulk_kind"], "default_cards")
        self.assertNotIn("/private", json.dumps(document))
        for changes in ({"updated_at": "2026-10-19"}, {"download_uri": "file:///private"},
                        {"kind": "oracle_cards"}):
            invalid = {**bulk, **changes}
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                MODULE.provenance({}, prepared, invalid, 1)
        with self.assertRaises(ValueError):
            MODULE.provenance({}, prepared, bulk, 2)

    def test_production_commands_and_report_paths_are_used(self):
        bulk = {"kind": "default_cards", "catalog": str(self.directory / "actual-snapshot.jsonl.gz")}
        prepared = {"image_plan": str(self.directory / "actual-plan.jsonl")}
        with patch.object(MODULE, "run_report", side_effect=[bulk, prepared]) as run:
            result = MODULE.prepare_plan(Path("/binary"), self.directory)
        self.assertEqual(result, (Path(prepared["image_plan"]), prepared, bulk))
        self.assertEqual(run.call_args_list[0].args[1], [
            "bulk", "--kind", "default-cards", "--output", str(self.directory / "bulk")])
        self.assertEqual(run.call_args_list[1].args[1], [
            "prepare", "--catalog", bulk["catalog"], "--output", str(self.directory / "prepared"),
            "--image-language", "en"])

    def test_adjacent_provenance_reports_are_resolved_without_source_data(self):
        plan = self.plan([card()])
        bulk_directory = self.directory / "bulk"
        bulk_directory.mkdir()
        source = bulk_directory / "snapshot.jsonl.gz"
        prepared = {"image_plan": str(plan), "source": str(source),
                    "coverage": {"image_language": "en", "selected_faces": 1}}
        bulk = {"catalog": str(source), "kind": "all_cards"}
        (self.directory / "coverage.json").write_text(json.dumps(prepared), encoding="utf-8")
        (bulk_directory / "all_cards-download.json").write_text(json.dumps(bulk), encoding="utf-8")
        self.assertEqual(MODULE.plan_reports(plan), (prepared, bulk))

    @unittest.skipUnless(os.environ.get("WTP_TEST_BINARY"), "Set WTP_TEST_BINARY for real CLI importer test")
    def test_real_cli_filters_paper_language_and_preserves_separate_faces(self):
        def raw_card(printing, **changes):
            value = {"object": "card", "id": printing, "name": "Forest", "oracle_id": ORACLE,
                     "set": "blb", "collector_number": "280", "lang": "en", "games": ["paper"],
                     "layout": "normal", "image_status": "highres_scan",
                     "image_uris": {"normal": card(printing)["image_url"]}}
            value.update(changes)
            return value
        third = "1000419b-0bba-4488-8f7a-6194544ce91e"
        fourth = "2000419b-0bba-4488-8f7a-6194544ce91e"
        fifth = "3000419b-0bba-4488-8f7a-6194544ce91e"
        dfc = raw_card(SECOND, name="Front // Back", layout="transform")
        del dfc["image_uris"]
        dfc["card_faces"] = [
            {"name": "Front", "image_uris": {"normal": card(SECOND)["image_url"]}},
            {"name": "Back", "image_uris": {"normal": card(SECOND, face=1)["image_url"]}},
        ]
        source = self.directory / "source.json"
        source.write_text(json.dumps([raw_card(PRINTING), dfc, raw_card(third, lang="ja"),
                                     raw_card(fourth, games=["arena"]),
                                     raw_card(fifth, image_status="placeholder")]), encoding="utf-8")
        prepared = MODULE.run_report(Path(os.environ["WTP_TEST_BINARY"]), [
            "prepare", "--catalog", str(source), "--output", str(self.directory / "prepared"),
            "--image-language", "en"])
        rows, _ = MODULE.read_cards(Path(prepared["image_plan"]))
        self.assertEqual(len(rows), 3)
        self.assertEqual({row["id"] for row in rows}, {PRINTING, SECOND})
        self.assertEqual([row["face_name"] for row in rows if row["id"] == SECOND], ["Front", "Back"])
        self.assertEqual(prepared["coverage"]["selected_printings"], 2)


if __name__ == "__main__":
    unittest.main()
