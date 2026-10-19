//! Independent text retrieval over observed titles, collector footers and rules.
//!
//! These scores rank catalogue entries; they do not replace visual acceptance or
//! establish a printing probability. English Oracle wording, printed wording,
//! reminder text and older terminology can differ. Missing text is not evidence
//! against a card, and a collector number without a set is not a unique key.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::CardMetadata;

/// Manually selected conventional portrait frame geometry.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameLayout {
    /// Conventional M15-style portrait frame with a dark information footer.
    Modern,
    /// Conventional older portrait frame, with collector information near the edge.
    Classic,
}

/// A proposed image region, rather than a detected text bounding box.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CardRegion {
    /// Stable field name consumed by OCR and the evidence view.
    pub key: String,
    /// Human-readable description of the selected field.
    pub label: String,
    /// Normalized `[left, top, right, bottom]` coordinates of an upright card.
    pub bounds: [f32; 4],
}

/// Return approximate portrait regions for a manually chosen layout.
///
/// Split, adventure, showcase, borderless and full-art layouts need their own
/// geometry. The artwork box here describes the selected template; the portable
/// matcher's fixed artwork descriptor uses its documented modern coordinates.
#[must_use]
pub fn regions(layout: FrameLayout) -> Vec<CardRegion> {
    let boxes = match layout {
        FrameLayout::Modern => [
            ("title", "Title", [0.07, 0.04, 0.80, 0.10]),
            ("artwork", "Artwork", [0.09, 0.16, 0.91, 0.57]),
            ("rules", "Rules text", [0.075, 0.60, 0.925, 0.90]),
            ("footer", "Collector footer", [0.035, 0.90, 0.96, 0.995]),
        ],
        FrameLayout::Classic => [
            ("title", "Title", [0.06, 0.05, 0.81, 0.12]),
            ("artwork", "Artwork", [0.09, 0.19, 0.91, 0.59]),
            ("rules", "Rules text", [0.08, 0.63, 0.92, 0.90]),
            ("footer", "Collector footer", [0.035, 0.92, 0.96, 0.995]),
        ],
    };
    boxes
        .into_iter()
        .map(|(key, label, bounds)| CardRegion {
            key: key.into(),
            label: label.into(),
            bounds,
        })
        .collect()
}

/// Text actually observed in an image, optionally corrected by a person.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TextObservations {
    /// OCR of the title bar, excluding the mana-cost region when possible.
    pub title: String,
    /// Raw footer OCR; exact set and collector tokens are searched together.
    pub footer: String,
    /// OCR of the body, which can contain reminder and flavour text as well.
    pub rules: String,
}

/// Independent text evidence for one candidate; no acceptance decision is implied.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TextMatch {
    /// Catalogue entry used for comparison, preserving identity and printing.
    pub card: CardMetadata,
    /// Best normalized edit similarity against known English/printed face titles.
    pub title_similarity: Option<f32>,
    /// Weighted fuzzy rules-token F1 against printed or Oracle rules wording.
    pub rules_similarity: Option<f32>,
    /// Fraction of weighted reference rules recovered in the observed body text.
    pub rules_coverage: Option<f32>,
    /// Whether both a set code and complete collector-number token occur exactly.
    pub footer_match: bool,
    /// Whether one recognizable footer language token agrees with this printing.
    pub language_match: Option<bool>,
    /// Retrieval priority in `[0, 1]`; this is not visual confidence or probability.
    pub score: f32,
    /// Concrete evidence and limitations supporting this entry's retrieval rank.
    pub reasons: Vec<String>,
}

