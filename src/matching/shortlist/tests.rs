//! Candidate coverage, bounded work and selective binary I/O regressions.

use super::*;
use crate::matching::{ShardedIndexBuilder, binary};
use crate::{CatalogIndex, Matcher};
use image::{DynamicImage, Rgb, RgbImage};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

/// Counter separates all test-owned temporary galleries.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Gallery files removed even when assertions fail.
struct Directory(PathBuf);

impl Directory {
    /// Reserve an isolated directory using only generated test inputs.
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "whosthatpokemon-shortlist-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A conventional synthetic frame with deterministic distinctive artwork.
fn image(seed: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(168, 234, |x, y| {
        if !(6..162).contains(&x) || !(6..228).contains(&y) {
            Rgb([12, 12, 12])
        } else if (38..134).contains(&y) {
            let value = x.wrapping_mul(41 + seed * 19)
                ^ y.wrapping_mul(73 + seed * 31)
                ^ (x / (3 + seed % 4) * y / 7).wrapping_mul(53);
            Rgb([
                (value & 255) as u8,
                ((value >> 3) & 255) as u8,
                ((value >> 6) & 255) as u8,
            ])
        } else if (145..215).contains(&y) && y % 11 < 2 {
            Rgb([35, 35, 35])
        } else {
            Rgb([190, 182, 155])
        }
    }))
}

/// Invented physical face with explicit semantic and artwork grouping.
fn card(id: &str, identity: &str, art: &str) -> CardMetadata {
    CardMetadata {
        game: "test".into(),
        id: id.into(),
        name: format!("Invented {identity}"),
        oracle_id: Some(identity.into()),
        illustration_id: Some(art.into()),
        ..CardMetadata::default()
    }
}

/// Build one generated face per binary shard and retain editable descriptor fixtures.
fn gallery(directory: &Directory, entries: &[(CardMetadata, u32)]) -> (CatalogIndex, ShardedIndex) {
    let mut memory = CatalogIndex::new();
    let mut builder = ShardedIndexBuilder::new(&directory.0, 1).unwrap();
    for (card, seed) in entries {
        let image = image(*seed);
        memory.add(card.clone(), &image).unwrap();
        builder.add(card.clone(), &image).unwrap();
    }
    (memory, builder.finish().unwrap())
}

/// Save changed synthetic descriptors before opening the index handle under test.
fn rewrite(directory: &Directory, memory: &CatalogIndex) {
    for (number, reference) in memory.references.iter().enumerate() {
        let shard = CatalogIndex {
            references: vec![reference.clone()],
        };
        binary::save(
            &shard,
            &directory.0.join(format!("shard-{number:06}.global.bin")),
            &directory.0.join(format!("shard-{number:06}.local.bin")),
        )
        .unwrap();
    }
}

/// Cached score fixtures isolate grouping/budget behavior from image extraction.
fn scores(values: &[f32]) -> Vec<CachedScore> {
    values
        .iter()
        .enumerate()
        .map(|(offset, similarity)| CachedScore {
            shard: 0,
            offset,
            similarity: *similarity,
            rotation_degrees: 0,
        })
        .collect()
}

/// An OCR identity outside visual top-k survives a large reprint group's budget pressure.
#[test]
fn reserves_distinct_hinted_identities_and_reports_omissions() {
    let mut metadata: Vec<_> = (0..12)
        .map(|number| card(&format!("many-{number}"), "many", "many-art"))
        .collect();
    metadata.push(card("hint-low", "hint", "hint-art"));
    metadata.push(card("hint-best", "hint", "other-art"));
    metadata.push(card("second-hint", "second", "second-art"));
    let references: Vec<_> = metadata.iter().collect();
    let mut values = vec![0.95; 12];
    values.extend([0.1, 0.6, 0.3]);
    let hints = [metadata[12].clone(), metadata[14].clone()];
    let options = ShortlistOptions {
        visual_candidates: 1,
        max_geometry: 2,
    };
    let selection = select(&references, &scores(&values), &options, &hints);
    assert_eq!(
        selection.selected,
        [13, 14],
        "best expanded artwork, then second hint, before common reprints"
    );
    assert_eq!(selection.candidates.len(), 15);
    assert_eq!(selection.omitted_hints, 0);
    assert_eq!(selection.omitted_visual, 1);
    let selection = select(
        &references,
        &scores(&values),
        &ShortlistOptions {
            max_geometry: 1,
            ..options
        },
        &hints,
    );
    assert_eq!(selection.selected, [13]);
    assert_eq!(selection.omitted_hints, 1);
}

