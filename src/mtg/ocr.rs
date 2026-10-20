//! Magic text recognition with `ocracle`, independent of catalogue matching.
//!
//! The engine is loaded once and reused across card crops. Results retain raw
//! observed text, line coordinates, model provenance and uncalibrated reading
//! scores. Approximate portrait templates assign lines to title, rules and footer;
//! other layouts retain their raw OCR but can need different field geometry.

use std::{fs, path::Path};

use image::DynamicImage;
use ocracle::{CardOcr, InferenceBackend, Line, ModelConfig, Options};
use serde::Serialize;

use super::evidence::{FrameLayout, TextObservations, regions};
use crate::Error;

/// Observed text and region assignments for one explicitly oriented card crop.
#[derive(Debug, Serialize)]
pub struct OcrEvidence {
    /// Clockwise rotation applied before OCR; line coordinates refer to this image.
    pub rotation_degrees: u16,
    /// Approximate portrait template used for assigning observed lines to fields.
    pub frame_layout: FrameLayout,
    /// Unmodified OCR output, including all observed text, model hashes and scores.
    pub reading: ocracle::Recognition,
    /// Text grouped from observed line positions, without catalogue substitutions.
    pub observations: TextObservations,
}

/// Detector-free title reading from one explicitly oriented portrait card.
///
/// [`Self::reading`] retains the raw recognizer text, uncalibrated confidence,
/// preprocessing method, competing candidates and model identity. Its line
/// coordinates are local to [`Self::crop_bounds`]; the remaining fields map the
/// crop back to the oriented card without consulting catalogue metadata.
#[derive(Debug, Serialize)]
pub struct TitleOcrEvidence {
    /// Clockwise rotation applied before cropping; either zero or 180 degrees.
    pub rotation_degrees: u16,
    /// Approximate portrait template used to localize the title line.
    pub frame_layout: FrameLayout,
    /// Dimensions of the explicitly oriented rectified card.
    pub oriented_image_size: [u32; 2],
    /// Title crop `[x, y, width, height]` in oriented-card pixels.
    pub crop_bounds: [u32; 4],
    /// Unmodified detector-free OCR output, including model provenance.
    pub reading: ocracle::Recognition,
}

/// Reusable native OCR engine with explicitly supplied, locally stored models.
pub struct MagicOcr {
    /// Loaded detection and recognition networks shared across successive crops.
    engine: CardOcr,
}

impl MagicOcr {
    /// Load the default `ocracle` recognizer for one printed language.
    ///
    /// `model_root` contains `models/profiles.json` and the separately downloaded
    /// weights. Loading and recognition never download models or submit images.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Backend`] for unsupported languages, unavailable model
    /// files, integrity failures or an incompatible model/runtime configuration.
    pub fn open(model_root: &Path, language: &str) -> Result<Self, Error> {
        let engine = CardOcr::open_language(language, model_root)
            .map_err(|error| Error::Backend(format!("ocracle: {error:#}")))?;
        Ok(Self { engine })
    }

    /// Load a language recognizer without the whole-card text detector.
    ///
    /// Use this constructor when the caller supplies title crops through
    /// [`Self::read_title`]. The selected language profile and its integrity
    /// checks are unchanged; only the unused detector model is omitted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Backend`] for unsupported languages, unavailable model
    /// files, integrity failures or an incompatible recognition model.
    pub fn open_title(model_root: &Path, language: &str) -> Result<Self, Error> {
        Self::open_title_with_threads(model_root, language, None)
    }

    /// Load a detector-free language profile with an explicit RTen worker count.
    ///
    /// `None` preserves ocracle's default runtime policy. Reuse the returned
    /// engine across frames so the configured inference pool remains warm.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Backend`] for the profile errors described by
    /// [`Self::open_title`] or an invalid worker count.
    pub fn open_title_with_threads(
        model_root: &Path,
        language: &str,
        threads: Option<usize>,
    ) -> Result<Self, Error> {
        let engine = CardOcr::load_profile_with_backend(
            language,
            model_root,
            None,
            InferenceBackend::Rten,
            threads,
        )
        .map_err(|error| Error::Backend(format!("ocracle: {error:#}")))?;
        Ok(Self { engine })
    }

