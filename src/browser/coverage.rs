//! Project verified canonical faces into their original camera frame without inventing pixels.

use serde::Serialize;

use crate::{
    CardMetadata, MatchStatus,
    matching::{CAMERA_FEATURE_DIMENSIONS, CameraProjection, GeometryVerification},
    mtg::{
        identify::{
            CardIdentity, CombinedStatus, FieldIdentification, IdentityDecision, PrintingDecision,
        },
        recognition::IdentifiedCard,
    },
};

/// Camera-only rejection policy; absence of a rejection is not a visibility certificate.
pub(super) const POLICY: &str = "canonical_source_visibility_v1";
/// Local features and homographies use this exact normalized reference width.
const NORMALIZED_WIDTH: f64 = CAMERA_FEATURE_DIMENSIONS.0 as f64;
/// Local features and homographies use this exact normalized reference height.
const NORMALIZED_HEIGHT: f64 = CAMERA_FEATURE_DIMENSIONS.1 as f64;
/// Inset the reference by the local matcher's three-pixel inlier tolerance.
///
/// This deliberately withholds near-boundary extrapolations from the rejection
/// rule. It is a conservative heuristic, not a calibrated uncertainty interval.
const REFERENCE_INSET: f64 = 3.0;

/// Incomplete-view feedback used by both effective decisions and the explicit witness.
const INCOMPLETE_REASON: &str = "Incomplete card—move the whole card into view. Verified reference geometry extends beyond the original camera frame; no identity or printing is accepted.";

/// Retain only actual verified faces supporting the selected accepted semantic identity.
///
/// Unverified text proposals, alternative identities, conflicts and abstentions
/// cannot become evidence for this camera-only rejection rule.
pub(super) fn supporting_faces(card: &IdentifiedCard) -> Vec<GeometryVerification> {
    if card.orientation_conflict {
        return Vec::new();
    }
    let Some(attempt) = card.attempts.get(card.selected_attempt) else {
        return Vec::new();
    };
    let result = &attempt.identification.result;
    let Some(identity) = result.identity.card.as_ref().filter(|_| {
        matches!(
            result.identity.status,
            CombinedStatus::Matched | CombinedStatus::VerifiedShortlist
        )
    }) else {
        return Vec::new();
    };
    let verified = result
        .visual
        .geometry_verified
        .iter()
        .filter(|verification| supports_identity(&verification.card, identity))
        .cloned()
        .collect::<Vec<_>>();
    if has_unrepresented_field_support(&attempt.identification, &verified, identity)
        || attempt
            .restricted_reference_alignment
            .as_ref()
            .is_some_and(|earlier| has_unrepresented_field_support(earlier, &verified, identity))
    {
        return Vec::new();
    }
    verified
}

/// Every retained visual pass must fit within the final bounded reference coverage.
///
/// A broader reference-orientation retry can replace both the main visual result
/// and its earlier exact-footer evidence. Neither retained narrow-pass geometry
/// nor either footer pass may silently disappear from a clipping claim.
fn has_unrepresented_field_support(
    earlier: &FieldIdentification,
    current: &[GeometryVerification],
    identity: &CardIdentity,
) -> bool {
    has_unrepresented_support(&earlier.result.visual.geometry_verified, current, identity)
        || earlier.footer_verification.as_ref().is_some_and(|footer| {
            has_unrepresented_support(&footer.geometry_verified, current, identity)
        })
}

/// An earlier same-attempt pass must not hide a compatible alternative extent.
///
/// Keep the existing eight-face recomputation bound: if the earlier exact-footer
/// or restricted-orientation pass verified a compatible face absent or changed
/// in the final pass, this camera-only rejection has insufficient reproducible
/// coverage and is withheld.
fn has_unrepresented_support(
    earlier: &[GeometryVerification],
    current: &[GeometryVerification],
    identity: &CardIdentity,
) -> bool {
    earlier
        .iter()
        .filter(|entry| supports_identity(&entry.card, identity))
        .any(|entry| {
            !current.iter().any(|other| {
                same_face(&entry.card, &other.card)
                    && entry.similarity.is_finite()
                    && other.similarity.is_finite()
                    && (entry.similarity - other.similarity).abs() <= 1e-5
                    && entry.rotation_degrees == other.rotation_degrees
            })
        })
}

