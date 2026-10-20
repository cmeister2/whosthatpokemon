//! Resumable webcam recognition with full metadata and candidate-only reference acquisition.

use image::{DynamicImage, RgbaImage};
use magicextract::{CardExtractor, ExtractorConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

use super::frame::{CameraCard, CameraReport, validate_frame};
use crate::matching::{CandidateOptions, RemoteCatalog, RemoteReference};
use crate::mtg::{
    evidence::{FooterIndex, FrameLayout, TitleIndex},
    identify::identify_fields_using,
    ocr::MagicOcr,
    recognition::CropIdentification,
};
use crate::{Error, MatchOptions, clock::Instant};

/// Full English metadata envelope, without preloaded visual feature banks.
#[derive(Deserialize)]
struct MetadataSnapshot {
    /// Supported snapshot representation revision.
    schema_version: u32,
    /// Public coverage declaration used to validate complete face counts.
    catalogue: Coverage,
    /// Every metadata face and its explicit canonical image source.
    cards: Vec<RemoteReference>,
}

/// Minimal coverage contract; richer source metadata stays in the deployment manifest.
#[derive(Deserialize)]
struct Coverage {
    /// Exact number of independently indexed physical faces.
    faces: usize,
}

/// A rectified image and the original detection coordinates.
struct PendingCrop {
    /// Real extracted pixels, never replaced by a catalogue reference.
    image: DynamicImage,
    /// Original source quadrilateral.
    corners: [[f32; 2]; 4],
    /// Original source detection area.
    area: f32,
}

/// One in-flight frame, retaining raw OCR across asynchronous reference acquisition.
struct PendingFrame {
    /// Actual decoded frame width.
    width: u32,
    /// Actual decoded frame height.
    height: u32,
    /// Caller-selected historic or modern field template.
    layout: FrameLayout,
    /// Actual extracted crops, bounded to four.
    crops: Vec<PendingCrop>,
    /// Next crop still awaiting a complete identification result.
    next: usize,
    /// Shared resumable OCR/orientation state for the current crop.
    work: Option<CropIdentification>,
    /// Completed native-shaped card results.
    cards: Vec<CameraCard>,
    /// Actual extraction stage elapsed time.
    extraction_ms: f64,
    /// Active frame processing time excluding downloads, enrollment and export.
    cpu_ms: f64,
    /// Actual diagnostic PNG serialization time.
    export_ms: f64,
}

/// A complete frame or a concrete request for only the next selected reference images.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum FrameProgress {
    /// No recognition decision is fabricated while reference pixels are unavailable.
    NeedsReferences {
        /// Physical faces selected by the unchanged native candidate recognizer.
        requests: Vec<RemoteReference>,
    },
    /// Actual finished OCR and shared candidate verification results.
    Complete {
        /// Native-shaped frame report with separately measured export costs.
        report: CameraReport,
    },
}

