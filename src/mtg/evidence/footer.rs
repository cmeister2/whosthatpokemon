//! Compact exact collector-footer postings without title aliases or rules text.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

use crate::{CardMetadata, Error};

use super::{
    TextMatch, attached_rarity_collector, collector_tokens, footer_language_code, footer_languages,
    footer_lookup_text, make_match, normalize, normalize_collector, normalize_number, tokens,
};

/// Exact collector-footer source retaining every matching physical face.
///
/// A result limit must not hide competing identities or translated printings.
/// Implementations use indexed set/collector postings rather than scanning a
/// catalogue for each observation. Language disagreements remain explicit.
/// [`FooterIndex`] additionally recognizes registered glued field boundaries and
/// documented printed-number aliases; [`super::TextIndex`] retains its historical
/// separated-token lookup without those extra parser forms.
pub trait FooterProposer {
    /// Return all exact set/collector matches, ordered by language agreement.
    #[must_use]
    fn propose_footer_printings(&self, observed_footer: &str) -> Vec<TextMatch>;
}

/// Marker for an absent interned optional field.
const ABSENT: u32 = u32::MAX;

/// Interned visual proposal and printing keys, without rules or source metadata.
#[derive(Debug)]
struct FooterRecord {
    /// Game, printing, name, Oracle, face name, illustration, set, number, language.
    strings: [u32; 9],
    /// Physical face within its printing.
    face_index: u32,
}

/// Compact collector lookup built once from enrolled visual metadata.
///
/// Shared strings and direct normalized `(set, collector)` postings are retained;
/// title aliases, rules, image pixels and source paths are not. Queries only
/// materialize metadata for exact posting hits. All enrolled languages and faces
/// remain visible, including language conflicts and ambiguous set/number pairs.
#[derive(Debug, Default)]
pub struct FooterIndex {
    /// Interned display and visual-proposal strings.
    strings: Vec<String>,
    /// Compact physical faces in enrollment order.
    records: Vec<FooterRecord>,
    /// Normalized set spelling to its posting-key ID.
    sets: HashMap<String, u32>,
    /// Distinct registered set-code byte lengths for bounded glued-token lookup.
    set_lengths: Vec<usize>,
    /// Normalized complete collector spelling to its posting-key ID.
    numbers: HashMap<String, u32>,
    /// Exact pair to every matching source-order physical face.
    postings: HashMap<(u32, u32), Vec<u32>>,
}