    /// Load the faster PP-OCRv6 small recognizer without a detector.
    ///
    /// The separately distributed model root must contain
    /// `models/ppocr-v6-small.onnx` and the matching
    /// `models/paddle-v6/<language>.json` configuration. This profile trades a
    /// small amount of title accuracy for substantially lower latency; callers
    /// still retain raw model provenance and must use visual verification.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Backend`] for unreadable or invalid configuration,
    /// mismatched language metadata, missing weights, or incompatible inference.
    pub fn open_title_small(model_root: &Path, language: &str) -> Result<Self, Error> {
        Self::open_title_small_with_threads(model_root, language, None)
    }

    /// Load the faster detector-free recognizer with explicit RTen workers.
    ///
    /// `None` preserves ocracle's default runtime policy. The worker count is
    /// owned by this engine and does not mutate process environment variables.
    ///
    /// # Errors
    ///
    /// Returns the errors described by [`Self::open_title_small`] or an invalid
    /// worker count.
    pub fn open_title_small_with_threads(
        model_root: &Path,
        language: &str,
        threads: Option<usize>,
    ) -> Result<Self, Error> {
        let config_path = model_root
            .join("models/paddle-v6")
            .join(format!("{language}.json"));
        let config: ModelConfig = serde_json::from_slice(
            &fs::read(&config_path)
                .map_err(|error| Error::Backend(format!("read {config_path:?}: {error}")))?,
        )
        .map_err(|error| Error::Backend(format!("parse {config_path:?}: {error}")))?;
        if config.language != language {
            return Err(Error::Backend(format!(
                "ocracle title configuration language {:?} does not match {language:?}",
                config.language
            )));
        }
        let engine = CardOcr::load_with_backend(
            model_root.join("models/ppocr-v6-small.onnx"),
            None,
            config,
            InferenceBackend::Rten,
            threads,
        )
        .map_err(|error| Error::Backend(format!("ocracle: {error:#}")))?;
        Ok(Self { engine })
    }

    /// Read a rectified card using an explicit orientation and portrait template.
    ///
    /// The entire image is read without masking its artwork. This preserves
    /// title evidence in crops with sleeve margins and in unusual layouts.
    /// Rotation is supplied by the caller rather than inferred from OCR confidence.
    ///
    /// # Errors
    ///
    /// Returns an error for empty images, a rotation other than 0/90/180/270,
    /// failed inference or invalid returned line geometry.
    pub fn read_crop(
        &self,
        image: &DynamicImage,
        layout: FrameLayout,
        rotation_degrees: u16,
    ) -> Result<OcrEvidence, Error> {
        crate::matching::validate_image(image)?;
        let oriented = match rotation_degrees {
            0 => image.clone(),
            90 => image.rotate90(),
            180 => image.rotate180(),
            270 => image.rotate270(),
            _ => {
                return Err(Error::InvalidOptions(
                    "OCR rotation must be 0, 90, 180 or 270 degrees".into(),
                ));
            }
        };
        let reading = self
            .engine
            .read(&oriented)
            .map_err(|error| Error::Backend(format!("ocracle: {error:#}")))?;
        let observations = observations_from_lines(&reading.lines, reading.image_size, layout)?;
        Ok(OcrEvidence {
            rotation_degrees,
            frame_layout: layout,
            reading,
            observations,
        })
    }

    /// Read only the title line of a rectified portrait card.
    ///
    /// This path applies the caller's explicit upright or upside-down
    /// orientation, takes a generously padded conventional title crop, and
    /// invokes `ocracle` in single-line mode. It therefore bypasses text
    /// detection. The result is literal model output and is not corrected or
    /// substituted from a card catalogue.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or non-portrait image, a rotation other
    /// than zero or 180 degrees, failed inference, or an invalid title crop.
    pub fn read_title(
        &self,
        image: &DynamicImage,
        layout: FrameLayout,
        rotation_degrees: u16,
    ) -> Result<TitleOcrEvidence, Error> {
        let (crop, crop_bounds) = title_crop(image, layout, rotation_degrees)?;
        let oriented_image_size = [image.width(), image.height()];
        let reading = self
            .engine
            .recognize(
                &crop,
                Options {
                    single_line: true,
                    ..Options::default()
                },
            )
            .map_err(|error| Error::Backend(format!("ocracle: {error:#}")))?;
        Ok(TitleOcrEvidence {
            rotation_degrees,
            frame_layout: layout,
            oriented_image_size,
            crop_bounds,
            reading,
        })
    }
}

