//! Reusable, lossless metadata retrieval with shared title and rules computations.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{CardMetadata, Error};

#[cfg(not(target_arch = "wasm32"))]
use crate::matching::ShardedIndex;

use super::{
    TextMatch, TextObservations, collector_tokens, edit_similarity_characters, footer_languages,
    footer_lookup_text, make_match, match_score, normalize, normalize_collector, normalize_number,
    rule_tokens, tokens,
};

mod codec;

#[cfg(test)]
mod tests;

/// Sentinel for an absent optional string or derived text field.
const ABSENT: u32 = u32::MAX;
/// Maximum title aliases subjected to exact edit-distance scoring for one fuzzy query.
const MAX_FUZZY_TITLE_ALIASES: usize = 512;
/// Minimum edit similarity retained by the OCR-first title proposal path.
const MIN_TITLE_SIMILARITY: f32 = 0.5;
/// Marker used only for the one-character form of a transient title gram.
const SINGLE_CHARACTER_GRAM: char = '\0';

/// Lightweight semantic identity proposed by an indexed OCR title lookup.
///
/// This form avoids expanding every physical printing. The representative carries
/// the game-scoped Oracle identity, or the printing fallback when Oracle identity
/// is absent, and can be passed directly to an indexed visual candidate lookup.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TitleIdentityProposal {
    /// Source-order catalogue face whose alias produced this identity's best score.
    pub representative: CardMetadata,
    /// Normalized edit similarity of the best title alias in `[0, 1]`.
    pub title_similarity: f32,
    /// Whether the normalized observed title exactly matched the winning alias.
    pub exact: bool,
}

/// One semantic identity proposed by an exact or bounded fuzzy title lookup.
///
/// The result is deliberately grouped before `limit` is applied. A card with many
/// reprints therefore occupies one result slot, while `printings` supplies every
/// physical face that a later visual stage may choose to verify.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TitleIdentityMatch {
    /// Source-order catalogue face whose alias produced this identity's best score.
    pub representative: CardMetadata,
    /// Every indexed printing/physical face in the same game-scoped identity.
    pub printings: Vec<CardMetadata>,
    /// Normalized edit similarity of the best title alias in `[0, 1]`.
    pub title_similarity: f32,
    /// Whether the normalized observed title exactly matched the winning alias.
    pub exact: bool,
}

/// Bounded title-proposal source for candidate-scoped visual identification.
///
/// Implementations return one compact representative per semantic identity.
/// The representative needs only the game, printing/face key, Oracle and
/// illustration identifiers, and display name used by the visual index.
pub trait TitleProposer {
    /// Propose up to `limit` semantic identities for one observed title.
    #[must_use]
    fn propose_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<TitleIdentityProposal>;
}

/// One title representative containing only fields used by live identification.
#[derive(Clone, Copy, Debug)]
struct CompactTitleRecord {
    /// Interned game namespace.
    game: u32,
    /// Interned physical printing identifier.
    printing: u32,
    /// Physical face within the printing.
    face_index: u32,
    /// Interned Oracle identity, or [`ABSENT`].
    oracle: u32,
    /// Interned illustration identity, or [`ABSENT`].
    illustration: u32,
    /// Interned complete canonical card name.
    name: u32,
    /// Interned canonical face name, or [`ABSENT`].
    face_name: u32,
}

/// Compact English title lookup built directly from a visual catalogue.
///
/// This index retains one small representative for each title/identity pair.
/// It deliberately omits rules, footers, non-English printed aliases, provenance,
/// set data, and other printing metadata. English alternate printed titles remain
/// searchable because they can be visible to OCR. The resulting proposals are suitable
/// for [`crate::matching::ShardedIndex::recognize_candidates_with`], which
/// expands their identity and illustration postings without scanning the
/// complete gallery.
#[derive(Debug)]
pub struct TitleIndex {
    /// Strings needed by aliases and compact representatives.
    strings: Vec<String>,
    /// One source-order representative for each distinct alias/identity pair.
    representatives: Vec<CompactTitleRecord>,
    /// Cached normalized title characters.
    title_characters: Vec<Vec<char>>,
    /// Normalized title spelling to its alias ID.
    title_ids: HashMap<String, u32>,
    /// Character-bigram to title-alias postings.
    title_grams: HashMap<[char; 2], Vec<u32>>,
    /// Number of distinct grams in each title alias.
    title_gram_counts: Vec<u16>,
    /// Title alias to distinct semantic identities and their representatives.
    title_identities: Vec<Vec<(u32, u32)>>,
}

/// All metadata fields in declaration order, with face index stored directly at slot seven.
#[derive(Clone, Debug)]
struct Record {
    /// Interned metadata strings; optional values use [`ABSENT`].
    metadata: [u32; 16],
    /// English/face and localized normalized title aliases.
    titles: [u32; 2],
    /// Printed and Oracle token sequences, preserving last-on-tie semantics.
    rules: [u32; 2],
    /// Normalized set and collector-number string keys.
    footer: [u32; 2],
}

/// A reusable text catalogue preserving every printing, language and physical face.
///
/// Shared strings, normalized title aliases, rules token sequences and semantic
/// document frequencies are prepared once. Queries use exactly the scoring,
/// explanations and five-printing tie order of [`super::search`]; scores remain
/// retrieval priorities, not probabilities or visual acceptance decisions.
/// Only returned printings are expanded into owned [`CardMetadata`] values.
#[derive(Debug)]
pub struct TextIndex {
    /// Shared original strings and normalized text keys.
    strings: Vec<String>,
    /// Printing/face metadata in original catalogue order.
    records: Vec<Record>,
    /// Normalized title string IDs, shared across printings and languages.
    titles: Vec<u32>,
    /// Unique ordered rules token sequences expressed as vocabulary IDs.
    rules: Vec<Vec<u32>>,
    /// Normalized rules vocabulary string IDs.
    words: Vec<u32>,
    /// Precomputed semantic-document IDF per vocabulary entry.
    weights: Vec<f32>,
    /// Cached title characters, truncated exactly like the original scorer.
    title_characters: Vec<Vec<char>>,
    /// Cached vocabulary characters for fuzzy token comparisons.
    word_characters: Vec<Vec<char>>,
    /// Vocabulary lookup for observed-word weights.
    word_ids: HashMap<String, u32>,
    /// Rule-sequence postings used to exclude references with no possible token match.
    word_rules: Vec<Vec<u32>>,
    /// Normalized title spelling to its compact alias ID.
    title_ids: HashMap<String, u32>,
    /// Character-bigram to title-alias postings for bounded fuzzy generation.
    title_grams: HashMap<[char; 2], Vec<u32>>,
    /// Number of distinct grams in each title alias.
    title_gram_counts: Vec<u16>,
    /// Title alias to distinct semantic identities and matching representative records.
    title_identities: Vec<Vec<(u32, u32)>>,
    /// Source-order physical faces belonging to each semantic identity.
    identity_records: Vec<Vec<u32>>,
    /// Normalized set spelling to its interned string ID.
    footer_sets: HashMap<String, u32>,
    /// Normalized collector spelling to its interned string ID.
    footer_numbers: HashMap<String, u32>,
    /// Exact normalized `(set, collector)` pair to source-order physical faces.
    footer_records: HashMap<(u32, u32), Vec<u32>>,
}