/// Compare exact physical-face keys without grouping different reprints together.
fn same_face(left: &CardMetadata, right: &CardMetadata) -> bool {
    left.game == right.game && left.id == right.id && left.face_index == right.face_index
}

/// Preserve the matcher's game-scoped Oracle grouping and absent-Oracle printing fallback.
fn supports_identity(card: &CardMetadata, identity: &CardIdentity) -> bool {
    card.game == identity.game
        && match (&card.oracle_id, &identity.oracle_id) {
            (Some(left), Some(right)) => left == right,
            (None, None) => identity.printing_id.as_deref() == Some(card.id.as_str()),
            _ => false,
        }
}

/// Reject only when every compatible verified face has reproducible outside-source evidence.
///
/// A missing, changed, invalid or inside-source projection prevents rejection;
/// no reference acquisition or alternative identity search is performed here.
pub(super) fn rejection(
    verified: &[GeometryVerification],
    projections: &[CameraProjection],
    crop_dimensions: (u32, u32),
    source_corners: [[f32; 2]; 4],
    source_dimensions: (u32, u32),
) -> Option<CaptureRejection> {
    if verified.is_empty() || projections.len() != verified.len() {
        return None;
    }
    let mut evidence = Vec::with_capacity(verified.len());
    for verification in verified {
        let projection = projections
            .iter()
            .find(|projection| same_face(&projection.card, &verification.card))?;
        if !projection.similarity.is_finite()
            || !verification.similarity.is_finite()
            || (projection.similarity - verification.similarity).abs() > 1e-5
            || projection.rotation_degrees != verification.rotation_degrees
        {
            return None;
        }
        let extent = source_extent(
            projection.projection,
            crop_dimensions,
            source_corners,
            source_dimensions,
        )?;
        if !extent.outside {
            return None;
        }
        evidence.push(ExtentEvidence {
            reference_card: projection.card.clone(),
            similarity: projection.similarity,
            rotation_degrees: projection.rotation_degrees,
            query_to_reference: projection.projection,
            canonical_corners: extent.corners,
            inset_corners: extent.inset_corners,
        });
    }
    Some(CaptureRejection {
        policy: POLICY,
        reason_code: "canonical_extent_outside_source",
        reason: INCOMPLETE_REASON,
        evidence,
        rejected_model_proposals: Vec::new(),
    })
}

/// One already-verified model reference and its projected camera-space extent.
#[derive(Serialize)]
pub(super) struct ExtentEvidence {
    /// Exact physical-face metadata used by the existing geometric verifier.
    pub(super) reference_card: CardMetadata,
    /// Existing geometric score, not a probability or human annotation.
    pub(super) similarity: f32,
    /// Orientation already contained in the homography; never applied a second time.
    pub(super) rotation_degrees: u16,
    /// Original normalized query-to-reference projective matrix.
    pub(super) query_to_reference: [f32; 9],
    /// Whole canonical raster boundary projected into original source pixels.
    pub(super) canonical_corners: [[f64; 2]; 4],
    /// Conservative three-normalized-pixel inset, still outside the camera frame.
    pub(super) inset_corners: [[f64; 2]; 4],
}

/// Explicit camera-policy rejection, never a human-supplied training label.
#[derive(Serialize)]
pub(super) struct CaptureRejection {
    /// Version of the camera-only policy producing this rejection.
    pub(super) policy: &'static str,
    /// Stable machine-readable reason for requiring a new whole-card view.
    pub(super) reason_code: &'static str,
    /// User-facing explanation; shared decisions are also demoted to abstentions.
    pub(super) reason: &'static str,
    /// Every compatible already-verified reference must support the rejection.
    pub(super) evidence: Vec<ExtentEvidence>,
    /// Former decisions retained explicitly as rejected model proposals, never labels.
    pub(super) rejected_model_proposals: Vec<RejectedModelProposal>,
}