impl FooterIndex {
    /// Build direct footer postings from a loaded visual index without cloning its catalogue.
    ///
    /// The visual index has already checked physical-face uniqueness. Callers can
    /// retain this index across frames. The native identification command builds
    /// it at startup and reads both text channels on every attempted orientation;
    /// neither index construction nor queries use image files or networking.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable visual metadata or 32-bit capacity overflow.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_visual_index(index: &crate::matching::ShardedIndex) -> Result<Self, Error> {
        let mut builder = Builder::default();
        let mut failure = None;
        index.for_each_metadata(|card| {
            if failure.is_none()
                && let Err(error) = builder.push(card)
            {
                failure = Some(error);
            }
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(builder.index)
    }

    /// Build indexed footer proposals from owned or borrowed metadata.
    ///
    /// Faces without a complete set/number key are omitted. Original case, zero
    /// padding, suffixes and language metadata survive in returned proposals.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate game/printing/face keys or capacity overflow.
    pub fn from_metadata<T>(cards: impl IntoIterator<Item = T>) -> Result<Self, Error>
    where
        T: Borrow<CardMetadata>,
    {
        let mut builder = Builder::default();
        let mut faces = HashSet::new();
        for item in cards {
            let card = item.borrow();
            if !faces.insert((card.game.clone(), card.id.clone(), card.face_index)) {
                return Err(Error::InvalidCatalog(format!(
                    "duplicate printing {} face {}",
                    card.id, card.face_index
                )));
            }
            builder.push(card)?;
        }
        Ok(builder.index)
    }

    /// Number of enrolled physical faces with searchable collector keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether no complete collector keys were enrolled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Retrieve complete exact-pair postings without a per-query catalogue pass.
    #[must_use]
    pub fn propose_footer_printings(&self, observed_footer: &str) -> Vec<TextMatch> {
        let lookup_text = footer_lookup_text(observed_footer);
        let footer: HashSet<_> = tokens(&lookup_text).into_iter().collect();
        let mut collectors: HashSet<_> =
            collector_tokens(observed_footer, |token| self.sets.contains_key(token))
                .into_iter()
                .collect();
        let glued = self.glued_keys(&lookup_text);
        let mut sets: Vec<_> = footer
            .iter()
            .filter_map(|set| self.sets.get(set).copied())
            .collect();
        let numbers: Vec<_> = collectors
            .iter()
            .filter_map(|number| self.numbers.get(number))
            .collect();
        let mut languages = footer_languages(footer.iter().map(String::as_str));
        let mut split_sets = HashSet::new();
        // The number can be a separate token while set/language remain glued,
        // e.g. `083 SLDEN`. Only a registered complete set prefix and a known
        // complete language suffix establish this additional literal boundary.
        for token in &footer {
            if token.len() > 128 {
                continue;
            }
            for &length in &self.set_lengths {
                if length >= token.len() || !token.is_char_boundary(length) {
                    continue;
                }
                if let Some(&set) = self.sets.get(&token[..length])
                    && let Some(language) = footer_language_code(&token[length..])
                {
                    if !sets.contains(&set) {
                        sets.push(set);
                    }
                    if !languages.contains(&language) {
                        languages.push(language);
                    }
                    split_sets.insert(set);
                }
            }
        }
        for key in &glued {
            if let Some(language) = key.language
                && !languages.contains(&language)
            {
                languages.push(language);
            }
        }
        let mut selected = HashSet::new();
        for &set in &sets {
            for &number in &numbers {
                if let Some(records) = self.postings.get(&(set, *number)) {
                    selected.extend(records.iter().copied());
                }
            }
        }
        let mut glued_selected = HashSet::new();
        for key in &glued {
            if let Some(&number) = self.numbers.get(&key.number)
                && let Some(records) = self.postings.get(&(key.set, number))
            {
                selected.extend(records.iter().copied());
                glued_selected.extend(records.iter().copied());
            }
            collectors.insert(key.number.clone());
        }
        let mut selected: Vec<_> = selected.into_iter().collect();
        selected.sort_unstable();
        let mut matches: Vec<_> = selected
            .into_iter()
            .map(|index| {
                let card = self.card(&self.records[index as usize]);
                let language_match = if languages.len() == 1 {
                    card.language
                        .as_deref()
                        .map(|language| language == languages[0])
                } else {
                    None
                };
                let mut hit = make_match(&card, None, None, true, language_match);
                if glued_selected.contains(&index) || card.set.as_deref().is_some_and(|set| {
                    self.sets.get(&normalize(set)).is_some_and(|set| split_sets.contains(set))
                }) {
                    hit.reasons.push("Separated adjacent literal collector, registered set-code and language fields; no OCR characters were corrected.".into());
                }
                if card.collector_number.as_deref().is_some_and(|number| {
                    !collectors.contains(&normalize_number(&normalize_collector(number)))
                }) {
                    hit.reasons[0] = "Exact set token and a documented printed collector-number alias; verify the OCR against the image.".into();
                    hit.reasons.push("Matched a documented Secret Lair printed-number alias; the catalogue's unprinted collector suffix remains in the proposal.".into());
                }
                (index, hit)
            })
            .collect();
        matches.sort_by(|(left_index, left), (right_index, right)| {
            right
                .score
                .total_cmp(&left.score)
                .then(left.card.id.cmp(&right.card.id))
                .then(left_index.cmp(right_index))
        });
        matches.into_iter().map(|(_, hit)| hit).collect()
    }

    /// Split short glued OCR fields using exact registered set boundaries, never fuzzy correction.
    fn glued_keys(&self, text: &str) -> Vec<GluedFooterKey> {
        let lower = text.split('©').next().unwrap_or_default().to_lowercase();
        let mut keys = Vec::new();
        for raw in lower.split(|character: char| {
            !character.is_alphanumeric() && character != '★' && character != '/'
        }) {
            if raw.len() > 128 || raw.is_empty() || self.sets.contains_key(raw) {
                continue;
            }
            let token = attached_rarity_collector(raw).unwrap_or(raw);
            if !token.starts_with(|character: char| character.is_ascii_digit()) {
                continue;
            }
            for (split, _) in token.char_indices().skip(1) {
                let prefix = &token[..split];
                for &length in &self.set_lengths {
                    let Some(end) = split
                        .checked_add(length)
                        .filter(|end| *end <= token.len() && token.is_char_boundary(*end))
                    else {
                        continue;
                    };
                    let Some(&set) = self.sets.get(&token[split..end]) else {
                        continue;
                    };
                    let tail = token[end..].trim_matches('★');
                    let language = footer_language_code(tail);
                    if !tail.is_empty() && language.is_none() {
                        continue;
                    }
                    let separated = format!("{prefix} {} {tail}", &token[split..end]);
                    for number in
                        collector_tokens(&separated, |token| self.sets.contains_key(token))
                    {
                        if self.numbers.contains_key(&number) {
                            keys.push(GluedFooterKey {
                                set,
                                number,
                                language,
                            });
                        }
                    }
                }
            }
        }
        keys
    }

    /// Expand only a returned physical-face proposal.
    fn card(&self, record: &FooterRecord) -> CardMetadata {
        let value = |position: usize| self.strings[record.strings[position] as usize].clone();
        let optional =
            |position: usize| (record.strings[position] != ABSENT).then(|| value(position));
        CardMetadata {
            game: value(0),
            id: value(1),
            name: value(2),
            oracle_id: optional(3),
            face_name: optional(4),
            illustration_id: optional(5),
            set: optional(6),
            collector_number: optional(7),
            language: optional(8),
            face_index: record.face_index,
            ..CardMetadata::default()
        }
    }
}

/// One literal, registered decomposition of adjacent footer OCR fields.
struct GluedFooterKey {
    /// Registered normalized set key.
    set: u32,
    /// Complete normalized collector numerator, preserving suffixes.
    number: String,
    /// Recognized exact language suffix, when present in the glued token.
    language: Option<&'static str>,
}

impl FooterProposer for FooterIndex {
    fn propose_footer_printings(&self, observed_footer: &str) -> Vec<TextMatch> {
        FooterIndex::propose_footer_printings(self, observed_footer)
    }
}

impl FooterProposer for super::TextIndex {
    fn propose_footer_printings(&self, observed_footer: &str) -> Vec<TextMatch> {
        self.search_footer_printings(observed_footer, usize::MAX)
    }
}

/// Transient interning table discarded after one-time index construction.
#[derive(Default)]
struct Builder {
    /// Final compact postings and metadata.
    index: FooterIndex,
    /// Original spelling to shared string IDs.
    ids: HashMap<String, u32>,
}

impl Builder {
    /// Intern one borrowed field, checking the reserved sentinel boundary.
    fn intern(&mut self, value: &str) -> Result<u32, Error> {
        if let Some(&id) = self.ids.get(value) {
            return Ok(id);
        }
        let id = position(self.index.strings.len())?;
        self.index.strings.push(value.into());
        self.ids.insert(value.into(), id);
        Ok(id)
    }