/// Crop a padded conventional title line in an explicit card orientation.
///
/// Cropping starts near the left border and above the nominal title frame to
/// tolerate small rectification and sleeve margins. The upper black card edge
/// is excluded: its strong horizontal line interferes with detector-free text
/// recognition. The crop stops before the mana cost and the type line on
/// conventional frames.
///
/// # Errors
///
/// Returns an error for an empty or non-portrait image, an unsupported
/// orientation, or dimensions too small to produce a nonempty title crop.
fn title_crop(
    image: &DynamicImage,
    layout: FrameLayout,
    rotation_degrees: u16,
) -> Result<(DynamicImage, [u32; 4]), Error> {
    crate::matching::validate_image(image)?;
    if image.width() >= image.height() {
        return Err(Error::InvalidInput(
            "title OCR requires a rectified portrait card".into(),
        ));
    }
    let normalized = match layout {
        FrameLayout::Modern => [0.02, 0.03, 0.82, 0.12],
        FrameLayout::Classic => [0.02, 0.03, 0.83, 0.14],
    };
    let bounds = pixel_bounds(image.width(), image.height(), normalized)?;
    let [x, y, width, height] = bounds;
    let crop = match rotation_degrees {
        0 => image.crop_imm(x, y, width, height),
        180 => image
            .crop_imm(
                image.width() - x - width,
                image.height() - y - height,
                width,
                height,
            )
            .rotate180(),
        _ => {
            return Err(Error::InvalidOptions(
                "title OCR rotation must be 0 or 180 degrees".into(),
            ));
        }
    };
    Ok((crop, bounds))
}

/// Convert normalized corners to a nonempty pixel rectangle with outward rounding.
///
/// # Errors
///
/// Returns an error when the source dimensions or normalized rectangle cannot
/// produce a nonempty in-image crop.
fn pixel_bounds(width: u32, height: u32, normalized: [f32; 4]) -> Result<[u32; 4], Error> {
    let [left, top, right, bottom] = normalized;
    let x = (left * width as f32).floor() as u32;
    let y = (top * height as f32).floor() as u32;
    let right = (right * width as f32).ceil().min(width as f32) as u32;
    let bottom = (bottom * height as f32).ceil().min(height as f32) as u32;
    let crop_width = right.saturating_sub(x);
    let crop_height = bottom.saturating_sub(y);
    if width == 0 || height == 0 || crop_width == 0 || crop_height == 0 {
        return Err(Error::InvalidInput(
            "title crop must contain at least one pixel".into(),
        ));
    }
    Ok([x, y, crop_width, crop_height])
}

/// Assign actual OCR lines to approximate portrait title, rules and footer regions.
///
/// The title uses the most substantial text row in the upper fifth, tolerating
/// extraction margins. Rules/footer use the supplied frame template. Fragments
/// on a row are joined left to right, retaining separate word boxes.
/// Unassigned text remains available in the complete [`OcrEvidence::reading`].
///
/// # Errors
///
/// Returns an error for zero image dimensions or line rectangles outside the image.
pub fn observations_from_lines(
    lines: &[Line],
    image_size: [u32; 2],
    layout: FrameLayout,
) -> Result<TextObservations, Error> {
    let [width, height] = image_size;
    if width == 0 || height == 0 {
        return Err(Error::InvalidInput(
            "OCR image dimensions must be positive".into(),
        ));
    }
    for line in lines {
        let [x, y, w, h] = line.bounds;
        if x.checked_add(w).is_none_or(|end| end > width)
            || y.checked_add(h).is_none_or(|end| end > height)
        {
            return Err(Error::Backend(
                "ocracle returned out-of-image line geometry".into(),
            ));
        }
    }
    let mut observations = TextObservations::default();
    for region in regions(layout) {
        if region.key == "artwork" {
            continue;
        }
        let [left, top, right, bottom] = if region.key == "title" {
            [0.0, 0.0, 0.82, 0.20]
        } else {
            region.bounds
        };
        let selected: Vec<_> = lines
            .iter()
            .filter(|line| {
                let [x, y, w, h] = line.bounds;
                let centre_x = (f64::from(x) + f64::from(w) / 2.0) / f64::from(width);
                let centre_y = (f64::from(y) + f64::from(h) / 2.0) / f64::from(height);
                w > 0
                    && h > 0
                    && (f64::from(left)..=f64::from(right)).contains(&centre_x)
                    && (f64::from(top)..=f64::from(bottom)).contains(&centre_y)
            })
            .collect();
        let rows = reading_rows(selected);
        let text = if region.key == "title" {
            // Prefer substantial observed text without consulting a title dictionary.
            // A strict improvement retains the earlier row when lengths are tied.
            rows.into_iter().fold(String::new(), |best, row| {
                if row.chars().filter(|c| c.is_alphanumeric()).count()
                    > best.chars().filter(|c| c.is_alphanumeric()).count()
                {
                    row
                } else {
                    best
                }
            })
        } else {
            rows.join("\n")
        };
        match region.key.as_str() {
            "title" => observations.title = text,
            "rules" => observations.rules = text,
            "footer" => observations.footer = text,
            _ => {}
        }
    }
    Ok(observations)
}