/// Full-catalogue browser recognizer with resumable candidate-image acquisition.
///
/// Compact title/footer and physical/identity/artwork postings cover every face
/// in the metadata snapshot. The native matcher requests only missing selected
/// reference images, which are encoded with the actual native descriptor builder.
/// At most 512 acquired faces remain cached. No whole-catalogue image scoring occurs.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct RemoteBrowserEngine {
    /// Complete immutable metadata with a bounded sparse visual cache.
    catalog: RemoteCatalog,
    /// Compact all-face title postings built at initialization.
    titles: TitleIndex,
    /// Complete all-face exact collector-footer postings.
    footers: FooterIndex,
    /// Reused actual OCRacle detector-free recognizer.
    ocr: MagicOcr,
    /// Production perspective extractor, bounded for live input.
    extractor: CardExtractor,
    /// Only one frame can retain asynchronous work at a time.
    frame: Option<PendingFrame>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl RemoteBrowserEngine {
    /// Load every metadata face and one OCRacle model without acquiring reference images.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid/incomplete metadata, model/configuration or geometry settings.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new(metadata_json: &str, model: &[u8], config_json: &str) -> Result<Self, String> {
        let snapshot: MetadataSnapshot =
            serde_json::from_str(metadata_json).map_err(|e| e.to_string())?;
        if snapshot.schema_version != 1 || snapshot.catalogue.faces != snapshot.cards.len() {
            return Err("incomplete or unsupported full reference metadata snapshot".into());
        }
        let catalog = RemoteCatalog::new(snapshot.cards).map_err(|e| e.to_string())?;
        let titles = TitleIndex::from_metadata(catalog.metadata()).map_err(|e| e.to_string())?;
        let footers = FooterIndex::from_metadata(catalog.metadata()).map_err(|e| e.to_string())?;
        let ocr = MagicOcr::from_bytes(model.to_vec(), config_json.as_bytes(), "en")
            .map_err(|e| e.to_string())?;
        let extractor = CardExtractor::new(ExtractorConfig {
            output_width: 500,
            output_height: 700,
            max_cards: 4,
            ..ExtractorConfig::default()
        })
        .map_err(|e| e.to_string())?;
        Ok(Self {
            catalog,
            titles,
            footers,
            ocr,
            extractor,
            frame: None,
        })
    }

    /// Begin an actual camera frame, returning a result or selected missing reference images.
    ///
    /// `rgba` is browser-decoded input. Camera frames remain in this worker.
    /// Reference acquisition can then await network I/O while raw OCR is retained.
    ///
    /// # Errors
    ///
    /// Returns an error for an overlapping frame, invalid pixels or failed extraction/OCR/matching.
    pub fn begin_rgba(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        cropped: bool,
        classic: bool,
    ) -> Result<String, String> {
        if self.frame.is_some() {
            return Err("a camera frame is already pending".into());
        }
        let started = Instant::now();
        validate_frame(width, height, rgba.len()).map_err(|e| e.to_string())?;
        let image = DynamicImage::ImageRgba8(
            RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or("invalid RGBA frame")?,
        );
        let extraction = Instant::now();
        let crops = if cropped {
            vec![PendingCrop {
                image,
                corners: [
                    [0.0, 0.0],
                    [width as f32, 0.0],
                    [width as f32, height as f32],
                    [0.0, height as f32],
                ],
                area: width as f32 * height as f32,
            }]
        } else {
            self.extractor
                .extract(&image)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|card| PendingCrop {
                    image: DynamicImage::ImageRgb8(card.image),
                    corners: card.detection.corners.map(|p| [p.x, p.y]),
                    area: card.detection.area,
                })
                .collect()
        };
        self.frame = Some(PendingFrame {
            width,
            height,
            layout: if classic {
                FrameLayout::Classic
            } else {
                FrameLayout::Modern
            },
            crops,
            next: 0,
            work: None,
            cards: Vec::new(),
            extraction_ms: if cropped {
                0.0
            } else {
                extraction.elapsed().as_secs_f64() * 1000.0
            },
            cpu_ms: started.elapsed().as_secs_f64() * 1000.0,
            export_ms: 0.0,
        });
        self.continue_frame()
    }

    /// Encode one explicitly requested canonical image with the native reference encoder.
    ///
    /// Returns the actual image SHA-256 for cache/provenance. Snapshot digests are
    /// checked when supplied; fresh metadata can omit a previously acquired digest.
    /// Decoding and reference feature preparation are separate from frame query timings.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown/unsolicited faces, a digest mismatch or invalid image pixels.
    pub fn add_reference(
        &mut self,
        game: &str,
        id: &str,
        face_index: u32,
        image_bytes: &[u8],
    ) -> Result<String, String> {
        if self.frame.is_none() {
            return Err("no camera frame is awaiting reference images".into());
        }
        let position = self
            .catalog
            .position(game, id, face_index)
            .map_err(|e| e.to_string())?;
        let digest = Sha256::digest(image_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if self
            .catalog
            .record(position)
            .map_err(|e| e.to_string())?
            .image_sha256
            .as_ref()
            .is_some_and(|expected| !expected.eq_ignore_ascii_case(&digest))
        {
            return Err("canonical reference image SHA-256 mismatch".into());
        }
        let image = super::decode_input(image_bytes).map_err(|e| e.to_string())?;
        self.catalog
            .enroll(position, &image)
            .map_err(|e| e.to_string())?;
        Ok(digest)
    }

    /// Resume retained OCR after loading only the requested reference pixels.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing frame, OCR/matching failure or serialization failure.
    pub fn continue_frame(&mut self) -> Result<String, String> {
        self.advance()
            .and_then(|p| serde_json::to_string(&p).map_err(Error::from))
            .map_err(|e| e.to_string())
    }
}

impl RemoteBrowserEngine {
    /// Execute the shared resumable crop operation until the next fetch or complete frame.
    ///
    /// # Errors
    ///
    /// Returns extraction/OCR/matching errors; expected missing records become concrete requests.
    fn advance(&mut self) -> Result<FrameProgress, Error> {
        let mut frame = self
            .frame
            .take()
            .ok_or_else(|| Error::InvalidInput("no pending camera frame".into()))?;
        while frame.next < frame.crops.len() {
            let crop = &frame.crops[frame.next];
            let work = frame.work.get_or_insert_with(|| {
                CropIdentification::new(
                    frame.layout,
                    0,
                    crop.corners,
                    crop.area,
                    &CandidateOptions::default(),
                )
            });
            let options = MatchOptions::default();
            let started = Instant::now();
            let result = work.advance(&self.ocr, &crop.image, &mut |observations, budget| {
                identify_fields_using(
                    &self.titles,
                    &self.footers,
                    observations,
                    &options,
                    5,
                    |cards, exact| {
                        self.catalog
                            .recognize(&crop.image, &options, budget, cards, exact)
                    },
                )
            });
            frame.cpu_ms += started.elapsed().as_secs_f64() * 1000.0;
            match result {
                Err(Error::MissingReferenceImages(positions)) => {
                    let requests = positions
                        .into_iter()
                        .map(|position| self.catalog.record(position))
                        .collect::<Result<Vec<_>, _>>()?;
                    self.frame = Some(frame);
                    return Ok(FrameProgress::NeedsReferences { requests });
                }
                Err(error) => return Err(error),
                Ok(identification) => {
                    let export = Instant::now();
                    frame.cards.push(CameraCard {
                        identification,
                        crop_data_url: super::data_url(&crop.image)?,
                    });
                    frame.export_ms += export.elapsed().as_secs_f64() * 1000.0;
                    frame.work = None;
                    frame.next += 1;
                }
            }
        }
        Ok(FrameProgress::Complete {
            report: CameraReport {
                width: frame.width,
                height: frame.height,
                extraction_ms: frame.extraction_ms,
                total_ms: frame.cpu_ms,
                diagnostic_export_ms: frame.export_ms,
                cards: frame.cards,
            },
        })
    }
}
