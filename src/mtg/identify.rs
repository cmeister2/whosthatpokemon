//! Combine independent text proposals with actual, explicitly scoped image evidence.
//!
//! Exhaustive text search retains independent title, rules and footer results.
//! The live indexed route combines bounded title identities with complete exact
//! footer postings. A footer cannot erase contradictory title evidence, and text
//! scores never become visual confidence.
//! The thresholds below are conservative policy choices, not learned weights or
//! calibrated probabilities. Complete visual decisions remain distinct from a
//! geometrically verified shortlist whose omitted geometry is still unknown.

#[cfg(not(target_arch = "wasm32"))]
use image::DynamicImage;
use serde::{Deserialize, Serialize};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use super::evidence::{
    FooterProposer, TextIndex, TextMatch, TextObservations, TitleIdentityProposal, TitleProposer,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::matching::{CandidateOptions, CandidateOrientations, ShardedIndex, ShortlistOptions};
use crate::matching::{CandidateRecognition, ShortlistScope, ShortlistedRecognition};
use crate::{CardMetadata, Error, MatchOptions, MatchStatus, Recognition};

/// Near-best title similarity required to treat disagreement as a strong conflict.
const STRONG_TITLE: f32 = 0.90;
/// Near-best title similarity sufficient to corroborate actual image verification.
const SUPPORTING_TITLE: f32 = 0.75;
/// Near-best rules similarity required to treat disagreement as a strong conflict.
const STRONG_RULES: f32 = 0.80;
/// Minimum reference-rules coverage for a strong rules conflict.
const STRONG_RULES_COVERAGE: f32 = 0.65;
/// Near-best rules similarity sufficient to corroborate actual image verification.
const SUPPORTING_RULES: f32 = 0.60;
/// Minimum reference-rules coverage for supporting rules evidence.
const SUPPORTING_RULES_COVERAGE: f32 = 0.45;
/// Candidates farther below a field's best similarity do not corroborate a decision.
const FIELD_TIE_WINDOW: f32 = 0.05;

/// Independent retrieval lists and their deterministic physical-face union.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TextCandidates {
    /// Identity-grouped title proposals used by the indexed title-first path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_identities: Vec<TitleIdentityProposal>,
    /// Title matches: printing-ranked for exhaustive search, identity-grouped for title-first.
    pub title: Vec<TextMatch>,
    /// At most five printings retrieved using only the observed rules.
    pub rules: Vec<TextMatch>,
    /// Exact collector-footer printings; indexed field retrieval retains every match.
    pub footer: Vec<TextMatch>,
    /// Visual proposals deduplicated by game, printing and face.
    ///
    /// The title-first path needs one representative per semantic identity because
    /// the visual index expands its identity and illustration postings directly.
    pub hints: Vec<CardMetadata>,
}

/// Retrieve each observed text field independently, without making an acceptance decision.
///
/// Each field uses [`TextIndex::search`]'s existing five-printing limit. Repeated
/// printings can therefore occupy a field's entire list; this is a proposal
/// union, not an exhaustive list of every plausible text identity.
#[must_use]
pub fn collect_text_candidates(
    index: &TextIndex,
    observations: &TextObservations,
) -> TextCandidates {
    let title = index.search(&TextObservations {
        title: observations.title.clone(),
        ..TextObservations::default()
    });
    let rules = index.search(&TextObservations {
        rules: observations.rules.clone(),
        ..TextObservations::default()
    });
    let footer = index.search(&TextObservations {
        footer: observations.footer.clone(),
        ..TextObservations::default()
    });
    let mut hints = Vec::new();
    for candidate in title.iter().chain(&rules).chain(&footer) {
        if !hints.iter().any(|card| same_face(card, &candidate.card)) {
            hints.push(candidate.card.clone());
        }
    }
    TextCandidates {
        title_identities: Vec::new(),
        title,
        rules,
        footer,
        hints,
    }
}

/// Retrieve a bounded identity-first title set without scanning catalogue records.
///
/// Exact normalized titles use a direct posting. Fuzzy input first uses the text
/// index's character postings and then scores only its bounded alias set. The
/// returned title and hint lists keep one representative per identity; the
/// visual index expands each representative through its direct identity and
/// illustration postings.
#[must_use]
pub fn collect_title_candidates<T: TitleProposer + ?Sized>(
    index: &T,
    observed_title: &str,
    max_identities: usize,
) -> TextCandidates {
    let title_identities = index.propose_title_identities(observed_title, max_identities);
    let title = title_identities
        .iter()
        .map(|candidate| TextMatch {
            card: candidate.representative.clone(),
            title_similarity: Some(candidate.title_similarity),
            rules_similarity: None,
            rules_coverage: None,
            footer_match: false,
            language_match: None,
            score: candidate.title_similarity,
            reasons: vec![if candidate.exact {
                "normalized title matched exactly".into()
            } else {
                "bounded fuzzy title alias matched".into()
            }],
        })
        .collect();
    let mut hints = Vec::new();
    let best_title = title_identities
        .first()
        .map_or(0.0, |candidate| candidate.title_similarity);
    for candidate in title_identities.iter().filter(|candidate| {
        candidate.title_similarity >= SUPPORTING_TITLE
            && candidate.title_similarity + FIELD_TIE_WINDOW >= best_title
    }) {
        if !hints
            .iter()
            .any(|card| same_face(card, &candidate.representative))
        {
            hints.push(candidate.representative.clone());
        }
    }
    TextCandidates {
        title_identities,
        title,
        rules: Vec::new(),
        footer: Vec::new(),
        hints,
    }
}

