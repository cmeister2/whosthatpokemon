//! Portable visual retrieval for rectified trading-card faces.
//!
//! The index stores compact image descriptors, rather than the reference images.
//! Every candidate is compared before results are truncated. Matching uses
//! luminance correlation, artwork structure and colour, four right-angle query
//! orientations, and reference crops that tolerate a modest boundary error.
//! If absolute global evidence is insufficient, oriented patch correspondences
//! and a verified homography provide a portable geometric fallback.
//!
//! Similarities and confidence values are visual heuristics, not probabilities.
//! Very similar reprints can remain ambiguous even when the card identity is clear.

use std::{
    collections::HashSet,
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::Path,
};

use image::{DynamicImage, GrayImage, RgbImage, imageops::FilterType};
use serde::{Deserialize, Serialize};

/// Portable local geometry used when global evidence is insufficient.
mod local;

use crate::{CardMetadata, Error, MatchCandidate, MatchOptions, MatchStatus, Matcher, Recognition};

/// On-disk index format; incompatible descriptor changes require a new version.
const FORMAT_VERSION: u32 = 2;
/// Stable description of the descriptor and scoring algorithm.
const SCHEME: &str = "crop-fusion-local-geometry-v2";
/// Normalized reference insets tested against each query orientation.
const INSETS: [f32; 5] = [0.0, 0.03, 0.06, 0.10, 0.15];
/// Width of the full-face luminance descriptor.
const FULL_WIDTH: u32 = 24;
/// Height of the full-face luminance descriptor.
const FULL_HEIGHT: u32 = 32;
/// Width of the artwork luminance descriptor.
const ART_WIDTH: u32 = 24;
/// Height of the artwork luminance descriptor.
const ART_HEIGHT: u32 = 20;
/// Number of hue bins in the artwork histogram.
const HUE_BINS: usize = 16;
/// Number of saturation bins in the artwork histogram.
const SATURATION_BINS: usize = 8;
/// Number of non-DC coefficients retained by the artwork perceptual hash.
const HASH_BITS: u32 = 63;
/// Quantization scale for signed, unit-length luminance vectors.
const LUMA_SCALE: f64 = i16::MAX as f64;
/// Quantization scale for the square-root probability histogram.
const HISTOGRAM_SCALE: f64 = u16::MAX as f64;
/// Minimum standard deviation in 8-bit luminance accepted as visual evidence.
const MIN_LUMA_DEVIATION: f64 = 2.0;
/// Similarity separation at which heuristic confidence reaches absolute similarity.
const CONFIDENCE_MARGIN: f32 = 0.06;

/// A compact, persisted descriptor of one normalized reference crop.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Descriptor {
    /// Zero-mean unit luminance vector, quantized with [`LUMA_SCALE`].
    full: Vec<i16>,
    /// Zero-mean unit artwork luminance vector, quantized with [`LUMA_SCALE`].
    art: Vec<i16>,
    /// Square-root hue/saturation probabilities, quantized with [`HISTOGRAM_SCALE`].
    histogram: Vec<u16>,
    /// Median-thresholded low-frequency artwork DCT, omitting the DC coefficient.
    hash: u64,
    /// Norm of the quantized full vector, recomputed when loading.
    #[serde(skip)]
    full_norm: f64,
    /// Norm of the quantized artwork vector, recomputed when loading.
    #[serde(skip)]
    art_norm: f64,
    /// Norm of the quantized square-root histogram, recomputed when loading.
    #[serde(skip)]
    histogram_norm: f64,
}

/// One physical face and its bank of reference crop descriptors.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Reference {
    /// Identity and attribution supplied by a game catalogue.
    card: CardMetadata,
    /// Descriptor for each inset in [`INSETS`], in the same order.
    views: Vec<Descriptor>,
    /// Bounded oriented patch descriptors for perspective-aware fallback verification.
    features: Vec<local::Feature>,
}

/// Serialized index envelope with an explicit compatibility contract.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredIndex {
    /// Version of the serialized representation.
    format_version: u32,
    /// Descriptor/scoring scheme supported by this implementation.
    scheme: String,
    /// Canonical faces and compact visual evidence.
    references: Vec<Reference>,
}

/// Borrowed serialization envelope avoids duplicating the full index during saving.
#[derive(Serialize)]
struct BorrowedIndex<'a> {
    /// Version of the serialized representation.
    format_version: u32,
    /// Descriptor/scoring scheme supported by this implementation.
    scheme: &'static str,
    /// References borrowed directly from the live index.
    references: &'a [Reference],
}

/// Actual visual components used when ranking one reference face.
///
/// Global components use weights 0.20 for full-face luminance, 0.45 for artwork
/// luminance, 0.05 for the artwork hash and 0.30 for artwork colour.
/// Geometric fallback uses its own verified-correspondence score; the global
/// components remain diagnostic.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VisualEvidence {
    /// Game namespace of the compared reference.
    pub game: String,
    /// Printing identifier of the compared reference.
    pub id: String,
    /// Physical face within the printing.
    pub face_index: u32,
    /// Final candidate similarity after any geometric fallback.
    pub similarity: f32,
    /// Positive full-face normalized luminance correlation.
    pub full_luminance: f32,
    /// Positive artwork normalized luminance correlation.
    pub artwork_luminance: f32,
    /// Artwork hue/saturation Hellinger coefficient.
    pub artwork_colour: f32,
    /// Artwork perceptual-hash agreement.
    pub artwork_hash: f32,
    /// Inset fraction of the reference view producing the best global score.
    pub reference_inset: f32,
    /// Orientation used for these global components, clockwise in degrees.
    pub global_rotation_degrees: u16,
    /// Weighted global similarity before geometric fallback.
    pub global_similarity: f32,
    /// Either `global_descriptors` or `verified_geometry`.
    pub method: &'static str,
    /// Artwork structure measured after homography registration, when used.
    pub registered_artwork_luminance: Option<f32>,
    /// Artwork colour measured after homography registration, when used.
    pub registered_artwork_colour: Option<f32>,
}

