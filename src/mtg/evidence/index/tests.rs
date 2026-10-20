//! Parity and persistence checks independent of downloaded catalogues or models.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};

use super::*;

/// An isolated directory removed after every test, including failures.
struct Scratch(PathBuf);

impl Scratch {
    /// Create a unique writable directory without external dependencies.
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("wtp-text-index-{}-{suffix}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Varied aliases, rules, language/collector distinctions and stable face ties.
fn catalogue() -> Vec<CardMetadata> {
    let base = CardMetadata {
        game: "mtg".into(),
        id: "z-bolt".into(),
        name: "Lightning Bolt".into(),
        oracle_id: Some("bolt-identity".into()),
        set: Some("m15".into()),
        collector_number: Some("127".into()),
        language: Some("en".into()),
        face_index: 0,
        face_name: None,
        illustration_id: Some("shared-art".into()),
        source: Some("https://example.invalid/bolt".into()),
        oracle_text: Some("Lightning Bolt deals 3 damage to any target.".into()),
        printed_text: None,
        printed_name: None,
        type_line: Some("Instant".into()),
        layout: Some("normal".into()),
    };
    let mut cards = vec![base.clone()];
    for index in 0..8 {
        let mut card = base.clone();
        card.id = format!("bolt-{index}");
        card.set = Some("lea".into());
        card.collector_number = Some("161".into());
        cards.push(card);
    }
    let mut french = base.clone();
    french.id = "a-french".into();
    french.language = Some("fr".into());
    french.printed_name = Some("Éclair".into());
    french.printed_text = Some("Éclair inflige trois blessures à une créature ciblée.".into());
    cards.push(french);
    let mut star = base.clone();
    star.id = "b-star".into();
    star.collector_number = Some("00127★".into());
    cards.push(star);
    let mut missing = CardMetadata {
        id: "missing".into(),
        name: "Missing Text".into(),
        ..CardMetadata::default()
    };
    missing.source = Some(String::new());
    cards.push(missing);
    cards.push(CardMetadata {
        game: "other".into(),
        id: "unknown-oracle".into(),
        name: "Counterspell".into(),
        oracle_text: Some("Counter target spell. Draw two replacement cards.".into()),
        ..CardMetadata::default()
    });
    let mut front = base.clone();
    front.id = "two-faces".into();
    front.name = "Front // Back".into();
    front.face_name = Some("Shared face name".into());
    front.oracle_id = Some("face-identity".into());
    let mut back = front.clone();
    back.face_index = 1;
    // Back precedes front: full ties must preserve source order, not face index.
    cards.extend([back, front]);
    for index in 0..12 {
        cards.push(CardMetadata {
            game: "mtg".into(), id: format!("rules-{index:02}"), name: format!("Distinct Creature {index}"),
            oracle_id: Some(format!("rules-identity-{}", index / 3)),
            oracle_text: Some(format!("Destroy target artifact. Return target creature to its owner's hand. Draw {index} cards.")),
            printed_text: Some("Destroy target artifact. (Reminder ignored) Return {U} target creature to its owner's hand.".into()),
            ..CardMetadata::default()
        });
    }
    cards
}

/// Compare complete public output, including all metadata, float scores and explanations.
fn assert_parity(index: &TextIndex, cards: &[CardMetadata], observations: &TextObservations) {
    let expected = super::super::search(cards, observations);
    let actual = index.search(observations);
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap(),
        "observations: {observations:?}"
    );
}

/// Lightweight and expanded title APIs must share every ranked identity decision.
fn assert_title_proposal_parity(index: &TextIndex, title: &str, limit: usize) {
    let proposals = index.propose_title_identities(title, limit);
    let expanded = index.search_title_identities(title, limit);
    assert_eq!(proposals.len(), expanded.len());
    for (proposal, expanded) in proposals.iter().zip(&expanded) {
        assert_eq!(proposal.representative, expanded.representative);
        assert_eq!(proposal.title_similarity, expanded.title_similarity);
        assert_eq!(proposal.exact, expanded.exact);
    }
}

/// Compare title ranking without requiring the compact index to retain unused metadata.
fn proposal_decisions(
    index: &dyn TitleProposer,
    title: &str,
    limit: usize,
) -> Vec<(String, Option<String>, String, u32, f32, bool)> {
    index
        .propose_title_identities(title, limit)
        .into_iter()
        .map(|proposal| {
            let card = proposal.representative;
            (
                card.game,
                card.oracle_id,
                card.id,
                card.face_index,
                proposal.title_similarity,
                proposal.exact,
            )
        })
        .collect()
}

/// Reference implementation of the original allocation-heavy fuzzy candidate path.
fn reference_fuzzy_title_aliases(index: &TextIndex, query: &[char]) -> Vec<(u32, f32)> {
    let query_grams = title_grams(query);
    if query_grams.is_empty() {
        return Vec::new();
    }
    let mut overlaps = HashMap::<u32, u16>::new();
    for gram in &query_grams {
        if let Some(postings) = index.title_grams.get(gram) {
            for &alias in postings {
                let overlap = overlaps.entry(alias).or_default();
                *overlap = overlap.saturating_add(1);
            }
        }
    }
    let query_count = query_grams.len();
    let mut candidates: Vec<_> = overlaps
        .into_iter()
        .filter(|(alias, _)| {
            let length = index.title_characters[*alias as usize].len();
            1. - query.len().abs_diff(length) as f32 / query.len().max(length) as f32
                >= MIN_TITLE_SIMILARITY
        })
        .collect();
    candidates.sort_unstable_by(|left, right| {
        compare_title_candidates(
            &index.title_characters,
            &index.title_gram_counts,
            query_count,
            query.len(),
            *left,
            *right,
        )
    });
    candidates.truncate(MAX_FUZZY_TITLE_ALIASES);
    let mut scored: Vec<_> = candidates
        .into_iter()
        .filter_map(|(alias, _)| {
            let score = bounded_similarity(
                query,
                &index.title_characters[alias as usize],
                MIN_TITLE_SIMILARITY,
            );
            (score >= MIN_TITLE_SIMILARITY).then_some((alias, score))
        })
        .collect();
    scored.sort_by(|(left, left_score), (right, right_score)| {
        right_score.total_cmp(left_score).then(left.cmp(right))
    });
    scored
}

/// Cached aliases/IDF preserve original matching on noisy and multilingual observations.
#[test]
fn cached_search_and_roundtrip_preserve_full_evidence_policy() {
    let cards = catalogue();
    let index = TextIndex::from_metadata(cards.clone()).unwrap();
    assert_eq!(index.len(), cards.len());
    let directory = Scratch::new();
    let path = directory.0.join("catalog.text");
    index.save(&path).unwrap();
    let loaded = TextIndex::load(&path).unwrap();
    let observations = [
        TextObservations::default(),
        TextObservations {
            title: "Lightnlng BoIt".into(),
            rules: "Lightnlng Bolt deals 3 darnage to any target".into(),
            ..Default::default()
        },
        TextObservations {
            title: "Éclair".into(),
            rules: "Éclair inflige trois blessures à une créature ciblée".into(),
            ..Default::default()
        },
        TextObservations {
            footer: "00127/269 C M15 FR".into(),
            ..Default::default()
        },
        TextObservations {
            footer: "M15 00127★ EN".into(),
            ..Default::default()
        },
        TextObservations {
            title: "Lightning Bolt".into(),
            footer: "M15 127 EN FR".into(),
            ..Default::default()
        },
        TextObservations {
            footer: "M15 ©2024 127a / 269".into(),
            ..Default::default()
        },
        TextObservations {
            rules: "Destroy targat artifect return targat creature to its owner's hand".into(),
            ..Default::default()
        },
        TextObservations {
            rules: "Novelwords target target target creature owner replacement cards".into(),
            ..Default::default()
        },
        TextObservations {
            title: "Shared face name".into(),
            rules: "Draw a card.".into(),
            ..Default::default()
        },
        TextObservations {
            title: "Missing Text".into(),
            ..Default::default()
        },
        TextObservations {
            title: "Counterspell".into(),
            rules: "Counter target spell. Draw two replacement cards.".into(),
            ..Default::default()
        },
        TextObservations {
            rules: "Counter target spell.".into(),
            ..Default::default()
        },
    ];
    for observation in &observations {
        assert_parity(&index, &cards, observation);
        assert_parity(&loaded, &cards, observation);
    }
    assert_eq!(
        loaded.search_title_identities("Lightnlng BoIt", 4),
        index.search_title_identities("Lightnlng BoIt", 4)
    );
    assert_eq!(
        serde_json::to_value(loaded.search_footer_printings("M15 127 FR", 8)).unwrap(),
        serde_json::to_value(index.search_footer_printings("M15 127 FR", 8)).unwrap()
    );
}

/// Title limits count semantic identities before any identity's reprints are expanded.
#[test]
fn exact_title_postings_group_reprints_before_applying_limit() {
    let mut cards = Vec::new();
    for printing in 0..20 {
        cards.push(CardMetadata {
            game: "mtg".into(),
            id: format!("many-{printing:02}"),
            name: "Shared Exact Title".into(),
            oracle_id: Some("many-reprints".into()),
            ..CardMetadata::default()
        });
    }
    cards.push(CardMetadata {
        game: "mtg".into(),
        id: "other-printing".into(),
        name: "Shared Exact Title".into(),
        oracle_id: Some("other-identity".into()),
        ..CardMetadata::default()
    });
    cards.push(CardMetadata {
        game: "other-game".into(),
        id: "other-game-printing".into(),
        name: "Shared Exact Title".into(),
        oracle_id: Some("many-reprints".into()),
        ..CardMetadata::default()
    });
    let index = TextIndex::from_metadata(cards).unwrap();

    let hits = index.search_title_identities("shared, EXACT title!", 3);
    let proposals = index.propose_title_identities("shared, EXACT title!", 3);

    assert_eq!(hits.len(), 3);
    assert_title_proposal_parity(&index, "shared, EXACT title!", 3);
    assert_eq!(proposals.len(), 3);
    assert!(hits.iter().all(|hit| hit.exact));
    assert!(hits.iter().all(|hit| hit.title_similarity == 1.0));
    assert_eq!(
        hits[0].representative.oracle_id.as_deref(),
        Some("many-reprints")
    );
    assert_eq!(hits[0].representative.game, "mtg");
    assert_eq!(hits[0].printings.len(), 20);
    assert_eq!(hits[0].printings[0].id, "many-00");
    assert_eq!(hits[0].printings[19].id, "many-19");
    assert_eq!(
        hits[1].representative.oracle_id.as_deref(),
        Some("other-identity")
    );
    assert_eq!(hits[1].printings.len(), 1);
    assert_eq!(hits[2].representative.game, "other-game");
    assert_eq!(hits[2].printings.len(), 1);
    let encoded = serde_json::to_string(&hits[0]).unwrap();
    let decoded: TitleIdentityMatch = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, hits[0]);
    let encoded = serde_json::to_value(&proposals[0]).unwrap();
    assert!(encoded.get("printings").is_none());
    let decoded: TitleIdentityProposal = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, proposals[0]);
    assert!(
        index
            .search_title_identities("Shared Exact Title", 0)
            .is_empty()
    );
}