impl TextIndex {
    /// Stream a supported local catalogue into a reusable metadata-only index.
    ///
    /// No reference images or network access are required. All languages and
    /// physical faces supplied by the catalogue remain searchable.
    ///
    /// # Errors
    ///
    /// Returns errors for invalid/incomplete catalogue data, duplicate printing
    /// faces, unavailable input or text/index counts exceeding the format limits.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn build(catalog: &Path) -> Result<Self, Error> {
        let mut builder = Builder::default();
        crate::mtg::for_each_reference(catalog, |reference| builder.push(reference.card))?;
        builder.finish()
    }

    /// Build the same reusable representation from already imported metadata.
    ///
    /// Input order is retained for tied physical faces. Empty input is valid and
    /// produces no candidates; duplicate printing/face keys are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate printing faces or unrepresentable counts.
    pub fn from_metadata(cards: impl IntoIterator<Item = CardMetadata>) -> Result<Self, Error> {
        let mut builder = Builder::default();
        for card in cards {
            builder.push(card)?;
        }
        builder.finish()
    }

    /// Load a versioned compact index and validate it before permitting searches.
    ///
    /// # Errors
    ///
    /// Returns errors for unsupported formats, invalid references/weights,
    /// duplicate physical faces, truncated or corrupt data, or unavailable files.
    pub fn load(path: &Path) -> Result<Self, Error> {
        codec::load(path)
    }

    /// Atomically save the lossless compact index with a version and gzip CRC.
    ///
    /// The original strings, metadata, normalized aliases, rule sequences and
    /// IDF are persisted. Transient character caches/postings are reconstructed
    /// at load; a query never reads the original Scryfall JSON or images.
    ///
    /// # Errors
    ///
    /// Returns errors for unavailable destinations, unrepresentable data, failed
    /// writes or failed publication. A failed write preserves an existing index.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        codec::save(self, path)
    }

    /// Number of retained printing/physical-face records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether this catalogue contains no printing/physical-face records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Rank up to five printings using exactly the original text evidence policy.
    ///
    /// Each distinct title alias is compared once, rather than once per printing.
    /// Rules use cached semantic IDF and compare each vocabulary pair/unique
    /// reference sequence once. Neither spelling nor observed OCR is replaced.
    #[must_use]
    pub fn search(&self, observations: &TextObservations) -> Vec<TextMatch> {
        let title = normalize(&observations.title.chars().take(512).collect::<String>());
        let title: Vec<_> = title.chars().take(1024).collect();
        let footer: HashSet<_> = tokens(&footer_lookup_text(&observations.footer))
            .into_iter()
            .collect();
        let collector: HashSet<_> = collector_tokens(&observations.footer, |token| {
            self.footer_sets.contains_key(token)
        })
        .into_iter()
        .collect();
        let languages = footer_languages(footer.iter().map(String::as_str));
        let query_rules = rule_tokens(&observations.rules.chars().take(16_384).collect::<String>());
        let enough_rules =
            query_rules.len() >= 4 && query_rules.iter().map(String::len).sum::<usize>() >= 20;
        if title.is_empty() && !enough_rules && (footer.is_empty() || collector.is_empty()) {
            return Vec::new();
        }
        let title_scores: Vec<_> = if title.is_empty() {
            Vec::new()
        } else {
            self.title_characters
                .iter()
                .map(|alias| bounded_similarity(&title, alias, 0.5))
                .collect()
        };
        let rule_scores = if enough_rules {
            self.rule_scores(&query_rules)
        } else {
            Vec::new()
        };
        let mut best = Vec::<Candidate>::with_capacity(6);
        for (index, record) in self.records.iter().enumerate() {
            let title_similarity = (!title.is_empty())
                .then(|| {
                    record
                        .titles
                        .iter()
                        .filter(|&&id| id != ABSENT)
                        .map(|&id| title_scores[id as usize])
                        .fold(0., f32::max)
                })
                .filter(|score| *score >= 0.5);
            let rule_evidence = enough_rules
                .then(|| {
                    record
                        .rules
                        .iter()
                        .filter(|&&id| id != ABSENT)
                        .filter_map(|&id| rule_scores[id as usize])
                        .max_by(|a, b| a.0.total_cmp(&b.0))
                })
                .flatten();
            let footer_match = record.footer[0] != ABSENT
                && record.footer[1] != ABSENT
                && footer.contains(self.string(record.footer[0]))
                && collector.contains(self.string(record.footer[1]));
            let language_match = if languages.len() == 1 && record.metadata[6] != ABSENT {
                Some(self.string(record.metadata[6]) == languages[0])
            } else {
                None
            };
            let score = match_score(
                title_similarity,
                rule_evidence,
                footer_match,
                language_match,
            );
            if score == 0. {
                continue;
            }
            let candidate = Candidate {
                index,
                score,
                title_similarity,
                rule_evidence,
                footer_match,
                language_match,
            };
            // At most six small candidates are sorted. The final source ordinal
            // reproduces stable sorting of equal score/printing IDs (two faces).
            best.push(candidate);
            best.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then(
                        self.string(self.records[a.index].metadata[1])
                            .cmp(self.string(self.records[b.index].metadata[1])),
                    )
                    .then(a.index.cmp(&b.index))
            });
            best.truncate(5);
        }
        best.into_iter()
            .map(|candidate| {
                make_match(
                    &self.card(&self.records[candidate.index]),
                    candidate.title_similarity,
                    candidate.rule_evidence,
                    candidate.footer_match,
                    candidate.language_match,
                )
            })
            .collect()
    }

    /// Propose semantic identities from one OCR title without scanning the catalogue.
    ///
    /// An exact normalized title uses a direct posting lookup. Otherwise character
    /// bigrams generate candidates and at most 512 aliases receive the full edit
    /// similarity calculation. Results below the existing `0.5` title threshold
    /// are discarded. `limit` counts semantic identities rather than printings;
    /// each retained identity then expands to all of its indexed physical faces.
    ///
    /// This is a fast proposal API for a later visual verifier. Similarity is OCR
    /// retrieval evidence, not an acceptance probability, and a missing result does
    /// not prove that the observed card is absent from the catalogue.
    #[must_use]
    pub fn search_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<TitleIdentityMatch> {
        self.rank_title_identities(observed_title, limit)
            .into_iter()
            .map(|candidate| TitleIdentityMatch {
                representative: self.card(&self.records[candidate.representative as usize]),
                printings: self.identity_records[candidate.identity as usize]
                    .iter()
                    .map(|&record| self.card(&self.records[record as usize]))
                    .collect(),
                title_similarity: candidate.title_similarity,
                exact: candidate.exact,
            })
            .collect()
    }

    /// Propose semantic identities without expanding their physical printings.
    ///
    /// Exact and fuzzy generation, scoring, identity grouping, ordering and limits
    /// are identical to [`Self::search_title_identities`]. Only the source-order
    /// representative for each retained identity is materialized, making this the
    /// preferred input for a visual index that can expand identity postings itself.
    #[must_use]
    pub fn propose_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<TitleIdentityProposal> {
        self.rank_title_identities(observed_title, limit)
            .into_iter()
            .map(|candidate| TitleIdentityProposal {
                representative: self.card(&self.records[candidate.representative as usize]),
                title_similarity: candidate.title_similarity,
                exact: candidate.exact,
            })
            .collect()
    }

    /// Retrieve exact physical-printing proposals from an OCR collector footer.
    ///
    /// The footer must contain both a known normalized set code and a complete
    /// collector-number token. Lookup is performed through transient pair postings;
    /// only matching records are materialized. When exactly one supported language
    /// token is present, matching-language records rank first while conflicting
    /// records remain visible. `limit` counts physical printing/face records.
    /// Set and collector fields must be separate tokens; [`super::FooterIndex`]
    /// supports additional glued fields and documented printed-number aliases.
    #[must_use]
    pub fn search_footer_printings(&self, observed_footer: &str, limit: usize) -> Vec<TextMatch> {
        if limit == 0 {
            return Vec::new();
        }
        let footer: HashSet<_> = tokens(&footer_lookup_text(observed_footer))
            .into_iter()
            .collect();
        let collectors: HashSet<_> = collector_tokens(observed_footer, |token| {
            self.footer_sets.contains_key(token)
        })
        .into_iter()
        .collect();
        if footer.is_empty() || collectors.is_empty() {
            return Vec::new();
        }
        let languages = footer_languages(footer.iter().map(String::as_str));
        let sets: Vec<_> = footer
            .iter()
            .filter_map(|set| self.footer_sets.get(set).copied())
            .collect();
        let numbers: Vec<_> = collectors
            .iter()
            .filter_map(|number| self.footer_numbers.get(number).copied())
            .collect();
        let mut seen = HashSet::new();
        let mut records = Vec::new();
        for set in sets {
            for &number in &numbers {
                if let Some(postings) = self.footer_records.get(&(set, number)) {
                    for &record in postings {
                        if seen.insert(record) {
                            records.push(record);
                        }
                    }
                }
            }
        }
        records.sort_unstable();
        let mut matches: Vec<_> = records
            .into_iter()
            .map(|index| {
                let record = &self.records[index as usize];
                let language_match = if languages.len() == 1 && record.metadata[6] != ABSENT {
                    Some(self.string(record.metadata[6]) == languages[0])
                } else {
                    None
                };
                (
                    index,
                    make_match(&self.card(record), None, None, true, language_match),
                )
            })
            .collect();
        matches.sort_by(|(left_index, left), (right_index, right)| {
            right
                .score
                .total_cmp(&left.score)
                .then(left.card.id.cmp(&right.card.id))
                .then(left_index.cmp(right_index))
        });
        matches.truncate(limit);
        matches
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect()
    }

    /// Generate and exactly score a bounded set of fuzzy title aliases.
    fn fuzzy_title_aliases(&self, query: &[char]) -> Vec<(u32, f32)> {
        let query_grams = title_grams(query);
        if query_grams.is_empty() {
            return Vec::new();
        }
        let mut overlaps = vec![0_u16; self.titles.len()];
        let capacity = query_grams
            .iter()
            .filter_map(|gram| self.title_grams.get(gram))
            .fold(0_usize, |sum, postings| sum.saturating_add(postings.len()))
            .min(self.titles.len());
        let mut candidates = Vec::with_capacity(capacity);
        for gram in &query_grams {
            if let Some(postings) = self.title_grams.get(gram) {
                for &alias in postings {
                    let overlap = &mut overlaps[alias as usize];
                    if *overlap == 0 {
                        candidates.push(alias);
                    }
                    *overlap = overlap.saturating_add(1);
                }
            }
        }
        let query_count = query_grams.len();
        candidates.retain(|alias| {
            let length = self.title_characters[*alias as usize].len();
            1. - query.len().abs_diff(length) as f32 / query.len().max(length) as f32
                >= MIN_TITLE_SIMILARITY
        });
        if candidates.len() > MAX_FUZZY_TITLE_ALIASES {
            candidates.select_nth_unstable_by(MAX_FUZZY_TITLE_ALIASES, |left, right| {
                compare_title_candidates(
                    &self.title_characters,
                    &self.title_gram_counts,
                    query_count,
                    query.len(),
                    (*left, overlaps[*left as usize]),
                    (*right, overlaps[*right as usize]),
                )
            });
            candidates.truncate(MAX_FUZZY_TITLE_ALIASES);
        }
        let mut edit_row = Vec::new();
        let mut scored: Vec<_> = candidates
            .into_iter()
            .filter_map(|alias| {
                let score = bounded_similarity_with_row(
                    query,
                    &self.title_characters[alias as usize],
                    MIN_TITLE_SIMILARITY,
                    &mut edit_row,
                );
                (score >= MIN_TITLE_SIMILARITY).then_some((alias, score))
            })
            .collect();
        scored.sort_by(|(left, left_score), (right, right_score)| {
            right_score.total_cmp(left_score).then(left.cmp(right))
        });
        scored
    }

    /// Select exact or fuzzy aliases, then apply the shared identity ranking.
    fn rank_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<RankedTitleIdentity> {
        if limit == 0 {
            return Vec::new();
        }
        let normalized = normalize(&observed_title.chars().take(512).collect::<String>());
        let query: Vec<_> = normalized.chars().take(1024).collect();
        if query.is_empty() {
            return Vec::new();
        }
        if let Some(&alias) = self.title_ids.get(&normalized) {
            return self.rank_scored_title_aliases([(alias, 1.0)], limit, true);
        }
        let aliases = self.fuzzy_title_aliases(&query);
        self.rank_scored_title_aliases(aliases, limit, false)
    }

    /// Group scored aliases by semantic identity before either public materialization.
    fn rank_scored_title_aliases(
        &self,
        aliases: impl IntoIterator<Item = (u32, f32)>,
        limit: usize,
        exact: bool,
    ) -> Vec<RankedTitleIdentity> {
        let mut identities = HashMap::<u32, (f32, u32)>::new();
        for (alias, score) in aliases {
            for &(identity, representative) in &self.title_identities[alias as usize] {
                let candidate = identities
                    .entry(identity)
                    .or_insert((score, representative));
                if score > candidate.0 || (score == candidate.0 && representative < candidate.1) {
                    *candidate = (score, representative);
                }
            }
        }
        let mut identities: Vec<_> = identities.into_iter().collect();
        identities.sort_by(
            |(left_identity, (left_score, left_record)),
             (right_identity, (right_score, right_record))| {
                right_score
                    .total_cmp(left_score)
                    .then(left_record.cmp(right_record))
                    .then(left_identity.cmp(right_identity))
            },
        );
        identities.truncate(limit);
        identities
            .into_iter()
            .map(
                |(identity, (title_similarity, representative))| RankedTitleIdentity {
                    identity,
                    representative,
                    title_similarity,
                    exact,
                },
            )
            .collect()
    }

    /// Borrow a validated interned string.
    fn string(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }

    /// Reconstruct only a returned printing, preserving absent versus empty fields.
    fn card(&self, record: &Record) -> CardMetadata {
        let m = record.metadata;
        let optional = |id| (id != ABSENT).then(|| self.string(id).to_owned());
        CardMetadata {
            game: self.string(m[0]).into(),
            id: self.string(m[1]).into(),
            name: self.string(m[2]).into(),
            oracle_id: optional(m[3]),
            set: optional(m[4]),
            collector_number: optional(m[5]),
            language: optional(m[6]),
            face_index: m[7],
            face_name: optional(m[8]),
            illustration_id: optional(m[9]),
            source: optional(m[10]),
            oracle_text: optional(m[11]),
            printed_text: optional(m[12]),
            printed_name: optional(m[13]),
            type_line: optional(m[14]),
            layout: optional(m[15]),
        }
    }

    /// Cache a query's fuzzy vocabulary matches and score only potentially matching rules.
    fn rule_scores(&self, query: &[String]) -> Vec<Option<(f32, f32)>> {
        let mut candidates = vec![false; self.rules.len()];
        let query: Vec<_> = query
            .iter()
            .map(|word| {
                let characters: Vec<_> = word.chars().take(1024).collect();
                let mut matches = HashMap::new();
                for (id, &string) in self.words.iter().enumerate() {
                    let candidate = self.string(string);
                    let score = if word == candidate {
                        1.
                    } else if word.len() >= 4 && candidate.len() >= 4 {
                        bounded_similarity(&characters, &self.word_characters[id], 0.72)
                    } else {
                        0.
                    };
                    if score >= 0.72 {
                        matches.insert(id as u32, score);
                        for &rule in &self.word_rules[id] {
                            candidates[rule as usize] = true;
                        }
                    }
                }
                let weight = self
                    .word_ids
                    .get(word)
                    .map_or(1., |&id| self.weights[id as usize]);
                RuleQuery { weight, matches }
            })
            .collect();
        self.rules
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                candidates[index]
                    .then(|| compare_cached_rules(&query, reference, &self.weights))
                    .flatten()
            })
            .collect()
    }

    /// Validate persisted IDs and initialize transient character caches and postings.
    fn prepare(mut self) -> Result<Self, Error> {
        let valid_string = |id: u32| (id as usize) < self.strings.len();
        if self.words.len() != self.weights.len()
            || self
                .weights
                .iter()
                .any(|weight| !weight.is_finite() || *weight < 1.)
            || self
                .titles
                .iter()
                .chain(&self.words)
                .any(|&id| !valid_string(id))
            || self
                .rules
                .iter()
                .flatten()
                .any(|&id| id as usize >= self.words.len())
        {
            return Err(Error::InvalidIndex(
                "invalid text vocabulary, token reference or document weight".into(),
            ));
        }
        let mut faces = HashSet::new();
        for record in &self.records {
            for (slot, &id) in record.metadata.iter().enumerate() {
                if slot != 7 && !(valid_string(id) || (slot >= 3 && id == ABSENT)) {
                    return Err(Error::InvalidIndex(
                        "text metadata contains an invalid string reference".into(),
                    ));
                }
            }
            if record
                .titles
                .iter()
                .any(|&id| id != ABSENT && id as usize >= self.titles.len())
                || record
                    .rules
                    .iter()
                    .any(|&id| id != ABSENT && id as usize >= self.rules.len())
                || record
                    .footer
                    .iter()
                    .any(|&id| id != ABSENT && !valid_string(id))
            {
                return Err(Error::InvalidIndex(
                    "text metadata contains an invalid derived reference".into(),
                ));
            }
            if !faces.insert((self.string(record.metadata[1]), record.metadata[7])) {
                return Err(Error::InvalidIndex(
                    "duplicate printing/face in text index".into(),
                ));
            }
        }
        self.title_characters = self
            .titles
            .iter()
            .map(|&id| self.string(id).chars().take(1024).collect())
            .collect();
        self.word_characters = self
            .words
            .iter()
            .map(|&id| self.string(id).chars().take(1024).collect())
            .collect();
        self.word_ids = self
            .words
            .iter()
            .enumerate()
            .map(|(index, &id)| (self.string(id).to_owned(), index as u32))
            .collect();
        self.word_rules = vec![Vec::new(); self.words.len()];
        for (index, rule) in self.rules.iter().enumerate() {
            let mut seen = HashSet::new();
            for &word in rule {
                if seen.insert(word) {
                    self.word_rules[word as usize].push(index as u32);
                }
            }
        }
        self.prepare_title_postings()?;
        self.prepare_footer_postings();
        Ok(self)
    }

    /// Reconstruct exact/fuzzy title postings and game-scoped semantic identities.
    fn prepare_title_postings(&mut self) -> Result<(), Error> {
        self.title_ids.clear();
        self.title_grams.clear();
        self.title_gram_counts.clear();
        for (alias, &string) in self.titles.iter().enumerate() {
            self.title_ids
                .insert(self.string(string).to_owned(), position(alias)?);
            let grams = title_grams(&self.title_characters[alias]);
            self.title_gram_counts
                .push(u16::try_from(grams.len()).map_err(|_| {
                    Error::InvalidIndex("text title exceeds transient gram capacity".into())
                })?);
            for gram in grams {
                self.title_grams
                    .entry(gram)
                    .or_default()
                    .push(position(alias)?);
            }
        }
        let mut identity_ids = HashMap::<(u32, bool, u32), u32>::new();
        let mut record_identities = Vec::with_capacity(self.records.len());
        self.identity_records.clear();
        for (index, record) in self.records.iter().enumerate() {
            let oracle = record.metadata[3] != ABSENT;
            let key = (
                record.metadata[0],
                oracle,
                if oracle {
                    record.metadata[3]
                } else {
                    record.metadata[1]
                },
            );
            let next = position(self.identity_records.len())?;
            let identity = *identity_ids.entry(key).or_insert_with(|| {
                self.identity_records.push(Vec::new());
                next
            });
            self.identity_records[identity as usize].push(position(index)?);
            record_identities.push(identity);
        }
        self.title_identities = vec![Vec::new(); self.titles.len()];
        let mut seen = HashSet::new();
        for (record_index, record) in self.records.iter().enumerate() {
            let identity = record_identities[record_index];
            for &alias in record.titles.iter().filter(|&&alias| alias != ABSENT) {
                if seen.insert((alias, identity)) {
                    self.title_identities[alias as usize].push((identity, position(record_index)?));
                }
            }
        }
        Ok(())
    }

    /// Reconstruct normalized set/collector dictionaries and exact pair postings.
    fn prepare_footer_postings(&mut self) {
        self.footer_sets.clear();
        self.footer_numbers.clear();
        self.footer_records.clear();
        for (index, record) in self.records.iter().enumerate() {
            let [set, number] = record.footer;
            if set == ABSENT || number == ABSENT {
                continue;
            }
            self.footer_sets
                .entry(self.string(set).to_owned())
                .or_insert(set);
            self.footer_numbers
                .entry(self.string(number).to_owned())
                .or_insert(number);
            self.footer_records
                .entry((set, number))
                .or_default()
                .push(index as u32);
        }
    }
}