/// Merge bounded title identities with complete indexed collector-footer proposals.
///
/// Footer lookup requires an exact set and complete collector token. All matching
/// faces are retained so language or identity ambiguity cannot disappear behind
/// a result limit. Exact footer faces precede title representatives in the visual
/// hint list; the matcher reserves supplied physical faces before expanding their
/// identity and illustration postings. Language-conflicting matches remain in
/// the evidence but do not propose new image candidates.
#[must_use]
pub fn collect_field_candidates<T: TitleProposer + ?Sized, F: FooterProposer + ?Sized>(
    title_index: &T,
    footer_index: &F,
    observations: &TextObservations,
    max_title_identities: usize,
) -> TextCandidates {
    let mut text = collect_title_candidates(title_index, &observations.title, max_title_identities);
    text.footer = footer_index.propose_footer_printings(&observations.footer);
    let mut hints = Vec::new();
    for card in text
        .footer
        .iter()
        .filter(|hit| hit.footer_match && hit.language_match != Some(false))
        .map(|hit| &hit.card)
        .chain(text.hints.iter())
    {
        if !hints.iter().any(|existing| same_face(existing, card)) {
            hints.push(card.clone());
        }
    }
    text.hints = hints;
    text
}

/// A semantic identity, explicitly separate from any accepted physical printing.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CardIdentity {
    /// Game namespace used to scope identifiers.
    pub game: String,
    /// Oracle identifier grouping printings, when available.
    pub oracle_id: Option<String>,
    /// Printing fallback used only when no Oracle identifier is available.
    pub printing_id: Option<String>,
    /// Display name supplied by the representative catalogue entry.
    pub name: String,
}

impl CardIdentity {
    /// Preserve the portable matcher's exact game-scoped semantic grouping.
    fn from_card(card: &CardMetadata) -> Self {
        Self {
            game: card.game.clone(),
            oracle_id: card.oracle_id.clone(),
            printing_id: card.oracle_id.is_none().then(|| card.id.clone()),
            name: card.name.clone(),
        }
    }
}

/// Evidence used for a decision or taking part in an explicit disagreement.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// Independently searched title text.
    Title,
    /// Independently searched rules text.
    Rules,
    /// Exact set and collector-number tokens from the footer.
    Footer,
    /// Unchanged complete-gallery global descriptor ranking.
    VisualGlobal,
    /// Actual successful correspondence and registered-artwork verification.
    VisualGeometry,
}

/// Acceptance scope of the combined semantic-identity decision.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CombinedStatus {
    /// A complete visual decision survived material text-conflict checks.
    Matched,
    /// Actual geometry and independent text agree within the stated candidate scope.
    ///
    /// Omitted geometry remains unknown; this is not an exact full-gallery match.
    VerifiedShortlist,
    /// Plausible visual evidence remains without sufficient separation or corroboration.
    Ambiguous,
    /// Absolute visual evidence is insufficient; text alone cannot accept a card.
    Unknown,
    /// Strong evidence proposes contradictory identities that remain unresolved.
    Conflict,
}

/// Semantic decision with its visual scope and concrete corroborating sources.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IdentityDecision {
    /// Acceptance, abstention or unresolved conflict.
    pub status: CombinedStatus,
    /// Accepted semantic identity; absent for all abstentions.
    pub card: Option<CardIdentity>,
    /// Scope of the visual evidence actually used for this decision.
    pub scope: ShortlistScope,
    /// Evidence supporting the decision, without invented joint confidence.
    pub sources: Vec<EvidenceSource>,
    /// Human-readable reason and any remaining scope limitation.
    pub reason: String,
}

/// Physical-printing decision, never upgraded using a collector footer alone.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PrintingDecision {
    /// Existing visual acceptance or an abstention; partial geometry never accepts a printing.
    pub status: MatchStatus,
    /// Accepted printing and face, only when a complete visual result identifies it.
    pub card: Option<CardMetadata>,
    /// Scope of the visual decision used for this physical printing.
    pub scope: ShortlistScope,
    /// Explanation of visual acceptance or retained printing ambiguity.
    pub reason: String,
}

/// Whether a disagreement concerns semantic identity or the physical printing.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Different game-scoped Oracle identities, or absent-Oracle printing fallbacks.
    Identity,
    /// Different physical printings or faces within the same semantic identity.
    Printing,
}

/// A retained disagreement, including evidence rejected by a resolved decision.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvidenceConflict {
    /// Semantic or physical-printing disagreement.
    pub kind: ConflictKind,
    /// Left and right evidence channels, in the same order as their alternatives.
    pub sources: [EvidenceSource; 2],
    /// Representative catalogue entries proposed by the first channel.
    pub left: Vec<CardMetadata>,
    /// Representative catalogue entries proposed by the second channel.
    pub right: Vec<CardMetadata>,
    /// Whether accepted visual/corroborating evidence resolved this disagreement.
    pub resolved: bool,
    /// Why the conflict was resolved or why abstention remains necessary.
    pub reason: String,
}

