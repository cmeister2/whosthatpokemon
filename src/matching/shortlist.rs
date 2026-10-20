//! Complete global scoring followed by explicitly scoped candidate geometry.

use super::{
    Descriptor, GeometryMatch, Reference,
    candidates::{CandidateLocation, CandidateSource, Ranking},
    has_texture, local, match_geometry, validate_image,
};
#[cfg(not(target_arch = "wasm32"))]
use super::{
    ShardedIndex, query_descriptors, same_face, same_identity, score_reference, verify_geometry,
};
use crate::clock::Instant;
use crate::{CardMetadata, Error, MatchCandidate, MatchOptions, MatchStatus, Recognition};
use image::{DynamicImage, RgbImage, imageops::FilterType};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Work budgets for global candidate selection and local geometric verification.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ShortlistOptions {
    /// Number of globally strongest physical faces whose identity/art groups are proposed.
    pub visual_candidates: usize,
    /// Maximum number of physical faces geometrically compared with one query.
    pub max_geometry: usize,
}

impl Default for ShortlistOptions {
    fn default() -> Self {
        Self {
            visual_candidates: 100,
            max_geometry: 256,
        }
    }
}

impl ShortlistOptions {
    /// Reject empty candidate budgets before opening or searching descriptor files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] when either budget is zero.
    pub fn validate(&self) -> Result<(), Error> {
        if self.visual_candidates == 0 || self.max_geometry == 0 {
            return Err(Error::InvalidOptions(
                "visual candidate and geometry budgets must be positive".into(),
            ));
        }
        Ok(())
    }
}

/// Query orientations considered by the OCR-candidate visual path.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrientations {
    /// Compare the supplied rectified crop without rotating it.
    Upright,
    /// Compare only the crop rotated by 180 degrees.
    Inverted,
    /// Compare both portrait orientations, excluding 90 and 270 degrees.
    #[default]
    UprightAndInverted,
}

impl CandidateOrientations {
    /// Whether one inferred clockwise orientation is allowed by this policy.
    fn allows(self, rotation_degrees: u16) -> bool {
        match self {
            Self::Upright => rotation_degrees == 0,
            Self::Inverted => rotation_degrees == 180,
            Self::UprightAndInverted => matches!(rotation_degrees, 0 | 180),
        }
    }
}

/// Independent budgets for candidate descriptors and expensive local geometry.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CandidateOptions {
    /// Maximum faces whose compact global descriptors are scored.
    pub max_descriptors: usize,
    /// Maximum globally shortlisted faces whose local features are loaded and verified.
    pub max_geometry: usize,
    /// Portrait orientations considered during both scoring and geometric acceptance.
    pub orientations: CandidateOrientations,
}

impl Default for CandidateOptions {
    fn default() -> Self {
        Self {
            max_descriptors: 256,
            max_geometry: 8,
            orientations: CandidateOrientations::default(),
        }
    }
}

impl CandidateOptions {
    /// Reject empty work budgets before opening descriptor files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] when either budget is zero.
    pub fn validate(&self) -> Result<(), Error> {
        if self.max_descriptors == 0 || self.max_geometry == 0 {
            return Err(Error::InvalidOptions(
                "candidate descriptor and geometry budgets must be positive".into(),
            ));
        }
        Ok(())
    }
}

/// Evidence scope used for one bounded recognition result.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortlistScope {
    /// Uniform input was rejected before any gallery search was necessary.
    NoVisualEvidence,
    /// Every face was scored globally and the existing global gate resolved the query.
    FullGalleryGlobal,
    /// Global scores cover every face, but geometric evidence covers selected faces only.
    CandidateGeometry,
    /// Every enrolled physical face also received geometric verification.
    FullGalleryGeometry,
    /// Only the bounded closure of caller-supplied catalogue candidates was examined.
    SuppliedCandidates,
}

/// Compact identity of a physical face selected for actual geometric verification.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ShortlistFace {
    /// Enrolled game namespace.
    pub game: String,
    /// Enrolled printing identifier.
    pub id: String,
    /// Physical face within the printing.
    pub face_index: u32,
    /// Semantic identity when supplied by the catalogue.
    pub oracle_id: Option<String>,
}

impl ShortlistFace {
    /// Retain only the fields needed for physical/semantic candidate-recall checks.
    fn from_card(card: &CardMetadata) -> Self {
        Self {
            game: card.game.clone(),
            id: card.id.clone(),
            face_index: card.face_index,
            oracle_id: card.oracle_id.clone(),
        }
    }
}

/// A face with successful correspondence, homography, coverage and artwork checks.
///
/// Verification is recorded even when its geometric score does not improve that
/// face's global score. It is supporting image evidence, not a calibrated probability.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GeometryVerification {
    /// Canonical enrolled identity and physical-face metadata.
    pub card: CardMetadata,
    /// Verified geometric evidence score, independent of the global weighted score.
    pub similarity: f32,
    /// Clockwise right-angle orientation inferred from the verified homography.
    pub rotation_degrees: u16,
    /// Best global-descriptor score for this face within the applicable search scope.
    pub global_similarity: f32,
}

