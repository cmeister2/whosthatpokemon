//! Deterministic, portable local correspondence and planar verification.

use std::collections::HashSet;

#[cfg(any(not(target_arch = "wasm32"), feature = "web", test))]
use image::RgbImage;
use image::{DynamicImage, GrayImage, imageops::FilterType};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Width of the normalized local-feature coordinate frame.
pub(super) const WIDTH: u32 = 252;
/// Height of the normalized local-feature coordinate frame.
pub(super) const HEIGHT: u32 = 351;
/// Number of increasingly coarse image pyramid levels.
const LEVELS: i32 = 6;
/// Maximum retained corners at each pyramid level.
const FEATURES_PER_LEVEL: usize = 170;
/// Fixed-point coordinate units per normalized image pixel.
const COORDINATE_SCALE: f64 = 128.0;
/// Minimum distinct correspondences before fitting a card homography.
const MIN_MATCHES: usize = 12;
/// Maximum deterministic minimal-sample hypotheses per reference.
const ITERATIONS: usize = 600;

/// One quantized keypoint and its rotated 256-bit patch descriptor.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Feature {
    /// Horizontal normalized-image position in fixed-point units.
    pub(super) x: u16,
    /// Vertical normalized-image position in fixed-point units.
    pub(super) y: u16,
    /// Fixed-pattern rotated binary intensity comparisons.
    pub(super) bits: [u64; 4],
}

/// A feature correspondence in unit image coordinates.
#[derive(Clone, Copy)]
struct Pair {
    /// Query position normalized by [`WIDTH`] and [`HEIGHT`].
    query: [f64; 2],
    /// Reference position normalized by [`WIDTH`] and [`HEIGHT`].
    reference: [f64; 2],
}

/// Geometrically supported evidence with a query-to-reference pixel transform.
pub(super) struct Evidence {
    /// Absolute inlier-count, inlier-ratio and spatial-coverage evidence.
    pub(super) score: f32,
    /// Closest clockwise right-angle query-to-reference orientation.
    pub(super) rotation_degrees: u16,
    /// Row-major query-to-reference transform in normalized-image pixels.
    pub(super) projection: [f32; 9],
}

/// Advance the local deterministic generator without external random state.
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Generate fixed, approximately Gaussian BRIEF comparisons inside a safe patch.
fn test_pairs() -> Vec<([f64; 2], [f64; 2])> {
    let mut state = 1701;
    let mut result = Vec::with_capacity(256);
    while result.len() < 256 {
        let mut coordinate = || {
            ((0..6)
                .map(|_| (random(&mut state) % 10_000) as f64 / 10_000.0)
                .sum::<f64>()
                - 3.0)
                * 7.5
        };
        let left = [coordinate(), coordinate()];
        let right = [coordinate(), coordinate()];
        if left
            .iter()
            .chain(right.iter())
            .all(|value| value.abs() < 13.0)
        {
            result.push((left, right));
        }
    }
    result
}

/// Read the nearest valid pixel in a rotated, smoothed descriptor patch.
fn sample(image: &GrayImage, x: f64, y: f64) -> u8 {
    let x = x.round().clamp(0.0, f64::from(image.width() - 1)) as u32;
    let y = y.round().clamp(0.0, f64::from(image.height() - 1)) as u32;
    image.get_pixel(x, y)[0]
}

/// Extract at most 1,020 reproducible oriented patch descriptors.
pub(super) fn extract(image: &DynamicImage) -> Vec<Feature> {
    let base = image
        .resize_exact(WIDTH, HEIGHT, FilterType::Triangle)
        .to_luma8();
    extract_luma(&base)
}