/// The live index keeps canonical titles and ranking while dropping full text metadata.
#[test]
fn compact_title_index_matches_canonical_title_decisions_and_filters_localized_aliases() {
    let mut cards = catalogue();
    cards.push(CardMetadata {
        game: "mtg".into(),
        id: "english-alternate".into(),
        name: "Cecily, Haunted Mage".into(),
        oracle_id: Some("alternate-identity".into()),
        language: Some("en".into()),
        printed_name: Some("Eleven, the Mage".into()),
        ..CardMetadata::default()
    });
    let full = TextIndex::from_metadata(cards.clone()).unwrap();
    let compact = TitleIndex::from_metadata(cards.iter()).unwrap();

    for title in [
        "Lightning Bolt",
        "Lightnlng BoIt",
        "Shared face name",
        "Missing Text",
        "Eleven, the Mage",
    ] {
        assert_eq!(
            proposal_decisions(&compact, title, 8),
            proposal_decisions(&full, title, 8),
            "canonical query {title}"
        );
    }
    assert!(
        compact.len() < full.len(),
        "reprints should share a representative"
    );

    let bolt = compact
        .propose_title_identities("Lightning Bolt", 1)
        .remove(0);
    assert_eq!(bolt.representative.name, "Lightning Bolt");
    assert_eq!(
        bolt.representative.illustration_id.as_deref(),
        Some("shared-art")
    );
    assert!(bolt.representative.set.is_none());
    assert!(bolt.representative.collector_number.is_none());
    assert!(bolt.representative.language.is_none());
    assert!(bolt.representative.source.is_none());
    assert!(bolt.representative.oracle_text.is_none());
    assert!(bolt.representative.printed_text.is_none());
    assert!(bolt.representative.printed_name.is_none());
    assert!(bolt.representative.type_line.is_none());
    assert!(bolt.representative.layout.is_none());

    assert!(
        full.propose_title_identities("Éclair", 4)
            .iter()
            .any(|proposal| proposal.exact)
    );
    assert!(compact.propose_title_identities("Éclair", 4).is_empty());
    assert!(
        compact
            .propose_title_identities("Eleven, the Mage", 1)
            .first()
            .is_some_and(|proposal| proposal.exact)
    );
    let face = compact
        .propose_title_identities("Shared face name", 1)
        .remove(0);
    assert_eq!(face.representative.name, "Front // Back");
    assert_eq!(
        face.representative.face_name.as_deref(),
        Some("Shared face name")
    );
}