    /// Intern an optional field without conflating absence with empty spelling.
    fn optional(&mut self, value: Option<&str>) -> Result<u32, Error> {
        value.map_or(Ok(ABSENT), |value| self.intern(value))
    }

    /// Add one already validated physical face with a complete pair key.
    fn push(&mut self, card: &CardMetadata) -> Result<(), Error> {
        let (Some(set), Some(number)) = (&card.set, &card.collector_number) else {
            return Ok(());
        };
        let normalized_set = normalize(set);
        let normalized_number = normalize_number(&normalize_collector(number));
        if normalized_set.is_empty() || normalized_number.is_empty() {
            return Ok(());
        }
        let set_id = self.intern(&normalized_set)?;
        let number_id = self.intern(&normalized_number)?;
        let record = FooterRecord {
            strings: [
                self.intern(&card.game)?,
                self.intern(&card.id)?,
                self.intern(&card.name)?,
                self.optional(card.oracle_id.as_deref())?,
                self.optional(card.face_name.as_deref())?,
                self.optional(card.illustration_id.as_deref())?,
                self.intern(set)?,
                self.intern(number)?,
                self.optional(card.language.as_deref())?,
            ],
            face_index: card.face_index,
        };
        let index = position(self.index.records.len())?;
        self.index.records.push(record);
        if !self.index.set_lengths.contains(&normalized_set.len()) {
            self.index.set_lengths.push(normalized_set.len());
        }
        self.index.sets.insert(normalized_set.clone(), set_id);
        self.index
            .numbers
            .insert(normalized_number.clone(), number_id);
        self.index
            .postings
            .entry((set_id, number_id))
            .or_default()
            .push(index);
        // These Secret Lair catalogue suffixes distinguish metadata variants or
        // meld faces but are absent from the printed collector column. Retain
        // every alias collision instead of pretending it identifies a printing.
        if normalized_set == "sld"
            && let Some(alias) = printed_secret_lair_number(&normalized_number)
        {
            let alias_id = self.intern(&alias)?;
            self.index.numbers.insert(alias, alias_id);
            self.index
                .postings
                .entry((set_id, alias_id))
                .or_default()
                .push(index);
        }
        Ok(())
    }
}

/// Recognize documented unprinted Secret Lair suffixes without discarding other suffixes.
fn printed_secret_lair_number(number: &str) -> Option<String> {
    let prefix: String = number.chars().take_while(char::is_ascii_digit).collect();
    if !prefix.is_empty() && matches!(&number[prefix.len()..], "★" | "φ" | "a" | "b") {
        Some(prefix)
    } else {
        None
    }
}

/// Convert a compact-table offset without consuming the absent-field sentinel.
fn position(length: usize) -> Result<u32, Error> {
    u32::try_from(length)
        .ok()
        .filter(|value| *value != ABSENT)
        .ok_or_else(|| Error::InvalidCatalog("footer index exceeds its 32-bit capacity".into()))
}

/// Indexed exact-pair and ambiguous-language regressions use invented metadata only.
#[cfg(test)]
mod tests {
    use super::*;

