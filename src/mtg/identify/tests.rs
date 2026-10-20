//! Synthetic policy regressions; no photographed cards or real card wording is used.

use super::*;
use crate::MatchCandidate;
use crate::matching::GeometryVerification;
use crate::matching::ShardedIndexBuilder;
use image::{Rgb, RgbImage};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

/// Separate every generated gallery without introducing a temporary-file dependency.
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Test-owned files removed even after an assertion failure.
struct Directory(PathBuf);

impl Directory {
    /// Reserve an absent process-specific gallery directory.
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "whosthatpokemon-identify-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A procedural portrait containing distinctive artwork and invented text-like stripes.
fn portrait(seed: u32) -> DynamicImage {
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

/// Invent a catalogue face with text and a unique collector footer.
fn card(id: &str, oracle: &str, name: &str, collector: &str) -> CardMetadata {
    CardMetadata {
        game: "invented".into(),
        id: id.into(),
        oracle_id: Some(oracle.into()),
        name: name.into(),
        set: Some("tst".into()),
        collector_number: Some(collector.into()),
        language: Some("en".into()),
        oracle_text: Some(format!(
            "Gather seven crystalline lanterns beneath the {name} observatory."
        )),
        ..CardMetadata::default()
    }
}

/// Supply ordinary scalar image evidence without pretending these fixtures ran geometry.
fn candidate(card: &CardMetadata, similarity: f32) -> MatchCandidate {
    MatchCandidate {
        card: card.clone(),
        similarity,
        confidence: 0.0,
        rotation_degrees: 0,
    }
}

/// Make an explicit visual fixture for isolated fusion-policy tests.
fn recognition(cards: &[(&CardMetadata, f32)], status: MatchStatus) -> Recognition {
    Recognition {
        status,
        identity_status: status,
        confidence: 0.0,
        identity_confidence: 0.0,
        candidates: cards
            .iter()
            .map(|(card, score)| candidate(card, *score))
            .collect(),
    }
}

/// Construct a complete-global visual result, overridable for partial-geometry cases.
fn visual(cards: &[(&CardMetadata, f32)], status: MatchStatus) -> ShortlistedRecognition {
    let recognition = recognition(cards, status);
    ShortlistedRecognition {
        global_recognition: recognition.clone(),
        recognition,
        combined_printing_margin: 0.12,
        combined_identity_margin: 0.12,
        scope: ShortlistScope::FullGalleryGlobal,
        search_complete: true,
        total_references: cards.len(),
        candidate_count: 0,
        geometry_attempted: 0,
        geometry_verified_count: 0,
        geometry_candidates: Vec::new(),
        budget_exhausted: false,
        ocr_conflict: false,
        omitted_hint_groups: 0,
        omitted_visual_groups: 0,
        geometry_verified: Vec::new(),
    }
}

/// Search actual invented metadata before applying the isolated fusion-policy fixture.
fn run(
    cards: Vec<CardMetadata>,
    observations: TextObservations,
    visual: ShortlistedRecognition,
) -> CombinedRecognition {
    let text = TextIndex::from_metadata(cards).expect("invented metadata is valid");
    combine(
        &observations,
        collect_text_candidates(&text, &observations),
        visual,
        &MatchOptions::default(),
    )
}

/// Mark an actual-verification fixture while keeping the partial result conservative.
fn partial_geometry(visual: &mut ShortlistedRecognition, card: &CardMetadata, score: f32) {
    visual.scope = ShortlistScope::CandidateGeometry;
    visual.search_complete = false;
    visual.geometry_attempted = 2;
    visual.geometry_verified_count = 1;
    visual.budget_exhausted = true;
    visual.recognition.status = MatchStatus::Ambiguous;
    visual.recognition.identity_status = MatchStatus::Ambiguous;
    visual.geometry_verified = vec![GeometryVerification {
        card: card.clone(),
        similarity: score,
        global_similarity: 0.70,
        rotation_degrees: 0,
    }];
}

#[test]
fn independent_fields_keep_correct_title_and_rules_despite_wrong_exact_footer() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let rules = card("b", "identity-b", "Violet Surveyor", "102");
    let footer = card("c", "identity-c", "Silver Aeronaut", "103");
    let index = TextIndex::from_metadata([title.clone(), rules.clone(), footer.clone()])
        .expect("invented catalogue");
    let observations = TextObservations {
        title: title.name.clone(),
        rules: rules.oracle_text.clone().expect("invented rules"),
        footer: "103 tst en".into(),
    };
    let text = collect_text_candidates(&index, &observations);
    assert_eq!(text.title[0].card.id, title.id);
    assert_eq!(text.rules[0].card.id, rules.id);
    assert_eq!(text.footer[0].card.id, footer.id);
    assert!(text.hints.iter().any(|hint| hint.id == title.id));
    assert!(text.hints.iter().any(|hint| hint.id == rules.id));
    assert!(text.hints.iter().any(|hint| hint.id == footer.id));
    assert_eq!(text.hints.len(), 3);
}

#[test]
fn correct_strong_title_preserves_complete_global_identity_against_wrong_footer() {
    let correct = card("a", "identity-a", "Amber Clockmaker", "101");
    let wrong = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&correct, 0.94), (&wrong, 0.60)], MatchStatus::Matched);
    partial_geometry(&mut image, &correct, 0.90);
    image.omitted_visual_groups = 1;
    let result = run(
        vec![correct.clone(), wrong],
        TextObservations {
            title: correct.name.clone(),
            footer: "102 tst en".into(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Matched);
    assert_eq!(result.identity.scope, ShortlistScope::FullGalleryGlobal);
    assert_eq!(result.printing.status, MatchStatus::Matched);
    assert!(result.conflicts.len() >= 2);
    assert!(result.conflicts.iter().all(|conflict| conflict.resolved));
    assert_eq!(result.observations.footer, "102 tst en");
}

#[test]
fn strong_title_conflicting_with_visual_identity_requires_resolution() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let image = card("b", "identity-b", "Violet Surveyor", "102");
    let result = run(
        vec![title.clone(), image.clone()],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        visual(&[(&image, 0.94), (&title, 0.65)], MatchStatus::Matched),
    );
    assert_eq!(result.identity.status, CombinedStatus::Conflict);
    assert!(result.identity.card.is_none());
    assert!(result.printing.card.is_none());
    assert!(!result.conflicts[0].resolved);
}