/// A formerly accepted model decision retained only for diagnosing the rejection.
#[derive(Serialize)]
pub(super) struct RejectedModelProposal {
    /// Original position of this orientation attempt.
    attempt_index: usize,
    /// Original semantic model proposal before the camera-only guard.
    identity: IdentityDecision,
    /// Original printing model proposal before the camera-only guard.
    printing: PrintingDecision,
}

impl CaptureRejection {
    /// Demote every effective orientation decision while preserving diagnostic evidence.
    pub(super) fn apply(&mut self, card: &mut IdentifiedCard) {
        for (attempt_index, attempt) in card.attempts.iter_mut().enumerate() {
            let result = &mut attempt.identification.result;
            self.rejected_model_proposals.push(RejectedModelProposal {
                attempt_index,
                identity: result.identity.clone(),
                printing: result.printing.clone(),
            });
            demote_decisions(&mut result.identity, &mut result.printing, self.reason);
        }
    }
}

/// Abstentions use existing public statuses so older status-based consumers remain honest.
fn demote_decisions(
    identity: &mut IdentityDecision,
    printing: &mut PrintingDecision,
    reason: &str,
) {
    identity.status = CombinedStatus::Unknown;
    identity.card = None;
    identity.sources.clear();
    identity.reason = reason.to_owned();
    printing.status = MatchStatus::Unknown;
    printing.card = None;
    printing.reason = reason.to_owned();
}

/// Finite, horizon-free reference extent and a conservative clipping observation.
pub(super) struct SourceExtent {
    /// Whole reference boundary, not a replacement crop or detected outline.
    pub(super) corners: [[f64; 2]; 4],
    /// Reference interior used to avoid rejecting small boundary-fit errors.
    pub(super) inset_corners: [[f64; 2]; 4],
    /// Even the inset reference extends beyond the original source raster.
    pub(super) outside: bool,
}

/// Compose the verified local map with the exact resize and rectification geometry.
///
/// The local map already includes any 180-degree matcher orientation. Image
/// resizing maps pixel centres as `(x + 0.5) * input / output - 0.5`; rectification
/// maps the supplied source corners to the crop's first and last pixel centres.
/// Invalid, mirrored, degenerate or horizon-crossing extrapolations return no
/// evidence. This function neither reads nor replaces any captured image pixels.
pub(super) fn source_extent(
    query_to_reference: [f32; 9],
    crop_dimensions: (u32, u32),
    source_corners: [[f32; 2]; 4],
    source_dimensions: (u32, u32),
) -> Option<SourceExtent> {
    let (crop_width, crop_height) = crop_dimensions;
    let (source_width, source_height) = source_dimensions;
    if crop_width < 2 || crop_height < 2 || source_width == 0 || source_height == 0 {
        return None;
    }
    let source_corners = source_corners.map(|point| point.map(f64::from));
    if !clockwise_convex(&source_corners)
        || source_corners
            .iter()
            .any(|point| outside_raster(*point, source_dimensions))
    {
        return None;
    }
    let reference_to_query = inverse(query_to_reference.map(f64::from))?;
    let crop_to_source = unit_square_to_quad(source_corners)?;
    let crop_width = f64::from(crop_width);
    let crop_height = f64::from(crop_height);
    let normalized_to_crop_unit = [
        crop_width / (NORMALIZED_WIDTH * (crop_width - 1.0)),
        0.0,
        (0.5 * crop_width / NORMALIZED_WIDTH - 0.5) / (crop_width - 1.0),
        0.0,
        crop_height / (NORMALIZED_HEIGHT * (crop_height - 1.0)),
        (0.5 * crop_height / NORMALIZED_HEIGHT - 0.5) / (crop_height - 1.0),
        0.0,
        0.0,
        1.0,
    ];
    let map = multiply(
        crop_to_source,
        multiply(normalized_to_crop_unit, reference_to_query),
    );
    let corners = project_rectangle(&map, 0.0)?;
    let inset_corners = project_rectangle(&map, REFERENCE_INSET)?;
    Some(SourceExtent {
        corners,
        outside: inset_corners
            .iter()
            .any(|point| outside_raster(*point, source_dimensions)),
        inset_corners,
    })
}

