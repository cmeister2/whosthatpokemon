//! Recognise trading cards in images using perspective correction and visual references.
//!
//! [`RecognitionPipeline`] detects and rectifies cards with `magicextract`, then
//! returns ranked visual matches. [`mtg`] imports Magic: The Gathering references.
//! Confidence scores describe visual evidence; they are not calibrated probabilities.

/// Browser-compatible diagnostics exposing actual extraction crops and visual evidence.
#[cfg(feature = "web")]
pub mod browser;

/// Monotonic elapsed timing on native targets and in the browser.
#[cfg(any(not(target_arch = "wasm32"), feature = "web"))]
mod clock;

/// Optional native CPU CLIP embeddings for difficult illumination or alignment.
#[cfg(all(feature = "clip", not(target_arch = "wasm32")))]
pub mod clip;
/// Errors shared by cataloguing, recognition, and image input.
mod error;
/// Shared source-validated card outlines for native and browser extraction.
pub mod extraction;
/// File decoding and EXIF orientation shared by references and scan inputs.
mod input;
/// Camera-friendly recognition scheduling and stable-card result reuse.
pub mod live;
/// Visual reference indexing and matching.
pub mod matching;
/// Magic: The Gathering catalogue ingestion and card identity metadata.
pub mod mtg;
/// Card extraction and recognition in a complete image.
mod pipeline;
/// Game-independent metadata and serializable recognition results.
mod types;

pub use error::Error;
pub use input::open_image;
pub use magicextract::{ExtractorConfig, image};
pub use matching::CatalogIndex;
pub use pipeline::{RecognitionPipeline, ScanReport, ScannedCard};
pub use types::{CardMetadata, MatchCandidate, MatchOptions, MatchStatus, Recognition};

/// An image matcher usable by the extraction pipeline, including custom backends.
pub trait Matcher {
    /// Return conservative defaults appropriate to this backend's score scale.
    fn default_options(&self) -> MatchOptions {
        MatchOptions::default()
    }

    /// Rank references for a single, already cropped card image.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty image, invalid matching options, an invalid
    /// or empty catalogue, or a backend-specific recognition failure.
    fn recognize(
        &self,
        image: &image::DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error>;
}

impl<T: Matcher + ?Sized> Matcher for Box<T> {
    fn default_options(&self) -> MatchOptions {
        (**self).default_options()
    }

    fn recognize(
        &self,
        image: &image::DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error> {
        (**self).recognize(image, options)
    }
}