/// Visual metadata can be streamed directly into the live title index.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn compact_title_index_builds_from_visual_metadata() {
    use crate::matching::ShardedIndexBuilder;
    use image::{DynamicImage, Rgb, RgbImage};

    let directory = Scratch::new();
    let image = DynamicImage::ImageRgb8(RgbImage::from_fn(96, 136, |x, y| {
        Rgb([
            ((x * 17 + y * 3) % 256) as u8,
            ((x * 7 + y * 11) % 256) as u8,
            ((x * 5 + y * 13) % 256) as u8,
        ])
    }));
    let cards = [
        CardMetadata {
            game: "mtg".into(),
            id: "first-printing".into(),
            name: "Invented Beacon".into(),
            oracle_id: Some("beacon-identity".into()),
            illustration_id: Some("beacon-art".into()),
            oracle_text: Some("This must not enter the title index.".into()),
            ..CardMetadata::default()
        },
        CardMetadata {
            game: "mtg".into(),
            id: "other-printing".into(),
            name: "Invented Survey".into(),
            oracle_id: Some("survey-identity".into()),
            ..CardMetadata::default()
        },
    ];
    let mut builder = ShardedIndexBuilder::new(&directory.0, 1).unwrap();
    for card in &cards {
        builder.add(card.clone(), &image).unwrap();
    }
    let visual = builder.finish().unwrap();

    let compact = TitleIndex::from_visual_index(&visual).unwrap();

    assert_eq!(compact.len(), 2);
    assert_eq!(
        proposal_decisions(&compact, "Invented Beacon", 3),
        proposal_decisions(
            &TextIndex::from_metadata(cards).unwrap(),
            "Invented Beacon",
            3
        )
    );
    let proposal = compact
        .propose_title_identities("Invented Beacon", 1)
        .remove(0)
        .representative;
    assert_eq!(proposal.id, "first-printing");
    assert_eq!(proposal.oracle_id.as_deref(), Some("beacon-identity"));
    assert_eq!(proposal.illustration_id.as_deref(), Some("beacon-art"));
    assert!(proposal.oracle_text.is_none());

    let identified = crate::mtg::identify::identify_title(
        &visual,
        &compact,
        &image,
        &TextObservations {
            title: "Invented Beacon".into(),
            ..TextObservations::default()
        },
        &crate::MatchOptions::default(),
        3,
        8,
    )
    .unwrap();
    assert_eq!(identified.visual.candidate_count, 1);
    assert_eq!(
        identified
            .visual
            .recognition
            .candidates
            .first()
            .map(|candidate| candidate.card.id.as_str()),
        Some("first-printing")
    );
}