/// Rank at most five catalogue printings using independent OCR evidence.
///
/// Titles tolerate substitutions, insertions and missing punctuation. Rules
/// require at least four words and twenty normalized characters; common boilerplate
/// such as “draw a card” cannot produce a rules hit. Longer common phrases can
/// still retrieve multiple cards. Rules tokens are weighted by their document
/// frequency across distinct card identities so repeated printings do not dominate.
/// A footer requires both the set and collector number. Scores never alter the
/// visual matcher's printing/identity acceptance decisions.
#[must_use]
pub fn search<'a>(
    cards: impl IntoIterator<Item = &'a CardMetadata>,
    observations: &TextObservations,
) -> Vec<TextMatch> {
    let cards: Vec<_> = cards.into_iter().collect();
    let title = normalize(&observations.title.chars().take(512).collect::<String>());
    let footer = tokens(&observations.footer.chars().take(2048).collect::<String>());
    let collector_numbers = collector_tokens(&observations.footer);
    let languages: Vec<_> = [
        "en", "de", "fr", "it", "es", "pt", "ja", "ko", "ru", "zhs", "zht",
    ]
    .into_iter()
    .filter(|code| footer.iter().any(|token| token == code))
    .collect();
    let rules = rule_tokens(&observations.rules.chars().take(16_384).collect::<String>());
    let weights = document_weights(&cards);
    let enough_rules = rules.len() >= 4 && rules.iter().map(String::len).sum::<usize>() >= 20;
    let mut ranked = Vec::new();
    for card in cards {
        let title_similarity = (!title.is_empty())
            .then(|| {
                [
                    Some(card.face_name.as_deref().unwrap_or(&card.name)),
                    card.printed_name.as_deref(),
                ]
                .into_iter()
                .flatten()
                .map(|name| edit_similarity(&title, &normalize(name)))
                .fold(0.0_f32, f32::max)
            })
            .filter(|score| *score >= 0.5);
        let rule_evidence = enough_rules
            .then(|| {
                [card.printed_text.as_deref(), card.oracle_text.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter_map(|text| compare_rules(&rules, &rule_tokens(text), &weights))
                    .max_by(|a, b| a.0.total_cmp(&b.0))
            })
            .flatten();
        let footer_match = card.set.as_ref().is_some_and(|set| {
            footer.contains(&normalize(set))
                && card.collector_number.as_ref().is_some_and(|number| {
                    let expected = normalize_number(&normalize_collector(number));
                    collector_numbers.contains(&expected)
                })
        });
        let title_score = title_similarity.unwrap_or(0.0) * 0.85;
        let rules_score = rule_evidence.map_or(0.0, |(score, _)| score * 0.75);
        let language_match = if languages.len() == 1 {
            card.language
                .as_deref()
                .map(|language| language == languages[0])
        } else {
            None
        };
        let score = if footer_match {
            match language_match {
                Some(true) => 1.0,
                None => 0.98,
                Some(false) => 0.90,
            }
        } else {
            (title_score.max(rules_score)
                + if title_score >= 0.60 && rules_score >= 0.40 {
                    0.05
                } else {
                    0.0
                })
            .min(0.95)
        };
        if score == 0.0 {
            continue;
        }
        let mut reasons = Vec::new();
        if footer_match {
            reasons.push(
                "Exact set code and collector-number tokens; verify the OCR against the image."
                    .into(),
            );
        }
        if let Some(matches) = language_match {
            reasons.push(
                if matches {
                    "Footer language token agrees with this printing."
                } else {
                    "Footer language token conflicts with this printing; inspect the OCR."
                }
                .into(),
            );
        }
        if title_similarity.is_some() {
            reasons.push(
                "Fuzzy title comparison, preserving other printings with the same name.".into(),
            );
        }
        if rule_evidence.is_some() {
            reasons.push("Fuzzy rules comparison against printed or Oracle text; this supports identity, not a unique printing.".into());
        }
        ranked.push(TextMatch {
            card: card.clone(),
            title_similarity,
            rules_similarity: rule_evidence.map(|(score, _)| score),
            rules_coverage: rule_evidence.map(|(_, coverage)| coverage),
            footer_match,
            language_match,
            score,
            reasons,
        });
    }
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.card.id.cmp(&b.card.id)));
    ranked.truncate(5);
    ranked
}

/// Normalize case and punctuation without assuming ASCII-only card names.
fn normalize(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

/// Split OCR tokens at punctuation, preserving complete numbers and suffixes.
fn tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .map(normalize)
        .filter(|token| !token.is_empty())
        .collect()
}

/// Remove mana symbols and parenthesized reminder text before rules comparison.
fn rule_tokens(text: &str) -> Vec<String> {
    let mut depth = 0_u32;
    let mut symbol = false;
    let mut visible = String::new();
    for character in text.chars().take(16_384) {
        match character {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            '{' => symbol = true,
            '}' => {
                symbol = false;
                visible.push(' ');
            }
            _ if depth == 0 && !symbol => visible.push(character),
            _ => {}
        }
    }
    tokens(&visible)
        .into_iter()
        .filter(|word| {
            !matches!(
                word.as_str(),
                "a" | "an" | "the" | "of" | "to" | "and" | "or"
            )
        })
        .take(256)
        .collect()
}

/// Preserve meaningful collector suffixes, including the promotional star.
fn normalize_collector(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric() || *character == '★')
        .collect()
}