/// Auditable image/text evidence with separate identity and printing decisions.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CombinedRecognition {
    /// Original unmodified observations, including contradictory or empty fields.
    pub observations: TextObservations,
    /// Independent field candidates and the exact geometry-hint union.
    pub text: TextCandidates,
    /// Raw complete-global and scoped geometry results, without confidence rewriting.
    pub visual: ShortlistedRecognition,
    /// Semantic identity decision, which can explicitly accept only a verified shortlist.
    pub identity: IdentityDecision,
    /// Separate exact-printing decision; matching a title or footer cannot upgrade it.
    pub printing: PrintingDecision,
    /// Strong identity disagreements and exact-footer printing disagreements.
    pub conflicts: Vec<EvidenceConflict>,
}

/// OCR-first identification whose image evidence covers only supplied candidates.
///
/// This result can confirm a semantic identity after actual geometry agrees with
/// title evidence. Unless its candidate closure happens to contain the complete
/// gallery, it cannot accept an exact printing or claim separation from identities
/// that OCR did not propose.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CandidateIdentificationTimings {
    /// Retrieve bounded semantic title proposals and any indexed footer faces.
    pub text_retrieval: f64,
    /// Candidate-scoped descriptor and geometric image matching.
    pub visual_matching: f64,
    /// Adapt evidence scope and apply conflict-aware fusion policy.
    pub fusion: f64,
    /// Complete elapsed time, including validation and coordination.
    pub total: f64,
}

/// Final visual search actually represented by candidate identification.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateVisualStrategy {
    /// Indexed title/footer identity and illustration closure under the normal budgets.
    #[default]
    ExpandedTextCandidates,
    /// Only all compatible physical faces from exact collector-footer postings.
    ExactFooterFaces,
}

impl CandidateVisualStrategy {
    /// Omit the legacy default so title-only serialized results remain compatible.
    fn is_default(&self) -> bool {
        *self == Self::ExpandedTextCandidates
    }
}

/// Candidate-scoped title/footer and image evidence with its combined decision.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CandidateIdentification {
    /// Original literal observations supplied by the OCR stage.
    pub observations: TextObservations,
    /// Indexed identity proposals and their expanded physical-face hints.
    pub text: TextCandidates,
    /// Raw candidate-scoped image evidence; no full-gallery scan was performed.
    pub visual: CandidateRecognition,
    /// Semantic identity decision with its incomplete evidence scope retained.
    pub identity: IdentityDecision,
    /// Exact-printing decision, normally ambiguous for a candidate-only search.
    pub printing: PrintingDecision,
    /// Strong title/image disagreements retained for audit and fallback policy.
    pub conflicts: Vec<EvidenceConflict>,
    /// Non-overlapping wall-clock stages for indexed retrieval, vision and fusion.
    #[serde(default)]
    pub timings: CandidateIdentificationTimings,
}

/// Indexed title/footer identification with explicit visual-pass diagnostics.
///
/// The shared result is flattened for JSON consumers while the existing
/// [`CandidateIdentification`] Rust structure and title-only APIs remain intact.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FieldIdentification {
    /// Literal observations, scoped visual evidence and combined decisions.
    #[serde(flatten)]
    pub result: CandidateIdentification,
    /// Final visual pass; exact-footer scoring does not search unrelated printings.
    #[serde(default, skip_serializing_if = "CandidateVisualStrategy::is_default")]
    pub visual_strategy: CandidateVisualStrategy,
    /// Earlier unsuccessful exact-footer pass, when expanded text verification followed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer_verification: Option<CandidateRecognition>,
}

/// Search independent text proposals and verify their union with visual candidates.
///
/// A complete global match can survive a contradictory footer when a strong,
/// unambiguous title corroborates it. A competing strong title requires actual
/// image verification and nonfooter corroboration. Shortlist acceptance requires
/// the combined visual winner and its margin against **all** global competitors,
/// including unverified faces and identities beyond the returned top-k.
///
/// # Errors
///
/// Returns errors for invalid options, invalid query images, or unavailable,
/// corrupt or otherwise invalid visual catalogue data. No network is used.
#[cfg(not(target_arch = "wasm32"))]
pub fn identify(
    visual_index: &ShardedIndex,
    text_index: &TextIndex,
    image: &DynamicImage,
    observations: &TextObservations,
    match_options: &MatchOptions,
    shortlist_options: &ShortlistOptions,
) -> Result<CombinedRecognition, Error> {
    match_options.validate()?;
    shortlist_options.validate()?;
    let text = collect_text_candidates(text_index, observations);
    let visual =
        visual_index.recognize_shortlisted(image, match_options, shortlist_options, &text.hints)?;
    Ok(combine(observations, text, visual, match_options))
}

/// Retrieve title identities and verify only their indexed visual closure.
///
/// This is the latency-bounded path for a rectified camera crop. It performs no
/// complete-gallery descriptor scan. A successful result remains
/// [`CombinedStatus::VerifiedShortlist`] unless the supplied closure happens to
/// cover every enrolled face. Call [`identify`] explicitly for exhaustive visual
/// recovery when latency permits it.
///
/// # Errors
///
/// Returns errors for zero budgets, invalid matching options or image input,
/// unsupported legacy visual storage, or unavailable/corrupt candidate records.
#[cfg(not(target_arch = "wasm32"))]
pub fn identify_title<T: TitleProposer + ?Sized>(
    visual_index: &ShardedIndex,
    text_index: &T,
    image: &DynamicImage,
    observations: &TextObservations,
    match_options: &MatchOptions,
    max_title_identities: usize,
    max_visual_candidates: usize,
) -> Result<CandidateIdentification, Error> {
    identify_title_with(
        visual_index,
        text_index,
        image,
        observations,
        match_options,
        max_title_identities,
        &CandidateOptions {
            max_descriptors: max_visual_candidates,
            max_geometry: max_visual_candidates,
            orientations: CandidateOrientations::UprightAndInverted,
        },
    )
}