/// OCR substitutions use bounded gram proposals while retaining the intended identity.
#[test]
fn fuzzy_title_postings_recall_noisy_titles_without_reprint_pressure() {
    let mut cards = catalogue();
    for index in 0..700 {
        cards.push(CardMetadata {
            game: "mtg".into(),
            id: format!("decoy-{index:03}"),
            name: format!("Invented Library Specimen {index}"),
            oracle_id: Some(format!("decoy-identity-{index:03}")),
            ..CardMetadata::default()
        });
    }
    let index = TextIndex::from_metadata(cards).unwrap();

    let hits = index.search_title_identities("Lightnlng BoIt", 4);

    assert_title_proposal_parity(&index, "Lightnlng BoIt", 4);
    assert_title_proposal_parity(&index, "Missing Text", 4);
    assert_title_proposal_parity(&index, "", 4);
    assert_title_proposal_parity(&index, "Lightning Bolt", 0);
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0].representative.oracle_id.as_deref(),
        Some("bolt-identity")
    );
    assert!(!hits[0].exact);
    assert!(hits[0].title_similarity > 0.8);
    assert_eq!(hits[0].printings.len(), 11);
    assert!(
        index
            .fuzzy_title_aliases(&"LightnlngBoIt".chars().collect::<Vec<_>>())
            .len()
            <= MAX_FUZZY_TITLE_ALIASES
    );
    assert!(index.search_title_identities("", 4).is_empty());
}