/// A reusable reference catalogue implementing the default portable matcher.
///
/// Enrollment requires a tight, upright image of each known face. Query images
/// should be rectified card crops, such as those supplied by
/// [`crate::RecognitionPipeline`]. The index searches every reference and groups
/// printings by their game and oracle ID for the separate identity decision.
#[derive(Clone, Debug, Default)]
pub struct CatalogIndex {
    /// Compact reference faces, kept in enrollment order for stable ties.
    references: Vec<Reference>,
}

impl CatalogIndex {
    /// Create an empty reference index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the number of enrolled physical faces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.references.len()
    }

    /// Return whether the index contains no references.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.references.is_empty()
    }

    /// Iterate over enrolled physical-face metadata without exposing descriptors.
    pub fn metadata(&self) -> impl Iterator<Item = &CardMetadata> {
        self.references.iter().map(|reference| &reference.card)
    }

    /// Add an upright reference face and its game-specific identity.
    ///
    /// The same printing may enroll multiple faces with different `face_index`
    /// values. Repeated `(game, id, face_index)` keys are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCatalog`] for incomplete identity metadata or a
    /// duplicate face, or [`Error::InvalidInput`] for an empty or nearly uniform
    /// image with insufficient visual evidence.
    pub fn add(&mut self, card: CardMetadata, image: &DynamicImage) -> Result<(), Error> {
        validate_metadata(&card).map_err(Error::InvalidCatalog)?;
        if self
            .references
            .iter()
            .any(|entry| same_face(&entry.card, &card))
        {
            return Err(Error::InvalidCatalog(format!(
                "duplicate reference face {}:{}:{}",
                card.game, card.id, card.face_index
            )));
        }
        validate_image(image)?;
        if !has_texture(image) {
            return Err(Error::InvalidInput(
                "reference image has insufficient luminance variation".into(),
            ));
        }
        let rgb = image.to_rgb8();
        let views = INSETS
            .iter()
            .map(|inset| Descriptor::from_image(&inset_image(&rgb, *inset)))
            .collect();
        let features = local::extract(image);
        self.references.push(Reference {
            card,
            views,
            features,
        });
        Ok(())
    }

    /// Write a versioned JSON index containing metadata and quantized descriptors.
    ///
    /// Source image pixels are not retained. Quantized global vectors, bounded
    /// binary local features and metadata are stored. Existing files at `path`
    /// are replaced; saving borrows the index without cloning its descriptors.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination cannot be written or serialization fails.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let stored = BorrowedIndex {
            format_version: FORMAT_VERSION,
            scheme: SCHEME,
            references: &self.references,
        };
        let mut writer = BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut writer, &stored)?;
        writer.flush()?;
        Ok(())
    }

    /// Read and validate a previously saved reference index.
    ///
    /// Descriptor dimensions, value ranges, normalization, metadata and duplicate
    /// face keys are checked before any references become available for matching.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be read, or
    /// [`Error::InvalidIndex`] for corrupt JSON, incompatible versions/schemes,
    /// invalid descriptors or inconsistent face metadata.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::from_reader(BufReader::new(File::open(path)?))
    }

    /// Read and validate an index from a stream, including in-memory browser data.
    ///
    /// Validation is identical to [`Self::load`]; no partially validated index is
    /// returned when a descriptor, metadata record or compatibility field fails.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidIndex`] for unreadable or invalid JSON, unsupported
    /// formats, invalid metadata, duplicate faces or corrupt descriptors.
    pub fn from_reader(reader: impl Read) -> Result<Self, Error> {
        let mut stored: StoredIndex = serde_json::from_reader(reader)
            .map_err(|error| Error::InvalidIndex(format!("could not decode index: {error}")))?;
        if stored.format_version != FORMAT_VERSION || stored.scheme != SCHEME {
            return Err(Error::InvalidIndex(format!(
                "unsupported index version {} or descriptor scheme {}",
                stored.format_version, stored.scheme
            )));
        }
        let mut keys = HashSet::new();
        for reference in &mut stored.references {
            validate_metadata(&reference.card).map_err(Error::InvalidIndex)?;
            let key = (
                reference.card.game.clone(),
                reference.card.id.clone(),
                reference.card.face_index,
            );
            if !keys.insert(key) {
                return Err(Error::InvalidIndex("duplicate reference face".into()));
            }
            if reference.views.len() != INSETS.len() {
                return Err(Error::InvalidIndex("invalid reference crop bank".into()));
            }
            for descriptor in &mut reference.views {
                descriptor.validate()?;
            }
            local::validate(&reference.features)?;
            if reference
                .views
                .first()
                .is_none_or(|view| view.full_norm == 0.0)
            {
                return Err(Error::InvalidIndex(
                    "reference has no luminance structure".into(),
                ));
            }
        }
        Ok(Self {
            references: stored.references,
        })
    }

    /// Rank references for one rectified face, considering four query orientations.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid matching settings, an empty image or catalogue.
    /// Nearly uniform images return [`MatchStatus::Unknown`] with zero confidence.
    pub fn recognize(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error> {
        <Self as Matcher>::recognize(self, image, options)
    }
}