/// Retrieve title identities with separate descriptor, geometry and orientation limits.
///
/// This variant lets a live caller score the complete bounded title closure with
/// compact descriptors while loading local features for only a smaller,
/// group-preserving geometric shortlist.
///
/// # Errors
///
/// Returns errors for a zero title budget, invalid matching/candidate options or
/// image input, unsupported legacy storage, or corrupt selected records.
#[cfg(not(target_arch = "wasm32"))]
pub fn identify_title_with<T: TitleProposer + ?Sized>(
    visual_index: &ShardedIndex,
    text_index: &T,
    image: &DynamicImage,
    observations: &TextObservations,
    match_options: &MatchOptions,
    max_title_identities: usize,
    candidate_options: &CandidateOptions,
) -> Result<CandidateIdentification, Error> {
    let total_started = Instant::now();
    match_options.validate()?;
    candidate_options.validate()?;
    if max_title_identities == 0 {
        return Err(Error::InvalidOptions(
            "title identity budget must be positive".into(),
        ));
    }
    let stage_started = Instant::now();
    let text = collect_title_candidates(text_index, &observations.title, max_title_identities);
    let text_retrieval = stage_started.elapsed().as_secs_f64() * 1_000.0;
    let stage_started = Instant::now();
    let visual = visual_index.recognize_candidates_with(
        image,
        match_options,
        candidate_options,
        &text.hints,
    )?;
    let visual_matching = stage_started.elapsed().as_secs_f64() * 1_000.0;
    let stage_started = Instant::now();
    let mut result = combine_candidates(observations, text, visual, match_options)?;
    let fusion = stage_started.elapsed().as_secs_f64() * 1_000.0;
    result.timings = CandidateIdentificationTimings {
        text_retrieval,
        visual_matching,
        fusion,
        total: total_started.elapsed().as_secs_f64() * 1_000.0,
    };
    Ok(result)
}

/// Verify exact collector-footer faces before expanding title and footer identities.
///
/// Both independently observed text channels participate in indexed retrieval;
/// this also supports stylized or unreadable titles. No complete-gallery scoring
/// is performed. A footer can corroborate semantic identity only
/// after actual geometry agrees, every candidate group is covered, its compatible
/// exact matches resolve one identity, and no strong text channel contradicts
/// that identity. Footer-only corroboration additionally requires the same verified
/// face's whole-card descriptor to meet the absolute image threshold, because
/// altered artwork can retain a different semantic card's printed footer.
/// Exact printing and global confidence retain their visual scope.
///
/// # Errors
///
/// Returns an error for invalid budgets/options/input, unavailable selected visual
/// records or matching failure. Indexes are reused; no image or model is downloaded.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub fn identify_fields_with<T: TitleProposer + ?Sized, F: FooterProposer + ?Sized>(
    visual_index: &ShardedIndex,
    title_index: &T,
    footer_index: &F,
    image: &DynamicImage,
    observations: &TextObservations,
    match_options: &MatchOptions,
    max_title_identities: usize,
    candidate_options: &CandidateOptions,
) -> Result<FieldIdentification, Error> {
    let total_started = Instant::now();
    match_options.validate()?;
    candidate_options.validate()?;
    if max_title_identities == 0 {
        return Err(Error::InvalidOptions(
            "title identity budget must be positive".into(),
        ));
    }
    let stage_started = Instant::now();
    let text = collect_field_candidates(
        title_index,
        footer_index,
        observations,
        max_title_identities,
    );
    let text_retrieval = stage_started.elapsed().as_secs_f64() * 1_000.0;
    let footer_faces: Vec<_> = text
        .footer
        .iter()
        .filter(|hit| hit.footer_match && hit.language_match != Some(false))
        .map(|hit| hit.card.clone())
        .collect();
    let mut visual_matching = 0.0;
    let mut fusion = 0.0;
    let mut footer_verification = None;
    if !footer_faces.is_empty() {
        let stage_started = Instant::now();
        let visual = visual_index.recognize_faces_with(
            image,
            match_options,
            candidate_options,
            &footer_faces,
        )?;
        visual_matching += stage_started.elapsed().as_secs_f64() * 1_000.0;
        let stage_started = Instant::now();
        let mut result = combine_candidates(observations, text.clone(), visual, match_options)?;
        fusion += stage_started.elapsed().as_secs_f64() * 1_000.0;
        if result.identity.card.is_some()
            && matches!(
                result.identity.status,
                CombinedStatus::Matched | CombinedStatus::VerifiedShortlist
            )
            && result.identity.sources.contains(&EvidenceSource::Footer)
            && result
                .visual
                .geometry_verified
                .iter()
                .any(|verified| verified.similarity >= match_options.min_similarity)
        {
            result.identity.reason = "Independent text and verified image evidence agree within the exact-footer physical-face candidates; other printings and identities remain outside this visual search scope.".into();
            result.timings = CandidateIdentificationTimings {
                text_retrieval,
                visual_matching,
                fusion,
                total: total_started.elapsed().as_secs_f64() * 1_000.0,
            };
            return Ok(FieldIdentification {
                result,
                visual_strategy: CandidateVisualStrategy::ExactFooterFaces,
                footer_verification: None,
            });
        }
        footer_verification = Some(result.visual);
    }
    let stage_started = Instant::now();
    let visual = visual_index.recognize_candidates_with(
        image,
        match_options,
        candidate_options,
        &text.hints,
    )?;
    visual_matching += stage_started.elapsed().as_secs_f64() * 1_000.0;
    let stage_started = Instant::now();
    let mut result = combine_candidates(observations, text, visual, match_options)?;
    fusion += stage_started.elapsed().as_secs_f64() * 1_000.0;
    result.timings = CandidateIdentificationTimings {
        text_retrieval,
        visual_matching,
        fusion,
        total: total_started.elapsed().as_secs_f64() * 1_000.0,
    };
    Ok(FieldIdentification {
        result,
        visual_strategy: CandidateVisualStrategy::default(),
        footer_verification,
    })
}