/// Oracle and artwork expansion remain game-scoped and include distinct physical faces.
#[test]
fn expands_identity_and_illustration_without_crossing_game_namespaces() {
    let mut other_game = card("foreign", "hint", "same-art");
    other_game.game = "other".into();
    let mut back = card("back", "hint", "other-art");
    back.face_index = 1;
    let metadata = [
        card("visual", "visual", "visual-art"),
        card("hint", "hint", "same-art"),
        back,
        card("shared-art", "separate-identity", "same-art"),
        other_game,
    ];
    let references: Vec<_> = metadata.iter().collect();
    let selection = select(
        &references,
        &scores(&[0.9, 0.3, 0.2, 0.1, 0.8]),
        &ShortlistOptions {
            visual_candidates: 1,
            max_geometry: 10,
        },
        &[metadata[1].clone()],
    );
    assert_eq!(selection.candidates, [0, 1, 2, 3]);
    assert!(!selection.selected.contains(&4));
}

/// An uncontested accepted global query retains the exact API's original result.
#[test]
fn global_fast_path_preserves_exact_result_without_reading_locals() {
    let directory = Directory::new();
    let (_, index) = gallery(
        &directory,
        &[(card("a", "alpha", "a"), 1), (card("b", "beta", "b"), 7)],
    );
    for number in 0..2 {
        fs::remove_file(directory.0.join(format!("shard-{number:06}.local.bin"))).unwrap();
    }
    let options = MatchOptions::default();
    let query = image(7);
    let expected = index.recognize(&query, &options).unwrap();
    let result = index
        .recognize_shortlisted(&query, &options, &ShortlistOptions::default(), &[])
        .unwrap();
    assert_eq!(result.recognition, expected);
    assert_eq!(result.global_recognition, expected);
    assert_eq!(result.scope, ShortlistScope::FullGalleryGlobal);
    assert!(result.search_complete);
    assert_eq!(result.geometry_attempted, 0);
}

/// A conflicting low-global OCR face gets actual geometry despite an accepted visual decoy.
#[test]
fn conflicting_hint_forces_bounded_geometry_outside_visual_top_k() {
    let directory = Directory::new();
    let hinted = card("hinted", "hinted-identity", "hinted-art");
    let (mut memory, _) = gallery(
        &directory,
        &[
            (card("decoy", "decoy", "decoy-art"), 1),
            (hinted.clone(), 7),
            (card("outside", "outside", "outside-art"), 19),
        ],
    );
    // Invent a misleading globally perfect decoy, but preserve its unrelated local
    // features. The true hinted reference keeps correct local/artwork evidence.
    memory.references[0].views = memory.references[1].views.clone();
    for view in &mut memory.references[1].views {
        for value in &mut view.full {
            *value = -*value;
        }
    }
    rewrite(&directory, &memory);
    let index = ShardedIndex::load(&directory.0).unwrap();
    // Startup validated these immutable banks. Their removal demonstrates that
    // shortlist execution never opens local files for noncandidate shards.
    for number in [0, 2] {
        fs::remove_file(directory.0.join(format!("shard-{number:06}.local.bin"))).unwrap();
    }
    let result = index
        .recognize_shortlisted(
            &image(7),
            &MatchOptions::default(),
            &ShortlistOptions {
                visual_candidates: 1,
                max_geometry: 1,
            },
            &[hinted],
        )
        .unwrap();
    assert_eq!(result.global_recognition.candidates[0].card.id, "decoy");
    assert_eq!(result.global_recognition.status, MatchStatus::Matched);
    assert!(result.ocr_conflict);
    assert_eq!(result.geometry_attempted, 1);
    assert_eq!(result.geometry_candidates.len(), 1);
    assert_eq!(result.geometry_candidates[0].id, "hinted");
    assert_eq!(result.geometry_verified_count, 1);
    assert_eq!(result.geometry_verified[0].card.id, "hinted");
    assert!(result.geometry_verified[0].similarity > 0.9);
    assert!(result.geometry_verified[0].global_similarity < 0.9);
    assert_eq!(result.scope, ShortlistScope::CandidateGeometry);
    assert!(!result.search_complete);
    assert!(result.budget_exhausted);
    assert_eq!(result.omitted_hint_groups, 0);
    assert_eq!(result.recognition.confidence, 0.0);
    assert_ne!(result.recognition.identity_status, MatchStatus::Matched);
}