impl CatalogIndex {
    /// Rank references and retain the actual descriptor evidence for each result.
    ///
    /// Scores and acceptance decisions are identical to [`Self::recognize`].
    /// Geometric scores are identified explicitly, rather than being attributed
    /// to the unrelated global descriptor sum.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options, an empty image or an empty catalogue.
    pub fn recognize_explained(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
    ) -> Result<(Recognition, Vec<VisualEvidence>), Error> {
        options.validate()?;
        validate_image(image)?;
        if self.references.is_empty() {
            return Err(Error::InvalidCatalog("reference index is empty".into()));
        }
        if !has_texture(image) {
            return Ok((
                Recognition {
                    status: MatchStatus::Unknown,
                    identity_status: MatchStatus::Unknown,
                    confidence: 0.0,
                    identity_confidence: 0.0,
                    candidates: Vec::new(),
                },
                Vec::new(),
            ));
        }
        let rgb = image.to_rgb8();
        let rotations = [
            rgb.clone(),
            image::imageops::rotate90(&rgb),
            image::imageops::rotate180(&rgb),
            image::imageops::rotate270(&rgb),
        ];
        let queries: Vec<_> = rotations.iter().map(Descriptor::from_image).collect();
        let mut explanations = Vec::with_capacity(self.references.len());
        let mut ranked: Vec<_> = self
            .references
            .iter()
            .map(|reference| {
                let mut similarity = 0.0_f32;
                let mut rotation_degrees = 0;
                let mut best_view = 0;
                for (rotation, query) in queries.iter().enumerate() {
                    for (view_index, view) in reference.views.iter().enumerate() {
                        let score = query.similarity(view);
                        if score > similarity {
                            similarity = score;
                            rotation_degrees = rotation as u16 * 90;
                            best_view = view_index;
                        }
                    }
                }
                let query = &queries[usize::from(rotation_degrees / 90)];
                explanations.push(query.explain(
                    &reference.views[best_view],
                    &reference.card,
                    INSETS[best_view],
                    rotation_degrees,
                ));
                MatchCandidate {
                    card: reference.card.clone(),
                    similarity,
                    confidence: 0.0,
                    rotation_degrees,
                }
            })
            .collect();
        // Global decisions that meet the absolute gate retain the inexpensive
        // descriptor behavior, including honest ambiguity between close reprints.
        if ranked
            .iter()
            .any(|candidate| candidate.similarity >= options.min_similarity)
        {
            let result = finish_recognition(ranked, options)?;
            return Ok(with_explanations(result, explanations));
        }
        let query_features = local::extract(image);
        let normalized_query =
            image::imageops::resize(&rgb, local::WIDTH, local::HEIGHT, FilterType::Triangle);
        let mut used_geometry = false;
        for ((candidate, reference), explanation) in ranked
            .iter_mut()
            .zip(&self.references)
            .zip(&mut explanations)
        {
            let Some(evidence) = local::match_evidence(&query_features, &reference.features) else {
                continue;
            };
            if evidence.score <= candidate.similarity {
                continue;
            }
            let Some(projection) =
                imageproc::geometric_transformations::Projection::from_matrix(evidence.projection)
            else {
                continue;
            };
            let aligned = imageproc::geometric_transformations::warp(
                &normalized_query,
                projection,
                imageproc::geometric_transformations::Interpolation::Bilinear,
                imageproc::geometric_transformations::Border::Constant(image::Rgb([0, 0, 0])),
            );
            let aligned_descriptor = Descriptor::from_image(&aligned);
            let Some(reference_descriptor) = reference.views.first() else {
                continue;
            };
            // Card-frame/text homographies also need independent artwork intensity
            // and colour agreement; geometrically consistent borders alone fail.
            let art = signed_cosine(
                &aligned_descriptor.art,
                &reference_descriptor.art,
                aligned_descriptor.art_norm,
                reference_descriptor.art_norm,
            );
            let colour = unsigned_cosine(
                &aligned_descriptor.histogram,
                &reference_descriptor.histogram,
                aligned_descriptor.histogram_norm,
                reference_descriptor.histogram_norm,
            );
            // Strong registered artwork structure survives white-balance changes
            // that shift hue/saturation; weaker structure also needs colour support.
            if art < 0.25 || (art < 0.75 && colour < 0.50) {
                continue;
            }
            candidate.similarity = evidence.score;
            candidate.rotation_degrees = evidence.rotation_degrees;
            explanation.similarity = evidence.score;
            explanation.method = "verified_geometry";
            explanation.registered_artwork_luminance = Some(art);
            explanation.registered_artwork_colour = Some(colour);
            used_geometry = true;
        }
        let mut result = finish_recognition(ranked, options)?;
        if used_geometry && result.status == MatchStatus::Matched {
            let (mut printing_ambiguous, mut identity_ambiguous) = (false, false);
            if let Some(best) = result.candidates.first()
                && let Some(illustration) = &best.card.illustration_id
            {
                for reference in &self.references {
                    if reference.card.game == best.card.game
                        && !same_face(&reference.card, &best.card)
                        && reference.card.illustration_id.as_ref() == Some(illustration)
                    {
                        printing_ambiguous = true;
                        identity_ambiguous |= !same_identity(&reference.card, &best.card);
                    }
                }
            }
            if printing_ambiguous {
                result.status = MatchStatus::Ambiguous;
                result.confidence = 0.0;
                for candidate in &mut result.candidates {
                    candidate.confidence = 0.0;
                }
            }
            if identity_ambiguous {
                result.identity_status = MatchStatus::Ambiguous;
                result.identity_confidence = 0.0;
            }
        }
        Ok(with_explanations(result, explanations))
    }
}