/// Extract descriptors from an RGB query already normalized for geometric verification.
#[cfg(any(not(target_arch = "wasm32"), feature = "web", test))]
pub(super) fn extract_normalized_rgb(image: &RgbImage) -> Vec<Feature> {
    debug_assert_eq!(image.dimensions(), (WIDTH, HEIGHT));
    // Keep DynamicImage's colour-space-aware conversion so this path produces
    // byte-identical descriptors to `extract` for normalized RGB inputs.
    let base = DynamicImage::ImageRgb8(image.clone()).to_luma8();
    extract_luma(&base)
}

/// Reuse normalized RGB for native RGB queries and preserve other format conversions.
#[cfg(any(not(target_arch = "wasm32"), feature = "web", test))]
pub(super) fn extract_query(image: &DynamicImage, normalized_rgb: &RgbImage) -> Vec<Feature> {
    if image.as_rgb8().is_some() {
        extract_normalized_rgb(normalized_rgb)
    } else {
        extract(image)
    }
}

/// Extract local features from normalized 8-bit luminance pixels.
fn extract_luma(base: &GrayImage) -> Vec<Feature> {
    let pairs = test_pairs();
    let mut features = Vec::new();
    for level in 0..LEVELS {
        let scale = 1.2_f64.powi(level);
        let width = (f64::from(WIDTH) / scale).round() as u32;
        let height = (f64::from(HEIGHT) / scale).round() as u32;
        // The centroid and rotated descriptor patches need a valid 18-pixel border.
        if width < 50 || height < 50 {
            break;
        }
        let layer = image::imageops::resize(base, width, height, FilterType::Triangle);
        let corners = imageproc::corners::oriented_fast(&layer, Some(16), 3000, 18, Some(1701));
        let smooth = imageproc::filter::gaussian_blur_f32(&layer, 1.5);
        let mut occupied = HashSet::new();
        let mut count = 0;
        for corner in corners {
            if !occupied.insert((corner.corner.x / 5, corner.corner.y / 5)) {
                continue;
            }
            let x = f64::from(corner.corner.x);
            let y = f64::from(corner.corner.y);
            let (sine, cosine) = f64::from(corner.orientation).sin_cos();
            let mut bits = [0; 4];
            for (bit, (left, right)) in pairs.iter().enumerate() {
                let left_x = x + left[0] * cosine - left[1] * sine;
                let left_y = y + left[0] * sine + left[1] * cosine;
                let right_x = x + right[0] * cosine - right[1] * sine;
                let right_y = y + right[0] * sine + right[1] * cosine;
                if sample(&smooth, left_x, left_y) < sample(&smooth, right_x, right_y) {
                    bits[bit / 64] |= 1 << (bit % 64);
                }
            }
            features.push(Feature {
                x: (x * scale * COORDINATE_SCALE).round() as u16,
                y: (y * scale * COORDINATE_SCALE).round() as u16,
                bits,
            });
            count += 1;
            if count >= FEATURES_PER_LEVEL {
                break;
            }
        }
    }
    features
}

/// Validate bounded descriptor counts and coordinates before accepting persisted data.
///
/// # Errors
///
/// Returns [`Error::InvalidIndex`] for excess features or coordinates outside the frame.
pub(super) fn validate(features: &[Feature]) -> Result<(), Error> {
    if features.len() > LEVELS as usize * FEATURES_PER_LEVEL
        || features.iter().any(|feature| {
            f64::from(feature.x) > f64::from(WIDTH) * COORDINATE_SCALE
                || f64::from(feature.y) > f64::from(HEIGHT) * COORDINATE_SCALE
        })
    {
        return Err(Error::InvalidIndex(
            "invalid local-feature count or position".into(),
        ));
    }
    Ok(())
}

/// Count disagreements until the result can no longer improve the two nearest patches.
fn hamming_bounded(left: &Feature, right: &Feature, stop_at: u32) -> u32 {
    let distance =
        (left.bits[0] ^ right.bits[0]).count_ones() + (left.bits[1] ^ right.bits[1]).count_ones();
    if distance >= stop_at {
        return distance;
    }
    distance
        + (left.bits[2] ^ right.bits[2]).count_ones()
        + (left.bits[3] ^ right.bits[3]).count_ones()
}

