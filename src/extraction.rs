//! Shared physical-outline recovery from luminance and opponent-color evidence.
//!
//! The unchanged production detector supplies the baseline. Full mode also tests
//! one coarser source image and a blue-minus-red opponent plane, whose contrast
//! can preserve card/table boundaries hidden by luminance. Additional outlines
//! need distributed edge evidence in the original RGB pixels before they may
//! replace an enclosed printed panel or add a separate card. Rectification always
//! samples the original image; transformed pixels are never recognition input.

use image::{DynamicImage, GrayImage, Luma, RgbImage};
use magicextract::{CardDetection, DetectionMode, Error, ExtractedCard, ExtractorConfig, Point};

/// Stable identifier for the shared source-supported color-outline policy.
///
/// Capture and recognition reports retain this value so saved evidence identifies
/// the extraction policy that produced its corners and original-pixel crops.
pub const POLICY_ID: &str = "source_supported_full_card_v2";

/// Required source-pixel clearance from the image or configured detection-window edge.
const BOUNDARY_MARGIN: f32 = 3.0;
/// Human-readable explanation retained with boundary-touching source outlines.
const INCOMPLETE_REASON: &str = "Incomplete card outline—move the whole card fully into view.";
/// Explains why an internal printed panel cannot stand in for its incomplete physical card.
const INCOMPLETE_PANEL_REASON: &str =
    "Region is inside an incomplete card outline—move the whole card fully into view.";
/// Explain source evidence outside an internal panel without inventing missing card corners.
const UNSEEN_OUTER_CORNER_REASON: &str =
    "Outer-border evidence extends beyond this view—move the whole card fully into view.";

/// Number of distributed original-image samples along each candidate side.
const EDGE_SAMPLES: usize = 40;
/// Distance on either side of a boundary used for independent RGB comparison.
const EDGE_OFFSET: f32 = 4.0;
/// Bound the additional opponent-plane contour pass while omitting broad color recovery.
const FAST_OPPONENT_DIMENSION: u32 = 1280;
/// Required lower-quartile mean RGB contrast on every additional candidate side.
const MINIMUM_EDGE_CONTRAST: f32 = 12.0;
/// Weaker physical context may veto an inner panel without admitting a new recovery outline.
const MINIMUM_CONTEXT_CONTRAST: f32 = 10.0;
/// Exclude the original edge's immediate localization band from outer-border evidence.
const MINIMUM_OUTER_EDGE_DISTANCE: usize = 7;
/// Search only a narrow source-pixel strip beyond a candidate, relative to its shortest side.
const MAXIMUM_OUTER_EDGE_FRACTION: f32 = 0.08;
/// Bound source sampling even for unusually large native images.
const MAXIMUM_OUTER_EDGE_DISTANCE: usize = 80;
/// Outer uncertainty needs sustained, substantial RGB contrast, not weak background texture.
const MINIMUM_OUTER_EDGE_CONTRAST: f32 = 24.0;
/// Require outer evidence comparable to the candidate edge, independently of pixel colors.
const MINIMUM_RELATIVE_OUTER_CONTRAST: f32 = 0.6;
/// Enclosed fraction that associates a proposal with one existing printed panel.
const MINIMUM_CONTAINMENT: f32 = 0.95;
/// Minimum enlargement that justifies replacing baseline geometry at all.
const MINIMUM_ENLARGEMENT: f32 = 1.15;
/// Conservative maximum enlargement of an observed, enclosed printed panel.
const MAXIMUM_ENLARGEMENT: f32 = 3.0;
/// Additional recovery outlines must cover at least this fraction of the detection window.
const MINIMUM_RECOVERY_AREA_FRACTION: f32 = 0.02;
/// A correction may remove at most fifteen percent of an unsupported outline's area.
const MINIMUM_SHRINK_AREA: f32 = 0.85;
/// Source-pixel side agreement required when correcting one unsupported boundary.
const SHARED_EDGE_TOLERANCE: f32 = 0.03;

/// One independently supported proposal, ranked by its least-supported side.
struct Proposal {
    /// Geometry established by an unchanged production detector pass.
    detection: CardDetection,
    /// Lower-quartile contrast on the weakest original-image side.
    support: f32,
}

/// A retained outline and the original area that bounds subsequent enlargement.
struct Retained {
    /// Current original-image coordinates for one card-shaped region.
    detection: CardDetection,
    /// First baseline or independent recovery area, preventing successive unbounded growth.
    anchor_area: f32,
}

/// Exact convex overlap used for duplicate and printed-panel association.
struct Overlap {
    /// Actual shared area in source-image pixels.
    intersection: f32,
    /// Shared area divided by the union area.
    iou: f32,
    /// Fraction of the smaller quadrilateral enclosed by the other.
    containment: f32,
}

/// One supported outward parallel line, used only to withhold uncertain inner-panel crops.
#[derive(Clone, Copy)]
struct OuterEdgeWitness {
    /// Unit normal pointing away from the candidate's center.
    normal: [f32; 2],
    /// Observed outward line in normal form: dot(normal, point) = level.
    level: f32,
}

/// One detected outline that cannot establish a whole card within the current view.
#[derive(Clone, Debug, PartialEq)]
pub struct IncompleteCard {
    /// Unchanged detected geometry in original source-image coordinates.
    pub detection: CardDetection,
    /// Why the outline must be excluded from rectification and recognition.
    pub reason: &'static str,
}

/// Accepted whole-card outlines and separately retained incomplete-view evidence.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetectionDiagnostics {
    /// Complete outlines eligible for rectification and recognition.
    pub cards: Vec<CardDetection>,
    /// Boundary-touching outlines and uncertain inner regions retained only for capture feedback.
    pub incomplete_cards: Vec<IncompleteCard>,
}

/// Shared production extraction with bounded, independently validated recovery.
///
/// Native recognition, live browser outlines and browser OCR all use this same
/// detector. Explicit [`DetectionMode::Fast`] retains upstream candidate generation.
/// Every mode excludes outlines within three pixels of the image or detection
/// window edge from recognition. Adjacent supported outer lines whose meeting
/// point is outside that complete-card window also withhold an uncertain inner
/// panel. Diagnostics retain the original detected outlines, not inferred crops.
/// Full mode uses at most four passes, honors the caller's detection window and
/// geometry limits. Supported enclosing proposals can recover one printed panel
/// without covering another card. A small inward correction is allowed only for
/// one unsupported original edge while three physical side lines remain aligned.
#[derive(Clone, Debug, Default)]
pub struct CardExtractor {
    /// Validated production detector and unchanged caller settings.
    baseline: magicextract::CardExtractor,
}

impl CardExtractor {
    /// Validate caller settings and construct the shared detector.
    ///
    /// # Errors
    ///
    /// Returns the production extractor's error for invalid configuration.
    pub fn new(config: ExtractorConfig) -> Result<Self, Error> {
        Ok(Self {
            baseline: magicextract::CardExtractor::new(config)?,
        })
    }

    /// Read the unchanged, validated caller configuration.
    pub fn config(&self) -> &ExtractorConfig {
        self.baseline.config()
    }