impl Matcher for CatalogIndex {
    fn recognize(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error> {
        self.recognize_explained(image, options)
            .map(|(recognition, _)| recognition)
    }
}

/// Retain explanations in the same order and with the same truncation as candidates.
fn with_explanations(
    recognition: Recognition,
    explanations: Vec<VisualEvidence>,
) -> (Recognition, Vec<VisualEvidence>) {
    let ranked = recognition
        .candidates
        .iter()
        .filter_map(|candidate| {
            explanations
                .iter()
                .find(|evidence| {
                    evidence.game == candidate.card.game
                        && evidence.id == candidate.card.id
                        && evidence.face_index == candidate.card.face_index
                })
                .cloned()
        })
        .collect();
    (recognition, ranked)
}

/// Sort complete-gallery evidence and compute printing and semantic-identity decisions.
///
/// # Errors
///
/// Returns [`Error::InvalidOptions`] for invalid settings or
/// [`Error::InvalidCatalog`] when the complete candidate list is empty.
pub(crate) fn finish_recognition(
    mut ranked: Vec<MatchCandidate>,
    options: &MatchOptions,
) -> Result<Recognition, Error> {
    options.validate()?;
    ranked.sort_by(|left, right| right.similarity.total_cmp(&left.similarity));
    // The nonempty catalogue guarantees a first candidate, without indexing.
    let Some(best) = ranked.first() else {
        return Err(Error::InvalidCatalog("reference index is empty".into()));
    };
    let best_score = best.similarity;
    let print_runner_up = ranked.get(1).map_or(0.0, |next| next.similarity);
    let identity_runner_up = ranked
        .iter()
        .find(|candidate| !same_identity(&best.card, &candidate.card))
        .map_or(0.0, |candidate| candidate.similarity);
    let print_margin = (best_score - print_runner_up).max(0.0);
    let identity_margin = (best_score - identity_runner_up).max(0.0);
    let status = decision(best_score, print_margin, options);
    let identity_status = decision(best_score, identity_margin, options);
    let confidence = evidence_confidence(best_score, print_margin, status);
    let identity_confidence = evidence_confidence(best_score, identity_margin, identity_status);
    for (index, candidate) in ranked.iter_mut().enumerate() {
        // A runner-up remains a plausible alternative, but cannot claim a
        // unique-match confidence while another printing has stronger evidence.
        let margin = if index == 0 { print_margin } else { 0.0 };
        candidate.confidence = evidence_confidence(
            candidate.similarity,
            margin,
            decision(candidate.similarity, margin, options),
        );
    }
    ranked.truncate(options.top_k);
    Ok(Recognition {
        status,
        identity_status,
        confidence,
        identity_confidence,
        candidates: ranked,
    })
}

impl Descriptor {
    /// Extract one compact descriptor from an already oriented RGB image.
    fn from_image(image: &RgbImage) -> Self {
        let full = luminance_vector(image, FULL_WIDTH, FULL_HEIGHT);
        let art_image = artwork_image(image);
        let art = luminance_vector(&art_image, ART_WIDTH, ART_HEIGHT);
        let histogram = colour_histogram(&art_image);
        Self {
            full_norm: signed_norm(&full),
            art_norm: signed_norm(&art),
            histogram_norm: unsigned_norm(&histogram),
            full,
            art,
            histogram,
            hash: perceptual_hash(&art_image),
        }
    }

    /// Check a persisted descriptor and restore its derived norms.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidIndex`] for an invalid dimension, hash or vector norm.
    fn validate(&mut self) -> Result<(), Error> {
        if self.full.len() != (FULL_WIDTH * FULL_HEIGHT) as usize
            || self.art.len() != (ART_WIDTH * ART_HEIGHT) as usize
            || self.histogram.len() != HUE_BINS * SATURATION_BINS
            || self.hash >> HASH_BITS != 0
        {
            return Err(Error::InvalidIndex(
                "invalid descriptor dimensions or hash".into(),
            ));
        }
        self.full_norm = signed_norm(&self.full);
        self.art_norm = signed_norm(&self.art);
        self.histogram_norm = unsigned_norm(&self.histogram);
        for (name, values, norm) in [
            ("full", &self.full, self.full_norm),
            ("artwork", &self.art, self.art_norm),
        ] {
            let sum: i64 = values.iter().map(|value| i64::from(*value)).sum();
            if values.contains(&i16::MIN)
                || sum.abs() > values.len() as i64 / 2 + 1
                || (norm != 0.0 && (norm - LUMA_SCALE).abs() > 32.0)
            {
                return Err(Error::InvalidIndex(format!(
                    "invalid {name} vector normalization"
                )));
            }
        }
        if (self.histogram_norm - HISTOGRAM_SCALE).abs() > 16.0 {
            return Err(Error::InvalidIndex(
                "invalid colour histogram normalization".into(),
            ));
        }
        Ok(())
    }