/// Apply the same conflict-aware identity policy to already collected candidate evidence.
///
/// This separates retrieval, visual verification and final decision for callers that
/// execute OCR outside Rust. The supplied evidence retains its original search scope;
/// text alone cannot accept a card or establish an exact physical printing.
/// Timing fields remain zero because the caller measures these separate stages.
///
/// # Errors
///
/// Returns an error for invalid match options.
pub fn combine_candidates(
    observations: &TextObservations,
    text: TextCandidates,
    visual: CandidateRecognition,
    match_options: &MatchOptions,
) -> Result<CandidateIdentification, Error> {
    match_options.validate()?;
    let adapted = candidate_shortlist_view(&visual);
    let combined = combine(observations, text.clone(), adapted, match_options);
    Ok(CandidateIdentification {
        observations: observations.clone(),
        text,
        visual,
        identity: combined.identity,
        printing: combined.printing,
        conflicts: combined.conflicts,
        timings: CandidateIdentificationTimings::default(),
    })
}

/// Adapt candidate-only evidence to the existing fusion policy without widening its scope.
fn candidate_shortlist_view(candidate: &CandidateRecognition) -> ShortlistedRecognition {
    let empty_global = Recognition {
        status: MatchStatus::Unknown,
        identity_status: MatchStatus::Unknown,
        confidence: 0.0,
        identity_confidence: 0.0,
        candidates: Vec::new(),
    };
    ShortlistedRecognition {
        recognition: candidate.recognition.clone(),
        global_recognition: empty_global,
        combined_printing_margin: candidate.candidate_printing_margin,
        combined_identity_margin: candidate.candidate_identity_margin,
        scope: candidate.scope,
        search_complete: candidate.search_complete,
        total_references: candidate.total_references,
        candidate_count: candidate.candidate_count,
        geometry_attempted: candidate.geometry_attempted,
        geometry_candidates: candidate.geometry_candidates.clone(),
        geometry_verified_count: candidate.geometry_verified_count,
        budget_exhausted: candidate.descriptor_budget_exhausted,
        ocr_conflict: false,
        omitted_hint_groups: candidate
            .omitted_descriptor_groups
            .saturating_add(candidate.omitted_geometry_groups),
        omitted_visual_groups: 0,
        geometry_verified: candidate.geometry_verified.clone(),
    }
}

/// Strong and supporting alternatives from one independently searched field.
struct FieldEvidence {
    /// Field that generated these alternatives.
    source: EvidenceSource,
    /// Near-best alternatives strong enough to raise a material disagreement.
    strong: Vec<CardMetadata>,
    /// Near-best alternatives sufficient to corroborate successful geometry.
    supporting: Vec<CardMetadata>,
}

impl FieldEvidence {
    /// Whether a field makes a strong proposal disjoint from this identity.
    fn contradicts(&self, card: &CardMetadata) -> bool {
        !self.strong.is_empty() && !self.strong.iter().any(|other| same_identity(card, other))
    }

    /// Whether every near-best alternative corroborates this one semantic identity.
    fn supports(&self, card: &CardMetadata, strong: bool) -> bool {
        let alternatives = if strong {
            &self.strong
        } else {
            &self.supporting
        };
        !alternatives.is_empty() && alternatives.iter().all(|other| same_identity(card, other))
    }

    /// Whether this field's retained alternatives include the visual identity.
    fn corroborates(&self, card: &CardMetadata) -> bool {
        !self.supporting.is_empty()
            && self
                .supporting
                .iter()
                .any(|other| same_identity(card, other))
    }
}