impl TitleProposer for TextIndex {
    fn propose_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<TitleIdentityProposal> {
        TextIndex::propose_title_identities(self, observed_title, limit)
    }
}

impl TitleIndex {
    /// Build a compact title lookup from an already loaded visual index.
    ///
    /// Metadata is visited in gallery order and is not cloned into an
    /// intermediate collection. Canonical card or face names and English
    /// alternate printed names are indexed; localized printed names and every
    /// noncandidate metadata field are discarded.
    ///
    /// # Errors
    ///
    /// Returns an error if visual metadata cannot be read or exceeds the
    /// compact index's 32-bit capacities. The visual index has already validated
    /// physical-face uniqueness while loading.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_visual_index(index: &ShardedIndex) -> Result<Self, Error> {
        let mut builder = CompactTitleBuilder::default();
        let mut failure = None;
        index.for_each_metadata(|card| {
            if failure.is_none()
                && let Err(error) = builder.push_visual(card)
            {
                failure = Some(error);
            }
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        builder.finish()
    }

    /// Build a compact title lookup from metadata without retaining full records.
    ///
    /// This is useful when a caller already has the same metadata that will be
    /// enrolled visually. Both owned and borrowed card iterators are accepted.
    /// Input order determines stable tie ordering.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate physical faces or unrepresentable counts.
    pub fn from_metadata<T>(cards: impl IntoIterator<Item = T>) -> Result<Self, Error>
    where
        T: Borrow<CardMetadata>,
    {
        let mut builder = CompactTitleBuilder::default();
        for card in cards {
            builder.push(card.borrow())?;
        }
        builder.finish()
    }

    /// Number of retained title/identity representatives.
    #[must_use]
    pub fn len(&self) -> usize {
        self.representatives.len()
    }

    /// Whether the index contains no searchable title identities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.representatives.is_empty()
    }