    /// Record the actual component measurements for a chosen global comparison.
    fn explain(
        &self,
        reference: &Self,
        card: &CardMetadata,
        reference_inset: f32,
        global_rotation_degrees: u16,
    ) -> VisualEvidence {
        VisualEvidence {
            game: card.game.clone(),
            id: card.id.clone(),
            face_index: card.face_index,
            similarity: self.similarity(reference),
            full_luminance: signed_cosine(
                &self.full,
                &reference.full,
                self.full_norm,
                reference.full_norm,
            ),
            artwork_luminance: signed_cosine(
                &self.art,
                &reference.art,
                self.art_norm,
                reference.art_norm,
            ),
            artwork_colour: unsigned_cosine(
                &self.histogram,
                &reference.histogram,
                self.histogram_norm,
                reference.histogram_norm,
            ),
            artwork_hash: 1.0 - (self.hash ^ reference.hash).count_ones() as f32 / HASH_BITS as f32,
            reference_inset,
            global_rotation_degrees,
            global_similarity: self.similarity(reference),
            method: "global_descriptors",
            registered_artwork_luminance: None,
            registered_artwork_colour: None,
        }
    }

    /// Fuse absolute full/artwork luminance, artwork frequency and colour evidence.
    fn similarity(&self, reference: &Self) -> f32 {
        let full = signed_cosine(
            &self.full,
            &reference.full,
            self.full_norm,
            reference.full_norm,
        );
        let art = signed_cosine(&self.art, &reference.art, self.art_norm, reference.art_norm);
        let colour = unsigned_cosine(
            &self.histogram,
            &reference.histogram,
            self.histogram_norm,
            reference.histogram_norm,
        );
        let hash = 1.0 - (self.hash ^ reference.hash).count_ones() as f32 / HASH_BITS as f32;
        (0.20 * full + 0.45 * art + 0.05 * hash + 0.30 * colour).clamp(0.0, 1.0)
    }
}

/// Validate the required identity fields and optional identifier consistency.
///
/// # Errors
///
/// Returns an explanation when a required identity or supplied optional identifier is empty.
pub(crate) fn validate_metadata(card: &CardMetadata) -> Result<(), String> {
    for (name, value) in [("game", &card.game), ("id", &card.id), ("name", &card.name)] {
        if value.trim().is_empty() {
            return Err(format!("reference {name} must not be empty"));
        }
    }
    for (name, value) in [
        ("oracle_id", &card.oracle_id),
        ("illustration_id", &card.illustration_id),
        ("set", &card.set),
        ("collector_number", &card.collector_number),
        ("language", &card.language),
        ("face_name", &card.face_name),
    ] {
        if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err(format!("reference {name} must be absent or nonempty"));
        }
    }
    Ok(())
}

/// Compare physical face keys across different game namespaces.
pub(crate) fn same_face(left: &CardMetadata, right: &CardMetadata) -> bool {
    left.game == right.game && left.id == right.id && left.face_index == right.face_index
}

/// Group reprints with supplied oracle IDs, otherwise using the printing ID.
fn same_identity(left: &CardMetadata, right: &CardMetadata) -> bool {
    left.game == right.game
        && match (&left.oracle_id, &right.oracle_id) {
            (Some(left), Some(right)) => left == right,
            (None, None) => left.id == right.id,
            _ => false,
        }
}

/// Reject a zero-sized image before any crop or resampling operation.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if either image dimension is zero.
pub(crate) fn validate_image(image: &DynamicImage) -> Result<(), Error> {
    if image.width() == 0 || image.height() == 0 {
        return Err(Error::InvalidInput(
            "image dimensions must be positive".into(),
        ));
    }
    Ok(())
}

/// Require actual luminance variation before considering any visual match.
pub(crate) fn has_texture(image: &DynamicImage) -> bool {
    let grey = image.resize_exact(64, 88, FilterType::Triangle).to_luma8();
    let count = f64::from(grey.width() * grey.height());
    let mean = grey.pixels().map(|pixel| f64::from(pixel[0])).sum::<f64>() / count;
    let variance = grey
        .pixels()
        .map(|pixel| (f64::from(pixel[0]) - mean).powi(2))
        .sum::<f64>()
        / count;
    variance >= MIN_LUMA_DEVIATION.powi(2)
}

/// Remove a symmetric fraction of the reference border, retaining at least one pixel.
fn inset_image(image: &RgbImage, inset: f32) -> RgbImage {
    let x = (image.width() as f32 * inset) as u32;
    let y = (image.height() as f32 * inset) as u32;
    image::imageops::crop_imm(
        image,
        x,
        y,
        image.width().saturating_sub(2 * x).max(1),
        image.height().saturating_sub(2 * y).max(1),
    )
    .to_image()
}