/// Wall-clock stages for one candidate-scoped recognition call.
///
/// Values are elapsed milliseconds. The local-read and geometry-verification
/// fields split [`Self::local_read_and_verify`]; the other stage fields do not
/// overlap. A stage remains zero when the call returns before that work begins;
/// [`Self::total`] also includes validation and small coordination costs.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CandidateTimings {
    /// Resolve supplied metadata through the exact-face, identity and illustration postings.
    pub candidate_selection: f64,
    /// Screen for texture, convert the query to RGB and compute allowed global descriptors.
    pub query_descriptors: f64,
    /// Read selected global records and score their compact descriptors.
    pub global_read_and_score: f64,
    /// Choose a group-preserving subset for local geometric verification.
    pub geometry_selection: f64,
    /// Extract query keypoints and build the normalized image used by geometry.
    pub query_local_features: f64,
    /// Read selected local records and run correspondence and artwork verification.
    pub local_read_and_verify: f64,
    /// Read selected local-feature records from their binary shards.
    #[serde(default)]
    pub local_feature_read: f64,
    /// Run correspondence and registered-artwork verification for selected faces.
    #[serde(default)]
    pub geometry_verification: f64,
    /// Rank candidates, calculate margins and construct the returned result.
    pub result_finalization: f64,
    /// Complete elapsed time for the candidate-scoped recognition call.
    pub total: f64,
}

/// Visual evidence from only the bounded closure of caller-supplied candidates.
///
/// Candidate margins never imply separation from the unsearched gallery. When
/// [`Self::search_complete`] is false, unique match statuses and confidence are
/// suppressed even if one loaded candidate is much stronger than the others.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CandidateRecognition {
    /// Ranked visual evidence within the selected candidate closure.
    pub recognition: Recognition,
    /// Printing lead over only the selected candidate closure.
    pub candidate_printing_margin: f32,
    /// Identity lead over only the selected candidate closure.
    pub candidate_identity_margin: f32,
    /// Explicit evidence scope; always [`ShortlistScope::SuppliedCandidates`].
    pub scope: ShortlistScope,
    /// Whether selected candidates happen to cover every enrolled face.
    pub search_complete: bool,
    /// Total enrolled physical faces, including separate backs.
    pub total_references: usize,
    /// Number of metadata proposals supplied by the caller, including duplicates.
    pub proposal_count: usize,
    /// Number of supplied proposals mapping to at least one enrolled face.
    pub matched_proposals: usize,
    /// Size of the exact-face, identity and illustration closure before its budget.
    pub candidate_count: usize,
    /// Number of selected physical faces whose compact global descriptors were scored.
    pub descriptors_scored: usize,
    /// Number of globally shortlisted physical faces passed to local geometry.
    pub geometry_attempted: usize,
    /// Actually selected physical faces, in deterministic budget priority order.
    pub geometry_candidates: Vec<ShortlistFace>,
    /// Number of selected faces passing correspondence and registered-artwork checks.
    pub geometry_verified_count: usize,
    /// Whether either descriptor or geometry work omitted physical faces.
    pub budget_exhausted: bool,
    /// Compatibility aggregate of omitted descriptor and geometry groups.
    pub omitted_candidate_groups: usize,
    /// Whether the descriptor budget omitted part of the mapped OCR closure.
    pub descriptor_budget_exhausted: bool,
    /// Whether some scored descriptor faces did not receive geometric verification.
    pub geometry_budget_exhausted: bool,
    /// Distinct proposed or expanded identities absent from descriptor scoring.
    pub omitted_descriptor_groups: usize,
    /// Proposed identity or relevant illustration groups absent from geometry.
    pub omitted_geometry_groups: usize,
    /// Successful candidate-scoped geometric verifications in gallery order.
    pub geometry_verified: Vec<GeometryVerification>,
    /// Wall-clock duration of each candidate recognition stage, in milliseconds.
    #[serde(default)]
    pub timings: CandidateTimings,
}

/// Bounded geometric evidence together with the exact complete-gallery global baseline.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ShortlistedRecognition {
    /// Ranked global/verified evidence. Incomplete geometric winners abstain from unique acceptance.
    pub recognition: Recognition,
    /// Unmodified result of complete-gallery global scoring, before geometric changes.
    pub global_recognition: Recognition,
    /// Printing lead over all global/selected-geometric scores, before incomplete-scope abstention.
    pub combined_printing_margin: f32,
    /// Lead over the best other identity across all global/selected-geometric scores.
    ///
    /// This includes competitors beyond returned top-k, but not omitted geometric evidence.
    pub combined_identity_margin: f32,
    /// Which evidence stages covered the complete gallery.
    pub scope: ShortlistScope,
    /// Whether the applicable evidence stage covered every enrolled face.
    pub search_complete: bool,
    /// Total enrolled physical faces, including separate backs.
    pub total_references: usize,
    /// Expanded visual/OCR face union before applying the geometry budget; zero on a global return.
    pub candidate_count: usize,
    /// Number of selected physical faces passed to the local verifier.
    pub geometry_attempted: usize,
    /// Actually selected physical faces, in deterministic budget-selection priority order.
    ///
    /// A selected face remains listed even if its geometric verification fails.
    pub geometry_candidates: Vec<ShortlistFace>,
    /// Number of faces that passed all geometric and registered-artwork checks.
    pub geometry_verified_count: usize,
    /// Whether some proposed faces were omitted because of the geometry budget.
    pub budget_exhausted: bool,
    /// Whether any supplied hint proposes an identity different from the global winner.
    pub ocr_conflict: bool,
    /// Distinct supplied identity groups with no face retained under the geometry budget.
    pub omitted_hint_groups: usize,
    /// Distinct visual seed identity groups with no face retained under the geometry budget.
    pub omitted_visual_groups: usize,
    /// Actual successful image verifications, in original gallery order.
    pub geometry_verified: Vec<GeometryVerification>,
}