/// Read collector numerators without treating denominators or copyright years as keys.
fn collector_tokens(text: &str) -> Vec<String> {
    let bounded: String = text.chars().take(2048).collect();
    let characters: Vec<_> = bounded.chars().collect();
    let compact: String = characters
        .iter()
        .enumerate()
        .filter_map(|(index, character)| {
            let adjacent_slash = characters[..index]
                .iter()
                .rev()
                .find(|value| !value.is_whitespace())
                == Some(&'/')
                || characters[index + 1..]
                    .iter()
                    .find(|value| !value.is_whitespace())
                    == Some(&'/');
            (!character.is_whitespace() || !adjacent_slash).then_some(*character)
        })
        .collect();
    let mut result = Vec::new();
    for line in compact.lines() {
        let lower = line.to_lowercase();
        for token in lower
            .split(|character: char| {
                !character.is_alphanumeric() && character != '★' && character != '/'
            })
            .filter(|token| !token.is_empty())
        {
            let numerator = token.split('/').next().unwrap_or_default();
            // Four-digit years are ambiguous without an explicit fraction or zero
            // padding. Leave that printing unresolved instead of using the year.
            if !token.contains('/')
                && numerator.len() == 4
                && numerator
                    .parse::<u32>()
                    .is_ok_and(|value| (1900..=2099).contains(&value))
            {
                continue;
            }
            if numerator.starts_with(|character: char| character.is_ascii_digit()) {
                result.push(normalize_number(&normalize_collector(numerator)));
            }
        }
    }
    result
}

/// Preserve numeric suffixes while removing insignificant collector zero padding.
fn normalize_number(token: &str) -> String {
    if token.starts_with(|character: char| character.is_ascii_digit()) {
        let trimmed = token.trim_start_matches('0');
        if trimmed.is_empty() {
            "0".into()
        } else {
            trimmed.into()
        }
    } else {
        token.into()
    }
}

/// Compute normalized Levenshtein similarity with a single dynamic-programming row.
fn edit_similarity(a: &str, b: &str) -> f32 {
    let a: Vec<_> = a.chars().take(1024).collect();
    let b: Vec<_> = b.chars().take(1024).collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (i, left) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(left != right));
            diagonal = above;
        }
    }
    1.0 - row[b.len()] as f32 / a.len().max(b.len()) as f32
}

/// Weight rules words by frequency across semantic identities, not printing count.
fn document_weights(cards: &[&CardMetadata]) -> HashMap<String, f32> {
    let mut documents: HashMap<String, HashSet<String>> = HashMap::new();
    for card in cards {
        let identity = format!(
            "{}:{}",
            card.game,
            card.oracle_id.as_deref().unwrap_or(&card.id)
        );
        let document = documents.entry(identity).or_default();
        for text in [card.printed_text.as_deref(), card.oracle_text.as_deref()]
            .into_iter()
            .flatten()
        {
            document.extend(rule_tokens(text));
        }
    }
    let count = documents.len() as f32;
    let mut frequencies = HashMap::<String, usize>::new();
    for words in documents.values() {
        for word in words {
            *frequencies.entry(word.clone()).or_default() += 1;
        }
    }
    frequencies
        .into_iter()
        .map(|(word, frequency)| (word, ((count + 1.0) / (frequency as f32 + 1.0)).ln() + 1.0))
        .collect()
}