#[test]
fn verified_geometry_and_title_can_resolve_a_bounded_identity_conflict() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let global = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&global, 0.83), (&title, 0.65)], MatchStatus::Matched);
    image.recognition = recognition(&[(&title, 0.95), (&global, 0.83)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &title, 0.95);
    let result = run(
        vec![title.clone(), global],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::VerifiedShortlist);
    assert_eq!(
        result.identity.card.expect("scoped identity").oracle_id,
        title.oracle_id
    );
    assert_eq!(result.printing.status, MatchStatus::Ambiguous);
    assert!(result.printing.card.is_none());
    assert!(result.conflicts.iter().all(|conflict| conflict.resolved));
    assert!(!result.visual.search_complete);
    assert_eq!(result.visual.recognition.identity_confidence, 0.0);
}

#[test]
fn lower_verified_identity_cannot_override_stronger_unverified_global_identity() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let global = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&global, 0.95), (&title, 0.87)], MatchStatus::Matched);
    partial_geometry(&mut image, &title, 0.87);
    image.combined_identity_margin = 0.08;
    let result = run(
        vec![title.clone(), global],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Conflict);
    assert!(result.identity.card.is_none());
}

#[test]
fn hidden_global_competitor_margin_prevents_verified_only_acceptance() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let global = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&global, 0.90), (&title, 0.65)], MatchStatus::Matched);
    image.recognition = recognition(&[(&title, 0.91)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &title, 0.91);
    image.combined_identity_margin = 0.01;
    let result = run(
        vec![title.clone(), global],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Conflict);
    assert!(result.identity.card.is_none());
}