    /// A small printable face with a deliberately substantial noncandidate payload.
    fn card(id: &str, set: &str, number: &str, language: &str) -> CardMetadata {
        CardMetadata {
            game: "invented".into(),
            id: id.into(),
            name: "Invented Portrait".into(),
            oracle_id: Some("identity".into()),
            illustration_id: Some("art".into()),
            set: Some(set.into()),
            collector_number: Some(number.into()),
            language: Some(language.into()),
            oracle_text: Some(
                "This wording is not retained by the compact footer projection.".into(),
            ),
            ..CardMetadata::default()
        }
    }

    /// Complete postings cannot hide a sixth printing, language conflict or physical face.
    #[test]
    fn complete_postings_retain_languages_and_match_full_index_order() {
        let mut cards: Vec<_> = (0..8)
            .map(|number| card(&format!("face-{number}"), "tst", "00127", "en"))
            .collect();
        cards.push(card("french", "tst", "127", "fr"));
        let mut back = cards[0].clone();
        back.face_index = 1;
        cards.push(back);
        let compact = FooterIndex::from_metadata(&cards).unwrap();
        let full = super::super::TextIndex::from_metadata(cards).unwrap();
        let hits = compact.propose_footer_printings("00127/269 R TST • FR");
        let expected = full.search_footer_printings("00127/269 R TST • FR", usize::MAX);
        assert_eq!(hits.len(), 10);
        assert_eq!(hits[0].card.id, "french");
        assert_eq!(hits[0].language_match, Some(true));
        assert!(
            hits[1..]
                .iter()
                .all(|hit| hit.language_match == Some(false))
        );
        assert_eq!(
            hits.iter()
                .map(|hit| (&hit.card.id, hit.card.face_index, hit.score))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|hit| (&hit.card.id, hit.card.face_index, hit.score))
                .collect::<Vec<_>>()
        );
        assert!(
            hits.iter()
                .all(|hit| hit.card.oracle_text.is_none() && hit.card.source.is_none())
        );
        assert_eq!(
            compact
                .propose_footer_printings("127 TST")
                .iter()
                .filter(|hit| hit.language_match.is_none())
                .count(),
            10
        );
    }