/// Use the artwork region shared by ordinary trading-card portrait layouts.
fn artwork_image(image: &RgbImage) -> RgbImage {
    let x = (image.width() as f32 * 0.09) as u32;
    let y = (image.height() as f32 * 0.16) as u32;
    let end_x = ((image.width() as f32 * 0.91) as u32).max(x + 1);
    let end_y = ((image.height() as f32 * 0.57) as u32).max(y + 1);
    image::imageops::crop_imm(image, x, y, end_x - x, end_y - y).to_image()
}

/// Resample luminance while preserving the public image dependency's colour conversion.
fn grayscale(image: &RgbImage, width: u32, height: u32) -> GrayImage {
    DynamicImage::ImageRgb8(image::imageops::resize(
        image,
        width,
        height,
        FilterType::Triangle,
    ))
    .to_luma8()
}

/// Quantize a zero-mean unit luminance vector, preserving a constant region as zeros.
fn luminance_vector(image: &RgbImage, width: u32, height: u32) -> Vec<i16> {
    let gray = grayscale(image, width, height);
    let mut values: Vec<_> = gray.pixels().map(|pixel| f64::from(pixel[0])).collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    for value in &mut values {
        *value -= mean;
    }
    let norm = values.iter().map(|value| value * value).sum::<f64>().sqrt();
    values
        .iter()
        .map(|value| {
            if norm == 0.0 {
                0
            } else {
                (value / norm * LUMA_SCALE).round() as i16
            }
        })
        .collect()
}