/// Count disagreements using the hardware population-count instruction.
///
/// # Safety
///
/// Call only after checking that this CPU supports `popcnt`.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "popcnt")]
unsafe fn hamming_popcnt(left: &Feature, right: &Feature) -> u32 {
    left.bits
        .iter()
        .zip(right.bits)
        .map(|(a, b)| (a ^ b).count_ones())
        .sum()
}

/// Find the two nearest reference patches without changing tie order.
fn nearest<const POPCNT: bool>(query: &Feature, reference: &[Feature]) -> (u32, u32, usize) {
    let (mut best, mut second, mut best_index) = (257, 257, 0);
    for (reference_index, reference_feature) in reference.iter().enumerate() {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let distance = if POPCNT {
            // SAFETY: the only `nearest::<true>` caller checks this CPU feature.
            // Four hardware POPCNTs are faster than data-dependent early-exit
            // branches; the bounded fallback is for CPUs without that instruction.
            unsafe { hamming_popcnt(query, reference_feature) }
        } else {
            hamming_bounded(query, reference_feature, second)
        };
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        let distance = hamming_bounded(query, reference_feature, second);
        if distance < best {
            second = best;
            best = distance;
            best_index = reference_index;
        } else if distance < second {
            second = distance;
        }
    }
    (best, second, best_index)
}

/// Apply the nearest-neighbor ratio test and keep each reference keypoint unique.
fn correspondences(query: &[Feature], reference: &[Feature]) -> Vec<Pair> {
    let mut matches = Vec::new();
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    let popcnt = std::is_x86_feature_detected!("popcnt");
    for (query_index, query_feature) in query.iter().enumerate() {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let (best, second, best_index) = if popcnt {
            nearest::<true>(query_feature, reference)
        } else {
            nearest::<false>(query_feature, reference)
        };
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        let (best, second, best_index) = nearest::<false>(query_feature, reference);
        if best < 80 && best * 4 < second * 3 {
            matches.push((best, query_index, best_index));
        }
    }
    matches.sort_unstable();
    let mut used = vec![false; reference.len()];
    matches
        .into_iter()
        .filter_map(|(_, query_index, reference_index)| {
            if used[reference_index] {
                return None;
            }
            used[reference_index] = true;
            let left = &query[query_index];
            let right = &reference[reference_index];
            Some(Pair {
                query: [
                    f64::from(left.x) / (COORDINATE_SCALE * f64::from(WIDTH)),
                    f64::from(left.y) / (COORDINATE_SCALE * f64::from(HEIGHT)),
                ],
                reference: [
                    f64::from(right.x) / (COORDINATE_SCALE * f64::from(WIDTH)),
                    f64::from(right.y) / (COORDINATE_SCALE * f64::from(HEIGHT)),
                ],
            })
        })
        .collect()
}

/// Solve a small augmented normal system with partial pivoting.
fn solve(mut matrix: [[f64; 9]; 8]) -> Option<[f64; 9]> {
    for column in 0..8 {
        let pivot = (column..8)
            .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))?;
        if matrix[pivot][column].abs() < 1e-9 {
            return None;
        }
        matrix.swap(pivot, column);
        let divisor = matrix[column][column];
        for value in &mut matrix[column][column..] {
            *value /= divisor;
        }
        let pivot_row = matrix[column];
        for (row_index, row) in matrix.iter_mut().enumerate() {
            if row_index == column {
                continue;
            }
            let multiplier = row[column];
            for (value, pivot_value) in row[column..].iter_mut().zip(&pivot_row[column..]) {
                *value -= multiplier * pivot_value;
            }
        }
    }
    let mut homography = [0.0; 9];
    for index in 0..8 {
        homography[index] = matrix[index][8];
    }
    homography[8] = 1.0;
    homography
        .iter()
        .all(|value| value.is_finite())
        .then_some(homography)
}