    /// Denominators, copyright years, suffixes and number-only observations remain distinct.
    #[test]
    fn collector_keys_preserve_existing_false_positive_guards() {
        let cards = [
            card("number", "m15", "127", "en"),
            card("denominator", "m15", "269", "en"),
            card("year", "m15", "2024", "en"),
            card("suffix", "m15", "127a", "en"),
            card("star", "m15", "127★", "en"),
        ];
        let index = FooterIndex::from_metadata(cards).unwrap();
        assert_eq!(
            index
                .propose_footer_printings("00127 / 269 M15 ©2024")
                .iter()
                .map(|hit| hit.card.id.as_str())
                .collect::<Vec<_>>(),
            ["number"]
        );
        assert_eq!(
            index.propose_footer_printings("M15 00127★")[0].card.id,
            "star"
        );
        assert_eq!(
            index.propose_footer_printings("M15 127a")[0].card.id,
            "suffix"
        );
        for footer in ["127", "M15 ©2024", "M15 2024 ©", "M15 1127"] {
            assert!(
                index.propose_footer_printings(footer).is_empty(),
                "{footer}"
            );
        }
    }

    /// Secret Lair year-range numbers require collector-column context, never a copyright row.
    #[test]
    fn secret_lair_four_digit_collectors_do_not_use_copyright_years() {
        let index = FooterIndex::from_metadata([card("collector", "sld", "2024", "en")]).unwrap();
        for footer in [
            "02024 R\nSLD EN ©2025",
            "2024 R\nSLD EN\n©2025",
            "SLD 2024 EN",
        ] {
            assert_eq!(
                index.propose_footer_printings(footer)[0].card.id,
                "collector",
                "{footer}"
            );
        }
        for footer in [
            "SLD EN ©2024",
            "SLD © 2024",
            "SLD EN 2024 Wizards of the Coast",
            "SLD 2024 ©",
        ] {
            assert!(
                index.propose_footer_printings(footer).is_empty(),
                "{footer}"
            );
        }
    }