/// Compare pixel boundaries, not an aspect-ratio guess or a clamped extrapolation.
fn outside_raster(point: [f64; 2], dimensions: (u32, u32)) -> bool {
    point[0] < -0.5
        || point[1] < -0.5
        || point[0] > f64::from(dimensions.0) - 0.5
        || point[1] > f64::from(dimensions.1) - 0.5
}

/// Reject nonfinite, mirrored, crossed and collapsed quadrilaterals.
fn clockwise_convex(points: &[[f64; 2]; 4]) -> bool {
    points.iter().flatten().all(|value| value.is_finite())
        && (0..4).all(|index| {
            let a = points[index];
            let b = points[(index + 1) % 4];
            let c = points[(index + 2) % 4];
            (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]) > 1e-8
        })
}

/// Build the projective map from a unit rectangle to an ordered convex source quad.
fn unit_square_to_quad(points: [[f64; 2]; 4]) -> Option<[f64; 9]> {
    let [a, b, c, d] = points;
    let dx1 = b[0] - c[0];
    let dx2 = d[0] - c[0];
    let dx3 = a[0] - b[0] + c[0] - d[0];
    let dy1 = b[1] - c[1];
    let dy2 = d[1] - c[1];
    let dy3 = a[1] - b[1] + c[1] - d[1];
    let denominator = dx1 * dy2 - dx2 * dy1;
    if !denominator.is_finite() || denominator.abs() < 1e-12 {
        return None;
    }
    let g = (dx3 * dy2 - dx2 * dy3) / denominator;
    let h = (dx1 * dy3 - dx3 * dy1) / denominator;
    let matrix = [
        b[0] - a[0] + g * b[0],
        d[0] - a[0] + h * d[0],
        a[0],
        b[1] - a[1] + g * b[1],
        d[1] - a[1] + h * d[1],
        a[1],
        g,
        h,
        1.0,
    ];
    matrix
        .iter()
        .all(|value| value.is_finite())
        .then_some(matrix)
}

/// Multiply row-major projective matrices in application order, using double precision.
fn multiply(left: [f64; 9], right: [f64; 9]) -> [f64; 9] {
    std::array::from_fn(|index| {
        let row = index / 3;
        let column = index % 3;
        (0..3)
            .map(|inner| left[row * 3 + inner] * right[inner * 3 + column])
            .sum()
    })
}

/// Invert a finite 3×3 matrix with a scale-independent singularity guard.
fn inverse(matrix: [f64; 9]) -> Option<[f64; 9]> {
    let scale = matrix.iter().map(|value| value.abs()).fold(0.0, f64::max);
    if !matrix.iter().all(|value| value.is_finite()) || scale == 0.0 {
        return None;
    }
    let [a, b, c, d, e, f, g, h, i] = matrix.map(|value| value / scale);
    let determinant = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if !determinant.is_finite() || determinant.abs() < 1e-12 {
        return None;
    }
    Some(
        [
            e * i - f * h,
            c * h - b * i,
            b * f - c * e,
            f * g - d * i,
            a * i - c * g,
            c * d - a * f,
            d * h - e * g,
            b * g - a * h,
            a * e - b * d,
        ]
        .map(|value| value / determinant),
    )
}