/// Same-art alternatives outside the winning identity still prevent unique acceptance.
#[test]
fn full_candidate_geometry_keeps_same_illustration_ambiguity() {
    let directory = Directory::new();
    let hinted = card("winner", "winner", "shared-art");
    let (mut memory, _) = gallery(
        &directory,
        &[
            (hinted.clone(), 7),
            (card("related", "other", "shared-art"), 19),
        ],
    );
    for reference in &mut memory.references {
        for view in &mut reference.views {
            for value in &mut view.full {
                *value = -*value;
            }
        }
    }
    rewrite(&directory, &memory);
    let index = ShardedIndex::load(&directory.0).unwrap();
    let options = MatchOptions {
        min_similarity: 0.9,
        ..MatchOptions::default()
    };
    let result = index
        .recognize_shortlisted(
            &image(7),
            &options,
            &ShortlistOptions {
                visual_candidates: 1,
                max_geometry: 10,
            },
            &[hinted],
        )
        .unwrap();
    assert_eq!(result.geometry_attempted, 2);
    assert_eq!(result.scope, ShortlistScope::FullGalleryGeometry);
    assert!(result.search_complete);
    assert_eq!(result.recognition.candidates[0].card.id, "winner");
    assert_eq!(result.recognition.status, MatchStatus::Ambiguous);
    assert_eq!(result.recognition.identity_status, MatchStatus::Ambiguous);
}

/// Selected-bank decoding checks counts/termination even when payload bytes are skipped.
#[test]
fn selective_local_reader_rejects_malformed_skipped_counts_and_termination() {
    let directory = Directory::new();
    fs::create_dir_all(&directory.0).unwrap();
    let mut memory = CatalogIndex::new();
    memory.add(card("a", "a", "a"), &image(1)).unwrap();
    memory.add(card("b", "b", "b"), &image(7)).unwrap();
    let global = directory.0.join("global.bin");
    let local = directory.0.join("local.bin");
    binary::save(&memory, &global, &local).unwrap();
    let cards: Vec<_> = memory.metadata().cloned().collect();
    let mut loaded = binary::globals_selected(&global, &cards, &[1]).unwrap();
    binary::locals_selected(&local, &mut loaded).unwrap();
    assert!(loaded[0].is_none());
    assert_eq!(
        loaded[1].as_ref().unwrap().features.len(),
        memory.references[1].features.len()
    );
    let original = fs::read(&local).unwrap();
    let mut bad = original.clone();
    bad[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&local, bad).unwrap();
    assert!(binary::locals_selected(&local, &mut loaded).is_err());
    let mut bad = original.clone();
    bad.push(0);
    fs::write(&local, bad).unwrap();
    assert!(binary::locals_selected(&local, &mut loaded).is_err());
    let mut bad = original;
    bad.truncate(bad.len() - 1);
    fs::write(&local, bad).unwrap();
    assert!(binary::locals_selected(&local, &mut loaded).is_err());
    assert!(binary::globals_selected(&global, &cards, &[2]).is_err());
}

/// Invalid work budgets fail before visual extraction or filesystem access.
#[test]
fn validates_candidate_budgets() {
    assert!(ShortlistOptions::default().validate().is_ok());
    assert!(
        ShortlistOptions {
            visual_candidates: 0,
            max_geometry: 1
        }
        .validate()
        .is_err()
    );
    assert!(
        ShortlistOptions {
            visual_candidates: 1,
            max_geometry: 0
        }
        .validate()
        .is_err()
    );
}

/// Full combined margins retain a competing identity beyond the returned printing list.
#[test]
fn combined_margins_include_identity_competitors_beyond_top_k() {
    let options = MatchOptions {
        top_k: 1,
        ..MatchOptions::default()
    };
    let mut ranking = Ranking::new(options.top_k);
    for number in 0..12 {
        ranking.push(MatchCandidate {
            card: card(&format!("print-{number}"), "same", "same-art"),
            similarity: 0.95 - number as f32 * 0.0001,
            confidence: 0.0,
            rotation_degrees: 0,
        });
    }
    ranking.push(MatchCandidate {
        card: card("other", "other", "other-art"),
        similarity: 0.93,
        confidence: 0.0,
        rotation_degrees: 0,
    });
    let (printing, identity) = ranking.margins();
    assert!((printing - 0.0001).abs() < 0.000001);
    assert!((identity - 0.02).abs() < 0.000001);
    let result = ranking.finish(&options).unwrap();
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.identity_status, MatchStatus::Ambiguous);
}