    /// Literal rarity boundaries preserve full collector fields and explicit language conflicts.
    #[test]
    fn attached_rarity_with_separate_set_preserves_collector_and_language() {
        let index = FooterIndex::from_metadata([
            card("collector", "sld", "001202", "en"),
            card("suffix", "m15", "127a", "en"),
            card("star", "m15", "127★", "en"),
            card("denominator", "m15", "269", "en"),
        ])
        .unwrap();
        for footer in ["M1202 SLD·EN", "R001202 SLD EN", "M1202 SLDEN"] {
            let hits = index.propose_footer_printings(footer);
            assert_eq!(hits.len(), 1, "{footer}");
            assert_eq!(hits[0].card.id, "collector");
            assert_eq!(hits[0].card.collector_number.as_deref(), Some("001202"));
            assert_eq!(hits[0].language_match, Some(true));
        }
        for (footer, expected) in [
            ("U00127a / 269 M15 EN", "suffix"),
            ("C00127★ M15 EN", "star"),
        ] {
            let hits = index.propose_footer_printings(footer);
            assert_eq!(hits.len(), 1, "{footer}");
            assert_eq!(hits[0].card.id, expected);
        }
        let hits = index.propose_footer_printings("M1202 SLD JP");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].language_match, Some(false));
        for footer in [
            "M1202",
            "MM1202 SLD EN",
            "A1202 SLD EN",
            "M12O2 SLD EN",
            "M1202 S1D EN",
            "M1202a SLD EN",
            "R00007 / 1202 SLD EN",
            "SLD EN ©M1202",
        ] {
            assert!(
                index.propose_footer_printings(footer).is_empty(),
                "{footer}"
            );
        }
        let full = super::super::TextIndex::from_metadata([
            card("collector", "sld", "001202", "en"),
            card("denominator", "sld", "269", "en"),
        ])
        .unwrap();
        let hits = full.search_footer_printings("M001202 / 269 SLD EN", usize::MAX);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].card.id, "collector");
    }

    /// Core, Commander, Portal and token set codes cannot supply their own collector number.
    #[test]
    fn registered_sets_are_not_reinterpreted_as_attached_rarity_collectors() {
        for (set, phantom_number) in [("m15", "15"), ("c17", "17"), ("p02", "2"), ("t30a", "30a")] {
            let cards = [
                card("wanted", set, "127", "en"),
                card("phantom", set, phantom_number, "en"),
                card("cross-pair", "sld", phantom_number, "en"),
            ];
            let compact = FooterIndex::from_metadata(cards.iter()).unwrap();
            let full = super::super::TextIndex::from_metadata(cards.iter().cloned()).unwrap();
            let check = |footer: &str, expected: &[&str]| {
                let observations = super::super::TextObservations {
                    footer: footer.into(),
                    ..super::super::TextObservations::default()
                };
                for hits in [
                    compact.propose_footer_printings(footer),
                    full.search_footer_printings(footer, usize::MAX),
                    full.search(&observations),
                    super::super::search(cards.iter(), &observations),
                ] {
                    assert_eq!(
                        hits.iter()
                            .map(|hit| hit.card.id.as_str())
                            .collect::<Vec<_>>(),
                        expected,
                        "{footer}"
                    );
                }
            };
            check(&format!("{} EN", set.to_uppercase()), &[]);
            check(&format!("{} SLD EN", set.to_uppercase()), &[]);
            check(&format!("00127/269 {} EN", set.to_uppercase()), &["wanted"]);
            check(
                &format!("R00127/269 {} EN", set.to_uppercase()),
                &["wanted"],
            );
        }
    }

    /// Year-like collectors require literal family-set context and never use a denominator.
    #[test]
    fn secret_lair_family_year_collectors_keep_copyright_and_denominator_guards() {
        let sets = ["sld", "slx", "slc", "slp", "slu", "pssc"];
        let index = FooterIndex::from_metadata(
            sets.into_iter()
                .flat_map(|set| [card(set, set, "1995", "en"), card(set, set, "2020", "en")])
                .enumerate()
                .map(|(position, mut card)| {
                    card.id.push_str(&position.to_string());
                    card
                })
                .chain([card("unrelated", "tst", "2020", "en")]),
        )
        .unwrap();
        for set in sets {
            for footer in [
                format!("1995 {set}·EN"),
                format!("2020{set}·EN"),
                format!("R2020 {set} EN"),
                format!("M01995 {set} EN"),
                format!("R2020 / 9999 {set} EN"),
            ] {
                let hits = index.propose_footer_printings(&footer);
                assert_eq!(hits.len(), 1, "{footer}");
                assert_eq!(hits[0].card.set.as_deref(), Some(set));
                assert_eq!(hits[0].language_match, Some(true));
            }
        }
        for footer in [
            "2020 TST EN",
            "R2020 TST EN",
            "SLC EN ©2020",
            "SLC EN ©M2020",
            "SLC EN M2020 Wizards of the Coast",
            "SLC 2020 ©",
            "M2020 © SLC EN",
            "007/2020 SLC EN",
            "007 / 2020 SLC EN",
            "R007 / 2020 SLC EN",
            "2020 S1C EN",
            "©2020SLCEN",
        ] {
            assert!(
                index.propose_footer_printings(footer).is_empty(),
                "{footer}"
            );
        }
    }

    /// Documented unprinted suffix aliases retain metadata variants and all collisions.
    #[test]
    fn secret_lair_printed_aliases_keep_physical_ambiguity() {
        let index = FooterIndex::from_metadata([
            card("base", "sld", "1494", "en"),
            card("star", "sld", "1494★", "en"),
            card("greek", "sld", "681Φ", "en"),
            card("meld", "sld", "1336b", "en"),
        ])
        .unwrap();
        let hits = index.propose_footer_printings("1494 R\nSLD EN");
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .any(|hit| hit.card.collector_number.as_deref() == Some("1494★")
                    && hit.reasons.iter().any(|reason| reason.contains("alias")))
        );
        assert_eq!(
            index.propose_footer_printings("681 SLD EN")[0]
                .card
                .collector_number
                .as_deref(),
            Some("681Φ")
        );
        assert_eq!(
            index.propose_footer_printings("1336 SLD EN")[0]
                .card
                .collector_number
                .as_deref(),
            Some("1336b")
        );
    }

    /// A printed counterpart annotation cannot manufacture a second set/number pairing.
    #[test]
    fn universes_within_counterpart_is_not_an_observed_printing_key() {
        let index = FooterIndex::from_metadata([
            card("physical", "slx", "1", "en"),
            card("counterpart", "sld", "340", "en"),
            card("cross-pair", "slx", "340", "en"),
        ])
        .unwrap();
        for footer in [
            "001 M = SLD 340\nSLX • EN",
            "001 M =SLD340 SLX EN",
            "001 M = SLD 340 SLX EN",
            "001 M =SLD340★ SLX EN",
            "001 M =SLD340SLX EN",
            "M001 =SLD340 SLX EN",
        ] {
            let hits = index.propose_footer_printings(footer);
            assert_eq!(hits.len(), 1, "{footer}");
            assert_eq!(hits[0].card.id, "physical");
        }
    }

    /// Duplicate guards use game/printing/face, not a cross-game printing identifier alone.
    #[test]
    fn duplicate_guard_is_scoped_to_game_and_physical_face() {
        let original = card("same", "tst", "127", "en");
        assert!(FooterIndex::from_metadata([original.clone(), original.clone()]).is_err());
        let mut other_game = original.clone();
        other_game.game = "other".into();
        let index = FooterIndex::from_metadata([original, other_game]).unwrap();
        assert_eq!(index.propose_footer_printings("TST 127 EN").len(), 2);
    }

    /// Registered boundaries recover glued fields without changing any OCR characters or suffixes.
    #[test]
    fn adjacent_literal_number_set_and_language_fields_use_direct_known_keys() {
        let index = FooterIndex::from_metadata([
            card("plain", "sld", "83", "en"),
            card("rarity", "sld", "1704", "en"),
            card("fraction", "gn2", "42", "en"),
            card("suffix", "sld", "13a", "en"),
            card("not-suffix", "sld", "13", "en"),
            card("year", "sld", "2024", "en"),
        ])
        .unwrap();
        for (footer, expected) in [
            ("083SLD★EN", "plain"),
            ("083 SLDEN", "plain"),
            ("R1704SLD·EN", "rarity"),
            ("042/064GN2·EN", "fraction"),
            ("13aSLDEN", "suffix"),
        ] {
            let hits = index.propose_footer_printings(footer);
            assert_eq!(hits.len(), 1, "{footer}");
            assert_eq!(hits[0].card.id, expected);
            assert_eq!(hits[0].language_match, Some(true));
            assert!(
                hits[0]
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("no OCR characters"))
            );
        }
        assert!(index.propose_footer_printings("©2024SLDEN").is_empty());
        assert!(index.propose_footer_printings("083S1DEN").is_empty());
    }

    /// Printed JP/KR/CS/CT codes preserve agreement and disagreement with catalogue languages.
    #[test]
    fn printed_language_aliases_are_not_treated_as_unknown() {
        let english = card("english", "sld", "83", "en");
        let mut japanese = english.clone();
        japanese.id = "japanese".into();
        japanese.language = Some("ja".into());
        let compact = FooterIndex::from_metadata([english.clone(), japanese.clone()]).unwrap();
        for footer in ["083 SLD JP", "083SLDJP", "083 SLDJP", "083 SLD JA JP"] {
            let hits = compact.propose_footer_printings(footer);
            assert_eq!(hits[0].card.id, "japanese");
            assert_eq!(hits[0].language_match, Some(true));
            assert_eq!(hits[1].language_match, Some(false));
        }
        let full = super::super::TextIndex::from_metadata([english, japanese]).unwrap();
        assert_eq!(
            full.search_footer_printings("083 SLD JP", usize::MAX)[0].language_match,
            Some(true)
        );
        for (printed, canonical) in [("kr", "ko"), ("cs", "zhs"), ("ct", "zht")] {
            assert_eq!(footer_language_code(printed), Some(canonical));
        }
    }
}