#[test]
fn geometry_changing_winner_prevents_retaining_stale_global_plus_title() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let other = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&title, 0.83), (&other, 0.65)], MatchStatus::Matched);
    image.recognition = recognition(&[(&other, 0.95), (&title, 0.83)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &other, 0.95);
    let result = run(
        vec![title.clone(), other],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Conflict);
    assert!(result.identity.card.is_none());
    assert!(result.printing.card.is_none());
    assert!(result.conflicts.iter().any(|conflict| {
        conflict.sources.contains(&EvidenceSource::VisualGeometry) && !conflict.resolved
    }));
}

#[test]
fn geometry_changing_printing_preserves_identity_but_not_stale_exact_printing() {
    let first = card("a", "identity-a", "Amber Clockmaker", "101");
    let second = card("b", "identity-a", "Amber Clockmaker", "102");
    let mut image = visual(&[(&first, 0.83), (&second, 0.65)], MatchStatus::Matched);
    image.recognition = recognition(&[(&second, 0.95), (&first, 0.83)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &second, 0.95);
    let result = run(
        vec![first, second],
        TextObservations {
            title: "Amber Clockmaker".into(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Matched);
    assert_eq!(result.printing.status, MatchStatus::Ambiguous);
    assert!(result.printing.card.is_none());
}

#[test]
fn close_verified_competitor_prevents_retaining_stale_global_margin() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let other = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&title, 0.94), (&other, 0.65)], MatchStatus::Matched);
    image.recognition = recognition(&[(&title, 0.94), (&other, 0.92)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &other, 0.92);
    image.combined_identity_margin = 0.02;
    let result = run(
        vec![title.clone(), other],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Ambiguous);
    assert!(result.identity.card.is_none());
}

#[test]
fn empty_ocr_preserves_complete_visual_acceptance() {
    let correct = card("a", "identity-a", "Amber Clockmaker", "101");
    let result = run(
        vec![correct.clone()],
        TextObservations::default(),
        visual(&[(&correct, 0.95)], MatchStatus::Matched),
    );
    assert_eq!(result.identity.status, CombinedStatus::Matched);
    assert_eq!(result.printing.status, MatchStatus::Matched);
    assert!(result.text.hints.is_empty());
    assert!(result.conflicts.is_empty());
}

#[test]
fn exact_text_and_footer_cannot_accept_without_image_evidence() {
    let correct = card("a", "identity-a", "Amber Clockmaker", "101");
    let result = run(
        vec![correct.clone()],
        TextObservations {
            title: correct.name.clone(),
            footer: "101 tst en".into(),
            ..TextObservations::default()
        },
        visual(&[(&correct, 0.65)], MatchStatus::Unknown),
    );
    assert_eq!(result.identity.status, CombinedStatus::Unknown);
    assert!(result.identity.card.is_none());
    assert_eq!(result.printing.status, MatchStatus::Unknown);
}

#[test]
fn tied_verified_identities_abstain_even_with_exact_title() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let other = card("b", "identity-b", "Violet Surveyor", "102");
    let mut image = visual(&[(&title, 0.65), (&other, 0.64)], MatchStatus::Unknown);
    image.recognition = recognition(&[(&title, 0.91), (&other, 0.91)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &title, 0.91);
    image.combined_identity_margin = 0.0;
    let result = run(
        vec![title.clone(), other],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Ambiguous);
    assert!(result.identity.card.is_none());
}