    /// Propose semantic identities using the full index's bounded title policy.
    ///
    /// Exact normalized titles use a direct posting. Fuzzy queries score at
    /// most 512 aliases selected by character grams. Results below `0.5` are
    /// discarded and `limit` counts semantic identities.
    #[must_use]
    pub fn propose_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<TitleIdentityProposal> {
        self.rank_title_identities(observed_title, limit)
            .into_iter()
            .map(|candidate| TitleIdentityProposal {
                representative: self.card(&self.representatives[candidate.representative as usize]),
                title_similarity: candidate.title_similarity,
                exact: candidate.exact,
            })
            .collect()
    }

    /// Generate and exactly score the same bounded fuzzy alias set as [`TextIndex`].
    fn fuzzy_title_aliases(&self, query: &[char]) -> Vec<(u32, f32)> {
        let query_grams = title_grams(query);
        if query_grams.is_empty() {
            return Vec::new();
        }
        let mut overlaps = vec![0_u16; self.title_characters.len()];
        let capacity = query_grams
            .iter()
            .filter_map(|gram| self.title_grams.get(gram))
            .fold(0_usize, |sum, postings| sum.saturating_add(postings.len()))
            .min(self.title_characters.len());
        let mut candidates = Vec::with_capacity(capacity);
        for gram in &query_grams {
            if let Some(postings) = self.title_grams.get(gram) {
                for &alias in postings {
                    let overlap = &mut overlaps[alias as usize];
                    if *overlap == 0 {
                        candidates.push(alias);
                    }
                    *overlap = overlap.saturating_add(1);
                }
            }
        }
        let query_count = query_grams.len();
        candidates.retain(|alias| {
            let length = self.title_characters[*alias as usize].len();
            1. - query.len().abs_diff(length) as f32 / query.len().max(length) as f32
                >= MIN_TITLE_SIMILARITY
        });
        if candidates.len() > MAX_FUZZY_TITLE_ALIASES {
            candidates.select_nth_unstable_by(MAX_FUZZY_TITLE_ALIASES, |left, right| {
                compare_title_candidates(
                    &self.title_characters,
                    &self.title_gram_counts,
                    query_count,
                    query.len(),
                    (*left, overlaps[*left as usize]),
                    (*right, overlaps[*right as usize]),
                )
            });
            candidates.truncate(MAX_FUZZY_TITLE_ALIASES);
        }
        let mut edit_row = Vec::new();
        let mut scored: Vec<_> = candidates
            .into_iter()
            .filter_map(|alias| {
                let score = bounded_similarity_with_row(
                    query,
                    &self.title_characters[alias as usize],
                    MIN_TITLE_SIMILARITY,
                    &mut edit_row,
                );
                (score >= MIN_TITLE_SIMILARITY).then_some((alias, score))
            })
            .collect();
        scored.sort_by(|(left, left_score), (right, right_score)| {
            right_score.total_cmp(left_score).then(left.cmp(right))
        });
        scored
    }