/// Scalar global evidence retained without duplicating the complete metadata catalogue.
#[cfg(not(target_arch = "wasm32"))]
struct CachedScore {
    /// Shard containing this enrolled face.
    shard: usize,
    /// Position inside that shard.
    offset: usize,
    /// Best unchanged global score over rotations and reference insets.
    similarity: f32,
    /// Clockwise orientation of the best global comparison.
    rotation_degrees: u16,
}

/// Game-scoped semantic identity, with a printing fallback when Oracle ID is absent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    /// Catalogue namespace.
    game: String,
    /// Oracle ID, or an explicit absent-ID discriminator.
    oracle: Option<String>,
    /// Printing ID only when the Oracle ID is absent.
    printing: Option<String>,
}

impl Identity {
    /// Construct the exact identity grouping used by the portable matcher.
    fn of(card: &CardMetadata) -> Self {
        Self {
            game: card.game.clone(),
            oracle: card.oracle_id.clone(),
            printing: card.oracle_id.is_none().then(|| card.id.clone()),
        }
    }
}

/// Selection outcome before touching any local-feature file.
#[cfg(not(target_arch = "wasm32"))]
struct Selection {
    /// All proposed physical faces in globally ranked order.
    candidates: Vec<usize>,
    /// Bounded selected physical faces in deterministic priority order.
    selected: Vec<usize>,
    /// Hint semantic groups lacking any selected face.
    omitted_hints: usize,
    /// Visual semantic groups lacking any selected face.
    omitted_visual: usize,
}

/// Candidate-only metadata expansion and its bounded deterministic selection.
struct CandidateSelection {
    /// Complete unique face closure in enrollment order.
    candidates: Vec<CandidateLocation>,
    /// Bounded descriptor face set in proposal-priority order.
    descriptors: Vec<CandidateLocation>,
    /// Number of raw proposals resolving to at least one enrolled face.
    matched_proposals: usize,
    /// Distinct proposed or expanded identities absent from descriptor scoring.
    omitted_descriptor_groups: usize,
}

/// One decoded global record and its current candidate score.
struct ScoredCandidate {
    /// Stable physical location in the sharded catalogue.
    location: CandidateLocation,
    /// Compact reference descriptors, populated with locals only if shortlisted.
    reference: Reference,
    /// Best global score, optionally replaced by stronger verified geometry.
    candidate: MatchCandidate,
}

/// Group-preserving shortlist chosen after every descriptor candidate is scored.
struct GeometrySelection {
    /// Positions within the gallery-ordered scored candidate vector.
    selected: Vec<usize>,
    /// Proposed identity or relevant illustration groups lacking a representative.
    omitted_groups: usize,
}

/// One compact query descriptor paired with its actual orientation.
struct CandidateQuery {
    /// Clockwise rotation applied to the supplied crop.
    rotation_degrees: u16,
    /// Compact visual evidence extracted at that orientation.
    descriptor: Descriptor,
}

/// Return an empty candidate-scoped recognition without opening descriptor files.
fn empty_candidate_recognition(
    index: &impl CandidateSource,
    proposal_count: usize,
    matched_proposals: usize,
    candidate_count: usize,
    descriptor_budget_exhausted: bool,
    omitted_descriptor_groups: usize,
    timings: CandidateTimings,
) -> CandidateRecognition {
    CandidateRecognition {
        recognition: Recognition {
            status: MatchStatus::Unknown,
            identity_status: MatchStatus::Unknown,
            confidence: 0.0,
            identity_confidence: 0.0,
            candidates: Vec::new(),
        },
        candidate_printing_margin: 0.0,
        candidate_identity_margin: 0.0,
        scope: ShortlistScope::SuppliedCandidates,
        search_complete: false,
        total_references: index.len(),
        proposal_count,
        matched_proposals,
        candidate_count,
        descriptors_scored: 0,
        geometry_attempted: 0,
        geometry_candidates: Vec::new(),
        geometry_verified_count: 0,
        budget_exhausted: descriptor_budget_exhausted,
        omitted_candidate_groups: omitted_descriptor_groups,
        descriptor_budget_exhausted,
        geometry_budget_exhausted: false,
        omitted_descriptor_groups,
        omitted_geometry_groups: 0,
        geometry_verified: Vec::new(),
        timings,
    }
}

/// Score and verify only the indexed closure of caller-supplied candidates.
///
/// # Errors
///
/// Returns an error for invalid options/input/budget, unsupported legacy storage,
/// or unavailable/corrupt selected descriptor records.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn recognize_candidates(
    index: &impl CandidateSource,
    image: &DynamicImage,
    options: &MatchOptions,
    max_candidates: usize,
    candidates: &[CardMetadata],
) -> Result<CandidateRecognition, Error> {
    let candidate_options = CandidateOptions {
        max_descriptors: max_candidates,
        max_geometry: max_candidates,
        orientations: CandidateOrientations::UprightAndInverted,
    };
    recognize_candidates_with(index, image, options, &candidate_options, candidates)
}

/// Score the indexed candidate closure before loading a group-preserving local shortlist.
///
/// # Errors
///
/// Returns an error for invalid options/input/budgets, unsupported legacy storage,
/// or unavailable/corrupt selected descriptor records.
pub(super) fn recognize_candidates_with(
    index: &impl CandidateSource,
    image: &DynamicImage,
    options: &MatchOptions,
    candidate_options: &CandidateOptions,
    candidates: &[CardMetadata],
) -> Result<CandidateRecognition, Error> {
    recognize_selected_with(index, image, options, candidate_options, candidates, true)
}

