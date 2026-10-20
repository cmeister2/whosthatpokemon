//! Camera frame bounds and native-shaped actual recognition reports.

use crate::{Error, mtg::recognition::IdentifiedCard};
use serde::Serialize;

/// Maximum frame edge accepted after camera preview downsampling.
const MAX_FRAME_EDGE: u32 = 1280;
/// Maximum decoded camera pixels accepted before copying into image storage.
const MAX_FRAME_PIXELS: u64 = 1_638_400;

/// One actual browser result, with its rectified image encoded after recognition timing.
#[derive(Serialize)]
pub(super) struct CameraCard {
    /// Unchanged shared native/browser recognition result.
    #[serde(flatten)]
    pub(super) identification: IdentifiedCard,
    /// Real rectified input pixels for display, without metadata substitution.
    pub(super) crop_data_url: String,
}

/// Actual elapsed browser processing and card evidence for one decoded frame.
#[derive(Serialize)]
pub(super) struct CameraReport {
    /// Decoded frame width supplied by the browser.
    pub(super) width: u32,
    /// Decoded frame height supplied by the browser.
    pub(super) height: u32,
    /// Detection and perspective rectification wall time in milliseconds.
    pub(super) extraction_ms: f64,
    /// Extraction, OCR and identification wall time, excluding preview PNG export.
    pub(super) total_ms: f64,
    /// Actual PNG serialization time, reported separately from recognition.
    pub(super) diagnostic_export_ms: f64,
    /// At most four detected card regions and their original evidence.
    pub(super) cards: Vec<CameraCard>,
}

/// Reject malformed and oversized frames before any pixel copy or model inference.
///
/// # Errors
///
/// Returns an invalid-input error unless dimensions and the complete RGBA length agree.
pub(super) fn validate_frame(width: u32, height: u32, bytes: usize) -> Result<(), Error> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_FRAME_EDGE
        || height > MAX_FRAME_EDGE
        || pixels > MAX_FRAME_PIXELS
        || pixels * 4 != bytes as u64
    {
        return Err(Error::InvalidInput(
            "camera frames require matching RGBA pixels and dimensions in 1..=1280".into(),
        ));
    }
    Ok(())
}

/// Frame bounds protect browser memory independently of expensive OCR/model tests.
#[cfg(test)]
mod tests {
    use super::validate_frame;

    /// Partial, excess and oversized transfers cannot enter inference or image allocation.
    #[test]
    fn rejects_malformed_and_oversized_camera_frames() {
        assert!(validate_frame(1280, 720, 1280 * 720 * 4).is_ok());
        assert!(validate_frame(0, 1, 0).is_err());
        assert!(validate_frame(1, 1, 3).is_err());
        assert!(validate_frame(1, 1, 5).is_err());
        assert!(validate_frame(1281, 1, 1281 * 4).is_err());
        assert!(validate_frame(u32::MAX, u32::MAX, 0).is_err());
    }
}