    /// Select exact or fuzzy aliases and group them by semantic identity.
    fn rank_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<RankedTitleIdentity> {
        if limit == 0 {
            return Vec::new();
        }
        let normalized = normalize(&observed_title.chars().take(512).collect::<String>());
        let query: Vec<_> = normalized.chars().take(1024).collect();
        if query.is_empty() {
            return Vec::new();
        }
        if let Some(&alias) = self.title_ids.get(&normalized) {
            return self.rank_scored_title_aliases([(alias, 1.0)], limit, true);
        }
        let aliases = self.fuzzy_title_aliases(&query);
        self.rank_scored_title_aliases(aliases, limit, false)
    }

    /// Apply stable identity grouping to scored aliases.
    fn rank_scored_title_aliases(
        &self,
        aliases: impl IntoIterator<Item = (u32, f32)>,
        limit: usize,
        exact: bool,
    ) -> Vec<RankedTitleIdentity> {
        let mut identities = HashMap::<u32, (f32, u32)>::new();
        for (alias, score) in aliases {
            for &(identity, representative) in &self.title_identities[alias as usize] {
                let candidate = identities
                    .entry(identity)
                    .or_insert((score, representative));
                if score > candidate.0 || (score == candidate.0 && representative < candidate.1) {
                    *candidate = (score, representative);
                }
            }
        }
        let mut identities: Vec<_> = identities.into_iter().collect();
        identities.sort_by(
            |(left_identity, (left_score, left_record)),
             (right_identity, (right_score, right_record))| {
                right_score
                    .total_cmp(left_score)
                    .then(left_record.cmp(right_record))
                    .then(left_identity.cmp(right_identity))
            },
        );
        identities.truncate(limit);
        identities
            .into_iter()
            .map(
                |(identity, (title_similarity, representative))| RankedTitleIdentity {
                    identity,
                    representative,
                    title_similarity,
                    exact,
                },
            )
            .collect()
    }