/// Indexed OCR proposals expand by identity/art without touching an unrelated shard.
#[test]
fn candidate_only_path_uses_bounded_indexed_closure() {
    let directory = Directory::new();
    let target = card("target", "spell", "shared-art");
    let entries = [
        (target.clone(), 7),
        (card("reprint", "spell", "other-art"), 11),
        (card("shared-art", "other-spell", "shared-art"), 7),
        (card("unrelated", "unrelated", "unrelated-art"), 19),
    ];
    let (_, index) = gallery(&directory, &entries);
    // Startup has already built the metadata postings. Removing an unrelated
    // descriptor shard makes any accidental complete-gallery pass observable.
    fs::remove_file(directory.0.join("shard-000003.global.bin")).unwrap();
    fs::remove_file(directory.0.join("shard-000003.local.bin")).unwrap();
    let result = index
        .recognize_candidates(&image(7), &MatchOptions::default(), 8, &[target])
        .unwrap();
    assert_eq!(result.scope, ShortlistScope::SuppliedCandidates);
    assert!(!result.search_complete);
    assert_eq!(result.proposal_count, 1);
    assert_eq!(result.matched_proposals, 1);
    assert_eq!(result.candidate_count, 3);
    assert_eq!(result.geometry_attempted, 3);
    assert!(!result.budget_exhausted);
    assert_eq!(result.omitted_candidate_groups, 0);
    assert!(
        result
            .recognition
            .candidates
            .iter()
            .all(|candidate| candidate.card.id != "unrelated")
    );
    assert_eq!(result.recognition.confidence, 0.0);
    assert_ne!(result.recognition.status, MatchStatus::Matched);
}

/// Candidate verification retains actual homography evidence but never claims a gallery win.
#[test]
fn candidate_only_path_verifies_geometry_and_abstains_outside_scope() {
    let directory = Directory::new();
    let target = card("target", "target", "target-art");
    let (_, index) = gallery(
        &directory,
        &[
            (target.clone(), 7),
            (card("unrelated", "unrelated", "unrelated-art"), 19),
        ],
    );
    fs::remove_file(directory.0.join("shard-000001.global.bin")).unwrap();
    fs::remove_file(directory.0.join("shard-000001.local.bin")).unwrap();
    let result = index
        .recognize_candidates(&image(7), &MatchOptions::default(), 1, &[target])
        .unwrap();
    assert_eq!(result.geometry_attempted, 1);
    assert_eq!(result.geometry_verified_count, 1);
    assert_eq!(result.geometry_verified[0].card.id, "target");
    assert!(result.geometry_verified[0].similarity > 0.9);
    assert_eq!(result.recognition.candidates[0].card.id, "target");
    assert_eq!(result.recognition.status, MatchStatus::Ambiguous);
    assert_eq!(result.recognition.identity_status, MatchStatus::Ambiguous);
    assert_eq!(result.recognition.confidence, 0.0);
    assert_eq!(result.recognition.identity_confidence, 0.0);
    assert!(result.candidate_printing_margin > 0.9);
    assert!(result.candidate_identity_margin > 0.9);
}

/// The budget reserves supplied faces and reports identity groups it could not retain.
#[test]
fn candidate_only_budget_reports_omitted_groups() {
    let directory = Directory::new();
    let first = card("first", "first", "first-art");
    let second = card("second", "second", "second-art");
    let (_, index) = gallery(&directory, &[(first.clone(), 1), (second.clone(), 7)]);
    let result = index
        .recognize_candidates(&image(1), &MatchOptions::default(), 1, &[first, second])
        .unwrap();
    assert_eq!(result.candidate_count, 2);
    assert_eq!(result.geometry_attempted, 1);
    assert_eq!(result.geometry_candidates[0].id, "first");
    assert!(result.budget_exhausted);
    assert_eq!(result.omitted_descriptor_groups, 1);
    assert_eq!(result.omitted_geometry_groups, 2);
    assert_eq!(result.omitted_candidate_groups, 3);
}

/// Shared-art expansion reports an unverified identity even with one supplied OCR group.
#[test]
fn candidate_only_budget_reports_omitted_expanded_identity() {
    let directory = Directory::new();
    let target = card("target", "target", "shared-art");
    let alternate = card("alternate", "alternate", "shared-art");
    let (_, index) = gallery(&directory, &[(target.clone(), 1), (alternate, 7)]);
    let result = index
        .recognize_candidates(&image(1), &MatchOptions::default(), 1, &[target])
        .unwrap();
    assert_eq!(result.candidate_count, 2);
    assert!(result.budget_exhausted);
    assert_eq!(result.omitted_candidate_groups, 1);
}