/// Fit a projective transform in normalized coordinates by linear least squares.
fn estimate<'a>(pairs: impl IntoIterator<Item = &'a Pair>) -> Option<[f64; 9]> {
    let mut normal = [[0.0; 9]; 8];
    for pair in pairs {
        let [x, y] = pair.query;
        let [u, v] = pair.reference;
        for equation in [
            [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y, u],
            [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y, v],
        ] {
            for row in 0..8 {
                for column in 0..9 {
                    normal[row][column] += equation[row] * equation[column];
                }
            }
        }
    }
    solve(normal)
}

/// Map one normalized query point into the reference frame.
fn project(homography: &[f64; 9], point: [f64; 2]) -> [f64; 2] {
    let [x, y] = point;
    let denominator = homography[6] * x + homography[7] * y + 1.0;
    [
        (homography[0] * x + homography[1] * y + homography[2]) / denominator,
        (homography[3] * x + homography[4] * y + homography[5]) / denominator,
    ]
}

/// Compute the signed cross product of three planar points.
fn cross(left: [f64; 2], middle: [f64; 2], right: [f64; 2]) -> f64 {
    (middle[0] - left[0]) * (right[1] - left[1]) - (middle[1] - left[1]) * (right[0] - left[0])
}

/// Reject mirrored, collapsed, nonfinite or extreme extrapolated quadrilaterals.
fn plausible(homography: &[f64; 9]) -> bool {
    let corners =
        [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]].map(|point| project(homography, point));
    corners
        .iter()
        .flatten()
        .all(|value| value.is_finite() && value.abs() <= 4.0)
        && (0..4).all(|index| {
            cross(
                corners[index],
                corners[(index + 1) % 4],
                corners[(index + 2) % 4],
            ) > 1e-5
        })
}

/// Whether one correspondence has at most three pixels of forward reprojection error.
fn is_inlier(homography: &[f64; 9], pair: &Pair) -> bool {
    let target = project(homography, pair.query);
    let error = ((target[0] - pair.reference[0]) * f64::from(WIDTH)).powi(2)
        + ((target[1] - pair.reference[1]) * f64::from(HEIGHT)).powi(2);
    error <= 9.0
}

/// Reuse one allocation while selecting inlier correspondence positions.
fn collect_inliers(homography: &[f64; 9], pairs: &[Pair], result: &mut Vec<usize>) {
    result.clear();
    result.extend(
        pairs
            .iter()
            .enumerate()
            .filter_map(|(index, pair)| is_inlier(homography, pair).then_some(index)),
    );
}

