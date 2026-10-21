//! Apply a reusable matcher to perspective-corrected cards from a scene.

use image::DynamicImage;
use magicextract::ExtractorConfig;
use serde::{Deserialize, Serialize};

use crate::{CatalogIndex, Error, MatchOptions, Matcher, Recognition, extraction::CardExtractor};

/// A detected outline with recognition evidence for the rectified card.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScannedCard {
    /// Clockwise corners in source-image pixels, starting at a short card edge.
    pub corners: [[f32; 2]; 4],
    /// Area enclosed by the outline, in source-image square pixels.
    pub area: f32,
    /// Ranked card matches and acceptance decisions.
    pub recognition: Recognition,
}

/// All detected cards in one input image, in extraction order.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScanReport {
    /// Input width in pixels.
    pub width: u32,
    /// Input height in pixels.
    pub height: u32,
    /// Detected outlines and their match evidence; empty if no card is detected.
    pub cards: Vec<ScannedCard>,
}

/// A reusable scene extractor and card matcher, independent of the game catalogue.
#[derive(Debug)]
pub struct RecognitionPipeline<M = CatalogIndex> {
    /// Validated geometric detection and rectification settings.
    extractor: CardExtractor,
    /// Visual reference matcher, which can be replaced with a custom backend.
    matcher: M,
    /// Validated retrieval and evidence thresholds.
    options: MatchOptions,
}

impl<M: Matcher> RecognitionPipeline<M> {
    /// Create a pipeline with default extraction and backend-specific matching settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the default settings fail validation.
    pub fn new(matcher: M) -> Result<Self, Error> {
        let options = matcher.default_options();
        Self::with_config(matcher, ExtractorConfig::default(), options)
    }

    /// Create a pipeline with explicit extraction and matching settings.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] for invalid thresholds, or
    /// [`Error::Extraction`] for invalid extractor settings.
    pub fn with_config(
        matcher: M,
        extractor: ExtractorConfig,
        options: MatchOptions,
    ) -> Result<Self, Error> {
        options.validate()?;
        Ok(Self {
            extractor: CardExtractor::new(extractor)?,
            matcher,
            options,
        })
    }

    /// Recognise an already cropped card without running outline detection.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for an empty image, or an error from the
    /// matcher for an unusable catalogue or failed recognition operation.
    pub fn recognize_crop(&self, image: &DynamicImage) -> Result<Recognition, Error> {
        if image.width() == 0 || image.height() == 0 {
            return Err(Error::InvalidInput(
                "the card image must not be empty".into(),
            ));
        }
        self.matcher.recognize(image, &self.options)
    }

    /// Detect, rectify, and independently recognise each card-shaped outline.
    ///
    /// The extractor supplies geometry, not a probability that the region is a
    /// card. An empty detection list is distinct from an unknown detected card.
    ///
    /// # Errors
    ///
    /// Returns an extraction error for empty or oversized images, or a matcher
    /// error if any extracted crop cannot be recognised.
    pub fn scan(&self, image: &DynamicImage) -> Result<ScanReport, Error> {
        let cards = self
            .extractor
            .extract(image)?
            .into_iter()
            .map(|card| {
                Ok(ScannedCard {
                    corners: card.detection.corners.map(|point| [point.x, point.y]),
                    area: card.detection.area,
                    recognition: self.recognize_crop(&DynamicImage::ImageRgb8(card.image))?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(ScanReport {
            width: image.width(),
            height: image.height(),
            cards,
        })
    }
}