    /// Detect whole-card source-image outlines using the shared native/browser policy.
    ///
    /// Additional passes operate only on detection pixels. Every additional
    /// outline must occupy at least two percent of the detection window and show
    /// distributed contrast across all four original RGB boundaries, including
    /// valid samples on both sides of each edge. A proposal
    /// cannot infer missing pixels beyond the image or configured detection window.
    /// The caller's card limit and source-coordinate spatial ordering are retained.
    ///
    /// # Errors
    ///
    /// Returns the production extractor's error for empty or oversized images,
    /// invalid detection windows, or invalid derived configuration.
    pub fn detect(&self, image: &DynamicImage) -> Result<Vec<CardDetection>, Error> {
        self.detect_with_diagnostics(image)
            .map(|report| report.cards)
    }

    /// Detect whole cards and retain boundary-touching outlines as incomplete evidence.
    ///
    /// An outline within three original pixels of the image or configured detection
    /// window boundary is incomplete. It is excluded before card association so a
    /// clipped region or oversized frame cannot suppress a supported internal card.
    /// This whole-card requirement also applies to explicit fast mode. Incomplete
    /// detections retain their original coordinates and a reason for capture feedback.
    /// A narrow outward source-edge scan may also withhold a panel when two adjacent
    /// supported outer lines meet outside this complete-card window. That evidence
    /// is diagnostic only: it never creates inferred corners or missing crop pixels.
    ///
    /// # Errors
    ///
    /// Returns the production extractor's error for empty or oversized images,
    /// invalid detection windows, or invalid derived configuration.
    pub fn detect_with_diagnostics(
        &self,
        image: &DynamicImage,
    ) -> Result<DetectionDiagnostics, Error> {
        // Validate original dimensions and the detection region before RGB allocation.
        let baseline = self.baseline.detect(image)?;
        let config = self.config();
        let mut incomplete_cards = Vec::new();
        let baseline = whole_cards(
            baseline,
            image.width(),
            image.height(),
            config,
            &mut incomplete_cards,
        );
        if config.detection_mode == DetectionMode::Fast {
            let context = incomplete_cards.clone();
            let original = image.to_rgb8();
            let cards = exclude_incomplete_panels(
                baseline,
                &original,
                config,
                &context,
                &mut incomplete_cards,
            );
            let cards =
                exclude_unseen_outer_corners(cards, &original, config, &mut incomplete_cards);
            return Ok(DetectionDiagnostics {
                cards,
                incomplete_cards,
            });
        }
        let original = image.to_rgb8();
        let mut additional = Vec::new();
        if config.max_detection_dimension > 640 {
            let mut coarse = config.clone();
            coarse.max_detection_dimension = 640;
            additional.extend(magicextract::CardExtractor::new(coarse)?.detect(image)?);
        }
        let opponent = DynamicImage::ImageLuma8(GrayImage::from_fn(
            original.width(),
            original.height(),
            |x, y| {
                let [red, _, blue] = original.get_pixel(x, y).0;
                Luma([(127.5 + 2.0 * (f32::from(blue) - f32::from(red))).clamp(0.0, 255.0) as u8])
            },
        ));
        let mut chroma = config.clone();
        chroma.max_detection_dimension = config.max_detection_dimension.min(960);
        additional.extend(magicextract::CardExtractor::new(chroma)?.detect(&opponent)?);
        // A full recovery pass can replace a valid chromatic perimeter with a
        // larger table/stack silhouette. Retain a separate bounded contour-only
        // proposal; the same original-RGB, whole-card and overlap guards apply.
        additional.extend(
            magicextract::CardExtractor::new(fast_opponent_config(config))?.detect(&opponent)?,
        );
        let additional = whole_cards(
            additional,
            image.width(),
            image.height(),
            config,
            &mut incomplete_cards,
        );
        let context = incomplete_cards.clone();
        let baseline =
            exclude_incomplete_panels(baseline, &original, config, &context, &mut incomplete_cards);
        let additional = exclude_incomplete_panels(
            additional,
            &original,
            config,
            &context,
            &mut incomplete_cards,
        );
        let cards = merge(baseline, additional, &original, config);
        Ok(DetectionDiagnostics {
            cards: exclude_unseen_outer_corners(cards, &original, config, &mut incomplete_cards),
            incomplete_cards,
        })
    }

    /// Perspective-correct supplied source-image corners without detecting again.
    ///
    /// # Errors
    ///
    /// Returns the production extractor's error for invalid source dimensions or
    /// corners that are out of bounds, nonconvex, nonfinite or degenerate.
    pub fn rectify(&self, image: &DynamicImage, corners: &[Point; 4]) -> Result<RgbImage, Error> {
        self.baseline.rectify(image, corners)
    }

    /// Detect shared outlines and rectify every valid region from original pixels.
    ///
    /// No opponent-plane or downsampled detection pixels enter the output crop.
    /// As with the production extractor, projectively degenerate detections are
    /// omitted rather than turning one region into a frame-wide extraction error.
    ///
    /// # Errors
    ///
    /// Returns a detection error for invalid source dimensions or detection windows.
    pub fn extract(&self, image: &DynamicImage) -> Result<Vec<ExtractedCard>, Error> {
        let detections = self.detect(image)?;
        if detections.is_empty() {
            return Ok(Vec::new());
        }
        let original = DynamicImage::ImageRgb8(image.to_rgb8());
        Ok(detections
            .into_iter()
            .filter_map(|detection| {
                self.rectify(&original, &detection.corners)
                    .ok()
                    .map(|image| ExtractedCard { image, detection })
            })
            .collect())
    }
}

/// Preserve caller geometry/output limits while bounding the contour-only color proposal pass.
fn fast_opponent_config(config: &ExtractorConfig) -> ExtractorConfig {
    let mut fast = config.clone();
    fast.max_detection_dimension = config.max_detection_dimension.min(FAST_OPPONENT_DIMENSION);
    fast.detection_mode = DetectionMode::Fast;
    fast
}

/// Separate incomplete frame-edge geometry before recovery proposals can associate with it.
fn whole_cards(
    candidates: Vec<CardDetection>,
    width: u32,
    height: u32,
    config: &ExtractorConfig,
    incomplete: &mut Vec<IncompleteCard>,
) -> Vec<CardDetection> {
    let (left, top, right, bottom) = config.detection_region.map_or(
        (0.0, 0.0, width as f32 - 1.0, height as f32 - 1.0),
        |region| {
            (
                region.x as f32,
                region.y as f32,
                (region.x + region.width - 1) as f32,
                (region.y + region.height - 1) as f32,
            )
        },
    );
    let mut complete = Vec::new();
    for detection in candidates {
        let touches_boundary = detection.corners.iter().any(|point| {
            point.x <= left + BOUNDARY_MARGIN
                || point.y <= top + BOUNDARY_MARGIN
                || point.x >= right - BOUNDARY_MARGIN
                || point.y >= bottom - BOUNDARY_MARGIN
        });
        if !touches_boundary {
            complete.push(detection);
            continue;
        }
        if incomplete
            .iter()
            .all(|old| overlap(&old.detection, &detection).iou < 0.95)
        {
            incomplete.push(IncompleteCard {
                detection,
                reason: INCOMPLETE_REASON,
            });
        }
    }
    complete
}