/// Verify only explicit physical-face proposals while preserving candidate-scope accounting.
///
/// # Errors
///
/// Returns an error for invalid options/input or unavailable selected records.
#[cfg(any(not(target_arch = "wasm32"), feature = "ocr"))]
pub(super) fn recognize_faces_with(
    index: &impl CandidateSource,
    image: &DynamicImage,
    options: &MatchOptions,
    candidate_options: &CandidateOptions,
    candidates: &[CardMetadata],
) -> Result<CandidateRecognition, Error> {
    recognize_selected_with(index, image, options, candidate_options, candidates, false)
}

/// Share bounded descriptor and geometry verification across explicit and expanded proposals.
///
/// # Errors
///
/// Returns an error for invalid options/input or unavailable selected records.
fn recognize_selected_with(
    index: &impl CandidateSource,
    image: &DynamicImage,
    options: &MatchOptions,
    candidate_options: &CandidateOptions,
    candidates: &[CardMetadata],
    expand_related: bool,
) -> Result<CandidateRecognition, Error> {
    let total_started = Instant::now();
    let mut timings = CandidateTimings::default();
    options.validate()?;
    candidate_options.validate()?;
    validate_image(image)?;

    let stage_started = Instant::now();
    let selection = select_candidates(
        index,
        candidates,
        candidate_options.max_descriptors,
        expand_related,
    )?;
    timings.candidate_selection = stage_started.elapsed().as_secs_f64() * 1_000.0;
    if selection.descriptors.is_empty() {
        timings.total = total_started.elapsed().as_secs_f64() * 1_000.0;
        return Ok(empty_candidate_recognition(
            index,
            candidates.len(),
            selection.matched_proposals,
            selection.candidates.len(),
            selection.descriptors.len() < selection.candidates.len(),
            selection.omitted_descriptor_groups,
            timings,
        ));
    }

    let stage_started = Instant::now();
    if !has_texture(image) {
        timings.query_descriptors = stage_started.elapsed().as_secs_f64() * 1_000.0;
        timings.total = total_started.elapsed().as_secs_f64() * 1_000.0;
        return Ok(empty_candidate_recognition(
            index,
            candidates.len(),
            selection.matched_proposals,
            selection.candidates.len(),
            selection.descriptors.len() < selection.candidates.len(),
            selection.omitted_descriptor_groups,
            timings,
        ));
    }
    let rgb = image.to_rgb8();
    let queries = candidate_queries(&rgb, candidate_options.orientations);
    timings.query_descriptors = stage_started.elapsed().as_secs_f64() * 1_000.0;

    let stage_started = Instant::now();
    let mut by_shard: BTreeMap<usize, Vec<CandidateLocation>> = BTreeMap::new();
    for &location in &selection.descriptors {
        by_shard.entry(location.shard).or_default().push(location);
    }
    for locations in by_shard.values_mut() {
        locations.sort_by_key(|location| location.offset);
    }

    let mut scored = Vec::with_capacity(selection.descriptors.len());
    for (shard, locations) in by_shard {
        let offsets: Vec<_> = locations.iter().map(|location| location.offset).collect();
        let mut references = index.read_candidate_globals(shard, &offsets)?;
        for location in locations {
            let reference = references.remove(&location.offset).ok_or_else(|| {
                Error::InvalidIndex("selected global candidate missing from shard".into())
            })?;
            let candidate = score_candidate_reference(&reference, &queries);
            scored.push(ScoredCandidate {
                location,
                reference,
                candidate,
            });
        }
    }
    timings.global_read_and_score = stage_started.elapsed().as_secs_f64() * 1_000.0;

    let stage_started = Instant::now();
    let geometry = select_geometry(&scored, candidates, candidate_options.max_geometry);
    timings.geometry_selection = stage_started.elapsed().as_secs_f64() * 1_000.0;

    let stage_started = Instant::now();
    let normalized =
        image::imageops::resize(&rgb, local::WIDTH, local::HEIGHT, FilterType::Triangle);
    let query_features = local::extract_query(image, &normalized);
    timings.query_local_features = stage_started.elapsed().as_secs_f64() * 1_000.0;

    let stage_started = Instant::now();
    let read_started = Instant::now();
    let mut geometry_by_shard: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &position in &geometry.selected {
        geometry_by_shard
            .entry(scored[position].location.shard)
            .or_default()
            .push(position);
    }
    let mut verified = Vec::new();
    for (shard, positions) in geometry_by_shard {
        let offsets = positions
            .iter()
            .map(|&position| scored[position].location.offset)
            .collect::<Vec<_>>();
        let mut features = index.read_candidate_local_features(shard, &offsets)?;
        for position in positions {
            let location = scored[position].location;
            scored[position].reference.features =
                features.remove(&location.offset).ok_or_else(|| {
                    Error::InvalidIndex("selected local candidate missing from shard".into())
                })?;
        }
    }
    timings.local_feature_read = read_started.elapsed().as_secs_f64() * 1_000.0;

    let verification_started = Instant::now();
    let verification =
        verify_candidate_geometry(&scored, &geometry.selected, &query_features, &normalized)?;
    let attempted = verification.len();
    for (position, geometry) in verification {
        let location = scored[position].location;
        let global_similarity = scored[position].candidate.similarity;
        // Geometry is evidence in its own right even when the compact global
        // descriptor remains the stronger final candidate score.
        if let Some(geometry) = geometry
            && candidate_options
                .orientations
                .allows(geometry.rotation_degrees)
        {
            verified.push((
                location,
                GeometryVerification {
                    card: scored[position].reference.card.clone(),
                    similarity: geometry.similarity,
                    rotation_degrees: geometry.rotation_degrees,
                    global_similarity,
                },
            ));
            if geometry.similarity > scored[position].candidate.similarity {
                scored[position].candidate.similarity = geometry.similarity;
                scored[position].candidate.rotation_degrees = geometry.rotation_degrees;
            }
        }
    }
    timings.geometry_verification = verification_started.elapsed().as_secs_f64() * 1_000.0;
    timings.local_read_and_verify = stage_started.elapsed().as_secs_f64() * 1_000.0;

    let stage_started = Instant::now();
    let mut ranking = Ranking::new(options.top_k);
    for scored in &scored {
        ranking.push(scored.candidate.clone());
    }
    let (candidate_printing_margin, candidate_identity_margin) = ranking.margins();
    let mut recognition = ranking.finish(options)?;
    let search_complete = scored.len() == index.len() && attempted == index.len();
    if search_complete && !verified.is_empty() && recognition.status != MatchStatus::Unknown {
        index.illustration_ambiguity(&mut recognition)?;
    }
    if !search_complete {
        abstain(&mut recognition);
    }
    verified.sort_by_key(|(location, _)| *location);
    let geometry_verified = verified
        .into_iter()
        .map(|(_, evidence)| evidence)
        .collect::<Vec<_>>();
    let geometry_candidates = geometry
        .selected
        .iter()
        .map(|&position| ShortlistFace::from_card(&scored[position].reference.card))
        .collect::<Vec<_>>();
    let descriptor_budget_exhausted = selection.descriptors.len() < selection.candidates.len();
    let geometry_budget_exhausted = geometry.selected.len() < scored.len();
    let omitted_candidate_groups = selection
        .omitted_descriptor_groups
        .saturating_add(geometry.omitted_groups);
    let mut result = CandidateRecognition {
        recognition,
        candidate_printing_margin,
        candidate_identity_margin,
        scope: ShortlistScope::SuppliedCandidates,
        search_complete,
        total_references: index.len(),
        proposal_count: candidates.len(),
        matched_proposals: selection.matched_proposals,
        candidate_count: selection.candidates.len(),
        descriptors_scored: scored.len(),
        geometry_attempted: attempted,
        geometry_candidates,
        geometry_verified_count: geometry_verified.len(),
        budget_exhausted: descriptor_budget_exhausted || geometry_budget_exhausted,
        omitted_candidate_groups,
        descriptor_budget_exhausted,
        geometry_budget_exhausted,
        omitted_descriptor_groups: selection.omitted_descriptor_groups,
        omitted_geometry_groups: geometry.omitted_groups,
        geometry_verified,
        timings,
    };
    result.timings.result_finalization = stage_started.elapsed().as_secs_f64() * 1_000.0;
    result.timings.total = total_started.elapsed().as_secs_f64() * 1_000.0;
    Ok(result)
}