/// Dense overlap counts, partial selection and row reuse preserve the original ranking.
#[test]
fn optimized_fuzzy_title_path_matches_reference_scores_and_order() {
    let mut cards = catalogue();
    for index in 0..900 {
        cards.push(CardMetadata {
            game: "mtg".into(),
            id: format!("generated-{index:03}"),
            name: format!("Invented Library Specimen {index}"),
            oracle_id: Some(format!("generated-identity-{index:03}")),
            ..CardMetadata::default()
        });
    }
    let index = TextIndex::from_metadata(cards).unwrap();
    let queries = [
        "LightnlngBoIt",
        "InventedLibrerySpecimen511",
        "InventedLibrarySpecimen",
        "Éclalr",
        "SharedFaceNarne",
        "DlstinctCreature7",
        "Counterspel",
        "NothingRelevant",
        "X",
    ];
    for query in queries {
        let query: Vec<_> = normalize(query).chars().collect();
        assert_eq!(
            index.fuzzy_title_aliases(&query),
            reference_fuzzy_title_aliases(&index, &query),
            "query {query:?}"
        );
    }
}

/// Set/collector postings preserve meaningful suffixes and rank an observed language.
#[test]
fn footer_postings_only_materialize_exact_pairs() {
    let index = TextIndex::from_metadata(catalogue()).unwrap();

    let hits = index.search_footer_printings("00127 / 269  M15  FR", 20);
    let generic = index.search(&TextObservations {
        footer: "00127 / 269  M15  FR".into(),
        ..Default::default()
    });

    assert_eq!(hits.len(), 4);
    assert_eq!(
        serde_json::to_value(&hits).unwrap(),
        serde_json::to_value(&generic).unwrap()
    );
    assert_eq!(hits[0].card.id, "a-french");
    assert_eq!(hits[0].language_match, Some(true));
    assert!(hits.iter().all(|hit| hit.footer_match));
    assert!(
        hits.iter()
            .all(|hit| hit.card.set.as_deref() == Some("m15"))
    );
    assert!(
        hits.iter()
            .all(|hit| hit.card.collector_number.as_deref() == Some("127"))
    );
    assert!(
        index
            .search_footer_printings("M15 00127★ EN", 20)
            .iter()
            .all(|hit| hit.card.id == "b-star")
    );
    assert!(index.search_footer_printings("M15 269 EN", 20).is_empty());
    assert!(index.search_footer_printings("M15 127 EN", 0).is_empty());
}

/// Repeated words, Unicode and equal-score greedy matches retain numerical parity.
#[test]
fn cached_rules_match_original_token_matching() {
    let cases = [
        (
            "target target creature owner hand",
            "target targat creature owner's hand hand",
        ),
        (
            "éclair créature ciblée blessures supplémentaires",
            "Éclair inflige blessures à créature ciblée",
        ),
        (
            "card cards discard discarded",
            "cards card discards discard",
        ),
        (
            "龍龍 龍龍龙 créatures créature",
            "龍龍龙 龍龍 creature créature",
        ),
        (
            "newtoken unknownword artifact creature",
            "artifact creature target opponent hand",
        ),
    ];
    for (observed, reference) in cases {
        let cards = vec![CardMetadata {
            id: "one".into(),
            name: "Rule Test".into(),
            oracle_text: Some(reference.into()),
            printed_text: Some(format!("{reference} (reminder text)")),
            ..CardMetadata::default()
        }];
        let index = TextIndex::from_metadata(cards.clone()).unwrap();
        assert_parity(
            &index,
            &cards,
            &TextObservations {
                rules: observed.into(),
                ..Default::default()
            },
        );
    }
}

/// Title/reference truncation and Unicode lowercasing agree at original scorer bounds.
#[test]
fn long_unicode_titles_follow_original_character_limits() {
    let title = "İÉ龍a".repeat(350);
    let cards = vec![CardMetadata {
        id: "long".into(),
        name: title.clone(),
        printed_name: Some("İÉ龍".repeat(400)),
        ..CardMetadata::default()
    }];
    let index = TextIndex::from_metadata(cards.clone()).unwrap();
    assert_parity(
        &index,
        &cards,
        &TextObservations {
            title,
            ..Default::default()
        },
    );
}