/// Compare useful rules tokens with one-to-one fuzzy matches and weighted coverage.
fn compare_rules(
    query: &[String],
    reference: &[String],
    weights: &HashMap<String, f32>,
) -> Option<(f32, f32)> {
    if reference.is_empty() {
        return None;
    }
    let weight = |word: &str| weights.get(word).copied().unwrap_or(1.0);
    let mut used = HashSet::new();
    let mut matched_query = 0.0;
    let mut matched_reference = 0.0;
    for word in query {
        let best = reference
            .iter()
            .enumerate()
            .filter(|(index, _)| !used.contains(index))
            .map(|(index, candidate)| {
                let score = if word == candidate {
                    1.0
                } else if word.len() >= 4 && candidate.len() >= 4 {
                    edit_similarity(word, candidate)
                } else {
                    0.0
                };
                (index, score)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((index, similarity)) = best.filter(|(_, similarity)| *similarity >= 0.72) {
            used.insert(index);
            matched_query += weight(word) * similarity;
            matched_reference += weight(&reference[index]) * similarity;
        }
    }
    let precision = matched_query / query.iter().map(|word| weight(word)).sum::<f32>();
    let coverage = matched_reference / reference.iter().map(|word| weight(word)).sum::<f32>();
    let score = 2.0 * precision * coverage / (precision + coverage).max(f32::EPSILON);
    (score >= 0.30).then_some((score.clamp(0.0, 1.0), coverage.clamp(0.0, 1.0)))
}

/// Text searches exercise missing fields, real OCR errors, and printing ambiguity.
#[cfg(test)]
mod tests {
    use super::*;

    /// A known card with meaningful rules wording and explicit printing metadata.
    fn bolt(id: &str, set: &str, number: &str) -> CardMetadata {
        CardMetadata {
            game: "mtg".into(),
            id: id.into(),
            name: "Lightning Bolt".into(),
            oracle_id: Some("bolt-oracle".into()),
            set: Some(set.into()),
            collector_number: Some(number.into()),
            oracle_text: Some("Lightning Bolt deals 3 damage to any target.".into()),
            ..CardMetadata::default()
        }
    }

    /// A noisy title/rules read finds the identity while retaining both printings.
    #[test]
    fn fuzzy_title_and_rules_preserve_printing_ambiguity() {
        let cards = [bolt("modern", "m15", "127"), bolt("old", "lea", "161")];
        let hits = search(
            &cards,
            &TextObservations {
                title: "Lightnlng BoIt".into(),
                rules: "Lightnlng Bolt deals 3 darnage to any target".into(),
                ..TextObservations::default()
            },
        );
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .all(|hit| hit.title_similarity.unwrap_or(0.0) > 0.8)
        );
        assert!(
            hits.iter()
                .all(|hit| hit.rules_similarity.unwrap_or(0.0) > 0.7)
        );
        assert_eq!(hits[0].score, hits[1].score);
        assert!(hits.iter().all(|hit| !hit.footer_match));
    }

    /// A complete set/number pair disambiguates a footer; a lone number cannot.
    #[test]
    fn footer_requires_complete_exact_set_and_number_tokens() {
        let cards = [bolt("modern", "m15", "127"), bolt("old", "lea", "127")];
        let hits = search(
            &cards,
            &TextObservations {
                footer: "00127/269 C M15 • EN".into(),
                ..TextObservations::default()
            },
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].card.id, "modern");
        assert!(hits[0].footer_match);
        for footer in ["127/269", "M15 1127", "M15 127a", "M15 2024 ©"] {
            assert!(
                search(
                    &cards,
                    &TextObservations {
                        footer: footer.into(),
                        ..TextObservations::default()
                    }
                )
                .is_empty()
            );
        }
        let traps = [
            bolt("denominator", "m15", "269"),
            bolt("year", "m15", "2024"),
            bolt("star", "m15", "127★"),
        ];
        for footer in [
            "127/269 M15 © 2024 Wizards",
            "127 / 269 M15",
            "127/ 269 M15",
            "127 /269 M15",
            "M15 127",
            "M15 ©2024",
        ] {
            assert!(
                search(
                    &traps,
                    &TextObservations {
                        footer: footer.into(),
                        ..TextObservations::default()
                    }
                )
                .is_empty()
            );
        }
        let star = search(
            &traps,
            &TextObservations {
                footer: "M15 00127★".into(),
                ..TextObservations::default()
            },
        );
        assert_eq!(star.len(), 1);
        assert_eq!(star[0].card.id, "star");
        let legacy = [
            bolt("legacy", "mir", "135"),
            bolt("denominator", "mir", "350"),
        ];
        let hits = search(
            &legacy,
            &TextObservations {
                footer: "MIR ©1996 Wizards of the Coast 135 / 350".into(),
                ..TextObservations::default()
            },
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].card.id, "legacy");
    }

    /// Short boilerplate and blank evidence do not manufacture a rules identity.
    #[test]
    fn rejects_short_generic_rules_and_missing_observations() {
        let cards = [bolt("modern", "m15", "127")];
        for rules in ["", "Draw a card.", "Counter target spell."] {
            assert!(
                search(
                    &cards,
                    &TextObservations {
                        rules: rules.into(),
                        ..TextObservations::default()
                    }
                )
                .is_empty()
            );
        }
    }

    /// A readable language separates translated printings sharing a set and number.
    #[test]
    fn footer_language_distinguishes_translated_printings() {
        let mut english = bolt("english", "m15", "127");
        english.language = Some("en".into());
        let mut french = bolt("french", "m15", "127");
        french.language = Some("fr".into());
        let hits = search(
            &[english, french],
            &TextObservations {
                footer: "00127/269 M15 FR".into(),
                ..TextObservations::default()
            },
        );
        assert_eq!(hits[0].card.id, "french");
        assert_eq!(hits[0].language_match, Some(true));
        assert_eq!(hits[1].language_match, Some(false));
        assert!(hits[0].score > hits[1].score);
    }

    /// Localized printed text can be used even when Oracle wording is English.
    #[test]
    fn searches_printed_rules_and_keeps_missing_text_unscored() {
        let mut translated = bolt("translated", "m15", "127");
        translated.printed_name = Some("Éclair".into());
        translated.printed_text =
            Some("Éclair inflige 3 blessures à n'importe quelle cible.".into());
        let empty = CardMetadata {
            id: "missing".into(),
            name: "Other Card".into(),
            ..CardMetadata::default()
        };
        let cards = [translated, empty];
        let hits = search(
            &cards,
            &TextObservations {
                rules: "Éclair inflige 3 blessures à n'importe quelle cible".into(),
                ..TextObservations::default()
            },
        );
        assert_eq!(hits[0].card.id, "translated");
        assert!(hits[0].rules_similarity.unwrap_or(0.0) > 0.95);
        assert!(hits.iter().all(|hit| hit.card.id != "missing"));
    }
}