/// Do not promote an interior printed panel after excluding its clipped physical outline.
fn exclude_incomplete_panels(
    candidates: Vec<CardDetection>,
    original: &RgbImage,
    config: &ExtractorConfig,
    context: &[IncompleteCard],
    incomplete: &mut Vec<IncompleteCard>,
) -> Vec<CardDetection> {
    let physical_context: Vec<_> = context
        .iter()
        .filter(|card| {
            // Require three supported sides before using rejected geometry as physical-card context.
            edge_contrast(original, &card.detection.corners, config)
                .into_iter()
                .filter(|value| *value >= MINIMUM_CONTEXT_CONTRAST)
                .count()
                >= 3
        })
        .collect();
    let mut complete = Vec::new();
    for candidate in candidates {
        let inside_clipped_card = physical_context.iter().any(|outer| {
            let overlap = overlap(&outer.detection, &candidate);
            outer.detection.area > candidate.area
                && overlap.intersection / candidate.area >= MINIMUM_CONTAINMENT
                && !corrects_unsupported_boundary(
                    &outer.detection,
                    &candidate,
                    &overlap,
                    original,
                    config,
                )
        });
        if inside_clipped_card {
            if incomplete
                .iter()
                .all(|old| overlap(&old.detection, &candidate).iou < 0.95)
            {
                incomplete.push(IncompleteCard {
                    detection: candidate,
                    reason: INCOMPLETE_PANEL_REASON,
                });
            }
        } else {
            complete.push(candidate);
        }
    }
    complete
}

/// Withhold an inner panel when adjacent observed outer lines cannot meet inside the source window.
fn exclude_unseen_outer_corners(
    candidates: Vec<CardDetection>,
    original: &RgbImage,
    config: &ExtractorConfig,
    incomplete: &mut Vec<IncompleteCard>,
) -> Vec<CardDetection> {
    let mut complete = Vec::new();
    for detection in candidates {
        if has_unseen_outer_corner(&detection, original, config) {
            if incomplete
                .iter()
                .all(|old| overlap(&old.detection, &detection).iou < 0.95)
            {
                incomplete.push(IncompleteCard {
                    detection,
                    reason: UNSEEN_OUTER_CORNER_REASON,
                });
            }
        } else {
            complete.push(detection);
        }
    }
    complete
}

/// Seek bounded source evidence of a missing physical corner, never a substitute crop quadrilateral.
fn has_unseen_outer_corner(
    detection: &CardDetection,
    original: &RgbImage,
    config: &ExtractorConfig,
) -> bool {
    let corners = &detection.corners;
    let shortest = (0..4)
        .map(|side| {
            let start = corners[side];
            let end = corners[(side + 1) % 4];
            (end.x - start.x).hypot(end.y - start.y)
        })
        .fold(f32::INFINITY, f32::min);
    let maximum =
        ((shortest * MAXIMUM_OUTER_EDGE_FRACTION) as usize).min(MAXIMUM_OUTER_EDGE_DISTANCE);
    if maximum <= MINIMUM_OUTER_EDGE_DISTANCE {
        return false;
    }
    let center = corners.iter().fold([0.0; 2], |sum, point| {
        [sum[0] + point.x * 0.25, sum[1] + point.y * 0.25]
    });
    let current = edge_contrast(original, corners, config);
    let witnesses: [Option<OuterEdgeWitness>; 4] = std::array::from_fn(|side| {
        let start = corners[side];
        let end = corners[(side + 1) % 4];
        let length = (end.x - start.x).hypot(end.y - start.y);
        let mut normal = [-(end.y - start.y) / length, (end.x - start.x) / length];
        let outward = ((start.x + end.x) * 0.5 - center[0]) * normal[0]
            + ((start.y + end.y) * 0.5 - center[1]) * normal[1];
        if outward < 0.0 {
            normal = normal.map(|value| -value);
        }
        let profile: Vec<_> = (0..=maximum)
            .map(|distance| {
                outward_side_contrast(original, start, end, normal, distance as f32, config)
            })
            .collect();
        let required =
            MINIMUM_OUTER_EDGE_CONTRAST.max(current[side] * MINIMUM_RELATIVE_OUTER_CONTRAST);
        (MINIMUM_OUTER_EDGE_DISTANCE..maximum)
            .filter(|distance| {
                profile[*distance] >= required
                    && profile[*distance] >= profile[*distance - 1]
                    && profile[*distance] > profile[*distance + 1]
            })
            .max_by(|left, right| profile[*left].total_cmp(&profile[*right]))
            .map(|distance| OuterEdgeWitness {
                normal,
                level: normal[0] * start.x + normal[1] * start.y + distance as f32,
            })
    });
    let (left, top, right, bottom) = config.detection_region.map_or(
        (
            0.0,
            0.0,
            original.width() as f32 - 1.0,
            original.height() as f32 - 1.0,
        ),
        |region| {
            (
                region.x as f32,
                region.y as f32,
                (region.x + region.width - 1) as f32,
                (region.y + region.height - 1) as f32,
            )
        },
    );
    (0..4).any(|side| {
        let (Some(first), Some(second)) = (witnesses[side], witnesses[(side + 1) % 4]) else {
            return false;
        };
        let determinant = first.normal[0] * second.normal[1] - first.normal[1] * second.normal[0];
        if determinant.abs() < 0.0001 {
            return false;
        }
        let x = (first.level * second.normal[1] - first.normal[1] * second.level) / determinant;
        let y = (first.normal[0] * second.level - first.level * second.normal[0]) / determinant;
        x.is_finite()
            && y.is_finite()
            && (x <= left + BOUNDARY_MARGIN
                || y <= top + BOUNDARY_MARGIN
                || x >= right - BOUNDARY_MARGIN
                || y >= bottom - BOUNDARY_MARGIN)
    })
}

/// Sample a parallel source line only when at least three quarters of its distributed pairs are visible.
fn outward_side_contrast(
    original: &RgbImage,
    start: Point,
    end: Point,
    normal: [f32; 2],
    distance: f32,
    config: &ExtractorConfig,
) -> f32 {
    let (left, top, right, bottom) =
        config
            .detection_region
            .map_or((0, 0, original.width(), original.height()), |region| {
                (
                    region.x,
                    region.y,
                    region.x + region.width,
                    region.y + region.height,
                )
            });
    let mut evidence = [0.0_f32; EDGE_SAMPLES];
    let mut valid = 0;
    for index in 0..EDGE_SAMPLES {
        let fraction = 0.12 + 0.76 * index as f32 / (EDGE_SAMPLES - 1) as f32;
        let center = [
            start.x + (end.x - start.x) * fraction + normal[0] * distance,
            start.y + (end.y - start.y) * fraction + normal[1] * distance,
        ];
        let sample = |offset: f32| {
            let x = (center[0] + normal[0] * offset).round();
            let y = (center[1] + normal[1] * offset).round();
            (x >= left as f32 && y >= top as f32 && x < right as f32 && y < bottom as f32)
                .then(|| original.get_pixel(x as u32, y as u32).0)
        };
        if let (Some(inside), Some(outside)) = (sample(EDGE_OFFSET), sample(-EDGE_OFFSET)) {
            evidence[valid] = inside
                .into_iter()
                .zip(outside)
                .map(|(inside, outside)| (f32::from(inside) - f32::from(outside)).abs())
                .sum::<f32>()
                / 3.0;
            valid += 1;
        }
    }
    if valid < EDGE_SAMPLES * 3 / 4 {
        return 0.0;
    }
    evidence[..valid].sort_unstable_by(f32::total_cmp);
    evidence[valid / 4]
}