/// Derive material conflicts and corroboration without altering retrieval scores.
fn field_evidence(text: &TextCandidates) -> [FieldEvidence; 3] {
    let title_best = text
        .title
        .iter()
        .filter_map(|hit| hit.title_similarity)
        .fold(0.0_f32, f32::max);
    let rules_best = text
        .rules
        .iter()
        .filter_map(|hit| hit.rules_similarity)
        .fold(0.0_f32, f32::max);
    let title = |threshold| {
        unique_identities(text.title.iter().filter(|hit| {
            hit.title_similarity
                .is_some_and(|score| score >= threshold && score + FIELD_TIE_WINDOW >= title_best)
        }))
    };
    let rules = |threshold, coverage| {
        unique_identities(text.rules.iter().filter(|hit| {
            hit.rules_similarity
                .is_some_and(|score| score >= threshold && score + FIELD_TIE_WINDOW >= rules_best)
                && hit.rules_coverage.is_some_and(|score| score >= coverage)
        }))
    };
    let footer = unique_identities(
        text.footer
            .iter()
            .filter(|hit| hit.footer_match && hit.language_match != Some(false)),
    );
    [
        FieldEvidence {
            source: EvidenceSource::Title,
            strong: title(STRONG_TITLE),
            supporting: title(SUPPORTING_TITLE),
        },
        FieldEvidence {
            source: EvidenceSource::Rules,
            strong: rules(STRONG_RULES, STRONG_RULES_COVERAGE),
            supporting: rules(SUPPORTING_RULES, SUPPORTING_RULES_COVERAGE),
        },
        FieldEvidence {
            source: EvidenceSource::Footer,
            strong: footer.clone(),
            supporting: footer,
        },
    ]
}

/// Collapse repeated printing hits while preserving first-seen representative metadata.
fn unique_identities<'a>(hits: impl Iterator<Item = &'a TextMatch>) -> Vec<CardMetadata> {
    let mut cards = Vec::new();
    for hit in hits {
        if !cards.iter().any(|card| same_identity(card, &hit.card)) {
            cards.push(hit.card.clone());
        }
    }
    cards
}