    /// Materialize only the sparse metadata contract needed by candidate lookup.
    fn card(&self, record: &CompactTitleRecord) -> CardMetadata {
        let optional = |id| (id != ABSENT).then(|| self.strings[id as usize].clone());
        CardMetadata {
            game: self.strings[record.game as usize].clone(),
            id: self.strings[record.printing as usize].clone(),
            name: self.strings[record.name as usize].clone(),
            oracle_id: optional(record.oracle),
            face_index: record.face_index,
            face_name: optional(record.face_name),
            illustration_id: optional(record.illustration),
            ..CardMetadata::default()
        }
    }
}

impl TitleProposer for TitleIndex {
    fn propose_title_identities(
        &self,
        observed_title: &str,
        limit: usize,
    ) -> Vec<TitleIdentityProposal> {
        TitleIndex::propose_title_identities(self, observed_title, limit)
    }
}

/// Shared title ranking row retained before choosing lightweight or expanded output.
struct RankedTitleIdentity {
    /// Transient semantic-identity posting ID.
    identity: u32,
    /// Source-order metadata record whose alias supplied the best score.
    representative: u32,
    /// Best normalized edit similarity for this identity.
    title_similarity: f32,
    /// Whether the normalized observed title matched its alias exactly.
    exact: bool,
}

/// One small ranked row, avoiding expansion/cloning of nonreturned metadata.
struct Candidate {
    /// Source-order printing/face position.
    index: usize,
    /// Combined retrieval priority.
    score: f32,
    /// Optional fuzzy title evidence.
    title_similarity: Option<f32>,
    /// Optional rules F1 and coverage.
    rule_evidence: Option<(f32, f32)>,
    /// Complete exact set/collector pair.
    footer_match: bool,
    /// Observed language consistency.
    language_match: Option<bool>,
}

/// Query token weight and qualifying fuzzy vocabulary matches.
struct RuleQuery {
    /// Cached semantic-document weight, or one for an unseen word.
    weight: f32,
    /// Only scores eligible for the original 0.72 token threshold.
    matches: HashMap<u32, f32>,
}

/// Apply the original greedy token matching using cached pair similarities.
fn compare_cached_rules(
    query: &[RuleQuery],
    reference: &[u32],
    weights: &[f32],
) -> Option<(f32, f32)> {
    if reference.is_empty() {
        return None;
    }
    let mut used = vec![false; reference.len()];
    let mut matched_query = 0.;
    let mut matched_reference = 0.;
    for word in query {
        let best = reference
            .iter()
            .enumerate()
            .filter(|(index, _)| !used[*index])
            .map(|(index, token)| (index, word.matches.get(token).copied().unwrap_or(0.)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((index, score)) = best.filter(|(_, score)| *score >= 0.72) {
            used[index] = true;
            matched_query += word.weight * score;
            matched_reference += weights[reference[index] as usize] * score;
        }
    }
    let precision = matched_query / query.iter().map(|word| word.weight).sum::<f32>();
    let coverage = matched_reference
        / reference
            .iter()
            .map(|&word| weights[word as usize])
            .sum::<f32>();
    let score = 2. * precision * coverage / (precision + coverage).max(f32::EPSILON);
    (score >= 0.30).then_some((score.clamp(0., 1.), coverage.clamp(0., 1.)))
}

/// Reject only impossible length matches before running the unchanged edit scorer.
fn bounded_similarity(left: &[char], right: &[char], minimum: f32) -> f32 {
    if left.is_empty()
        || right.is_empty()
        || 1. - left.len().abs_diff(right.len()) as f32 / (left.len().max(right.len()) as f32)
            < minimum
    {
        0.
    } else {
        edit_similarity_characters(left, right)
    }
}

/// Compare fuzzy preselection rows by Dice overlap, length distance and alias ID.
fn compare_title_candidates(
    title_characters: &[Vec<char>],
    title_gram_counts: &[u16],
    query_gram_count: usize,
    query_length: usize,
    left: (u32, u16),
    right: (u32, u16),
) -> std::cmp::Ordering {
    let (left_alias, left_overlap) = left;
    let (right_alias, right_overlap) = right;
    let left_count = title_gram_counts[left_alias as usize] as usize;
    let right_count = title_gram_counts[right_alias as usize] as usize;
    let left_denominator = query_gram_count + left_count;
    let right_denominator = query_gram_count + right_count;
    (usize::from(right_overlap) * left_denominator)
        .cmp(&(usize::from(left_overlap) * right_denominator))
        .then_with(|| {
            query_length
                .abs_diff(title_characters[left_alias as usize].len())
                .cmp(&query_length.abs_diff(title_characters[right_alias as usize].len()))
        })
        .then(left_alias.cmp(&right_alias))
}

/// Compute the unchanged edit score while reusing one dynamic-programming row.
fn bounded_similarity_with_row(
    left: &[char],
    right: &[char],
    minimum: f32,
    row: &mut Vec<usize>,
) -> f32 {
    if left.is_empty()
        || right.is_empty()
        || 1. - left.len().abs_diff(right.len()) as f32 / (left.len().max(right.len()) as f32)
            < minimum
    {
        return 0.;
    }
    row.resize(right.len() + 1, 0);
    for (index, value) in row.iter_mut().enumerate() {
        *value = index;
    }
    for (i, left) in left.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, right) in right.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(left != right));
            diagonal = above;
        }
    }
    1. - row[right.len()] as f32 / left.len().max(right.len()) as f32
}