/// Merge independently supported candidates without replacing known geometry blindly.
fn merge(
    baseline: Vec<CardDetection>,
    additional: Vec<CardDetection>,
    original: &RgbImage,
    config: &ExtractorConfig,
) -> Vec<CardDetection> {
    let window_area = config.detection_region.map_or(
        original.width() as f32 * original.height() as f32,
        |region| region.width as f32 * region.height as f32,
    );
    let minimum_area = window_area * config.min_area_fraction.max(MINIMUM_RECOVERY_AREA_FRACTION);
    let mut proposals: Vec<_> = additional
        .into_iter()
        .filter_map(|detection| {
            if detection.area < minimum_area {
                return None;
            }
            let support = edge_contrast(original, &detection.corners, config)
                .into_iter()
                .fold(f32::INFINITY, f32::min);
            (support >= MINIMUM_EDGE_CONTRAST).then_some(Proposal { detection, support })
        })
        .collect();
    // Strong evidence on the weakest side wins over a larger, weakly supported box.
    proposals.sort_by(|left, right| {
        right
            .support
            .total_cmp(&left.support)
            .then_with(|| right.detection.area.total_cmp(&left.detection.area))
    });
    let mut retained: Vec<_> = baseline
        .into_iter()
        .map(|detection| Retained {
            anchor_area: detection.area,
            detection,
        })
        .collect();
    for proposal in proposals {
        let overlaps: Vec<_> = retained
            .iter()
            .enumerate()
            .filter_map(|(index, old)| {
                let overlap = overlap(&old.detection, &proposal.detection);
                (overlap.iou > 0.02 || overlap.containment >= MINIMUM_CONTAINMENT)
                    .then_some((index, overlap))
            })
            .collect();
        if overlaps.is_empty() {
            if retained.len() < config.max_cards {
                retained.push(Retained {
                    anchor_area: proposal.detection.area,
                    detection: proposal.detection,
                });
            }
            continue;
        }
        if overlaps.len() != 1 {
            continue;
        }
        let (index, overlap) = &overlaps[0];
        let old = &retained[*index];
        let enlargement = proposal.detection.area / old.detection.area;
        let encloses_panel = enlargement >= MINIMUM_ENLARGEMENT
            && proposal.detection.area <= old.anchor_area * MAXIMUM_ENLARGEMENT
            && overlap.intersection / old.detection.area >= MINIMUM_CONTAINMENT;
        let corrects_boundary = enlargement < 1.0
            && corrects_unsupported_boundary(
                &old.detection,
                &proposal.detection,
                overlap,
                original,
                config,
            );
        if !encloses_panel && !corrects_boundary {
            continue;
        }
        retained[*index] = Retained {
            anchor_area: old.anchor_area,
            detection: proposal.detection,
        };
    }
    let mut detections: Vec<_> = retained.into_iter().map(|old| old.detection).collect();
    detections.sort_by(|left, right| {
        let origin = |detection: &CardDetection| {
            detection
                .corners
                .iter()
                .fold((f32::INFINITY, f32::INFINITY), |(top, left), point| {
                    (top.min(point.y), left.min(point.x))
                })
        };
        let (left_top, left_x) = origin(left);
        let (right_top, right_x) = origin(right);
        left_top
            .total_cmp(&right_top)
            .then_with(|| left_x.total_cmp(&right_x))
    });
    detections
}

/// Correct only one unsupported edge while preserving three observed physical side lines.
fn corrects_unsupported_boundary(
    old: &CardDetection,
    candidate: &CardDetection,
    overlap: &Overlap,
    original: &RgbImage,
    config: &ExtractorConfig,
) -> bool {
    if candidate.area < old.area * MINIMUM_SHRINK_AREA
        || overlap.iou < MINIMUM_SHRINK_AREA
        || overlap.intersection / candidate.area < MINIMUM_CONTAINMENT
    {
        return false;
    }
    if edge_contrast(original, &candidate.corners, config)
        .into_iter()
        .any(|value| value < MINIMUM_EDGE_CONTRAST)
    {
        return false;
    }
    let evidence = edge_contrast(original, &old.corners, config);
    if evidence.iter().all(|value| *value >= MINIMUM_EDGE_CONTRAST) {
        return false;
    }
    let lengths: [f32; 4] = std::array::from_fn(|side| {
        let start = old.corners[side];
        let end = old.corners[(side + 1) % 4];
        (end.x - start.x).hypot(end.y - start.y)
    });
    let tolerance = lengths.into_iter().fold(f32::INFINITY, f32::min) * SHARED_EDGE_TOLERANCE;
    (0..4).any(|shift| {
        let mut shared = 0;
        let mut corrected = 0;
        for side in 0..4 {
            let start = old.corners[side];
            let end = old.corners[(side + 1) % 4];
            let new_start = candidate.corners[(side + shift) % 4];
            let new_end = candidate.corners[(side + shift + 1) % 4];
            let new_length = (new_end.x - new_start.x).hypot(new_end.y - new_start.y);
            let direction = ((end.x - start.x) * (new_end.x - new_start.x)
                + (end.y - start.y) * (new_end.y - new_start.y))
                / (lengths[side] * new_length);
            if direction < 0.995 {
                return false;
            }
            let nearby = cross(start, end, new_start).abs() / lengths[side] <= tolerance
                && cross(start, end, new_end).abs() / lengths[side] <= tolerance;
            if nearby {
                shared += 1;
            } else if evidence[side] < MINIMUM_EDGE_CONTRAST {
                corrected += 1;
            } else {
                return false;
            }
        }
        shared == 3 && corrected == 1
    })
}

/// Measure lower-quartile two-sided RGB contrast across every original-image edge.
fn edge_contrast(original: &RgbImage, corners: &[Point; 4], config: &ExtractorConfig) -> [f32; 4] {
    let (left, top, right, bottom) =
        config
            .detection_region
            .map_or((0, 0, original.width(), original.height()), |region| {
                (
                    region.x,
                    region.y,
                    region.x + region.width,
                    region.y + region.height,
                )
            });
    std::array::from_fn(|side| {
        let start = corners[side];
        let end = corners[(side + 1) % 4];
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let length = dx.hypot(dy);
        if !length.is_finite() || length <= 0.0 {
            return 0.0;
        }
        let normal = [-dy / length, dx / length];
        let mut evidence = [0.0_f32; EDGE_SAMPLES];
        for (index, value) in evidence.iter_mut().enumerate() {
            let fraction = 0.12 + 0.76 * index as f32 / (EDGE_SAMPLES - 1) as f32;
            let center = [start.x + dx * fraction, start.y + dy * fraction];
            let sample = |offset: f32| {
                let x = (center[0] + normal[0] * offset).round();
                let y = (center[1] + normal[1] * offset).round();
                (x >= left as f32 && y >= top as f32 && x < right as f32 && y < bottom as f32)
                    .then(|| original.get_pixel(x as u32, y as u32).0)
            };
            if let (Some(inside), Some(outside)) = (sample(EDGE_OFFSET), sample(-EDGE_OFFSET)) {
                *value = inside
                    .into_iter()
                    .zip(outside)
                    .map(|(inside, outside)| (f32::from(inside) - f32::from(outside)).abs())
                    .sum::<f32>()
                    / 3.0;
            }
        }
        evidence.sort_unstable_by(f32::total_cmp);
        evidence[EDGE_SAMPLES / 4]
    })
}