/// Apply the explicit fusion policy to immutable image evidence.
fn combine(
    observations: &TextObservations,
    text: TextCandidates,
    visual: ShortlistedRecognition,
    options: &MatchOptions,
) -> CombinedRecognition {
    let fields = field_evidence(&text);
    let mut conflicts = collect_conflicts(&fields, &text, &visual, options);
    let global = visual.global_recognition.candidates.first();
    let combined = visual.recognition.candidates.first();
    let mut accepted = None;
    let mut identity = IdentityDecision {
        status: CombinedStatus::Unknown,
        card: None,
        scope: visual.scope,
        sources: Vec::new(),
        reason: "No sufficient absolute image evidence; text proposals do not accept a card."
            .into(),
    };
    if let Some(best) = global {
        let nonfooter_conflict = fields[..2]
            .iter()
            .any(|field| field.contradicts(&best.card));
        let footer_conflict = fields[2].contradicts(&best.card);
        if visual.global_recognition.identity_status == MatchStatus::Matched
            && !nonfooter_conflict
            && (!footer_conflict || fields[0].supports(&best.card, true))
            && combined.is_none_or(|winner| same_identity(&winner.card, &best.card))
            && sufficient_lead(best.similarity, visual.combined_identity_margin, options)
        {
            let mut sources = vec![EvidenceSource::VisualGlobal];
            append_supporting_sources(&mut sources, &fields, &best.card);
            identity = accepted_identity(
                &best.card,
                CombinedStatus::Matched,
                ShortlistScope::FullGalleryGlobal,
                sources,
                if footer_conflict {
                    "Complete global visual identity and strong title agree; the contradictory footer is rejected."
                } else {
                    "Complete global visual identity has no unresolved strong text disagreement."
                },
            );
            accepted = Some((
                &visual.global_recognition,
                ShortlistScope::FullGalleryGlobal,
            ));
        }
    }
    if accepted.is_none()
        && let Some(best) = combined
    {
        let strong_conflict = fields.iter().any(|field| field.contradicts(&best.card));
        let nonfooter_support = fields[..2].iter().any(|field| {
            if visual.scope == ShortlistScope::SuppliedCandidates {
                field.corroborates(&best.card)
            } else {
                field.supports(&best.card, false)
            }
        });
        // Reused or altered artwork can preserve the original collector footer
        // while changing the semantic card. When a footer supplies the only text
        // corroboration, artwork homography alone is insufficient: the same
        // verified face must also pass the existing whole-card descriptor gate.
        let footer_global_agrees = visual.geometry_verified.iter().any(|verification| {
            same_identity(&verification.card, &best.card)
                && verification.similarity >= options.min_similarity
                && verification.global_similarity >= options.min_similarity
        });
        let footer_support = visual.scope == ShortlistScope::SuppliedCandidates
            && fields[2].supports(&best.card, false)
            && !strong_conflict
            && footer_global_agrees;
        let verified = visual.geometry_verified.iter().any(|verification| {
            same_identity(&verification.card, &best.card)
                && verification.similarity >= options.min_similarity
        });
        let footer_needs_geometry = visual.scope == ShortlistScope::SuppliedCandidates
            && !text.footer.is_empty()
            && !nonfooter_support;
        if visual.search_complete
            && visual.recognition.identity_status == MatchStatus::Matched
            && !strong_conflict
            && (!footer_needs_geometry || (footer_support && verified))
        {
            let geometry_verified = visual
                .geometry_verified
                .iter()
                .any(|verified| same_identity(&verified.card, &best.card));
            let mut sources = vec![if visual.scope == ShortlistScope::SuppliedCandidates
                && geometry_verified
            {
                EvidenceSource::VisualGeometry
            } else {
                EvidenceSource::VisualGlobal
            }];
            if geometry_verified && !sources.contains(&EvidenceSource::VisualGeometry) {
                sources.push(EvidenceSource::VisualGeometry);
            }
            if visual.scope == ShortlistScope::SuppliedCandidates {
                append_corroborating_sources(&mut sources, &fields, &best.card);
            } else {
                append_supporting_sources(&mut sources, &fields, &best.card);
            }
            identity = accepted_identity(
                &best.card,
                CombinedStatus::Matched,
                visual.scope,
                sources,
                "Complete visual identity has no unresolved strong text disagreement.",
            );
            accepted = Some((&visual.recognition, visual.scope));
        } else {
            let sufficient_margin =
                sufficient_lead(best.similarity, visual.combined_identity_margin, options);
            let groups_covered = visual.omitted_hint_groups == 0
                && visual.omitted_visual_groups == 0
                && (visual.scope != ShortlistScope::SuppliedCandidates || !visual.budget_exhausted);
            if verified
                && sufficient_margin
                && (nonfooter_support || footer_support)
                && groups_covered
                && (visual.scope != ShortlistScope::SuppliedCandidates || !strong_conflict)
            {
                let mut sources = vec![EvidenceSource::VisualGeometry];
                if visual.scope == ShortlistScope::SuppliedCandidates {
                    append_corroborating_sources(&mut sources, &fields, &best.card);
                } else {
                    append_supporting_sources(&mut sources, &fields, &best.card);
                }
                identity = accepted_identity(
                    &best.card,
                    if visual.search_complete {
                        CombinedStatus::Matched
                    } else {
                        CombinedStatus::VerifiedShortlist
                    },
                    visual.scope,
                    sources,
                    if footer_support && !nonfooter_support {
                        "Actual geometry, whole-card appearance and an unambiguous exact collector footer agree within every supplied candidate group; identities outside this indexed candidate scope remain untested."
                    } else if visual.search_complete {
                        "Actual geometry and independent nonfooter text resolve the complete visual ranking."
                    } else if visual.scope == ShortlistScope::SuppliedCandidates {
                        "Actual geometry and independent nonfooter text agree with the winner and margin among every supplied candidate group; identities not proposed by OCR remain untested."
                    } else {
                        "Actual geometry and independent nonfooter text agree with the winner and margin over all global competitors; untested geometry remains unknown."
                    },
                );
                accepted = Some((&visual.recognition, visual.scope));
            }
        }
    }
    if accepted.is_none() {
        let strong_image = global
            .into_iter()
            .chain(combined)
            .any(|candidate| candidate.similarity >= options.min_similarity);
        let unresolved_identity_conflict = conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::Identity);
        if unresolved_identity_conflict {
            identity.status = CombinedStatus::Conflict;
            identity.reason = "Strong evidence proposes contradictory identities; image verification, corroboration or competitor separation is insufficient to resolve them.".into();
        } else if strong_image {
            identity.status = CombinedStatus::Ambiguous;
            identity.reason = "Plausible image evidence remains, but identity separation, verified text corroboration or candidate coverage is insufficient.".into();
        }
    }
    let printing = printing_decision(accepted, &identity, &visual, options);
    for conflict in &mut conflicts {
        conflict.resolved = match conflict.kind {
            ConflictKind::Identity => identity.card.is_some(),
            ConflictKind::Printing => printing.card.is_some(),
        };
        conflict.reason = match (conflict.kind, conflict.resolved) {
            (ConflictKind::Identity, true) => identity.reason.clone(),
            (ConflictKind::Printing, true) =>
                "The complete visual printing decision takes precedence; footer tokens do not establish a printing.".into(),
            (ConflictKind::Identity, false) =>
                "Strong identity disagreement remains unresolved; no combined identity is accepted.".into(),
            (ConflictKind::Printing, false) =>
                "Physical-printing ambiguity remains; exact footer tokens cannot upgrade visual acceptance.".into(),
        };
    }
    CombinedRecognition {
        observations: observations.clone(),
        text,
        visual,
        identity,
        printing,
        conflicts,
    }
}

/// Construct a semantic acceptance without adding any confidence or probability.
fn accepted_identity(
    card: &CardMetadata,
    status: CombinedStatus,
    scope: ShortlistScope,
    sources: Vec<EvidenceSource>,
    reason: &str,
) -> IdentityDecision {
    IdentityDecision {
        status,
        card: Some(CardIdentity::from_card(card)),
        scope,
        sources,
        reason: reason.into(),
    }
}

/// Record agreeing title/rules evidence without treating a footer as independent corroboration.
fn append_supporting_sources(
    sources: &mut Vec<EvidenceSource>,
    fields: &[FieldEvidence; 3],
    card: &CardMetadata,
) {
    for field in &fields[..2] {
        if field.supports(card, false) {
            sources.push(field.source);
        }
    }
}

/// Record fields that proposed the verified identity among bounded alternatives.
fn append_corroborating_sources(
    sources: &mut Vec<EvidenceSource>,
    fields: &[FieldEvidence; 3],
    card: &CardMetadata,
) {
    for field in &fields[..2] {
        if field.corroborates(card) {
            sources.push(field.source);
        }
    }
    if fields[2].supports(card, false) {
        sources.push(EvidenceSource::Footer);
    }
}