/// Build a square-root normalized HSV histogram suitable for Hellinger similarity.
fn colour_histogram(image: &RgbImage) -> Vec<u16> {
    let sampled = image::imageops::resize(image, 96, 128, FilterType::Triangle);
    let mut counts = vec![0_u32; HUE_BINS * SATURATION_BINS];
    for pixel in sampled.pixels() {
        let [red, green, blue] = pixel.0.map(|channel| f32::from(channel) / 255.0);
        let maximum = red.max(green).max(blue);
        let minimum = red.min(green).min(blue);
        let delta = maximum - minimum;
        let hue = if delta == 0.0 {
            0.0
        } else if maximum == red {
            ((green - blue) / delta).rem_euclid(6.0) / 6.0
        } else if maximum == green {
            ((blue - red) / delta + 2.0) / 6.0
        } else {
            ((red - green) / delta + 4.0) / 6.0
        };
        let saturation = if maximum == 0.0 { 0.0 } else { delta / maximum };
        let hue_bin = ((hue * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        let saturation_bin =
            ((saturation * SATURATION_BINS as f32) as usize).min(SATURATION_BINS - 1);
        counts[hue_bin * SATURATION_BINS + saturation_bin] += 1;
    }
    let total = f64::from(sampled.width() * sampled.height());
    counts
        .iter()
        .map(|count| ((f64::from(*count) / total).sqrt() * HISTOGRAM_SCALE).round() as u16)
        .collect()
}

/// Compute the 63-bit low-frequency DCT fingerprint of the artwork region.
fn perceptual_hash(image: &RgbImage) -> u64 {
    let gray = grayscale(image, 32, 32);
    let mut coefficients = Vec::with_capacity(HASH_BITS as usize);
    // A separable transform requires only 256 cosines and about ten thousand
    // products, keeping hashing inexpensive without an FFT dependency.
    let mut cosine = [[0.0_f64; 32]; 8];
    for (frequency, row) in cosine.iter_mut().enumerate() {
        for (position, value) in row.iter_mut().enumerate() {
            *value =
                ((2 * position + 1) as f64 * frequency as f64 * std::f64::consts::PI / 64.0).cos();
        }
    }
    let mut horizontal = [[0.0_f64; 32]; 8];
    for (frequency_x, row) in horizontal.iter_mut().enumerate() {
        for (y, value) in row.iter_mut().enumerate() {
            *value = (0..32)
                .map(|x| f64::from(gray.get_pixel(x as u32, y as u32)[0]) * cosine[frequency_x][x])
                .sum();
        }
    }
    for (frequency_y, cosine_y) in cosine.iter().enumerate() {
        for (frequency_x, horizontal_x) in horizontal.iter().enumerate() {
            if frequency_x == 0 && frequency_y == 0 {
                continue;
            }
            let value: f64 = horizontal_x
                .iter()
                .zip(cosine_y)
                .map(|(value, cosine)| value * cosine)
                .sum();
            let alpha_x = if frequency_x == 0 {
                1.0 / 2.0_f64.sqrt()
            } else {
                1.0
            };
            let alpha_y = if frequency_y == 0 {
                1.0 / 2.0_f64.sqrt()
            } else {
                1.0
            };
            coefficients.push(value * alpha_x * alpha_y);
        }
    }
    let mut sorted = coefficients.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    coefficients
        .iter()
        .enumerate()
        .fold(0, |hash, (bit, value)| {
            hash | (u64::from(*value > median) << bit)
        })
}

/// Compute the Euclidean norm of a signed quantized vector.
fn signed_norm(values: &[i16]) -> f64 {
    values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Compute the Euclidean norm of an unsigned quantized histogram.
fn unsigned_norm(values: &[u16]) -> f64 {
    values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Compute positive normalized correlation, rejecting absent luminance structure.
fn signed_cosine(left: &[i16], right: &[i16], left_norm: f64, right_norm: f64) -> f32 {
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    let dot: i64 = left
        .iter()
        .zip(right)
        .map(|(a, b)| i64::from(*a) * i64::from(*b))
        .sum();
    (dot as f64 / (left_norm * right_norm)).clamp(0.0, 1.0) as f32
}

/// Compute the Hellinger coefficient from quantized square-root probabilities.
fn unsigned_cosine(left: &[u16], right: &[u16], left_norm: f64, right_norm: f64) -> f32 {
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    let dot: u64 = left
        .iter()
        .zip(right)
        .map(|(a, b)| u64::from(*a) * u64::from(*b))
        .sum();
    (dot as f64 / (left_norm * right_norm)).clamp(0.0, 1.0) as f32
}

/// Require absolute evidence and a strictly positive lead over competing matches.
fn decision(similarity: f32, margin: f32, options: &MatchOptions) -> MatchStatus {
    if similarity < options.min_similarity {
        MatchStatus::Unknown
    } else if margin < options.min_margin || margin <= f32::EPSILON {
        MatchStatus::Ambiguous
    } else {
        MatchStatus::Matched
    }
}

/// Combine absolute evidence with a conservative lead, without catalogue softmax.
fn evidence_confidence(similarity: f32, margin: f32, status: MatchStatus) -> f32 {
    if status == MatchStatus::Unknown {
        0.0
    } else {
        similarity * (margin / CONFIDENCE_MARGIN).clamp(0.0, 1.0)
    }
}

/// Regression tests exercise public enrollment, retrieval and persistence behavior.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use image::{Rgb, RgbImage};

    use super::*;

    /// Counter keeps temporary index files distinct across parallel tests.
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    /// A temporary index file removed when its test completes.
    struct TemporaryIndex(std::path::PathBuf);

    impl TemporaryIndex {
        /// Reserve a unique path without sharing mutable test state.
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "whosthatpokemon-matcher-{}-{}.json",
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl Drop for TemporaryIndex {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Generate distinct card-like artwork surrounded by shared frame and rules areas.
    fn reference(seed: u32) -> DynamicImage {
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

    /// Construct complete test metadata with an explicit reprint identity group.
    fn metadata(id: &str, oracle: &str) -> CardMetadata {
        CardMetadata {
            game: "mtg".into(),
            id: id.into(),
            name: format!("Card {oracle}"),
            oracle_id: Some(oracle.into()),
            ..CardMetadata::default()
        }
    }

    /// Enroll two independent artworks to make retrieval and rejection nontrivial.
    fn two_card_index() -> CatalogIndex {
        let mut index = CatalogIndex::new();
        index
            .add(metadata("first", "alpha"), &reference(1))
            .unwrap();
        index
            .add(metadata("second", "beta"), &reference(7))
            .unwrap();
        index
    }

    /// Right-angle orientation and changed exposure preserve the correct card evidence.
    #[test]
    fn recognizes_rotated_exposure_changed_reference() {
        let index = two_card_index();
        let query = reference(7).brighten(12).rotate90();
        let result = index.recognize(&query, &MatchOptions::default()).unwrap();
        assert_eq!(result.status, MatchStatus::Matched);
        assert_eq!(result.candidates[0].card.id, "second");
        assert_eq!(result.candidates[0].rotation_degrees, 270);
        assert!(result.confidence > 0.8, "{result:?}");
    }

    /// A border-trimmed extraction can match a corresponding reference crop.
    #[test]
    fn tolerates_a_trimmed_card_boundary() {
        let index = two_card_index();
        let image = reference(1).to_rgb8();
        let query = DynamicImage::ImageRgb8(inset_image(&image, 0.10));
        let result = index.recognize(&query, &MatchOptions::default()).unwrap();
        assert_eq!(result.status, MatchStatus::Matched);
        assert_eq!(result.candidates[0].card.id, "first");
    }

    /// A close reprint remains ambiguous even when the caller requests only one result.
    #[test]
    fn distinguishes_printing_ambiguity_from_oracle_identity() {
        let mut index = two_card_index();
        index
            .add(metadata("reprint", "alpha"), &reference(1))
            .unwrap();
        let result = index
            .recognize(
                &reference(1),
                &MatchOptions {
                    top_k: 1,
                    ..MatchOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.status, MatchStatus::Ambiguous);
        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.identity_status, MatchStatus::Matched);
        assert!(result.identity_confidence > 0.9);
    }

    /// Visually identical references from different games do not merge their identities.
    #[test]
    fn namespaces_semantic_identity_by_game() {
        let mut index = two_card_index();
        let mut other_game = metadata("copy", "alpha");
        other_game.game = "other-game".into();
        index.add(other_game, &reference(1)).unwrap();
        let result = index
            .recognize(&reference(1), &MatchOptions::default())
            .unwrap();
        assert_eq!(result.identity_status, MatchStatus::Ambiguous);
        assert_eq!(result.identity_confidence, 0.0);
    }

    /// A printing ID cannot collide with the distinct namespace of an oracle ID.
    #[test]
    fn keeps_printing_and_oracle_identifier_namespaces_distinct() {
        let mut index = two_card_index();
        let mut missing_oracle = metadata("alpha", "unused");
        missing_oracle.oracle_id = None;
        index.add(missing_oracle, &reference(1)).unwrap();
        let result = index
            .recognize(&reference(1), &MatchOptions::default())
            .unwrap();
        assert_eq!(result.status, MatchStatus::Ambiguous);
        assert_eq!(result.identity_status, MatchStatus::Ambiguous);
        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.identity_confidence, 0.0);
    }

    /// Uniform backgrounds and unseen art cannot acquire confidence from catalogue size.
    #[test]
    fn rejects_uniform_and_unseen_inputs() {
        let index = two_card_index();
        for query in [
            DynamicImage::ImageRgb8(RgbImage::from_pixel(200, 280, Rgb([200, 200, 200]))),
            reference(19),
        ] {
            let result = index.recognize(&query, &MatchOptions::default()).unwrap();
            assert_eq!(result.status, MatchStatus::Unknown, "{result:?}");
            assert_eq!(result.confidence, 0.0);
            assert_eq!(result.identity_confidence, 0.0);
        }
    }

    /// Invalid identities, duplicate physical faces and textureless references are rejected.
    #[test]
    fn validates_enrollment_before_mutation() {
        let mut index = two_card_index();
        assert!(
            index
                .add(metadata("first", "alpha"), &reference(1))
                .is_err()
        );
        assert!(index.add(CardMetadata::default(), &reference(1)).is_err());
        assert!(
            index
                .add(metadata("blank", "blank"), &DynamicImage::new_rgb8(10, 10))
                .is_err()
        );
        assert_eq!(index.len(), 2);
    }

    /// Empty dimensions and nonfinite configuration fail before descriptor extraction.
    #[test]
    fn rejects_invalid_queries_and_options() {
        let index = two_card_index();
        assert!(
            index
                .recognize(&DynamicImage::new_rgb8(0, 1), &MatchOptions::default())
                .is_err()
        );
        assert!(
            index
                .recognize(
                    &reference(1),
                    &MatchOptions {
                        min_similarity: f32::NAN,
                        ..MatchOptions::default()
                    }
                )
                .is_err()
        );
        assert!(
            CatalogIndex::new()
                .recognize(&reference(1), &MatchOptions::default())
                .is_err()
        );
    }

    /// Persistence preserves rankings and validates shape, normalization, format and identity.
    #[test]
    fn round_trips_and_rejects_corrupt_indexes() {
        let index = two_card_index();
        let temporary = TemporaryIndex::new();
        index.save(&temporary.0).unwrap();
        let loaded = CatalogIndex::load(&temporary.0).unwrap();
        let memory =
            CatalogIndex::from_reader(std::fs::read(&temporary.0).unwrap().as_slice()).unwrap();
        let query = reference(7).rotate180();
        assert_eq!(
            index.recognize(&query, &MatchOptions::default()).unwrap(),
            loaded.recognize(&query, &MatchOptions::default()).unwrap()
        );
        assert_eq!(
            memory.recognize(&query, &MatchOptions::default()).unwrap(),
            loaded.recognize(&query, &MatchOptions::default()).unwrap()
        );
        let original: serde_json::Value =
            serde_json::from_reader(File::open(&temporary.0).unwrap()).unwrap();
        let mut corruptions = Vec::new();
        let mut version = original.clone();
        version["format_version"] = 999.into();
        corruptions.push(version);
        let mut dimension = original.clone();
        dimension["references"][0]["views"][0]["art"] = serde_json::json!([1, 2]);
        corruptions.push(dimension);
        let mut normalization = original.clone();
        normalization["references"][0]["views"][0]["histogram"] =
            serde_json::json!(vec![0; HUE_BINS * SATURATION_BINS]);
        corruptions.push(normalization);
        let mut feature_position = original.clone();
        feature_position["references"][0]["features"][0]["x"] = 65535.into();
        corruptions.push(feature_position);
        let mut duplicate = original.clone();
        duplicate["references"][1] = duplicate["references"][0].clone();
        corruptions.push(duplicate);
        let mut identity = original.clone();
        identity["references"][0]["card"]["game"] = "".into();
        corruptions.push(identity);
        for value in corruptions {
            serde_json::to_writer(File::create(&temporary.0).unwrap(), &value).unwrap();
            assert!(matches!(
                CatalogIndex::load(&temporary.0),
                Err(Error::InvalidIndex(_))
            ));
            assert!(matches!(
                CatalogIndex::from_reader(serde_json::to_vec(&value).unwrap().as_slice()),
                Err(Error::InvalidIndex(_))
            ));
        }
    }

    /// Reported global components reproduce the score at the chosen inset and rotation.
    #[test]
    fn explains_the_actual_chosen_comparison() {
        let index = two_card_index();
        let query = DynamicImage::ImageRgb8(inset_image(&reference(7).to_rgb8(), 0.10)).rotate90();
        let (recognition, evidence) = index
            .recognize_explained(&query, &MatchOptions::default())
            .unwrap();
        let best = &recognition.candidates[0];
        let explanation = &evidence[0];
        assert_eq!(best.card.id, "second");
        assert_eq!(explanation.id, best.card.id);
        assert_eq!(explanation.global_rotation_degrees, 270);
        assert_eq!(explanation.reference_inset, 0.10);
        assert_eq!(explanation.method, "global_descriptors");
        let sum = 0.20 * explanation.full_luminance
            + 0.45 * explanation.artwork_luminance
            + 0.05 * explanation.artwork_hash
            + 0.30 * explanation.artwork_colour;
        assert!((best.similarity - sum).abs() < 1e-6);
        assert!((explanation.global_similarity - sum).abs() < 1e-6);
    }
}