/// Compute convex-quadrilateral overlap with exact clipping rather than bounding boxes.
fn overlap(first: &CardDetection, second: &CardDetection) -> Overlap {
    let mut clipped = first.corners.to_vec();
    for side in 0..4 {
        let start = second.corners[side];
        let end = second.corners[(side + 1) % 4];
        let input = std::mem::take(&mut clipped);
        if input.is_empty() {
            break;
        }
        let mut previous = input[input.len() - 1];
        let mut before = cross(start, end, previous);
        for current in input {
            let after = cross(start, end, current);
            if (before >= 0.0) != (after >= 0.0) {
                let fraction = before / (before - after);
                clipped.push(Point {
                    x: previous.x + fraction * (current.x - previous.x),
                    y: previous.y + fraction * (current.y - previous.y),
                });
            }
            if after >= 0.0 {
                clipped.push(current);
            }
            previous = current;
            before = after;
        }
    }
    let intersection = polygon_area(&clipped);
    let union = first.area + second.area - intersection;
    Overlap {
        intersection,
        iou: if union > 0.0 {
            intersection / union
        } else {
            0.0
        },
        containment: intersection / first.area.min(second.area).max(f32::MIN_POSITIVE),
    }
}

/// Signed image-coordinate triangle area, positive inside a clockwise clip edge.
fn cross(start: Point, end: Point, point: Point) -> f32 {
    (end.x - start.x) * (point.y - start.y) - (end.y - start.y) * (point.x - start.x)
}

/// Compute enclosed area for an arbitrarily clipped convex polygon.
fn polygon_area(points: &[Point]) -> f32 {
    if points.len() < 3 {
        return 0.0;
    }
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .map(|(first, second)| {
            f64::from(first.x) * f64::from(second.y) - f64::from(second.x) * f64::from(first.y)
        })
        .sum::<f64>()
        .abs() as f32
        * 0.5
}