#[test]
fn exact_footer_never_upgrades_ambiguous_visual_printings() {
    let first = card("a", "identity-a", "Amber Clockmaker", "101");
    let second = card("b", "identity-a", "Amber Clockmaker", "102");
    let mut image = visual(&[(&first, 0.95), (&second, 0.95)], MatchStatus::Ambiguous);
    image.global_recognition.identity_status = MatchStatus::Matched;
    image.recognition.identity_status = MatchStatus::Matched;
    let result = run(
        vec![first, second],
        TextObservations {
            title: "Amber Clockmaker".into(),
            footer: "102 tst en".into(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Matched);
    assert_eq!(result.printing.status, MatchStatus::Ambiguous);
    assert!(result.printing.card.is_none());
    assert!(
        result
            .conflicts
            .iter()
            .any(|conflict| { conflict.kind == ConflictKind::Printing && !conflict.resolved })
    );
}

#[test]
fn omitted_identity_representatives_prevent_shortlist_acceptance() {
    let title = card("a", "identity-a", "Amber Clockmaker", "101");
    let mut image = visual(&[(&title, 0.65)], MatchStatus::Unknown);
    image.recognition = recognition(&[(&title, 0.95)], MatchStatus::Ambiguous);
    partial_geometry(&mut image, &title, 0.95);
    image.omitted_hint_groups = 1;
    let result = run(
        vec![title.clone()],
        TextObservations {
            title: title.name.clone(),
            ..TextObservations::default()
        },
        image,
    );
    assert_eq!(result.identity.status, CombinedStatus::Ambiguous);
    assert!(result.identity.card.is_none());
}

#[test]
fn semantic_grouping_requires_same_game_and_uses_printing_fallback() {
    let mut first = card("a", "identity-a", "Amber Clockmaker", "101");
    let mut second = first.clone();
    second.game = "another-game".into();
    assert!(!same_identity(&first, &second));
    second.game.clone_from(&first.game);
    first.oracle_id = None;
    assert!(!same_identity(&first, &second));
    second.oracle_id = None;
    second.face_index = 1;
    assert!(same_identity(&first, &second));
    assert!(!same_face(&first, &second));
    assert_eq!(
        CardIdentity::from_card(&first).printing_id.as_deref(),
        Some("a")
    );
}

#[test]
fn public_identify_reuses_local_indexes_and_rejects_uniform_input() {
    let directory = Directory::new();
    let correct = card("a", "identity-a", "Amber Clockmaker", "101");
    let wrong = card("b", "identity-b", "Violet Surveyor", "102");
    let image = portrait(1);
    let mut builder = ShardedIndexBuilder::new(&directory.0, 1).expect("local gallery");
    builder
        .add(correct.clone(), &image)
        .expect("procedural face");
    builder
        .add(wrong.clone(), &portrait(9))
        .expect("other face");
    let visual = builder.finish().expect("published gallery");
    let text = TextIndex::from_metadata([correct.clone(), wrong]).expect("invented text index");
    let observations = TextObservations {
        title: correct.name.clone(),
        footer: "102 tst en".into(),
        ..TextObservations::default()
    };
    let result = identify(
        &visual,
        &text,
        &image,
        &observations,
        &MatchOptions::default(),
        &ShortlistOptions::default(),
    )
    .expect("public identify runs without OCR models or networking");
    assert_eq!(result.identity.status, CombinedStatus::Matched);
    assert_eq!(
        result.identity.card.expect("accepted identity").oracle_id,
        correct.oracle_id
    );
    assert!(result.visual.geometry_attempted > 0);
    assert!(result.conflicts.iter().all(|conflict| conflict.resolved));
    let blank = DynamicImage::ImageRgb8(RgbImage::from_pixel(168, 234, Rgb([255; 3])));
    let blank_result = identify(
        &visual,
        &text,
        &blank,
        &TextObservations {
            title: observations.title,
            ..TextObservations::default()
        },
        &MatchOptions::default(),
        &ShortlistOptions::default(),
    )
    .expect("uniform input returns an abstention");
    assert_eq!(blank_result.identity.status, CombinedStatus::Unknown);
    assert_eq!(blank_result.visual.scope, ShortlistScope::NoVisualEvidence);
    assert!(blank_result.identity.card.is_none());
    assert!(blank_result.printing.card.is_none());
}

#[test]
fn title_first_identify_verifies_only_the_indexed_candidate_closure() {
    let directory = Directory::new();
    let correct = card("a", "identity-a", "Amber Clockmaker", "101");
    let wrong = card("b", "identity-b", "Violet Surveyor", "102");
    let image = portrait(1);
    let mut builder = ShardedIndexBuilder::new(&directory.0, 1).expect("local gallery");
    builder
        .add(correct.clone(), &image)
        .expect("procedural face");
    builder
        .add(wrong.clone(), &portrait(9))
        .expect("other face");
    let visual = builder.finish().expect("published gallery");
    let text = TextIndex::from_metadata([correct.clone(), wrong]).expect("invented text index");
    let observations = TextObservations {
        title: correct.name.clone(),
        ..TextObservations::default()
    };
    let result = identify_title(
        &visual,
        &text,
        &image,
        &observations,
        &MatchOptions::default(),
        3,
        16,
    )
    .expect("candidate-only identification");
    assert_eq!(result.visual.scope, ShortlistScope::SuppliedCandidates);
    assert!(!result.visual.search_complete);
    assert_eq!(result.visual.total_references, 2);
    assert_eq!(result.visual.candidate_count, 1);
    assert_eq!(result.visual.geometry_attempted, 1);
    assert_eq!(result.identity.status, CombinedStatus::VerifiedShortlist);
    assert_eq!(
        result.identity.card.expect("verified identity").oracle_id,
        correct.oracle_id
    );
    assert_eq!(result.printing.status, MatchStatus::Ambiguous);
    assert!(result.printing.card.is_none());
    assert!(result.identity.reason.contains("identities not proposed"));
}

#[test]
fn title_first_identify_does_not_fall_back_to_a_gallery_scan() {
    let directory = Directory::new();
    let correct = card("a", "identity-a", "Amber Clockmaker", "101");
    let image = portrait(1);
    let mut builder = ShardedIndexBuilder::new(&directory.0, 1).expect("local gallery");
    builder
        .add(correct.clone(), &image)
        .expect("procedural face");
    let visual = builder.finish().expect("published gallery");
    let text = TextIndex::from_metadata([correct]).expect("invented text index");
    let result = identify_title(
        &visual,
        &text,
        &image,
        &TextObservations::default(),
        &MatchOptions::default(),
        3,
        16,
    )
    .expect("empty OCR returns an immediate abstention");
    assert_eq!(result.identity.status, CombinedStatus::Unknown);
    assert!(result.visual.recognition.candidates.is_empty());
    assert_eq!(result.visual.candidate_count, 0);
    assert_eq!(result.visual.geometry_attempted, 0);
}

#[test]
fn candidate_geometry_resolves_multiple_exact_title_identities() {
    let directory = Directory::new();
    let first = card("a", "identity-a", "Shared Signal", "101");
    let second = card("b", "identity-b", "Shared Signal", "102");
    let image = portrait(7);
    let mut builder = ShardedIndexBuilder::new(&directory.0, 1).expect("local gallery");
    builder
        .add(first.clone(), &portrait(1))
        .expect("first identity");
    builder
        .add(second.clone(), &image)
        .expect("second identity");
    let visual = builder.finish().expect("published gallery");
    let text = TextIndex::from_metadata([first, second.clone()]).expect("invented text index");
    let result = identify_title(
        &visual,
        &text,
        &image,
        &TextObservations {
            title: "Shared Signal".into(),
            ..TextObservations::default()
        },
        &MatchOptions::default(),
        3,
        16,
    )
    .expect("candidate geometry disambiguates shared title");
    assert_eq!(result.text.title.len(), 2);
    assert_eq!(result.identity.status, CombinedStatus::Matched);
    assert_eq!(
        result.identity.card.expect("resolved identity").oracle_id,
        second.oracle_id
    );
    assert!(result.identity.sources.contains(&EvidenceSource::Title));
    assert!(
        result
            .identity
            .sources
            .contains(&EvidenceSource::VisualGeometry)
    );
}