/// Preserve an actual complete visual printing match or retain explicit ambiguity.
fn printing_decision(
    accepted: Option<(&Recognition, ShortlistScope)>,
    identity: &IdentityDecision,
    combined: &ShortlistedRecognition,
    options: &MatchOptions,
) -> PrintingDecision {
    if let Some((visual, visual_scope)) = accepted {
        if combined.search_complete
            && combined.recognition.status == MatchStatus::Matched
            && let Some(best) = combined.recognition.candidates.first()
            && visual
                .candidates
                .first()
                .is_some_and(|accepted| same_identity(&accepted.card, &best.card))
        {
            return PrintingDecision {
                status: MatchStatus::Matched,
                card: Some(best.card.clone()),
                scope: combined.scope,
                reason: "The complete visual result uniquely identifies this printing and face."
                    .into(),
            };
        }
        let unchanged_face = visual.candidates.first().is_some_and(|best| {
            combined
                .recognition
                .candidates
                .first()
                .is_some_and(|winner| same_face(&best.card, &winner.card))
        });
        if visual.status == MatchStatus::Matched
            && unchanged_face
            && combined.recognition.candidates.first().is_some_and(|best| {
                sufficient_lead(best.similarity, combined.combined_printing_margin, options)
            })
            && matches!(
                visual_scope,
                ShortlistScope::FullGalleryGlobal | ShortlistScope::FullGalleryGeometry
            )
        {
            return PrintingDecision {
                status: MatchStatus::Matched,
                card: visual
                    .candidates
                    .first()
                    .map(|candidate| candidate.card.clone()),
                scope: visual_scope,
                reason: "The complete visual result uniquely identifies this printing and face."
                    .into(),
            };
        }
        return PrintingDecision {
            status: MatchStatus::Ambiguous,
            card: None,
            scope: visual_scope,
            reason: "Identity support does not resolve physical printings; incomplete geometry or close reprints retain ambiguity.".into(),
        };
    }
    PrintingDecision {
        status: if identity.status == CombinedStatus::Unknown {
            MatchStatus::Unknown
        } else {
            MatchStatus::Ambiguous
        },
        card: None,
        scope: combined.scope,
        reason: "No accepted visual identity and complete unique printing decision are available."
            .into(),
    }
}

/// Use the same absolute gate, nonzero lead and margin threshold as visual acceptance.
fn sufficient_lead(similarity: f32, margin: f32, options: &MatchOptions) -> bool {
    similarity >= options.min_similarity && margin >= options.min_margin && margin > f32::EPSILON
}

/// Retain material cross-channel identity disagreements and exact-footer printing conflicts.
fn collect_conflicts(
    fields: &[FieldEvidence; 3],
    text: &TextCandidates,
    visual: &ShortlistedRecognition,
    options: &MatchOptions,
) -> Vec<EvidenceConflict> {
    let mut channels: Vec<_> = fields
        .iter()
        .filter(|field| !field.strong.is_empty())
        .map(|field| (field.source, field.strong.clone()))
        .collect();
    if visual.global_recognition.identity_status == MatchStatus::Matched
        && let Some(best) = visual.global_recognition.candidates.first()
    {
        channels.push((EvidenceSource::VisualGlobal, vec![best.card.clone()]));
    }
    if let Some(best) = visual.recognition.candidates.first()
        && visual.geometry_verified.iter().any(|verification| {
            same_identity(&best.card, &verification.card)
                && verification.similarity >= options.min_similarity
        })
    {
        channels.push((EvidenceSource::VisualGeometry, vec![best.card.clone()]));
    }
    let mut conflicts = Vec::new();
    for (offset, (left_source, left)) in channels.iter().enumerate() {
        for (right_source, right) in &channels[offset + 1..] {
            if !left
                .iter()
                .any(|a| right.iter().any(|b| same_identity(a, b)))
            {
                conflicts.push(EvidenceConflict {
                    kind: ConflictKind::Identity,
                    sources: [*left_source, *right_source],
                    left: left.clone(),
                    right: right.clone(),
                    resolved: false,
                    reason: String::new(),
                });
            }
        }
    }
    if let Some(best) = visual.global_recognition.candidates.first() {
        let footer: Vec<_> = text
            .footer
            .iter()
            .filter(|hit| hit.footer_match && same_identity(&hit.card, &best.card))
            .collect();
        if !footer.is_empty() && !footer.iter().any(|hit| same_face(&hit.card, &best.card)) {
            conflicts.push(EvidenceConflict {
                kind: ConflictKind::Printing,
                sources: [EvidenceSource::Footer, EvidenceSource::VisualGlobal],
                left: footer.into_iter().map(|hit| hit.card.clone()).collect(),
                right: vec![best.card.clone()],
                resolved: false,
                reason: String::new(),
            });
        }
    }
    conflicts
}

/// Match the game-scoped semantic grouping used by the visual catalogue.
fn same_identity(left: &CardMetadata, right: &CardMetadata) -> bool {
    left.game == right.game
        && match (&left.oracle_id, &right.oracle_id) {
            (Some(left), Some(right)) => left == right,
            (None, None) => left.id == right.id,
            _ => false,
        }
}

/// Compare a physical printing and face without crossing game namespaces.
fn same_face(left: &CardMetadata, right: &CardMetadata) -> bool {
    left.game == right.game && left.id == right.id && left.face_index == right.face_index
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