/// Procedural controls exercise physical boundaries and unchanged source pixels.
#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, imageops};
    use magicextract::DetectionRegion;

    /// The additional contour pass is bounded and preserves all caller geometry constraints.
    #[test]
    fn bounded_fast_opponent_preserves_caller_geometry_constraints() {
        let config = ExtractorConfig {
            max_detection_dimension: 2048,
            max_cards: 2,
            detection_region: Some(DetectionRegion {
                x: 20,
                y: 30,
                width: 400,
                height: 300,
            }),
            aspect_ratio_tolerance: 0.12,
            ..ExtractorConfig::default()
        };
        let fast = fast_opponent_config(&config);
        assert_eq!(fast.max_detection_dimension, 1280);
        assert_eq!(fast.detection_mode, DetectionMode::Fast);
        let mut expected = config.clone();
        expected.max_detection_dimension = fast.max_detection_dimension;
        expected.detection_mode = DetectionMode::Fast;
        assert_eq!(fast, expected);
        let small = ExtractorConfig {
            max_detection_dimension: 480,
            ..config
        };
        assert_eq!(fast_opponent_config(&small).max_detection_dimension, 480);
    }

    /// Render equal-luminance outer colors around a clearly visible inner panel.
    fn isoluminant_scene() -> DynamicImage {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([180, 80, 80]));
        let mut card = RgbImage::from_pixel(180, 252, Rgb([40, 120, 96]));
        imageops::replace(
            &mut card,
            &RgbImage::from_pixel(120, 168, Rgb([220, 220, 220])),
            30,
            42,
        );
        imageops::replace(&mut frame, &card, 120, 70);
        DynamicImage::ImageRgb8(frame)
    }

    /// Hue evidence recovers the physical card while crops retain its original colors.
    #[test]
    fn recovers_isoluminant_outer_card_and_preserves_source_pixels() {
        let frame = isoluminant_scene();
        let config = ExtractorConfig::default();
        let baseline = magicextract::CardExtractor::new(config.clone())
            .unwrap()
            .detect(&frame)
            .unwrap();
        assert_eq!(baseline.len(), 1);
        assert!(baseline[0].area < 25_000.0);
        let recovered = CardExtractor::new(config).unwrap().extract(&frame).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].detection.area > 43_000.0);
        assert!(recovered[0].detection.area < 47_000.0);
        assert_eq!(recovered[0].image.get_pixel(20, 20).0, [40, 120, 96]);
        assert_eq!(recovered[0].image.get_pixel(125, 175).0, [220, 220, 220]);
    }

    /// Neutral, clearly observed baseline geometry is not displaced by another pass.
    #[test]
    fn preserves_observed_baseline_geometry() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([24, 31, 37]));
        imageops::replace(
            &mut frame,
            &RgbImage::from_pixel(180, 252, Rgb([230, 210, 180])),
            120,
            70,
        );
        let frame = DynamicImage::ImageRgb8(frame);
        let baseline = magicextract::CardExtractor::default()
            .detect(&frame)
            .unwrap();
        assert_eq!(baseline.len(), 1);
        assert_eq!(CardExtractor::default().detect(&frame).unwrap(), baseline);
    }

    /// Clipped cards stay available as evidence but cannot enter crops or OCR in either mode.
    #[test]
    fn excludes_clipped_cards_from_full_and_fast_recognition() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([24, 31, 37]));
        imageops::replace(
            &mut frame,
            &RgbImage::from_pixel(180, 252, Rgb([230, 210, 180])),
            -20,
            70,
        );
        let frame = DynamicImage::ImageRgb8(frame);
        for mode in [DetectionMode::Full, DetectionMode::Fast] {
            let extractor = CardExtractor::new(ExtractorConfig {
                detection_mode: mode,
                ..ExtractorConfig::default()
            })
            .unwrap();
            let diagnostics = extractor.detect_with_diagnostics(&frame).unwrap();
            assert!(diagnostics.cards.is_empty());
            assert!(!diagnostics.incomplete_cards.is_empty());
            assert!(diagnostics.incomplete_cards.iter().all(|card| {
                card.detection.corners.iter().any(|point| point.x <= 3.0)
                    && card.reason.contains("fully into view")
            }));
            assert!(extractor.detect(&frame).unwrap().is_empty());
            assert!(extractor.extract(&frame).unwrap().is_empty());
        }
    }

    /// The caller's detection-window edge enforces the same whole-card rule as the image edge.
    #[test]
    fn excludes_detection_window_boundary_but_preserves_visible_control() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([24, 31, 37]));
        imageops::replace(
            &mut frame,
            &RgbImage::from_pixel(180, 252, Rgb([230, 210, 180])),
            50,
            70,
        );
        let frame = DynamicImage::ImageRgb8(frame);
        let visible = CardExtractor::default()
            .detect_with_diagnostics(&frame)
            .unwrap();
        assert_eq!(visible.cards.len(), 1);
        assert!(visible.incomplete_cards.is_empty());
        let limited = CardExtractor::new(ExtractorConfig {
            detection_region: Some(DetectionRegion {
                x: 50,
                y: 0,
                width: 300,
                height: 420,
            }),
            ..ExtractorConfig::default()
        })
        .unwrap()
        .detect_with_diagnostics(&frame)
        .unwrap();
        assert!(limited.cards.is_empty());
        assert!(!limited.incomplete_cards.is_empty());
        assert!(
            limited
                .incomplete_cards
                .iter()
                .all(|card| { card.detection.corners.iter().any(|point| point.x <= 53.0) })
        );
    }

    /// A fully in-bounds artwork panel cannot replace its clipped surrounding physical card.
    #[test]
    fn retains_incomplete_outer_context_instead_of_promoting_its_artwork() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([24, 31, 37]));
        // Luminance observes the clipped outer card, while the opponent plane isolates its panel.
        paint(&mut frame, [0, 70, 160, 322], [200, 210, 220]);
        paint(&mut frame, [10, 112, 130, 280], [80, 70, 60]);
        let outer = rectangle([0.5, 70.0, 160.0, 322.0]);
        let panel = rectangle([10.0, 112.0, 130.0, 280.0]);
        let config = ExtractorConfig::default();
        let mut incomplete = Vec::new();
        let whole = whole_cards(
            vec![outer.clone()],
            frame.width(),
            frame.height(),
            &config,
            &mut incomplete,
        );
        assert!(whole.is_empty());
        let context = incomplete.clone();
        assert!(
            exclude_incomplete_panels(
                vec![panel.clone()],
                &frame,
                &config,
                &context,
                &mut incomplete,
            )
            .is_empty()
        );
        assert!(
            incomplete
                .iter()
                .any(|card| { card.detection == panel && card.reason == INCOMPLETE_PANEL_REASON })
        );
        let actual = CardExtractor::default()
            .detect_with_diagnostics(&DynamicImage::ImageRgb8(frame))
            .unwrap();
        assert!(actual.cards.is_empty(), "{actual:?}");
        assert!(!actual.incomplete_cards.is_empty());
    }

    /// A near-boundary corner remains incomplete even when the centers of all four edges are visible.
    #[test]
    fn supported_edge_centers_do_not_override_an_incomplete_corner() {
        let mut frame = RgbImage::from_pixel(300, 300, Rgb([80, 80, 80]));
        let corners = [
            Point { x: 2.0, y: 100.0 },
            Point { x: 140.0, y: 30.0 },
            Point { x: 280.0, y: 200.0 },
            Point { x: 130.0, y: 270.0 },
        ];
        let outer = CardDetection {
            area: polygon_area(&corners),
            corners,
        };
        let inner = corners.map(|point| Point {
            x: 138.0 + (point.x - 138.0) * 0.6,
            y: 150.0 + (point.y - 150.0) * 0.6,
        });
        let panel = CardDetection {
            area: polygon_area(&inner),
            corners: inner,
        };
        for (outline, tone) in [(&outer, 200), (&panel, 230)] {
            for y in 0..frame.height() {
                for x in 0..frame.width() {
                    let point = Point {
                        x: x as f32 + 0.5,
                        y: y as f32 + 0.5,
                    };
                    if (0..4).all(|side| {
                        cross(
                            outline.corners[side],
                            outline.corners[(side + 1) % 4],
                            point,
                        ) >= 0.0
                    }) {
                        frame.put_pixel(x, y, Rgb([tone, tone, tone]));
                    }
                }
            }
        }
        let config = ExtractorConfig::default();
        assert!(
            edge_contrast(&frame, &outer.corners, &config)
                .into_iter()
                .all(|value| value >= MINIMUM_EDGE_CONTRAST)
        );
        let mut incomplete = Vec::new();
        assert!(whole_cards(vec![outer], 300, 300, &config, &mut incomplete).is_empty());
        let context = incomplete.clone();
        assert!(
            exclude_incomplete_panels(vec![panel], &frame, &config, &context, &mut incomplete,)
                .is_empty()
        );
    }

    /// Paint a procedural physical perimeter or printed panel, including off-image corners.
    fn paint_outline(frame: &mut RgbImage, corners: &[Point; 4], color: [u8; 3]) {
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                let point = Point {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                };
                if (0..4).all(|side| cross(corners[side], corners[(side + 1) % 4], point) >= 0.0) {
                    frame.put_pixel(x, y, Rgb(color));
                }
            }
        }
    }

    /// Produce all quarter-turn source/geometry pairs without changing their physical association.
    fn quarter_turns(frame: RgbImage, detection: CardDetection) -> Vec<(RgbImage, CardDetection)> {
        let mut result = vec![(frame, detection)];
        for _ in 0..3 {
            let (frame, detection) = result.last().unwrap();
            let height = frame.height();
            let detection = CardDetection {
                corners: detection.corners.map(|point| Point {
                    x: height as f32 - 1.0 - point.y,
                    y: point.x,
                }),
                area: detection.area,
            };
            result.push((imageops::rotate90(frame), detection));
        }
        result
    }

    /// Render an in-bounds panel inside a physical outline with an unseen outer top-left corner.
    fn clipped_outer_panel() -> (RgbImage, CardDetection) {
        let mut frame = RgbImage::from_pixel(480, 640, Rgb([180, 180, 180]));
        let outer = [
            Point { x: -4.0, y: 116.0 },
            Point { x: 314.0, y: 86.0 },
            Point { x: 394.0, y: 545.0 },
            Point { x: 66.0, y: 574.0 },
        ];
        let panel = [
            Point { x: 10.0, y: 130.0 },
            Point { x: 300.0, y: 100.0 },
            Point { x: 375.0, y: 515.0 },
            Point { x: 80.0, y: 548.0 },
        ];
        paint_outline(&mut frame, &outer, [30, 30, 30]);
        paint_outline(&mut frame, &panel, [200, 200, 200]);
        (
            frame,
            CardDetection {
                corners: panel,
                area: polygon_area(&panel),
            },
        )
    }

    /// Weak but distributed third-side context can reject artwork without admitting weak recovery.
    #[test]
    fn weaker_incomplete_context_does_not_lower_recovery_admission() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([80, 80, 80]));
        paint(&mut frame, [0, 60, 240, 350], [130, 130, 130]);
        paint(&mut frame, [0, 350, 240, 380], [119, 119, 119]);
        paint(&mut frame, [15, 90, 200, 280], [200, 200, 200]);
        let outer = rectangle([0.5, 60.0, 240.0, 350.0]);
        let panel = rectangle([15.0, 90.0, 200.0, 280.0]);
        let config = ExtractorConfig::default();
        let support = edge_contrast(&frame, &outer.corners, &config);
        assert_eq!(support, [50.0, 50.0, 11.0, 0.0]);
        assert_eq!(
            support
                .iter()
                .filter(|value| **value >= MINIMUM_EDGE_CONTRAST)
                .count(),
            2
        );
        let context = vec![IncompleteCard {
            detection: outer,
            reason: INCOMPLETE_REASON,
        }];
        let mut incomplete = Vec::new();
        assert!(
            exclude_incomplete_panels(vec![panel], &frame, &config, &context, &mut incomplete)
                .is_empty()
        );
        assert_eq!(incomplete[0].reason, INCOMPLETE_PANEL_REASON);

        let mut weak = RgbImage::from_pixel(300, 400, Rgb([80, 80, 80]));
        paint(&mut weak, [40, 60, 220, 312], [91, 91, 91]);
        assert!(
            merge(
                Vec::new(),
                vec![rectangle([40.0, 60.0, 220.0, 312.0])],
                &weak,
                &config,
            )
            .is_empty()
        );
    }

    /// An unseen physical corner with two observed outer sides withholds the unchanged inner panel.
    #[test]
    fn adjacent_outer_source_lines_withhold_clipped_panel_in_every_orientation() {
        let (frame, panel) = clipped_outer_panel();
        let config = ExtractorConfig::default();
        for (frame, panel) in quarter_turns(frame, panel) {
            let mut incomplete = Vec::new();
            let complete =
                exclude_unseen_outer_corners(vec![panel.clone()], &frame, &config, &mut incomplete);
            assert!(complete.is_empty());
            assert_eq!(incomplete.len(), 1);
            assert_eq!(incomplete[0].detection, panel);
            assert_eq!(incomplete[0].reason, UNSEEN_OUTER_CORNER_REASON);
        }
    }

    /// Whole physical cards ten pixels from the source edge remain valid on slatted backgrounds.
    #[test]
    fn outer_uncertainty_preserves_near_edge_cards_and_background_texture() {
        let config = ExtractorConfig::default();
        let near_edge = rectangle([10.0, 80.0, 310.0, 500.0]);
        let mut frame = RgbImage::from_fn(480, 640, |x, y| {
            let grain = ((x / 19 + y / 43) % 2) as u8 * 60;
            Rgb([110 + grain, 100 + grain, 90 + grain])
        });
        paint_outline(&mut frame, &near_edge.corners, [30, 30, 30]);
        paint(&mut frame, [32, 102, 288, 478], [200, 200, 200]);
        for (frame, physical) in quarter_turns(frame, near_edge) {
            assert!(!has_unseen_outer_corner(&physical, &frame, &config));
            let mut incomplete = Vec::new();
            assert_eq!(
                exclude_unseen_outer_corners(
                    vec![physical.clone()],
                    &frame,
                    &config,
                    &mut incomplete,
                ),
                vec![physical],
            );
            assert!(incomplete.is_empty());
        }
        let (_, panel) = clipped_outer_panel();
        let mut wood = RgbImage::from_fn(480, 640, |x, y| {
            let grain = ((x / 19 + y / 43) % 2) as u8 * 60;
            Rgb([110 + grain, 100 + grain, 90 + grain])
        });
        paint_outline(&mut wood, &panel.corners, [230, 230, 230]);
        for (wood, physical) in quarter_turns(wood, panel) {
            assert!(!has_unseen_outer_corner(&physical, &wood, &config));
        }
    }

    /// Outer-line visibility is evaluated against the caller's source window, not unseen pixels.
    #[test]
    fn outer_uncertainty_respects_detection_window() {
        let (source, panel) = clipped_outer_panel();
        let mut frame = RgbImage::from_pixel(540, 640, Rgb([180, 180, 180]));
        imageops::replace(&mut frame, &source, 60, 0);
        let panel = CardDetection {
            corners: panel.corners.map(|point| Point {
                x: point.x + 60.0,
                y: point.y,
            }),
            area: panel.area,
        };
        assert!(!has_unseen_outer_corner(
            &panel,
            &frame,
            &ExtractorConfig::default(),
        ));
        let limited = ExtractorConfig {
            detection_region: Some(DetectionRegion {
                x: 60,
                y: 0,
                width: 480,
                height: 640,
            }),
            ..ExtractorConfig::default()
        };
        assert!(has_unseen_outer_corner(&panel, &frame, &limited));
    }

    /// A new hue-defined card is added without evicting a caller-limited baseline card.
    #[test]
    fn adds_observed_disjoint_card_and_respects_card_limit() {
        let mut frame = RgbImage::from_pixel(720, 450, Rgb([180, 80, 80]));
        imageops::replace(
            &mut frame,
            &RgbImage::from_pixel(180, 252, Rgb([40, 120, 96])),
            60,
            80,
        );
        imageops::replace(
            &mut frame,
            &RgbImage::from_pixel(180, 252, Rgb([230, 230, 230])),
            420,
            80,
        );
        let frame = DynamicImage::ImageRgb8(frame);
        let baseline = magicextract::CardExtractor::default()
            .detect(&frame)
            .unwrap();
        assert_eq!(baseline.len(), 1);
        assert!(baseline[0].corners.iter().all(|point| point.x > 400.0));
        let observed = CardExtractor::default().detect(&frame).unwrap();
        assert_eq!(observed.len(), 2);
        assert!(
            observed
                .iter()
                .any(|card| card.corners.iter().all(|point| point.x < 250.0))
        );
        let limited = ExtractorConfig {
            max_cards: 1,
            ..ExtractorConfig::default()
        };
        assert_eq!(
            CardExtractor::new(limited).unwrap().detect(&frame).unwrap(),
            baseline
        );
    }

    /// A transformed frame edge without two-sided original evidence is never admitted.
    #[test]
    fn rejects_unsupported_frame_boundary() {
        let original = RgbImage::from_pixel(300, 400, Rgb([80, 100, 120]));
        let corners = [
            Point { x: 0.5, y: 0.5 },
            Point { x: 298.5, y: 0.5 },
            Point { x: 298.5, y: 398.5 },
            Point { x: 0.5, y: 398.5 },
        ];
        let candidate = CardDetection {
            area: polygon_area(&corners),
            corners,
        };
        assert!(
            merge(
                Vec::new(),
                vec![candidate],
                &original,
                &ExtractorConfig::default()
            )
            .is_empty()
        );
    }

    /// Construct source-coordinate rectangles for independently observed test boundaries.
    fn rectangle([left, top, right, bottom]: [f32; 4]) -> CardDetection {
        let corners = [
            Point { x: left, y: top },
            Point { x: right, y: top },
            Point {
                x: right,
                y: bottom,
            },
            Point { x: left, y: bottom },
        ];
        CardDetection {
            area: polygon_area(&corners),
            corners,
        }
    }

    /// Paint an actual observed rectangle rather than supplying unsupported inferred pixels.
    fn paint(frame: &mut RgbImage, bounds: [u32; 4], color: [u8; 3]) {
        for y in bounds[1]..bounds[3] {
            for x in bounds[0]..bounds[2] {
                frame.put_pixel(x, y, Rgb(color));
            }
        }
    }

    /// A stronger inner frame cannot prevent a later observed outer card from winning.
    #[test]
    fn enclosing_physical_recovery_can_replace_an_earlier_inner_recovery() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([100, 100, 100]));
        paint(&mut frame, [80, 50, 260, 302], [120, 120, 120]);
        paint(&mut frame, [95, 71, 245, 281], [230, 230, 230]);
        paint(&mut frame, [110, 92, 230, 260], [60, 60, 60]);
        let panel = rectangle([110.0, 92.0, 230.0, 260.0]);
        let inner_frame = rectangle([95.0, 71.0, 245.0, 281.0]);
        let physical = rectangle([80.0, 50.0, 260.0, 302.0]);
        let config = ExtractorConfig {
            max_cards: 1,
            ..ExtractorConfig::default()
        };
        // Both baseline repair and independent recovery may encounter the stronger inner frame first.
        for baseline in [Vec::new(), vec![panel]] {
            assert_eq!(
                merge(
                    baseline,
                    vec![inner_frame.clone(), physical.clone()],
                    &frame,
                    &config,
                ),
                vec![physical.clone()]
            );
        }
    }

    /// A small high-contrast chip cannot consume the card slot before a full physical outline.
    #[test]
    fn excludes_tiny_additional_chips_before_card_association() {
        let mut frame = RgbImage::from_pixel(480, 420, Rgb([100, 100, 100]));
        paint(&mut frame, [80, 50, 260, 302], [120, 120, 120]);
        paint(&mut frame, [140, 130, 170, 172], [240, 240, 240]);
        let chip = rectangle([140.0, 130.0, 170.0, 172.0]);
        let physical = rectangle([80.0, 50.0, 260.0, 302.0]);
        let config = ExtractorConfig {
            max_cards: 1,
            ..ExtractorConfig::default()
        };
        assert_eq!(
            merge(Vec::new(), vec![chip, physical.clone()], &frame, &config,),
            vec![physical]
        );
    }

    /// Only an unsupported single edge may move inward; a supported card keeps its frame.
    #[test]
    fn corrects_one_unsupported_boundary_without_shrinking_supported_cards() {
        let mut frame = RgbImage::from_pixel(280, 400, Rgb([80, 80, 80]));
        paint(&mut frame, [80, 60, 260, 312], [200, 200, 200]);
        paint(&mut frame, [85, 65, 255, 307], [230, 230, 230]);
        let oversized = rectangle([80.0, 60.0, 279.0, 312.0]);
        let physical = rectangle([80.0, 60.0, 260.0, 312.0]);
        let panel = rectangle([85.0, 65.0, 255.0, 307.0]);
        assert_eq!(
            merge(
                vec![oversized],
                vec![physical.clone()],
                &frame,
                &ExtractorConfig::default(),
            ),
            vec![physical.clone()]
        );
        assert_eq!(
            merge(
                vec![physical.clone()],
                vec![panel],
                &frame,
                &ExtractorConfig::default(),
            ),
            vec![physical]
        );
    }

    /// Incomplete baseline geometry cannot occupy the association slot of a complete physical card.
    #[test]
    fn incomplete_baseline_does_not_suppress_internal_whole_card() {
        let mut frame = RgbImage::from_pixel(280, 400, Rgb([80, 80, 80]));
        paint(&mut frame, [80, 60, 260, 312], [200, 200, 200]);
        let oversized = rectangle([80.0, 60.0, 279.0, 312.0]);
        let physical = rectangle([80.0, 60.0, 260.0, 312.0]);
        let config = ExtractorConfig {
            max_cards: 1,
            ..ExtractorConfig::default()
        };
        let mut incomplete = Vec::new();
        let baseline = whole_cards(
            vec![oversized.clone()],
            frame.width(),
            frame.height(),
            &config,
            &mut incomplete,
        );
        assert!(baseline.is_empty());
        assert_eq!(incomplete[0].detection, oversized);
        let context = incomplete.clone();
        let candidates = exclude_incomplete_panels(
            vec![physical.clone()],
            &frame,
            &config,
            &context,
            &mut incomplete,
        );
        assert_eq!(merge(baseline, candidates, &frame, &config), vec![physical]);
    }

    /// An unsupported frame-wide mat cannot veto a separately observed whole physical card.
    #[test]
    fn frame_wide_background_is_not_physical_incomplete_card_context() {
        let mut frame = RgbImage::from_pixel(280, 400, Rgb([80, 80, 80]));
        paint(&mut frame, [80, 60, 260, 312], [200, 200, 200]);
        let background = rectangle([0.5, 10.0, 279.0, 390.0]);
        let physical = rectangle([80.0, 60.0, 260.0, 312.0]);
        let config = ExtractorConfig::default();
        let mut incomplete = Vec::new();
        assert!(
            whole_cards(
                vec![background],
                frame.width(),
                frame.height(),
                &config,
                &mut incomplete,
            )
            .is_empty()
        );
        let context = incomplete.clone();
        let candidates = exclude_incomplete_panels(
            vec![physical.clone()],
            &frame,
            &config,
            &context,
            &mut incomplete,
        );
        assert_eq!(
            merge(Vec::new(), candidates, &frame, &config),
            vec![physical]
        );
    }

    /// Explicit fast mode and a caller's source window remain meaningful constraints.
    #[test]
    fn respects_fast_mode_detection_window_and_card_limit() {
        let frame = isoluminant_scene();
        let fast = ExtractorConfig {
            detection_mode: DetectionMode::Fast,
            ..ExtractorConfig::default()
        };
        assert_eq!(
            CardExtractor::new(fast.clone())
                .unwrap()
                .detect(&frame)
                .unwrap(),
            magicextract::CardExtractor::new(fast)
                .unwrap()
                .detect(&frame)
                .unwrap()
        );
        let window = ExtractorConfig {
            detection_region: Some(DetectionRegion {
                x: 0,
                y: 0,
                width: 80,
                height: 420,
            }),
            max_cards: 1,
            ..ExtractorConfig::default()
        };
        assert!(
            CardExtractor::new(window)
                .unwrap()
                .detect(&frame)
                .unwrap()
                .is_empty()
        );
        let config = ExtractorConfig {
            max_cards: 1,
            ..ExtractorConfig::default()
        };
        assert_eq!(
            CardExtractor::new(config)
                .unwrap()
                .detect(&frame)
                .unwrap()
                .len(),
            1
        );
    }

    /// Convex clipping keeps separated rotated cards distinct despite bounding-box overlap.
    #[test]
    fn measures_quadrilaterals_instead_of_axis_aligned_bounds() {
        let first = CardDetection {
            corners: [
                Point { x: 50.0, y: 0.0 },
                Point { x: 100.0, y: 50.0 },
                Point { x: 50.0, y: 100.0 },
                Point { x: 0.0, y: 50.0 },
            ],
            area: 5000.0,
        };
        let mut second = first.clone();
        for corner in &mut second.corners {
            corner.x += 80.0;
            corner.y += 80.0;
        }
        assert_eq!(overlap(&first, &second).intersection, 0.0);
        assert!(overlap(&first, &first).iou > 0.999);
        assert!(overlap(&first, &first).containment > 0.999);
    }
}