/// Return sorted distinct character bigrams, retaining one-character titles.
fn title_grams(characters: &[char]) -> Vec<[char; 2]> {
    let mut grams = if characters.len() == 1 {
        vec![[characters[0], SINGLE_CHARACTER_GRAM]]
    } else {
        characters
            .windows(2)
            .map(|window| [window[0], window[1]])
            .collect()
    };
    grams.sort_unstable();
    grams.dedup();
    grams
}

/// Construction state discarded after the compact title representation is ready.
#[derive(Default)]
struct CompactTitleBuilder {
    /// Shared canonical, normalized, and candidate-key strings.
    strings: Vec<String>,
    /// Construction-only string interning dictionary.
    string_ids: HashMap<String, u32>,
    /// Normalized title string IDs in alias order.
    titles: Vec<u32>,
    /// Normalized string ID to alias ID.
    title_ids: HashMap<u32, u32>,
    /// Game-scoped Oracle or fallback-printing key to identity ID.
    identity_ids: HashMap<(u32, bool, u32), u32>,
    /// Alias/identity pairs already represented.
    alias_identities: HashSet<(u32, u32)>,
    /// Source-order compact representatives.
    representatives: Vec<CompactTitleRecord>,
    /// Alias postings populated as representatives are retained.
    title_identities: Vec<Vec<(u32, u32)>>,
    /// Duplicate physical-face guard matching the visual index key.
    faces: HashSet<(String, String, u32)>,
}

impl CompactTitleBuilder {
    /// Intern one string without altering its exact Unicode spelling.
    fn intern(&mut self, text: &str) -> Result<u32, Error> {
        if let Some(&id) = self.string_ids.get(text) {
            return Ok(id);
        }
        let id = position(self.strings.len())?;
        self.strings.push(text.into());
        self.string_ids.insert(text.into(), id);
        Ok(id)
    }

    /// Intern one optional candidate-key string.
    fn optional(&mut self, text: Option<&str>) -> Result<u32, Error> {
        text.map_or(Ok(ABSENT), |text| self.intern(text))
    }

    /// Validate and retain one standalone metadata face.
    fn push(&mut self, card: &CardMetadata) -> Result<(), Error> {
        if !self
            .faces
            .insert((card.game.clone(), card.id.clone(), card.face_index))
        {
            return Err(Error::InvalidCatalog(format!(
                "duplicate printing {} face {}",
                card.id, card.face_index
            )));
        }
        self.push_unique(card)
    }

    /// Retain metadata whose physical-face uniqueness the visual index validated.
    #[cfg(not(target_arch = "wasm32"))]
    fn push_visual(&mut self, card: &CardMetadata) -> Result<(), Error> {
        self.push_unique(card)
    }

    /// Retain canonical and English printed title aliases with one compact visual key.
    fn push_unique(&mut self, card: &CardMetadata) -> Result<(), Error> {
        let game = self.intern(&card.game)?;
        let oracle = self.optional(card.oracle_id.as_deref())?;
        let fallback_printing = if oracle == ABSENT {
            self.intern(&card.id)?
        } else {
            ABSENT
        };
        let identity_key = (
            game,
            oracle != ABSENT,
            if oracle == ABSENT {
                fallback_printing
            } else {
                oracle
            },
        );
        let identity = if let Some(&identity) = self.identity_ids.get(&identity_key) {
            identity
        } else {
            let identity = position(self.identity_ids.len())?;
            self.identity_ids.insert(identity_key, identity);
            identity
        };
        let display = card.face_name.as_deref().unwrap_or(&card.name);
        let printed = match card.language.as_deref() {
            None | Some("en") => card.printed_name.as_deref(),
            Some(_) => None,
        };
        let mut representative = None;
        for title in [Some(display), printed].into_iter().flatten() {
            let normalized = self.intern(&normalize(title))?;
            let alias = if let Some(&alias) = self.title_ids.get(&normalized) {
                alias
            } else {
                let alias = position(self.titles.len())?;
                self.titles.push(normalized);
                self.title_ids.insert(normalized, alias);
                self.title_identities.push(Vec::new());
                alias
            };
            if self.alias_identities.insert((alias, identity)) {
                let representative = if let Some(representative) = representative {
                    representative
                } else {
                    let printing = if fallback_printing == ABSENT {
                        self.intern(&card.id)?
                    } else {
                        fallback_printing
                    };
                    let name = self.intern(&card.name)?;
                    let face_name = self.optional(card.face_name.as_deref())?;
                    let illustration = self.optional(card.illustration_id.as_deref())?;
                    let index = position(self.representatives.len())?;
                    self.representatives.push(CompactTitleRecord {
                        game,
                        printing,
                        face_index: card.face_index,
                        oracle,
                        illustration,
                        name,
                        face_name,
                    });
                    representative = Some(index);
                    index
                };
                self.title_identities[alias as usize].push((identity, representative));
            }
        }
        Ok(())
    }

    /// Build transient direct and gram postings, dropping all construction maps.
    fn finish(self) -> Result<TitleIndex, Error> {
        let mut title_characters = Vec::with_capacity(self.titles.len());
        let mut title_ids = HashMap::with_capacity(self.titles.len());
        let mut title_grams_index = HashMap::<[char; 2], Vec<u32>>::new();
        let mut title_gram_counts = Vec::with_capacity(self.titles.len());
        for (index, &string) in self.titles.iter().enumerate() {
            let alias = position(index)?;
            let characters: Vec<_> = self.strings[string as usize].chars().take(1024).collect();
            title_ids.insert(self.strings[string as usize].clone(), alias);
            let grams = title_grams(&characters);
            title_gram_counts.push(u16::try_from(grams.len()).map_err(|_| {
                Error::InvalidIndex("compact title exceeds transient gram capacity".into())
            })?);
            for gram in grams {
                title_grams_index.entry(gram).or_default().push(alias);
            }
            title_characters.push(characters);
        }
        Ok(TitleIndex {
            strings: self.strings,
            representatives: self.representatives,
            title_characters,
            title_ids,
            title_grams: title_grams_index,
            title_gram_counts,
            title_identities: self.title_identities,
        })
    }
}