/// Resolve physical faces and optional related postings without a gallery scan.
fn select_candidates(
    index: &impl CandidateSource,
    proposals: &[CardMetadata],
    max_candidates: usize,
    expand_related: bool,
) -> Result<CandidateSelection, Error> {
    let matches = proposals
        .iter()
        .map(|proposal| {
            let mut found = index.candidate_matches(proposal)?;
            if !expand_related {
                found.identity.clear();
                found.illustration.clear();
            }
            Ok(found)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let matched_proposals = matches
        .iter()
        .filter(|matches| {
            !matches.exact.is_empty()
                || !matches.identity.is_empty()
                || !matches.illustration.is_empty()
        })
        .count();
    let candidates = matches
        .iter()
        .flat_map(|matches| {
            matches
                .exact
                .iter()
                .chain(&matches.identity)
                .chain(&matches.illustration)
                .copied()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let candidate_set: HashSet<_> = candidates.iter().copied().collect();
    let mut descriptors = Vec::new();
    let mut descriptor_set = HashSet::new();
    let mut retain = |location: CandidateLocation| {
        if descriptors.len() < max_candidates
            && candidate_set.contains(&location)
            && descriptor_set.insert(location)
        {
            descriptors.push(location);
        }
    };
    // Preserve every supplied physical-face proposal before expanding large
    // reprint or shared-art groups.
    for matches in &matches {
        for &location in &matches.exact {
            retain(location);
        }
    }
    // Reserve one representative from each identity and illustration proposal.
    for matches in &matches {
        if let Some(&location) = matches.identity.first() {
            retain(location);
        }
    }
    for matches in &matches {
        if let Some(&location) = matches.illustration.first() {
            retain(location);
        }
    }
    // Fill the remaining budget in stable gallery order.
    for &location in &candidates {
        retain(location);
    }

    let mut groups = Vec::new();
    for proposal in proposals {
        let group = Identity::of(proposal);
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    // Illustration expansion can introduce identities that OCR did not name.
    // Retain those groups in the coverage report so fusion cannot mistake a
    // budgeted-away shared-art alternative for a fully checked OCR scope.
    for &location in &candidates {
        let group = Identity::of(index.candidate_metadata(location)?);
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    let selected_groups = descriptors
        .iter()
        .map(|&location| index.candidate_metadata(location).map(Identity::of))
        .collect::<Result<HashSet<_>, Error>>()?;
    let omitted_descriptor_groups = groups
        .iter()
        .filter(|group| !selected_groups.contains(*group))
        .count();
    Ok(CandidateSelection {
        candidates,
        descriptors,
        matched_proposals,
        omitted_descriptor_groups,
    })
}

/// Extract only the portrait query orientations allowed by candidate policy.
fn candidate_queries(rgb: &RgbImage, orientations: CandidateOrientations) -> Vec<CandidateQuery> {
    match orientations {
        CandidateOrientations::Upright => vec![CandidateQuery {
            rotation_degrees: 0,
            descriptor: Descriptor::from_image(rgb),
        }],
        CandidateOrientations::Inverted => {
            let rotated = image::imageops::rotate180(rgb);
            vec![CandidateQuery {
                rotation_degrees: 180,
                descriptor: Descriptor::from_image(&rotated),
            }]
        }
        CandidateOrientations::UprightAndInverted => {
            let rotated = image::imageops::rotate180(rgb);
            vec![
                CandidateQuery {
                    rotation_degrees: 0,
                    descriptor: Descriptor::from_image(rgb),
                },
                CandidateQuery {
                    rotation_degrees: 180,
                    descriptor: Descriptor::from_image(&rotated),
                },
            ]
        }
    }
}

/// Score one reference over explicitly labelled candidate query orientations.
fn score_candidate_reference(reference: &Reference, queries: &[CandidateQuery]) -> MatchCandidate {
    let mut similarity = 0.0_f32;
    let mut best_query = 0;
    for (query_index, query) in queries.iter().enumerate() {
        for view in &reference.views {
            let score = query.descriptor.similarity(view);
            if score > similarity {
                similarity = score;
                best_query = query_index;
            }
        }
    }
    let query = &queries[best_query];
    MatchCandidate {
        card: reference.card.clone(),
        similarity,
        confidence: 0.0,
        rotation_degrees: query.rotation_degrees,
    }
}

/// Verify independent candidate faces concurrently and retain selection order.
#[cfg(not(target_arch = "wasm32"))]
fn verify_candidate_geometry(
    scored: &[ScoredCandidate],
    selected: &[usize],
    query_features: &[local::Feature],
    normalized: &RgbImage,
) -> Result<Vec<(usize, Option<GeometryMatch>)>, Error> {
    const MAX_WORKERS: usize = 4;

    let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let workers = available.min(MAX_WORKERS).min(selected.len());
    if workers <= 1 {
        return Ok(selected
            .iter()
            .map(|&position| {
                (
                    position,
                    match_geometry(&scored[position].reference, query_features, normalized, 0.0),
                )
            })
            .collect());
    }

    let chunk_size = selected.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles = selected
            .chunks(chunk_size)
            .map(|positions| {
                scope.spawn(move || {
                    positions
                        .iter()
                        .map(|&position| {
                            (
                                position,
                                match_geometry(
                                    &scored[position].reference,
                                    query_features,
                                    normalized,
                                    0.0,
                                ),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(selected.len());
        for handle in handles {
            results.extend(
                handle.join().map_err(|_| {
                    Error::Backend("parallel candidate geometry worker failed".into())
                })?,
            );
        }
        Ok(results)
    })
}

/// Verify the same selected faces serially where browser threads are unavailable.
#[cfg(target_arch = "wasm32")]
fn verify_candidate_geometry(
    scored: &[ScoredCandidate],
    selected: &[usize],
    query_features: &[local::Feature],
    normalized: &RgbImage,
) -> Result<Vec<(usize, Option<GeometryMatch>)>, Error> {
    Ok(selected
        .iter()
        .map(|&position| {
            (
                position,
                match_geometry(&scored[position].reference, query_features, normalized, 0.0),
            )
        })
        .collect())
}

/// Preserve proposed identities and scored illustration groups before score filling.
fn select_geometry(
    scored: &[ScoredCandidate],
    proposals: &[CardMetadata],
    max_geometry: usize,
) -> GeometrySelection {
    let mut ranked: Vec<_> = (0..scored.len()).collect();
    ranked.sort_by(|&left, &right| {
        scored[right]
            .candidate
            .similarity
            .total_cmp(&scored[left].candidate.similarity)
            .then(scored[left].location.cmp(&scored[right].location))
    });
    let mut proposed_identities = Vec::new();
    for proposal in proposals {
        let identity = Identity::of(proposal);
        if !proposed_identities.contains(&identity) {
            proposed_identities.push(identity);
        }
    }
    // Illustration expansion is relevant only for the art IDs named by the
    // proposals. Other art variants enter through identity expansion and may
    // be omitted as same-identity reprints once their globals affect the margin.
    let mut illustrations = Vec::new();
    for proposal in proposals {
        if let Some(illustration) = &proposal.illustration_id {
            let group = (proposal.game.clone(), illustration.clone());
            if !illustrations.contains(&group) {
                illustrations.push(group);
            }
        }
    }
    let mut selected = Vec::new();
    let mut selected_set = HashSet::new();
    let mut retain = |position: usize| {
        if selected.len() < max_geometry && selected_set.insert(position) {
            selected.push(position);
        }
    };
    for identity in &proposed_identities {
        if let Some(&position) = ranked
            .iter()
            .find(|&&position| Identity::of(&scored[position].reference.card) == *identity)
        {
            retain(position);
        }
    }
    for (game, illustration) in &illustrations {
        if let Some(&position) = ranked.iter().find(|&&position| {
            let card = &scored[position].reference.card;
            card.game == *game && card.illustration_id.as_ref() == Some(illustration)
        }) {
            retain(position);
        }
    }
    for position in ranked {
        retain(position);
    }
    let selected_identities = selected
        .iter()
        .map(|&position| Identity::of(&scored[position].reference.card))
        .collect::<HashSet<_>>();
    let selected_illustrations = selected
        .iter()
        .filter_map(|&position| {
            let card = &scored[position].reference.card;
            card.illustration_id
                .as_ref()
                .map(|illustration| (card.game.clone(), illustration.clone()))
        })
        .collect::<HashSet<_>>();
    let omitted_groups = proposed_identities
        .iter()
        .filter(|identity| !selected_identities.contains(*identity))
        .count()
        + illustrations
            .iter()
            .filter(|illustration| !selected_illustrations.contains(*illustration))
            .count();
    GeometrySelection {
        selected,
        omitted_groups,
    }
}

/// Run the bounded policy while leaving the exact [`crate::Matcher`] path unchanged.
///
/// # Errors
///
/// Returns an error for invalid settings/input, unreadable descriptors or invalid gallery files.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn recognize(
    index: &ShardedIndex,
    image: &DynamicImage,
    options: &MatchOptions,
    shortlist: &ShortlistOptions,
    hints: &[CardMetadata],
) -> Result<ShortlistedRecognition, Error> {
    options.validate()?;
    shortlist.validate()?;
    validate_image(image)?;
    if !has_texture(image) {
        let empty = Recognition {
            status: MatchStatus::Unknown,
            identity_status: MatchStatus::Unknown,
            confidence: 0.0,
            identity_confidence: 0.0,
            candidates: Vec::new(),
        };
        return Ok(ShortlistedRecognition {
            recognition: empty.clone(),
            global_recognition: empty,
            combined_printing_margin: 0.0,
            combined_identity_margin: 0.0,
            scope: ShortlistScope::NoVisualEvidence,
            search_complete: true,
            total_references: index.len(),
            candidate_count: 0,
            geometry_attempted: 0,
            geometry_candidates: Vec::new(),
            geometry_verified_count: 0,
            budget_exhausted: false,
            ocr_conflict: false,
            omitted_hint_groups: 0,
            omitted_visual_groups: 0,
            geometry_verified: Vec::new(),
        });
    }
    let rgb = image.to_rgb8();
    let queries = query_descriptors(&rgb);
    let mut scores = Vec::with_capacity(index.len());
    let mut legacy_metadata = Vec::new();
    let cache_metadata = index.cached_metadata().is_some();
    let mut ranking = Ranking::new(options.top_k);
    index.for_each_global_shard(|shard, references| {
        if !cache_metadata {
            legacy_metadata.push(references.metadata().cloned().collect::<Vec<_>>());
        }
        for (offset, reference) in references.references.iter().enumerate() {
            let (candidate, _) = score_reference(reference, &queries);
            scores.push(CachedScore {
                shard,
                offset,
                similarity: candidate.similarity,
                rotation_degrees: candidate.rotation_degrees,
            });
            ranking.push(candidate);
        }
        Ok(())
    })?;
    let metadata = index.cached_metadata().unwrap_or(&legacy_metadata);
    let cards: Vec<_> = scores
        .iter()
        .map(|score| &metadata[score.shard][score.offset])
        .collect();
    let (global_printing_margin, global_identity_margin) = ranking.margins();
    let global = ranking.finish(options)?;
    let ocr_conflict = global
        .candidates
        .first()
        .is_some_and(|best| hints.iter().any(|hint| !same_identity(&best.card, hint)));
    if global
        .candidates
        .first()
        .is_some_and(|best| best.similarity >= options.min_similarity)
        && !ocr_conflict
    {
        return Ok(ShortlistedRecognition {
            recognition: global.clone(),
            global_recognition: global,
            combined_printing_margin: global_printing_margin,
            combined_identity_margin: global_identity_margin,
            scope: ShortlistScope::FullGalleryGlobal,
            search_complete: true,
            total_references: index.len(),
            candidate_count: 0,
            geometry_attempted: 0,
            geometry_candidates: Vec::new(),
            geometry_verified_count: 0,
            budget_exhausted: false,
            ocr_conflict,
            omitted_hint_groups: 0,
            omitted_visual_groups: 0,
            geometry_verified: Vec::new(),
        });
    }
    let selection = select(&cards, &scores, shortlist, hints);
    let query_features = local::extract(image);
    let normalized =
        image::imageops::resize(&rgb, local::WIDTH, local::HEIGHT, FilterType::Triangle);
    let mut by_shard: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &position in &selection.selected {
        by_shard
            .entry(scores[position].shard)
            .or_default()
            .push(position);
    }
    let mut verified = Vec::new();
    let mut attempted = 0;
    for (shard, positions) in by_shard {
        let offsets: Vec<_> = positions
            .iter()
            .map(|position| scores[*position].offset)
            .collect();
        let references = index.read_candidate_shard(shard, &offsets)?;
        for position in positions {
            let cached = &scores[position];
            let reference = references
                .get(cached.offset)
                .and_then(Option::as_ref)
                .ok_or_else(|| {
                    Error::InvalidIndex("selected candidate missing from shard".into())
                })?;
            let (mut candidate, mut explanation) = score_reference(reference, &queries);
            let global_similarity = cached.similarity;
            // A scratch zero score requests all successful geometric evidence,
            // including evidence that does not uplift a stronger global score.
            candidate.similarity = 0.0;
            attempted += 1;
            if verify_geometry(
                reference,
                &query_features,
                &normalized,
                &mut candidate,
                &mut explanation,
            ) {
                verified.push((
                    position,
                    GeometryVerification {
                        card: candidate.card,
                        similarity: candidate.similarity,
                        rotation_degrees: candidate.rotation_degrees,
                        global_similarity,
                    },
                ));
                if candidate.similarity > scores[position].similarity {
                    scores[position].similarity = candidate.similarity;
                    scores[position].rotation_degrees = candidate.rotation_degrees;
                }
            }
        }
    }
    let mut ranking = Ranking::new(options.top_k);
    for (position, score) in scores.iter().enumerate() {
        ranking.push(MatchCandidate {
            card: (*cards[position]).clone(),
            similarity: score.similarity,
            confidence: 0.0,
            rotation_degrees: score.rotation_degrees,
        });
    }
    let (combined_printing_margin, combined_identity_margin) = ranking.margins();
    let mut recognition = ranking.finish(options)?;
    if !verified.is_empty() && recognition.status != MatchStatus::Unknown {
        index.illustration_ambiguity(&mut recognition)?;
    }
    let complete = attempted == index.len();
    if !complete {
        abstain(&mut recognition);
    }
    verified.sort_by_key(|(position, _)| *position);
    let geometry_verified = verified
        .into_iter()
        .map(|(_, evidence)| evidence)
        .collect::<Vec<_>>();
    Ok(ShortlistedRecognition {
        recognition,
        global_recognition: global,
        combined_printing_margin,
        combined_identity_margin,
        scope: if complete {
            ShortlistScope::FullGalleryGeometry
        } else {
            ShortlistScope::CandidateGeometry
        },
        search_complete: complete,
        total_references: index.len(),
        candidate_count: selection.candidates.len(),
        geometry_attempted: attempted,
        geometry_candidates: selection
            .selected
            .iter()
            .map(|position| ShortlistFace::from_card(cards[*position]))
            .collect(),
        geometry_verified_count: geometry_verified.len(),
        budget_exhausted: selection.selected.len() < selection.candidates.len(),
        ocr_conflict,
        omitted_hint_groups: selection.omitted_hints,
        omitted_visual_groups: selection.omitted_visual,
        geometry_verified,
    })
}

/// Expand seed identities/artwork and reserve diverse representatives before score filling.
#[cfg(not(target_arch = "wasm32"))]
fn select(
    cards: &[&CardMetadata],
    scores: &[CachedScore],
    options: &ShortlistOptions,
    hints: &[CardMetadata],
) -> Selection {
    let mut ordered: Vec<_> = (0..scores.len()).collect();
    ordered.sort_by(|a, b| scores[*b].similarity.total_cmp(&scores[*a].similarity));
    let visual: Vec<_> = ordered
        .iter()
        .take(options.visual_candidates)
        .copied()
        .collect();
    let mut hint_groups = Vec::new();
    let mut visual_groups = Vec::new();
    let mut identities = HashSet::new();
    let mut illustrations = HashSet::new();
    for hint in hints {
        let identity = Identity::of(hint);
        if !hint_groups.contains(&identity) {
            hint_groups.push(identity.clone());
        }
        identities.insert(identity);
        if let Some(illustration) = &hint.illustration_id {
            illustrations.insert((hint.game.clone(), illustration.clone()));
        }
    }
    for &position in &visual {
        let card = cards[position];
        let identity = Identity::of(card);
        if !visual_groups.contains(&identity) {
            visual_groups.push(identity.clone());
        }
        identities.insert(identity);
        if let Some(illustration) = &card.illustration_id {
            illustrations.insert((card.game.clone(), illustration.clone()));
        }
    }
    let direct: Vec<_> = ordered
        .iter()
        .copied()
        .filter(|position| hints.iter().any(|hint| same_face(cards[*position], hint)))
        .collect();
    let candidates: Vec<_> = ordered
        .iter()
        .copied()
        .filter(|position| {
            let card = cards[*position];
            identities.contains(&Identity::of(card))
                || card.illustration_id.as_ref().is_some_and(|illustration| {
                    illustrations.contains(&(card.game.clone(), illustration.clone()))
                })
                || direct.contains(position)
        })
        .collect();
    let mut selected = Vec::new();
    let mut selected_set = HashSet::new();
    let mut retain = |position: usize| {
        if selected.len() < options.max_geometry && selected_set.insert(position) {
            selected.push(position);
        }
    };
    for group in &hint_groups {
        let position = candidates
            .iter()
            .find(|position| Identity::of(cards[**position]) == *group);
        if let Some(&position) = position {
            retain(position);
        }
    }
    for group in &visual_groups {
        if let Some(&position) = visual
            .iter()
            .find(|position| Identity::of(cards[**position]) == *group)
        {
            retain(position);
        }
    }
    for &position in direct.iter().chain(&candidates) {
        retain(position);
    }
    let omitted = |groups: &[Identity]| {
        groups
            .iter()
            .filter(|group| {
                !selected
                    .iter()
                    .any(|position| Identity::of(cards[*position]) == **group)
            })
            .count()
    };
    Selection {
        omitted_hints: omitted(&hint_groups),
        omitted_visual: omitted(&visual_groups),
        candidates,
        selected,
    }
}

/// Remove unique-acceptance claims when omitted geometric competitors remain possible.
fn abstain(recognition: &mut Recognition) {
    if recognition.status == MatchStatus::Matched {
        recognition.status = MatchStatus::Ambiguous;
    }
    if recognition.identity_status == MatchStatus::Matched {
        recognition.identity_status = MatchStatus::Ambiguous;
    }
    recognition.confidence = 0.0;
    recognition.identity_confidence = 0.0;
    for candidate in &mut recognition.candidates {
        candidate.confidence = 0.0;
    }
}

/// Procedural-image and invented-metadata regressions, without research artifacts.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