/// Compute convex-hull area for a bounded set of normalized image positions.
fn hull_area(mut points: Vec<[f64; 2]>) -> f64 {
    points.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    let (mut lower, mut upper) = (Vec::new(), Vec::new());
    for point in &points {
        while lower.len() >= 2
            && cross(lower[lower.len() - 2], lower[lower.len() - 1], *point) <= 0.0
        {
            lower.pop();
        }
        lower.push(*point);
    }
    for point in points.iter().rev() {
        while upper.len() >= 2
            && cross(upper[upper.len() - 2], upper[upper.len() - 1], *point) <= 0.0
        {
            upper.pop();
        }
        upper.push(*point);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    if lower.len() < 3 {
        return 0.0;
    }
    (0..lower.len())
        .map(|index| {
            let left = lower[index];
            let right = lower[(index + 1) % lower.len()];
            left[0] * right[1] - right[0] * left[1]
        })
        .sum::<f64>()
        .abs()
        / 2.0
}

/// Fit deterministic RANSAC and require evidence spread across both face and artwork.
///
/// No score is returned for short correspondence lists, degenerate geometry,
/// concentrated frame/text matches, or insufficient artwork evidence.
pub(super) fn match_evidence(query: &[Feature], reference: &[Feature]) -> Option<Evidence> {
    let pairs = correspondences(query, reference);
    if pairs.len() < MIN_MATCHES {
        return None;
    }
    let mut best = Vec::with_capacity(pairs.len());
    let mut hits = Vec::with_capacity(pairs.len());
    let mut state = 1701;
    for _ in 0..ITERATIONS {
        let mut indices = [0; 4];
        let mut selected = 0;
        while selected < indices.len() {
            let index = (random(&mut state) % pairs.len() as u64) as usize;
            if !indices[..selected].contains(&index) {
                indices[selected] = index;
                selected += 1;
            }
        }
        let sample = indices.map(|index| pairs[index]);
        if let Some(homography) = estimate(&sample).filter(plausible) {
            collect_inliers(&homography, &pairs, &mut hits);
            if hits.len() > best.len() {
                std::mem::swap(&mut best, &mut hits);
                if best.len() as f64 / pairs.len() as f64 > 0.95 {
                    break;
                }
            }
        }
    }
    if best.len() < MIN_MATCHES {
        return None;
    }
    let homography = estimate(best.iter().map(|index| &pairs[*index])).filter(plausible)?;
    collect_inliers(&homography, &pairs, &mut hits);
    std::mem::swap(&mut best, &mut hits);
    if best.len() < MIN_MATCHES {
        return None;
    }
    let coverage = hull_area(best.iter().map(|index| pairs[*index].reference).collect());
    let query_coverage = hull_area(best.iter().map(|index| pairs[*index].query).collect());
    let artwork: Vec<_> = best
        .iter()
        .map(|index| pairs[*index].reference)
        .filter(|point| (0.09..=0.91).contains(&point[0]) && (0.16..=0.57).contains(&point[1]))
        .collect();
    if coverage < 0.08 || query_coverage < 0.06 || artwork.len() < 8 || hull_area(artwork) < 0.015 {
        return None;
    }
    let count = best.len() as f64;
    let score =
        (1.0 - (-count / 20.0).exp()) * (count / pairs.len() as f64) * (coverage / 0.20).min(1.0);
    let left = project(&homography, [0.4, 0.5]);
    let right = project(&homography, [0.6, 0.5]);
    let angle =
        ((right[1] - left[1]) * f64::from(HEIGHT)).atan2((right[0] - left[0]) * f64::from(WIDTH));
    let rotation = (angle.to_degrees() / 90.0).round() as i32;
    Some(Evidence {
        score: score.clamp(0.0, 1.0) as f32,
        rotation_degrees: (rotation.rem_euclid(4) * 90) as u16,
        projection: [
            homography[0] as f32,
            (homography[1] * f64::from(WIDTH) / f64::from(HEIGHT)) as f32,
            (homography[2] * f64::from(WIDTH)) as f32,
            (homography[3] * f64::from(HEIGHT) / f64::from(WIDTH)) as f32,
            homography[4] as f32,
            (homography[5] * f64::from(HEIGHT)) as f32,
            (homography[6] / f64::from(WIDTH)) as f32,
            (homography[7] / f64::from(HEIGHT)) as f32,
            1.0,
        ],
    })
}

/// Geometric regressions use synthetic correspondences, independent of stored card art.
#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayAlphaImage, GrayImage, ImageBuffer, Luma, LumaA, Rgb, Rgba, RgbaImage};

    /// Compare persisted feature fields without requiring them to be public API values.
    fn assert_same_features(left: &[Feature], right: &[Feature]) {
        assert!(!left.is_empty());
        assert_eq!(left.len(), right.len());
        for (left, right) in left.iter().zip(right) {
            assert_eq!((left.x, left.y, left.bits), (right.x, right.y, right.bits));
        }
    }

    /// Run the format-preserving query route used by candidate geometry.
    fn extract_query_path(image: &DynamicImage) -> Vec<Feature> {
        let rgb = image.to_rgb8();
        let normalized = image::imageops::resize(&rgb, WIDTH, HEIGHT, FilterType::Triangle);
        extract_query(image, &normalized)
    }

    /// Sharing normalization preserves every local-feature byte for ordinary RGB input.
    #[test]
    fn normalized_rgb_extraction_matches_compatibility_rgb_path() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_fn(413, 577, |x, y| {
            let value = x.wrapping_mul(47) ^ y.wrapping_mul(83) ^ (x * y).wrapping_mul(17);
            Rgb([
                (value & 255) as u8,
                ((value >> 3) & 255) as u8,
                ((value >> 7) & 255) as u8,
            ])
        }));
        let rgb = image.to_rgb8();
        let normalized = image::imageops::resize(&rgb, WIDTH, HEIGHT, FilterType::Triangle);
        assert_same_features(&extract(&image), &extract_normalized_rgb(&normalized));
    }

    /// Non-RGB query formats retain the compatibility conversion and feature bytes.
    #[test]
    fn query_extraction_preserves_non_rgb_dynamic_paths() {
        let luma = DynamicImage::ImageLuma8(GrayImage::from_fn(413, 577, |x, y| {
            Luma([((x * 47) ^ (y * 83) ^ (x * y * 17)) as u8])
        }));
        let luma_alpha = DynamicImage::ImageLumaA8(GrayAlphaImage::from_fn(413, 577, |x, y| {
            LumaA([
                ((x * 47) ^ (y * 83) ^ (x * y * 17)) as u8,
                ((x * 13 + y * 29) & 255) as u8,
            ])
        }));
        let rgba = DynamicImage::ImageRgba8(RgbaImage::from_fn(413, 577, |x, y| {
            let value = x.wrapping_mul(47) ^ y.wrapping_mul(83) ^ (x * y).wrapping_mul(17);
            Rgba([
                (value & 255) as u8,
                ((value >> 3) & 255) as u8,
                ((value >> 7) & 255) as u8,
                ((x * 13 + y * 29) & 255) as u8,
            ])
        }));
        let luma16 = DynamicImage::ImageLuma16(ImageBuffer::<Luma<u16>, Vec<u16>>::from_fn(
            413,
            577,
            |x, y| Luma([((x * 1217) ^ (y * 2137) ^ (x * y * 173)) as u16]),
        ));
        for image in [luma, luma_alpha, rgba, luma16] {
            assert_same_features(&extract(&image), &extract_query_path(&image));
        }
    }

    /// Full-distance baseline for proving that bounded sums preserve strict tie handling.
    fn nearest_exhaustive(query: &Feature, reference: &[Feature]) -> (u32, u32, usize) {
        let (mut best, mut second, mut best_index) = (257, 257, 0);
        for (reference_index, reference_feature) in reference.iter().enumerate() {
            let distance = query
                .bits
                .iter()
                .zip(reference_feature.bits)
                .map(|(left, right)| (left ^ right).count_ones())
                .sum();
            if distance < best {
                second = best;
                best = distance;
                best_index = reference_index;
            } else if distance < second {
                second = distance;
            }
        }
        (best, second, best_index)
    }

    /// Bounded portable distances retain exact nearest neighbors, runners-up and ties.
    #[test]
    fn bounded_hamming_preserves_exhaustive_nearest_neighbors() {
        let mut state = 91517;
        let mut references = Vec::new();
        for _ in 0..256 {
            references.push(Feature {
                x: 0,
                y: 0,
                bits: std::array::from_fn(|_| random(&mut state)),
            });
        }
        references.push(references[3].clone());
        references.push(Feature {
            x: 0,
            y: 0,
            bits: [0; 4],
        });
        references.push(Feature {
            x: 0,
            y: 0,
            bits: [u64::MAX; 4],
        });
        for query in &references {
            assert_eq!(
                nearest::<false>(query, &references),
                nearest_exhaustive(query, &references)
            );
        }
    }

    /// Hardware and portable patch distances preserve nearest-neighbor ties.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn hardware_population_count_preserves_nearest_neighbors() {
        if !std::is_x86_feature_detected!("popcnt") {
            return;
        }
        let mut state = 91517;
        let mut references = Vec::new();
        for _ in 0..256 {
            references.push(Feature {
                x: 0,
                y: 0,
                bits: std::array::from_fn(|_| random(&mut state)),
            });
        }
        references.push(references[3].clone());
        references.push(Feature {
            x: 0,
            y: 0,
            bits: [0; 4],
        });
        references.push(Feature {
            x: 0,
            y: 0,
            bits: [u64::MAX; 4],
        });
        for query in &references {
            assert_eq!(
                nearest::<true>(query, &references),
                nearest_exhaustive(query, &references)
            );
        }
    }

    /// Produce distributed, distinctive descriptors and their known projective locations.
    fn projected_features() -> (Vec<Feature>, Vec<Feature>) {
        let mut state = 9173;
        let mut query = Vec::new();
        let mut reference = Vec::new();
        let homography = [0.95, 0.04, 0.01, -0.02, 0.98, 0.01, 0.02, 0.01, 1.0];
        for row in 0..5 {
            for column in 0..8 {
                let point = [0.12 + column as f64 * 0.10, 0.18 + row as f64 * 0.08];
                let mapped = project(&homography, point);
                let bits = [
                    random(&mut state),
                    random(&mut state),
                    random(&mut state),
                    random(&mut state),
                ];
                query.push(Feature {
                    x: (point[0] * f64::from(WIDTH) * COORDINATE_SCALE) as u16,
                    y: (point[1] * f64::from(HEIGHT) * COORDINATE_SCALE) as u16,
                    bits,
                });
                reference.push(Feature {
                    x: (mapped[0] * f64::from(WIDTH) * COORDINATE_SCALE) as u16,
                    y: (mapped[1] * f64::from(HEIGHT) * COORDINATE_SCALE) as u16,
                    bits,
                });
            }
        }
        // Descriptor matches with deliberately inconsistent geometry must be rejected.
        for feature in reference.iter_mut().take(5) {
            feature.x = (0.92 * f64::from(WIDTH) * COORDINATE_SCALE) as u16;
            feature.y = (0.88 * f64::from(HEIGHT) * COORDINATE_SCALE) as u16;
        }
        (query, reference)
    }

    /// RANSAC recovers repeatable projective evidence despite geometrically false matches.
    #[test]
    fn verifies_projective_matches_and_rejects_outliers() {
        let (query, reference) = projected_features();
        let first = match_evidence(&query, &reference).unwrap();
        let second = match_evidence(&query, &reference).unwrap();
        assert_eq!(first.score, second.score);
        assert_eq!(first.projection, second.projection);
        assert!(first.score > 0.6);
        assert_eq!(first.rotation_degrees, 0);
    }

    /// Shared frame corners cannot establish identity without artwork correspondences.
    #[test]
    fn rejects_frame_only_geometric_agreement() {
        let (mut query, mut reference) = projected_features();
        for (index, (left, right)) in query.iter_mut().zip(&mut reference).enumerate() {
            left.y = (if index % 2 == 0 { 0.05 } else { 0.92 }
                * f64::from(HEIGHT)
                * COORDINATE_SCALE) as u16;
            right.y = left.y;
            right.x = left.x;
        }
        assert!(match_evidence(&query, &reference).is_none());
    }

    /// Singular or mirrored transforms are rejected before they can become evidence.
    #[test]
    fn rejects_degenerate_and_mirrored_geometry() {
        assert!(!plausible(&[-1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]));
        let repeated = Pair {
            query: [0.5, 0.5],
            reference: [0.5, 0.5],
        };
        assert!(estimate(&[repeated; 4]).is_none());
    }
}