/// Empty/unmapped OCR output returns immediately without opening descriptor shards.
#[test]
fn unmapped_candidate_scope_returns_unknown_without_gallery_io() {
    let directory = Directory::new();
    let (_, index) = gallery(&directory, &[(card("known", "known", "known-art"), 1)]);
    fs::remove_file(directory.0.join("shard-000000.global.bin")).unwrap();
    fs::remove_file(directory.0.join("shard-000000.local.bin")).unwrap();
    let result = index
        .recognize_candidates(
            &image(1),
            &MatchOptions::default(),
            8,
            &[card("missing", "missing", "missing-art")],
        )
        .unwrap();
    assert_eq!(result.matched_proposals, 0);
    assert_eq!(result.candidate_count, 0);
    assert_eq!(result.geometry_attempted, 0);
    assert_eq!(result.recognition.status, MatchStatus::Unknown);
    assert_eq!(result.omitted_candidate_groups, 1);
    assert!(result.timings.candidate_selection.is_finite());
    assert_eq!(result.timings.query_descriptors, 0.0);
    assert_eq!(result.timings.global_read_and_score, 0.0);
    assert_eq!(result.timings.geometry_selection, 0.0);
    assert_eq!(result.timings.query_local_features, 0.0);
    assert_eq!(result.timings.local_read_and_verify, 0.0);
    assert_eq!(result.timings.result_finalization, 0.0);
    assert!(result.timings.total >= result.timings.candidate_selection);
}

/// A textureless mapped query reports screening time without claiming later work ran.
#[test]
fn textureless_candidate_scope_skips_descriptor_and_geometry_stages() {
    let directory = Directory::new();
    let target = card("known", "known", "known-art");
    let (_, index) = gallery(&directory, &[(target.clone(), 1)]);
    let uniform = DynamicImage::ImageRgb8(RgbImage::from_pixel(168, 234, Rgb([128; 3])));
    let result = index
        .recognize_candidates(&uniform, &MatchOptions::default(), 1, &[target])
        .unwrap();

    assert_eq!(result.recognition.status, MatchStatus::Unknown);
    assert_eq!(result.descriptors_scored, 0);
    assert!(result.timings.candidate_selection.is_finite());
    assert!(result.timings.query_descriptors.is_finite());
    assert_eq!(result.timings.global_read_and_score, 0.0);
    assert_eq!(result.timings.geometry_selection, 0.0);
    assert_eq!(result.timings.query_local_features, 0.0);
    assert_eq!(result.timings.local_read_and_verify, 0.0);
    assert_eq!(result.timings.result_finalization, 0.0);
}

/// Candidate-only work requires a positive independent face budget.
#[test]
fn candidate_only_path_rejects_zero_budget() {
    let directory = Directory::new();
    let (_, index) = gallery(&directory, &[(card("known", "known", "known-art"), 1)]);
    assert!(
        index
            .recognize_candidates(&image(1), &MatchOptions::default(), 0, &[])
            .is_err()
    );
}

/// Compact descriptors cover the OCR closure while local I/O touches only the shortlist.
#[test]
fn candidate_two_stage_path_loads_locals_only_for_geometry_shortlist() {
    let directory = Directory::new();
    let target = card("target", "shared", "shared-art");
    let (_, index) = gallery(
        &directory,
        &[
            (target.clone(), 7),
            (card("reprint-a", "shared", "shared-art"), 11),
            (card("reprint-b", "shared", "shared-art"), 19),
        ],
    );
    // The exact query makes shard zero the sole geometric candidate. Global
    // files for all three remain available, but unrelated local files do not.
    for number in [1, 2] {
        fs::remove_file(directory.0.join(format!("shard-{number:06}.local.bin"))).unwrap();
    }
    let result = index
        .recognize_candidates_with(
            &image(7),
            &MatchOptions::default(),
            &CandidateOptions {
                max_descriptors: 8,
                max_geometry: 1,
                orientations: CandidateOrientations::Upright,
            },
            &[target],
        )
        .unwrap();
    assert_eq!(result.candidate_count, 3);
    assert_eq!(result.descriptors_scored, 3);
    assert_eq!(result.geometry_attempted, 1);
    assert!(!result.descriptor_budget_exhausted);
    assert!(result.geometry_budget_exhausted);
    assert_eq!(result.omitted_descriptor_groups, 0);
    assert_eq!(result.omitted_geometry_groups, 0);
    assert_eq!(result.geometry_candidates[0].id, "target");
    assert_eq!(result.geometry_verified[0].card.id, "target");
    for elapsed in [
        result.timings.candidate_selection,
        result.timings.query_descriptors,
        result.timings.global_read_and_score,
        result.timings.geometry_selection,
        result.timings.query_local_features,
        result.timings.local_read_and_verify,
        result.timings.result_finalization,
        result.timings.total,
    ] {
        assert!(elapsed.is_finite());
        assert!(elapsed >= 0.0);
    }
}

