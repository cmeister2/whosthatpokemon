//! Bounded resident-only homographies for camera-specific source completeness checks.

use image::{DynamicImage, imageops::FilterType};
use std::collections::HashSet;

use super::{Reference, local, validate_image};
use crate::{CardMetadata, Error};

/// Exact normalized query/reference dimensions used by portable local geometry.
pub(crate) const CAMERA_FEATURE_DIMENSIONS: (u32, u32) = (local::WIDTH, local::HEIGHT);
/// Maximum exact faces recomputed after one already completed bounded camera identification.
const MAX_CAMERA_PROJECTIONS: usize = 8;

/// Camera-only local evidence; it does not change global ranking or cropped-image recognition.
pub(crate) struct CameraProjection {
    /// Canonical metadata of the resident exact physical face.
    pub(crate) card: CardMetadata,
    /// Unmodified deterministic local correspondence/coverage evidence score.
    pub(crate) similarity: f32,
    /// Clockwise right-angle query-to-reference orientation already encoded in the projection.
    pub(crate) rotation_degrees: u16,
    /// Row-major query-to-canonical transform in the exact normalized feature pixel frame.
    pub(crate) projection: [f32; 9],
}

/// Validate bounded exact-face requests before lookup, feature allocation or reference work.
///
/// # Errors
///
/// Returns invalid input for more than eight requests or duplicate physical-face keys.
pub(super) fn validate_requests(cards: &[CardMetadata]) -> Result<(), Error> {
    if cards.len() > MAX_CAMERA_PROJECTIONS {
        return Err(Error::InvalidInput(
            "camera projection requests exceed the eight-face bound".into(),
        ));
    }
    let mut seen = HashSet::new();
    for card in cards {
        if !seen.insert((card.game.as_str(), card.id.as_str(), card.face_index)) {
            return Err(Error::InvalidInput(
                "camera projection requests contain a duplicate physical face".into(),
            ));
        }
    }
    Ok(())
}

/// Recompute local transforms from one exact query bank and already-resident verified faces.
///
/// The caller supplies only physical faces verified for this same immutable query.
/// This returns local evidence without introducing a new score threshold, choosing
/// an identity, fetching references, or interpreting canonical bounds as source bounds.
///
/// # Errors
///
/// Returns an input error for excessive resident faces or invalid query dimensions.
pub(super) fn project_resident(
    image: &DynamicImage,
    references: &[&Reference],
) -> Result<Vec<CameraProjection>, Error> {
    if references.len() > MAX_CAMERA_PROJECTIONS {
        return Err(Error::InvalidInput(
            "camera projection references exceed the eight-face bound".into(),
        ));
    }
    validate_image(image)?;
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let rgb = image.to_rgb8();
    let normalized =
        image::imageops::resize(&rgb, local::WIDTH, local::HEIGHT, FilterType::Triangle);
    let query = local::extract_query(image, &normalized);
    Ok(references
        .iter()
        .filter_map(|reference| {
            let evidence = local::match_evidence(&query, &reference.features)?;
            Some(CameraProjection {
                card: reference.card.clone(),
                similarity: evidence.score,
                rotation_degrees: evidence.rotation_degrees,
                projection: evidence.projection,
            })
        })
        .collect())
}