/// Project an entire canonical rectangle only when its denominator has no horizon.
fn project_rectangle(matrix: &[f64; 9], inset: f64) -> Option<[[f64; 2]; 4]> {
    if !matrix.iter().all(|value| value.is_finite()) {
        return None;
    }
    let points = [
        [-0.5 + inset, -0.5 + inset],
        [NORMALIZED_WIDTH - 0.5 - inset, -0.5 + inset],
        [
            NORMALIZED_WIDTH - 0.5 - inset,
            NORMALIZED_HEIGHT - 0.5 - inset,
        ],
        [-0.5 + inset, NORMALIZED_HEIGHT - 0.5 - inset],
    ];
    let denominators = points.map(|[x, y]| matrix[6] * x + matrix[7] * y + matrix[8]);
    let maximum = denominators
        .iter()
        .map(|value| value.abs())
        .fold(0.0, f64::max);
    if maximum == 0.0
        || denominators.iter().any(|value| {
            !value.is_finite()
                || value.abs() <= maximum * 1e-8
                || value.is_sign_positive() != denominators[0].is_sign_positive()
        })
    {
        return None;
    }
    let projected = std::array::from_fn(|index| {
        let [x, y] = points[index];
        [
            (matrix[0] * x + matrix[1] * y + matrix[2]) / denominators[index],
            (matrix[3] * x + matrix[4] * y + matrix[5]) / denominators[index],
        ]
    });
    clockwise_convex(&projected).then_some(projected)
}

/// Synthetic geometry checks avoid making private captures into committed test fixtures.
#[cfg(test)]
mod tests {
    use super::*;

    /// Identity local registration for a whole-card crop.
    const IDENTITY: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    /// Ordinary portrait camera region, away from every source boundary.
    const QUAD: [[f32; 2]; 4] = [
        [100.0, 200.0],
        [599.0, 200.0],
        [599.0, 899.0],
        [100.0, 899.0],
    ];

    /// Pixel centres and outer boundaries remain distinct under exact normalization.
    #[test]
    fn full_reference_inside_source_keeps_its_geometry() {
        let extent = source_extent(IDENTITY, (500, 700), QUAD, (720, 1280)).unwrap();
        assert!(!extent.outside);
        assert!((extent.corners[0][0] - 99.5).abs() < 1e-8);
        assert!((extent.corners[0][1] - 199.5).abs() < 1e-8);
        assert!((extent.corners[2][0] - 599.5).abs() < 1e-8);
        assert!((extent.corners[2][1] - 899.5).abs() < 1e-8);
    }

