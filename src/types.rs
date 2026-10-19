//! Stable data exchanged between game catalogues, matchers, and callers.

use serde::{Deserialize, Serialize};

use crate::Error;

/// The game, printing, and physical face associated with one reference image.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CardMetadata {
    /// Game namespace, such as `mtg`.
    pub game: String,
    /// Stable printing identifier; Scryfall UUID for Magic cards.
    pub id: String,
    /// Complete card name, including both faces for double-faced Magic cards.
    pub name: String,
    /// Semantic card identifier grouping reprints, when supplied by the catalogue.
    pub oracle_id: Option<String>,
    /// Edition or expansion code.
    pub set: Option<String>,
    /// Collector number within the expansion.
    pub collector_number: Option<String>,
    /// Printed language code.
    pub language: Option<String>,
    /// Zero-based physical face index; zero for shared-image split/adventure cards.
    pub face_index: u32,
    /// Face-specific name, when different from the complete card name.
    pub face_name: Option<String>,
    /// Artwork identifier, which can be shared by different printings.
    pub illustration_id: Option<String>,
    /// Attribution or catalogue URL, when available.
    pub source: Option<String>,
    /// Current Oracle rules text for this face; wording may differ from the image.
    #[serde(default)]
    pub oracle_text: Option<String>,
    /// Printed rules text in the printing's language, when the catalogue supplies it.
    #[serde(default)]
    pub printed_text: Option<String>,
    /// Printed localized title, when different from the English catalogue title.
    #[serde(default)]
    pub printed_name: Option<String>,
    /// Face-specific card type, retained for diagnostics and future text constraints.
    #[serde(default)]
    pub type_line: Option<String>,
    /// Scryfall layout identifier; a portrait region template is not valid for every layout.
    #[serde(default)]
    pub layout: Option<String>,
}

/// Controls retrieval size and acceptance of visual evidence.
///
/// [`Self::default`] uses the portable matcher's score scale. Prefer
/// [`crate::Matcher::default_options`] when selecting defaults for another backend.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MatchOptions {
    /// Maximum number of candidate printings returned; must be positive.
    pub top_k: usize,
    /// Minimum absolute similarity required for an accepted match, in `[0, 1]`.
    pub min_similarity: f32,
    /// Minimum similarity lead over a competing printing or card, in `[0, 1]`.
    pub min_margin: f32,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            top_k: 5,
            min_similarity: 0.80,
            min_margin: 0.03,
        }
    }
}

impl MatchOptions {
    /// Reject settings that cannot produce a finite, meaningful ranking.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] for a zero candidate count or a
    /// nonfinite or out-of-range threshold.
    pub fn validate(&self) -> Result<(), Error> {
        if self.top_k == 0 {
            return Err(Error::InvalidOptions("top_k must be positive".into()));
        }
        for (name, value) in [
            ("min_similarity", self.min_similarity),
            ("min_margin", self.min_margin),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(Error::InvalidOptions(format!(
                    "{name} must be finite and in [0, 1]"
                )));
            }
        }
        Ok(())
    }
}

/// Whether evidence supports a unique match, competing matches, or no match.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    /// Absolute evidence and the lead over other candidates meet the thresholds.
    Matched,
    /// Strong visual evidence exists, but competing candidates are too similar.
    Ambiguous,
    /// Absolute visual evidence is insufficient, even if one candidate ranks first.
    Unknown,
}

/// One candidate printing, with independent absolute evidence and relative confidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MatchCandidate {
    /// Catalogue identity of this reference face.
    pub card: CardMetadata,
    /// Absolute visual similarity in `[0, 1]`; not normalized over the catalogue.
    pub similarity: f32,
    /// Heuristic match confidence in `[0, 1]`, not a calibrated probability.
    pub confidence: f32,
    /// Clockwise right-angle rotation applied to the query to match this reference.
    pub rotation_degrees: u16,
}

/// Ranked references plus separate printing and semantic-identity decisions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Recognition {
    /// Decision about the exact printing and face.
    pub status: MatchStatus,
    /// Decision about the card identity, grouping printings by oracle ID when known.
    pub identity_status: MatchStatus,
    /// Confidence in the best exact printing; close reprints reduce this score.
    pub confidence: f32,
    /// Confidence in the best semantic card identity, independently of its printing.
    pub identity_confidence: f32,
    /// Ranked candidate printings, limited by [`MatchOptions::top_k`].
    pub candidates: Vec<MatchCandidate>,
}