/// Geometry reserves each proposed identity before filling remaining slots by score.
#[test]
fn geometry_shortlist_preserves_proposed_identities_and_illustrations() {
    let cards = [
        card("a-one", "a", "art-one"),
        card("a-two", "a", "art-two"),
        card("b-one", "b", "art-three"),
    ];
    let mut scored = Vec::new();
    for (position, ((card, seed), similarity)) in cards
        .iter()
        .zip([1, 7, 19])
        .zip([0.99, 0.98, 0.50])
        .enumerate()
    {
        let mut catalogue = CatalogIndex::new();
        catalogue.add(card.clone(), &image(seed)).unwrap();
        let reference = catalogue.references.pop().unwrap();
        let queries = candidate_queries(&image(seed).to_rgb8(), CandidateOrientations::Upright);
        let mut candidate = score_candidate_reference(&reference, &queries);
        candidate.similarity = similarity;
        scored.push(ScoredCandidate {
            location: CandidateLocation {
                shard: position,
                offset: 0,
            },
            reference,
            candidate,
        });
    }
    // The first proposal names art-two. Identity a's stronger art-one reprint
    // is reserved first, then the proposal's own illustration remains a group
    // that must either be selected or explicitly reported as omitted.
    let selection = select_geometry(&scored, &[cards[1].clone(), cards[2].clone()], 2);
    assert_eq!(selection.selected, [0, 2]);
    assert_eq!(selection.omitted_groups, 1, "art-two exceeds the budget");
    let selection = select_geometry(&scored, &[cards[1].clone(), cards[2].clone()], 3);
    assert_eq!(selection.selected, [0, 2, 1]);
    assert_eq!(selection.omitted_groups, 0);
}

/// Candidate scoring never emits an orientation excluded by its explicit policy.
#[test]
fn candidate_orientation_policy_excludes_quarter_turns() {
    let directory = Directory::new();
    let target = card("target", "target", "target-art");
    let (_, index) = gallery(&directory, &[(target.clone(), 7)]);
    for (orientation, expected) in [
        (CandidateOrientations::Upright, 0),
        (CandidateOrientations::Inverted, 180),
        (CandidateOrientations::UprightAndInverted, 180),
    ] {
        let result = index
            .recognize_candidates_with(
                &image(7).rotate180(),
                &MatchOptions::default(),
                &CandidateOptions {
                    max_descriptors: 1,
                    max_geometry: 1,
                    orientations: orientation,
                },
                std::slice::from_ref(&target),
            )
            .unwrap();
        assert_eq!(result.recognition.candidates[0].rotation_degrees, expected);
        assert!(
            result
                .geometry_verified
                .iter()
                .all(|verified| orientation.allows(verified.rotation_degrees))
        );
    }
    let quarter_turn = index
        .recognize_candidates_with(
            &image(7).rotate90(),
            &MatchOptions::default(),
            &CandidateOptions::default(),
            &[target],
        )
        .unwrap();
    assert!(
        quarter_turn.recognition.candidates[0].rotation_degrees == 0
            || quarter_turn.recognition.candidates[0].rotation_degrees == 180
    );
    assert!(quarter_turn.geometry_verified.is_empty());
}

/// Timing reports written before the read/verification split remain loadable.
#[test]
fn candidate_timings_accepts_legacy_serialized_fields() {
    let mut value = serde_json::to_value(CandidateTimings::default()).unwrap();
    let fields = value.as_object_mut().unwrap();
    fields.remove("local_feature_read");
    fields.remove("geometry_verification");
    let timings: CandidateTimings = serde_json::from_value(value).unwrap();
    assert_eq!(timings.local_feature_read, 0.0);
    assert_eq!(timings.geometry_verification, 0.0);
}