    /// A shortened capture can register well while its real right edge is off-camera.
    #[test]
    fn inverse_registration_exposes_clipped_reference() {
        let shifted = [1.0, 0.0, -70.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let extent = source_extent(shifted, (500, 700), QUAD, (720, 1280)).unwrap();
        assert!(extent.outside);
        assert!(extent.inset_corners[2][0] > 719.5);
    }

    /// Do not turn sub-inlier boundary noise into a clipped-card rejection.
    #[test]
    fn inset_protects_near_boundary_whole_cards() {
        let near_edge = QUAD.map(|[x, y]| [x + 120.0, y]);
        let extent = source_extent(IDENTITY, (500, 700), near_edge, (720, 1280)).unwrap();
        assert!(!extent.outside);
        let slightly_shifted = [1.0, 0.0, -0.25, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let extent = source_extent(slightly_shifted, (500, 700), near_edge, (720, 1280)).unwrap();
        assert!(extent.corners[2][0] > 719.5);
        assert!(!extent.outside);
    }

    /// The homography contains the half-turn already; applying it twice is incorrect.
    #[test]
    fn half_turn_preserves_the_same_source_extent() {
        let half_turn = [-1.0, 0.0, 251.0, 0.0, -1.0, 350.0, 0.0, 0.0, 1.0];
        let forward = source_extent(IDENTITY, (500, 700), QUAD, (720, 1280)).unwrap();
        let inverted = source_extent(half_turn, (500, 700), QUAD, (720, 1280)).unwrap();
        for index in 0..4 {
            for axis in 0..2 {
                assert!(
                    (forward.corners[index][axis] - inverted.corners[(index + 2) % 4][axis]).abs()
                        < 1e-8
                );
            }
        }
        assert!(!inverted.outside);
    }

    /// Source control-point ordering, nonfinite maps and singular maps cannot prove clipping.
    #[test]
    fn malformed_geometry_has_no_visibility_evidence() {
        let mut nonfinite = IDENTITY;
        nonfinite[0] = f32::NAN;
        assert!(source_extent(nonfinite, (500, 700), QUAD, (720, 1280)).is_none());
        assert!(source_extent([0.0; 9], (500, 700), QUAD, (720, 1280)).is_none());
        assert!(source_extent(IDENTITY, (1, 700), QUAD, (720, 1280)).is_none());
        let mirrored = [QUAD[1], QUAD[0], QUAD[3], QUAD[2]];
        assert!(source_extent(IDENTITY, (500, 700), mirrored, (720, 1280)).is_none());
        let crossed = [QUAD[0], QUAD[2], QUAD[1], QUAD[3]];
        assert!(source_extent(IDENTITY, (500, 700), crossed, (720, 1280)).is_none());
        let invalid_source = QUAD.map(|[x, y]| [x + 500.0, y]);
        assert!(source_extent(IDENTITY, (500, 700), invalid_source, (720, 1280)).is_none());
    }

    /// Finite projected corners alone do not exclude a horizon inside the reference.
    #[test]
    fn horizon_crossing_and_mirrored_reference_are_rejected() {
        let horizon = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.01, 0.0, -1.0];
        assert!(project_rectangle(&horizon, 0.0).is_none());
        let reflection = [-1.0, 0.0, 251.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        assert!(source_extent(reflection, (500, 700), QUAD, (720, 1280)).is_none());
    }

    /// Source rectification agrees with the dependency's actual perspective map.
    #[test]
    fn perspective_composition_agrees_with_rectification() {
        use imageproc::geometric_transformations::Projection;
        let quad = [
            [100.0, 200.0],
            [610.0, 245.0],
            [570.0, 1000.0],
            [80.0, 880.0],
        ];
        let extent = source_extent(IDENTITY, (500, 700), quad, (720, 1280)).unwrap();
        let map = Projection::from_control_points(
            [(0.0, 0.0), (499.0, 0.0), (499.0, 699.0), (0.0, 699.0)],
            quad.map(|[x, y]| (x, y)),
        )
        .unwrap();
        for (index, point) in [(-0.5, -0.5), (499.5, -0.5), (499.5, 699.5), (-0.5, 699.5)]
            .into_iter()
            .enumerate()
        {
            let expected = map * point;
            assert!((extent.corners[index][0] - f64::from(expected.0)).abs() < 0.001);
            assert!((extent.corners[index][1] - f64::from(expected.1)).abs() < 0.001);
        }
    }

    /// Identity and printing acceptance must both disappear, not only a UI flag.
    #[test]
    fn rejection_demotes_identity_and_printing_decisions() {
        use crate::{
            matching::ShortlistScope,
            mtg::identify::{CardIdentity, EvidenceSource},
        };
        let mut identity = IdentityDecision {
            status: CombinedStatus::VerifiedShortlist,
            card: Some(CardIdentity {
                game: "invented".into(),
                oracle_id: Some("identity".into()),
                printing_id: None,
                name: "Model proposal".into(),
            }),
            scope: ShortlistScope::SuppliedCandidates,
            sources: vec![EvidenceSource::VisualGeometry, EvidenceSource::Title],
            reason: "Original accepted model decision".into(),
        };
        let mut printing = PrintingDecision {
            status: MatchStatus::Matched,
            card: Some(CardMetadata::default()),
            scope: ShortlistScope::SuppliedCandidates,
            reason: "Original printing proposal".into(),
        };
        demote_decisions(
            &mut identity,
            &mut printing,
            "Move the whole card into view",
        );
        assert_eq!(identity.status, CombinedStatus::Unknown);
        assert!(identity.card.is_none());
        assert!(identity.sources.is_empty());
        assert_eq!(printing.status, MatchStatus::Unknown);
        assert!(printing.card.is_none());
        assert_eq!(identity.reason, printing.reason);
    }

    /// Missing or contradictory compatible reference geometry cannot prove clipping.
    #[test]
    fn rejection_requires_every_verified_reference_to_agree() {
        let metadata = |id: &str| CardMetadata {
            game: "invented".into(),
            id: id.into(),
            oracle_id: Some("identity".into()),
            ..CardMetadata::default()
        };
        let verified = |id: &str| GeometryVerification {
            card: metadata(id),
            similarity: 0.9,
            rotation_degrees: 0,
            global_similarity: 0.8,
        };
        let projection = |id: &str, matrix| CameraProjection {
            card: metadata(id),
            similarity: 0.9,
            rotation_degrees: 0,
            projection: matrix,
        };
        let clipped = [1.0, 0.0, -70.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let faces = [verified("one"), verified("two")];
        assert!(
            rejection(
                &faces[..1],
                &[projection("one", clipped)],
                (500, 700),
                QUAD,
                (720, 1280)
            )
            .is_some()
        );
        assert!(
            rejection(
                &faces,
                &[projection("one", clipped)],
                (500, 700),
                QUAD,
                (720, 1280)
            )
            .is_none()
        );
        assert!(
            rejection(
                &faces,
                &[projection("one", clipped), projection("two", IDENTITY)],
                (500, 700),
                QUAD,
                (720, 1280)
            )
            .is_none()
        );
        let all_clipped = [projection("one", clipped), projection("two", clipped)];
        let witness = rejection(&faces, &all_clipped, (500, 700), QUAD, (720, 1280)).unwrap();
        assert_eq!(witness.evidence.len(), 2);
        assert_eq!(witness.reason_code, "canonical_extent_outside_source");
        let mut changed = projection("one", clipped);
        changed.similarity = 0.85;
        assert!(rejection(&faces[..1], &[changed], (500, 700), QUAD, (720, 1280)).is_none());
        let mut changed = projection("one", clipped);
        changed.rotation_degrees = 180;
        assert!(rejection(&faces[..1], &[changed], (500, 700), QUAD, (720, 1280)).is_none());
        assert!(rejection(&[], &[], (500, 700), QUAD, (720, 1280)).is_none());
    }

    /// No-Oracle cards use the printing fallback, and game namespaces never cross.
    #[test]
    fn supporting_identity_matches_the_existing_semantic_grouping() {
        let mut card = CardMetadata {
            game: "invented".into(),
            id: "printing".into(),
            oracle_id: Some("oracle".into()),
            ..CardMetadata::default()
        };
        let mut identity = CardIdentity {
            game: "invented".into(),
            oracle_id: Some("oracle".into()),
            printing_id: None,
            name: "diagnostic".into(),
        };
        assert!(supports_identity(&card, &identity));
        identity.game = "other".into();
        assert!(!supports_identity(&card, &identity));
        identity.game = card.game.clone();
        identity.oracle_id = None;
        identity.printing_id = Some(card.id.clone());
        assert!(!supports_identity(&card, &identity));
        card.oracle_id = None;
        assert!(supports_identity(&card, &identity));
        identity.printing_id = Some("other-printing".into());
        assert!(!supports_identity(&card, &identity));
    }

    /// Earlier compatible exact-footer verifications cannot silently disappear after expansion.
    #[test]
    fn earlier_verified_alternatives_prevent_an_incomplete_coverage_claim() {
        let identity = CardIdentity {
            game: "invented".into(),
            oracle_id: Some("oracle".into()),
            printing_id: None,
            name: "diagnostic".into(),
        };
        let verified = |id: &str, oracle: &str| GeometryVerification {
            card: CardMetadata {
                game: "invented".into(),
                id: id.into(),
                oracle_id: Some(oracle.into()),
                ..CardMetadata::default()
            },
            similarity: 0.9,
            rotation_degrees: 0,
            global_similarity: 0.8,
        };
        let final_faces = [verified("current", "oracle")];
        assert!(!has_unrepresented_support(
            &final_faces,
            &final_faces,
            &identity
        ));
        assert!(!has_unrepresented_support(
            &[verified("unrelated", "other")],
            &final_faces,
            &identity
        ));
        assert!(has_unrepresented_support(
            &[verified("earlier", "oracle")],
            &final_faces,
            &identity
        ));
        let mut changed = verified("current", "oracle");
        changed.rotation_degrees = 180;
        assert!(has_unrepresented_support(
            &[changed],
            &final_faces,
            &identity
        ));
        let mut changed = verified("current", "oracle");
        changed.similarity = f32::NAN;
        assert!(has_unrepresented_support(
            &[changed],
            &final_faces,
            &identity
        ));
    }

    /// Retained narrow retries and their footer passes cannot lose compatible extents.
    #[test]
    fn restricted_alignment_and_its_footer_require_complete_reference_coverage() {
        let identity = CardIdentity {
            game: "invented".into(),
            oracle_id: Some("oracle".into()),
            printing_id: None,
            name: "diagnostic".into(),
        };
        let verified = |id: &str| GeometryVerification {
            card: CardMetadata {
                game: "invented".into(),
                id: id.into(),
                oracle_id: Some("oracle".into()),
                ..CardMetadata::default()
            },
            similarity: 0.9,
            rotation_degrees: 0,
            global_similarity: 0.8,
        };
        let final_faces = [verified("current")];
        let mut earlier: FieldIdentification = serde_json::from_value(serde_json::json!({
            "observations": {"title": "", "footer": "", "rules": ""},
            "text": {"title": [], "rules": [], "footer": [], "hints": []},
            "visual": {
                "recognition": {"status": "unknown", "identity_status": "unknown",
                    "confidence": 0.0, "identity_confidence": 0.0, "candidates": []},
                "candidate_printing_margin": 0.0, "candidate_identity_margin": 0.0,
                "scope": "supplied_candidates", "search_complete": false,
                "total_references": 2, "proposal_count": 0, "matched_proposals": 0,
                "candidate_count": 0, "descriptors_scored": 0, "geometry_attempted": 0,
                "geometry_candidates": [], "geometry_verified_count": 0,
                "budget_exhausted": false, "omitted_candidate_groups": 0,
                "descriptor_budget_exhausted": false, "geometry_budget_exhausted": false,
                "omitted_descriptor_groups": 0, "omitted_geometry_groups": 0,
                "geometry_verified": []
            },
            "identity": {"status": "unknown", "card": null,
                "scope": "supplied_candidates", "sources": [], "reason": "prior abstention"},
            "printing": {"status": "unknown", "card": null,
                "scope": "supplied_candidates", "reason": "prior abstention"},
            "conflicts": []
        }))
        .unwrap();
        earlier.result.visual.geometry_verified = final_faces.to_vec();
        earlier.footer_verification = Some(earlier.result.visual.clone());
        assert!(!has_unrepresented_field_support(
            &earlier,
            &final_faces,
            &identity
        ));

        earlier.result.visual.geometry_verified = vec![verified("narrow-only")];
        assert!(has_unrepresented_field_support(
            &earlier,
            &final_faces,
            &identity
        ));

        earlier.result.visual.geometry_verified = final_faces.to_vec();
        earlier
            .footer_verification
            .as_mut()
            .unwrap()
            .geometry_verified = vec![verified("narrow-footer-only")];
        assert!(has_unrepresented_field_support(
            &earlier,
            &final_faces,
            &identity
        ));

        earlier.footer_verification = None;
        earlier.result.visual.geometry_verified[0].rotation_degrees = 180;
        assert!(has_unrepresented_field_support(
            &earlier,
            &final_faces,
            &identity
        ));
        earlier.result.visual.geometry_verified[0].rotation_degrees = 0;
        earlier.result.visual.geometry_verified[0].similarity = 0.8;
        assert!(has_unrepresented_field_support(
            &earlier,
            &final_faces,
            &identity
        ));

        earlier.result.visual.geometry_verified[0].card.oracle_id = Some("unrelated".into());
        assert!(!has_unrepresented_field_support(
            &earlier,
            &final_faces,
            &identity
        ));
        assert_eq!(
            final_faces.len(),
            1,
            "earlier evidence never expands the final budget"
        );
    }
}
