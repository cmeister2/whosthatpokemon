//! Real image-decoder behavior, including camera orientation metadata.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use whosthatpokemon::{
    image::{self, Rgb, RgbImage},
    open_image,
};

/// Unique input paths for parallel decoder tests.
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// Remove an isolated image input when its test finishes.
struct TemporaryImage(PathBuf);

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Honor EXIF rotation consistently even when the filename has no image extension.
#[test]
fn reads_format_from_bytes_and_applies_camera_orientation() {
    let image = RgbImage::from_fn(32, 48, |x, y| Rgb([(x * 7) as u8, (y * 5) as u8, 85]));
    let mut original = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut original, 95)
        .encode_image(&image)
        .unwrap();
    let raw = image::load_from_memory(&original).unwrap().to_rgb8();
    // Original minimal TIFF/EXIF payload: one orientation SHORT with value 6.
    let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
    bytes.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    bytes.extend_from_slice(exif);
    bytes.extend_from_slice(&original[2..]);
    let serial = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let temporary = TemporaryImage(std::env::temp_dir().join(format!(
        "whosthatpokemon-orientation-{}-{serial}.input",
        std::process::id()
    )));
    fs::write(&temporary.0, bytes).unwrap();
    let decoded = open_image(&temporary.0).unwrap().to_rgb8();
    assert_eq!(decoded.dimensions(), (48, 32));
    assert_eq!(decoded, image::imageops::rotate90(&raw));
}