/// Group overlapping text fragments into rows, then preserve their reading order.
fn reading_rows(mut lines: Vec<&Line>) -> Vec<String> {
    lines.sort_by_key(|line| u64::from(line.bounds[1]) * 2 + u64::from(line.bounds[3]));
    let mut rows: Vec<Vec<&Line>> = Vec::new();
    for line in lines {
        let centre = f64::from(line.bounds[1]) + f64::from(line.bounds[3]) / 2.0;
        let row = rows.iter_mut().find(|row| {
            let anchor = row[0];
            let anchor_centre = f64::from(anchor.bounds[1]) + f64::from(anchor.bounds[3]) / 2.0;
            (centre - anchor_centre).abs() <= f64::from(line.bounds[3].min(anchor.bounds[3])) / 2.0
        });
        if let Some(row) = row {
            row.push(line);
        } else {
            rows.push(vec![line]);
        }
    }
    rows.into_iter()
        .map(|mut row| {
            row.sort_by_key(|line| line.bounds[0]);
            row.into_iter()
                .map(|line| line.text.trim())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|row| !row.is_empty())
        .collect()
}

/// Offline contract tests use invented lines, without requiring model weights.
#[cfg(test)]
mod tests {
    use super::*;

    /// Conventional frame templates round outward and retain useful padding.
    #[test]
    fn title_crop_bounds_are_stable_for_rectified_cards() {
        let image = DynamicImage::new_rgb8(500, 700);
        let (_, modern) = title_crop(&image, FrameLayout::Modern, 0).unwrap();
        let (_, classic) = title_crop(&image, FrameLayout::Classic, 0).unwrap();
        assert_eq!(modern, [10, 21, 400, 63]);
        assert_eq!(classic, [10, 21, 405, 77]);

        assert_eq!(
            pixel_bounds(101, 141, [0.02, 0.0, 0.83, 0.14]).unwrap(),
            [2, 0, 82, 20]
        );
    }

    /// Rotation changes the sampled source edge while preserving oriented bounds.
    #[test]
    fn title_crop_applies_explicit_half_turn_before_localization() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_fn(100, 200, |x, y| {
            image::Rgb([x as u8, y as u8, 0])
        }));
        let (upright, upright_bounds) = title_crop(&image, FrameLayout::Modern, 0).unwrap();
        let (inverted, inverted_bounds) = title_crop(&image, FrameLayout::Modern, 180).unwrap();

        assert_eq!(upright_bounds, [2, 6, 80, 18]);
        assert_eq!(inverted_bounds, upright_bounds);
        assert_eq!(upright.to_rgb8().get_pixel(0, 0).0, [2, 6, 0]);
        assert_eq!(inverted.to_rgb8().get_pixel(0, 0).0, [97, 193, 0]);
    }

    /// Fast title OCR has a deliberately narrower input contract than whole-card OCR.
    #[test]
    fn title_crop_rejects_unsupported_geometry_and_rotation() {
        let portrait = DynamicImage::new_rgb8(100, 140);
        assert!(title_crop(&portrait, FrameLayout::Modern, 90).is_err());
        assert!(title_crop(&portrait, FrameLayout::Classic, 270).is_err());
        assert!(title_crop(&DynamicImage::new_rgb8(140, 100), FrameLayout::Modern, 0).is_err());
        assert!(title_crop(&DynamicImage::new_rgb8(0, 100), FrameLayout::Modern, 0).is_err());
        assert!(pixel_bounds(100, 140, [0.5, 0.5, 0.5, 0.6]).is_err());
    }

    /// Preserve literal OCR, spatial word order and independent catalogue searching.
    #[test]
    fn split_title_fragments_and_footer_keep_observed_reading_order() {
        let lines = vec![
            Line {
                text: "Spell".into(),
                confidence: 0.9,
                bounds: [240, 42, 80, 16],
            },
            Line {
                text: "Example".into(),
                confidence: 0.9,
                bounds: [40, 45, 190, 16],
            },
            Line {
                text: "Noise inside artwork".into(),
                confidence: 0.9,
                bounds: [70, 200, 250, 30],
            },
            Line {
                text: "Draw a card.".into(),
                confidence: 0.9,
                bounds: [45, 450, 250, 18],
            },
            Line {
                text: "127 M15 EN".into(),
                confidence: 0.9,
                bounds: [35, 650, 250, 18],
            },
        ];
        let observed = observations_from_lines(&lines, [500, 700], FrameLayout::Modern).unwrap();
        assert_eq!(observed.title, "Example Spell");
        assert_eq!(observed.rules, "Draw a card.");
        assert_eq!(observed.footer, "127 M15 EN");
    }

    /// Invented rows exercise varied margins and retain literal recognition errors.
    #[test]
    fn title_fragments_survive_varied_crop_margins() {
        for top in [0, 25, 50] {
            let lines = vec![
                Line {
                    text: "42".into(),
                    confidence: 1.0,
                    bounds: [220, 0, 20, 10],
                },
                Line {
                    text: "Spel1".into(),
                    confidence: 0.7,
                    bounds: [105, top + 2, 55, 12],
                },
                Line {
                    text: "Example".into(),
                    confidence: 0.8,
                    bounds: [15, top, 80, 16],
                },
            ];
            let original = serde_json::to_value(&lines).unwrap();
            for layout in [FrameLayout::Modern, FrameLayout::Classic] {
                let observed = observations_from_lines(&lines, [250, 350], layout).unwrap();
                assert_eq!(observed.title, "Example Spel1");
                assert!(observed.rules.is_empty() && observed.footer.is_empty());
            }
            // Region assignment retains the literal recognition error and leaves
            // source text, confidences and rectangles available for inspection.
            assert_eq!(serde_json::to_value(&lines).unwrap(), original);
        }
    }

    /// Title selection uses observed row content, rather than confidence or byte length.
    #[test]
    fn substantial_title_row_preserves_unicode_and_deterministic_ties() {
        let mut lines = vec![
            Line {
                text: "PAGE 1".into(),
                confidence: 1.0,
                bounds: [10, 2, 90, 12],
            },
            Line {
                text: "dragon".into(),
                confidence: 0.6,
                bounds: [140, 40, 80, 16],
            },
            Line {
                text: "Éveil du".into(),
                confidence: 0.7,
                bounds: [20, 38, 110, 18],
            },
            Line {
                text: "Artwork lettering is longer".into(),
                confidence: 1.0,
                bounds: [20, 170, 200, 20],
            },
        ];
        let observed = observations_from_lines(&lines, [250, 350], FrameLayout::Modern).unwrap();
        assert_eq!(observed.title, "Éveil du dragon");

        lines.truncate(2);
        lines[0].text = "Aura".into();
        lines[1].text = "Élan".into();
        // Both have four alphanumeric characters. UTF-8 byte length and the
        // greater confidence must not change which equally substantial row wins.
        lines[0].confidence = 0.2;
        lines[1].confidence = 1.0;
        let observed = observations_from_lines(&lines, [250, 350], FrameLayout::Modern).unwrap();
        assert_eq!(observed.title, "Aura");
    }

    /// Invalid geometry is rejected; absent OCR stays absent instead of borrowing metadata.
    #[test]
    fn empty_observations_and_invalid_geometry() {
        let observed = observations_from_lines(&[], [500, 700], FrameLayout::Classic).unwrap();
        assert!(
            observed.title.is_empty() && observed.rules.is_empty() && observed.footer.is_empty()
        );
        for size in [[0, 700], [500, 0]] {
            assert!(observations_from_lines(&[], size, FrameLayout::Modern).is_err());
        }
        for bounds in [
            [u32::MAX, 0, 10, 1],
            [0, u32::MAX, 1, 10],
            [499, 0, 2, 1],
            [0, 699, 1, 2],
        ] {
            let bad = Line {
                text: "bad".into(),
                confidence: 0.9,
                bounds,
            };
            assert!(observations_from_lines(&[bad], [500, 700], FrameLayout::Modern).is_err());
        }
    }
}