/// An empty handle works, while duplicate printing/face keys cannot silently disappear.
#[test]
fn empty_and_duplicate_catalogue_contracts() {
    let empty = TextIndex::from_metadata([]).unwrap();
    let compact = TitleIndex::from_metadata(std::iter::empty::<CardMetadata>()).unwrap();
    assert!(empty.is_empty());
    assert!(compact.is_empty());
    assert!(
        empty
            .search(&TextObservations {
                title: "Anything".into(),
                ..Default::default()
            })
            .is_empty()
    );
    let cards = catalogue();
    assert!(TextIndex::from_metadata([cards[0].clone(), cards[0].clone()]).is_err());
    assert!(TitleIndex::from_metadata([cards[0].clone(), cards[0].clone()]).is_err());
}

/// Write a modified uncompressed payload with a fresh CRC to test structural validation.
fn write_payload(path: &Path, payload: &[u8]) {
    let mut writer = GzEncoder::new(fs::File::create(path).unwrap(), Compression::fast());
    writer.write_all(payload).unwrap();
    writer.finish().unwrap();
}

/// CRC, version, UTF-8, bounds, nonfinite weights and trailing/truncated data fail closed.
#[test]
fn loader_rejects_corrupt_and_inconsistent_indexes() {
    let directory = Scratch::new();
    let path = directory.0.join("catalog.text");
    let cards = catalogue();
    let valid = TextIndex::from_metadata(cards.clone()).unwrap();
    valid.save(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 5]).unwrap();
    assert!(TextIndex::load(&path).is_err());
    let mut damaged = bytes.clone();
    let end = damaged.len();
    damaged[end - 8] ^= 1;
    fs::write(&path, damaged).unwrap();
    assert!(TextIndex::load(&path).is_err());
    let mut payload = Vec::new();
    GzDecoder::new(&bytes[..])
        .read_to_end(&mut payload)
        .unwrap();
    let mut version = payload.clone();
    version[8..12].copy_from_slice(&99_u32.to_le_bytes());
    write_payload(&path, &version);
    assert!(TextIndex::load(&path).is_err());
    let mut trailing = payload.clone();
    trailing.push(0);
    write_payload(&path, &trailing);
    assert!(TextIndex::load(&path).is_err());
    // First interned string begins after magic/version/count/length.
    let mut utf8 = payload;
    utf8[20] = 255;
    write_payload(&path, &utf8);
    assert!(TextIndex::load(&path).is_err());
    for corruption in 0..5 {
        let mut index = TextIndex::from_metadata(cards.clone()).unwrap();
        match corruption {
            0 => index.records[0].metadata[1] = ABSENT,
            1 => index.records[0].titles[0] = ABSENT - 1,
            2 => index.rules[0][0] = ABSENT,
            3 => index.weights[0] = f32::NAN,
            _ => index.records.push(index.records[0].clone()),
        }
        index.save(&path).unwrap();
        assert!(TextIndex::load(&path).is_err(), "corruption {corruption}");
    }
}

/// Failed publication leaves its destination and cleans the abandoned staged file.
#[test]
fn failed_publication_cleans_staging_without_replacing_destination() {
    let directory = Scratch::new();
    let destination = directory.0.join("existing-directory");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("preserved"), b"original").unwrap();
    let index = TextIndex::from_metadata(catalogue()).unwrap();
    assert!(index.save(&destination).is_err());
    assert_eq!(
        fs::read(destination.join("preserved")).unwrap(),
        b"original"
    );
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

/// The catalogue builder streams physical faces without loading any declared images.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn streams_catalogue_and_preserves_missing_local_images() {
    let directory = Scratch::new();
    let catalog = directory.0.join("cards.jsonl");
    fs::write(&catalog, "{\"id\":\"one\",\"name\":\"Example Card\",\"file\":\"absent.png\"}\n{\"id\":\"two\",\"name\":\"Éclair\",\"lang\":\"fr\"}\n").unwrap();
    let index = TextIndex::build(&catalog).unwrap();
    assert_eq!(index.len(), 2);
    let hits = index.search(&TextObservations {
        title: "Example Card".into(),
        ..Default::default()
    });
    assert_eq!(hits[0].card.id, "one");
    assert!(!directory.0.join("absent.png").exists());
}
