//! Consistent file decoding for reference images and scene inputs.

use std::path::Path;

use crate::{Error, image};

/// Decode an image file and apply its EXIF orientation before recognition.
///
/// Format detection uses the file contents. Coordinates from a subsequent scan
/// refer to the resulting oriented image, whose dimensions may be swapped.
///
/// # Errors
///
/// Returns an error for an inaccessible file, unsupported image format, invalid
/// image bytes, or unreadable orientation metadata.
pub fn open_image(path: impl AsRef<Path>) -> Result<image::DynamicImage, Error> {
    let mut decoder = image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = image::ImageDecoder::orientation(&mut decoder)?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}