/// Streaming construction state, discarded after the compact representation is ready.
#[derive(Default)]
struct Builder {
    /// Shared original and normalized strings.
    strings: Vec<String>,
    /// Construction-only string interning dictionary.
    string_ids: HashMap<String, u32>,
    /// Compact printing/face records.
    records: Vec<Record>,
    /// Construction-only duplicate physical-face guard.
    faces: HashSet<(u32, u32)>,
    /// Unique normalized title string IDs.
    titles: Vec<u32>,
    /// Normalized title string to alias ID.
    title_ids: HashMap<u32, u32>,
    /// Original title string to prepared alias ID.
    raw_titles: HashMap<u32, u32>,
    /// Ordered unique rules-token sequences.
    rules: Vec<Vec<u32>>,
    /// Token sequence interning dictionary.
    rule_ids: HashMap<Vec<u32>, u32>,
    /// Original rules string to prepared token sequence ID.
    raw_rules: HashMap<u32, u32>,
    /// Unique normalized vocabulary string IDs.
    words: Vec<u32>,
    /// Normalized string ID to vocabulary ID.
    word_ids: HashMap<u32, u32>,
    /// Semantic documents, including empty identities, for original IDF semantics.
    documents: HashMap<String, HashSet<u32>>,
    /// Cached normalization for repeated set strings.
    sets: HashMap<u32, u32>,
    /// Cached normalization for repeated collector strings.
    collectors: HashMap<u32, u32>,
}

impl Builder {
    /// Intern one string without altering its exact Unicode spelling.
    fn intern(&mut self, text: &str) -> Result<u32, Error> {
        if let Some(&id) = self.string_ids.get(text) {
            return Ok(id);
        }
        let id = position(self.strings.len())?;
        self.strings.push(text.into());
        self.string_ids.insert(text.into(), id);
        Ok(id)
    }

    /// Intern an optional string while preserving absence versus an empty value.
    fn optional(&mut self, text: Option<&str>) -> Result<u32, Error> {
        text.map_or(Ok(ABSENT), |text| self.intern(text))
    }

    /// Cache one title alias shared by all metadata using its normalized spelling.
    fn title(&mut self, raw: u32) -> Result<u32, Error> {
        if raw == ABSENT {
            return Ok(ABSENT);
        }
        if let Some(&id) = self.raw_titles.get(&raw) {
            return Ok(id);
        }
        let normalized = self.intern(&normalize(&self.strings[raw as usize]))?;
        let id = if let Some(&id) = self.title_ids.get(&normalized) {
            id
        } else {
            let id = position(self.titles.len())?;
            self.titles.push(normalized);
            self.title_ids.insert(normalized, id);
            id
        };
        self.raw_titles.insert(raw, id);
        Ok(id)
    }

    /// Cache a rules string as ordered, shared normalized vocabulary IDs.
    fn rules(&mut self, raw: u32) -> Result<u32, Error> {
        if raw == ABSENT {
            return Ok(ABSENT);
        }
        if let Some(&id) = self.raw_rules.get(&raw) {
            return Ok(id);
        }
        let mut words = Vec::new();
        for word in rule_tokens(&self.strings[raw as usize]) {
            let string = self.intern(&word)?;
            let next = position(self.words.len())?;
            let id = *self.word_ids.entry(string).or_insert_with(|| {
                self.words.push(string);
                next
            });
            words.push(id);
        }
        let id = if let Some(&id) = self.rule_ids.get(&words) {
            id
        } else {
            let id = position(self.rules.len())?;
            self.rules.push(words.clone());
            self.rule_ids.insert(words, id);
            id
        };
        self.raw_rules.insert(raw, id);
        Ok(id)
    }

    /// Add exactly one metadata record and its semantic document memberships.
    fn push(&mut self, card: CardMetadata) -> Result<(), Error> {
        position(self.records.len())?;
        let m = [
            self.intern(&card.game)?,
            self.intern(&card.id)?,
            self.intern(&card.name)?,
            self.optional(card.oracle_id.as_deref())?,
            self.optional(card.set.as_deref())?,
            self.optional(card.collector_number.as_deref())?,
            self.optional(card.language.as_deref())?,
            card.face_index,
            self.optional(card.face_name.as_deref())?,
            self.optional(card.illustration_id.as_deref())?,
            self.optional(card.source.as_deref())?,
            self.optional(card.oracle_text.as_deref())?,
            self.optional(card.printed_text.as_deref())?,
            self.optional(card.printed_name.as_deref())?,
            self.optional(card.type_line.as_deref())?,
            self.optional(card.layout.as_deref())?,
        ];
        if !self.faces.insert((m[1], m[7])) {
            return Err(Error::InvalidCatalog(format!(
                "duplicate printing {} face {}",
                card.id, card.face_index
            )));
        }
        let titles = [
            self.title(if m[8] == ABSENT { m[2] } else { m[8] })?,
            self.title(m[13])?,
        ];
        let rules = [self.rules(m[12])?, self.rules(m[11])?];
        let document = self
            .documents
            .entry(format!(
                "{}:{}",
                card.game,
                card.oracle_id.as_deref().unwrap_or(&card.id)
            ))
            .or_default();
        for &rule in &rules {
            if rule != ABSENT {
                document.extend(self.rules[rule as usize].iter().copied());
            }
        }
        let set = if m[4] == ABSENT {
            ABSENT
        } else if let Some(&id) = self.sets.get(&m[4]) {
            id
        } else {
            let id = self.intern(&normalize(&self.strings[m[4] as usize]))?;
            self.sets.insert(m[4], id);
            id
        };
        let number = if m[5] == ABSENT {
            ABSENT
        } else if let Some(&id) = self.collectors.get(&m[5]) {
            id
        } else {
            let id = self.intern(&normalize_number(&normalize_collector(
                &self.strings[m[5] as usize],
            )))?;
            self.collectors.insert(m[5], id);
            id
        };
        self.records.push(Record {
            metadata: m,
            titles,
            rules,
            footer: [set, number],
        });
        Ok(())
    }

    /// Freeze semantic IDF and discard construction-only dictionaries.
    fn finish(self) -> Result<TextIndex, Error> {
        let mut frequencies = vec![0_u32; self.words.len()];
        for words in self.documents.values() {
            for &word in words {
                frequencies[word as usize] += 1;
            }
        }
        let count = self.documents.len() as f32;
        let weights = frequencies
            .into_iter()
            .map(|frequency| ((count + 1.) / (frequency as f32 + 1.)).ln() + 1.)
            .collect();
        TextIndex {
            strings: self.strings,
            records: self.records,
            titles: self.titles,
            rules: self.rules,
            words: self.words,
            weights,
            title_characters: Vec::new(),
            word_characters: Vec::new(),
            word_ids: HashMap::new(),
            word_rules: Vec::new(),
            title_ids: HashMap::new(),
            title_grams: HashMap::new(),
            title_gram_counts: Vec::new(),
            title_identities: Vec::new(),
            identity_records: Vec::new(),
            footer_sets: HashMap::new(),
            footer_numbers: HashMap::new(),
            footer_records: HashMap::new(),
        }
        .prepare()
    }
}

/// Convert a stored position while reserving the absent sentinel.
fn position(length: usize) -> Result<u32, Error> {
    u32::try_from(length)
        .ok()
        .filter(|&value| value != ABSENT)
        .ok_or_else(|| Error::InvalidIndex("text index exceeds 32-bit format capacity".into()))
}
