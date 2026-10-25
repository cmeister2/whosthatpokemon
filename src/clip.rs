//! Optional CPU CLIP image retrieval using a user-supplied ONNX model and runtime.
//!
//! This backend never downloads models or native libraries. The model must accept
//! a self-contained export with embedded weights, accept an RGB `pixel_values`
//! tensor with shape `[1, 3, size, size]`, and return an
//! `image_embeds` float tensor with shape `[1, dimensions]`. Text encoders and
//! unpooled transformer outputs are intentionally rejected. Every persisted index
//! records the actual model SHA-256, preprocessing settings, and vector dimension.
//!
//! Scores are cosine similarities, clipped to `[0, 1]`, not probabilities.
//! Card printing and semantic identity decisions use the same evidence rules as
//! the portable matcher. Near-identical printings can remain ambiguous.

use std::{
    collections::HashSet,
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use image::{DynamicImage, RgbImage, imageops::FilterType};
use ort::{session::Session, value::Tensor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::matching::{
    finish_recognition, has_texture, same_face, validate_image, validate_metadata,
};
use crate::{CardMetadata, Error, MatchCandidate, MatchOptions, MatchStatus, Matcher, Recognition};

/// Version of the persisted CLIP index envelope.
const FORMAT_VERSION: u32 = 1;
/// Versioned contract for this backend's tensor names, normalization and scoring.
const MODEL_SCHEME: &str = "clip-vision-image-embeds-v1";
/// Maximum supported spatial tensor side, bounding accidental configuration sizes.
const MAX_IMAGE_SIZE: u32 = 2048;
/// Bound the intermediate resize before taking the model's centered square.
const MAX_RESIZED_PIXELS: u64 = 16_777_216;
/// Maximum accepted image embedding dimension.
const MAX_EMBEDDING_DIMENSIONS: usize = 65_536;
/// Tolerance allowed when verifying persisted unit-length embeddings.
const UNIT_NORM_TOLERANCE: f64 = 0.001;

/// Spatial transform applied before CLIP channel normalization.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipResize {
    /// Resize the shorter edge, then take the centered square, preserving shape.
    #[default]
    CenterCrop,
    /// Resize the entire image directly to a square, preserving every card region.
    Stretch,
}

/// Complete, persisted input transform for a CLIP-compatible vision export.
///
/// Resizing uses the image crate's Catmull-Rom bicubic filter on RGB8 pixels.
/// Inputs become channel-first floats using `(pixel / 255 - mean) / std`.
/// These details are part of the versioned index contract: settings must match
/// enrollment exactly when an index is loaded.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipPreprocessing {
    /// Height and width of the final square input tensor, from 1 through 2048.
    pub image_size: u32,
    /// Aspect-ratio and crop policy before normalization.
    pub resize: ClipResize,
    /// Mean subtracted from the red, green and blue channels.
    pub mean: [f32; 3],
    /// Positive standard deviations dividing the three channels.
    pub std: [f32; 3],
}

impl Default for ClipPreprocessing {
    fn default() -> Self {
        Self {
            image_size: 224,
            resize: ClipResize::CenterCrop,
            mean: [0.481_454_66, 0.457_827_5, 0.408_210_73],
            std: [0.268_629_54, 0.261_302_6, 0.275_777_1],
        }
    }
}

impl ClipPreprocessing {
    /// Validate finite normalization parameters and bounded input dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] for an invalid image size, nonfinite
    /// channel mean, or nonpositive/nonfinite standard deviation.
    pub fn validate(&self) -> Result<(), Error> {
        if self.image_size == 0 || self.image_size > MAX_IMAGE_SIZE {
            return Err(Error::InvalidOptions(format!(
                "CLIP image_size must be between 1 and {MAX_IMAGE_SIZE}"
            )));
        }
        if self.mean.iter().any(|value| !value.is_finite())
            || self
                .std
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return Err(Error::InvalidOptions(
                "CLIP means must be finite and standard deviations finite and positive".into(),
            ));
        }
        Ok(())
    }
}

/// Identity of the exact encoder and input transform used by a saved index.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipEncoderMetadata {
    /// Versioned tensor and scoring contract; checkpoint identity is its SHA-256.
    pub model_id: String,
    /// Lowercase SHA-256 of the supplied ONNX file's complete bytes.
    pub model_sha256: String,
    /// Exact RGB resizing and normalization configuration used for enrollment.
    pub preprocessing: ClipPreprocessing,
    /// Length of each normalized `image_embeds` vector.
    pub dimensions: usize,
}

/// A shared CPU inference session and its immutable compatibility identity.
struct EncoderInner {
    /// Model hash and preprocessing contract checked against persisted indices.
    metadata: ClipEncoderMetadata,
    /// ONNX Runtime requires mutable session access for inference.
    session: Mutex<Session>,
}

/// A CPU CLIP vision encoder loaded from explicit local model/runtime paths.
///
/// Clones share one serialized inference session. ONNX Runtime configuration is
/// process-global; its first initialization selects the native runtime. The
/// caller should load this encoder before initializing ONNX Runtime elsewhere.
#[derive(Clone)]
pub struct ClipEncoder {
    /// Shared ONNX session; no source images are retained by the encoder.
    inner: Arc<EncoderInner>,
}

impl std::fmt::Debug for ClipEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipEncoder")
            .field("metadata", self.metadata())
            .finish_non_exhaustive()
    }
}

impl ClipEncoder {
    /// Load a local CLIP vision ONNX export using a local ONNX Runtime library.
    ///
    /// No model, runtime or reference downloads occur. Only the CPU execution
    /// provider is requested, with one intra-operation thread. The model is
    /// probed once to verify its input/output contract and embedding dimension.
    /// Runtime telemetry is disabled when this call initializes the environment.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid preprocessing, unreadable paths, an
    /// incompatible native runtime or ONNX model, missing tensor names, invalid
    /// output dimensions, or nonfinite/zero model output.
    pub fn load(
        model_path: impl AsRef<Path>,
        runtime_path: impl AsRef<Path>,
        preprocessing: ClipPreprocessing,
    ) -> Result<Self, Error> {
        preprocessing.validate()?;
        let model_path = model_path.as_ref();
        let runtime_path = runtime_path.as_ref();
        // Load and hash the exact bytes used to create the session, rather than
        // hashing a file that could change before a later runtime read.
        let model_bytes = std::fs::read(model_path)?;
        let digest = model_digest(&model_bytes);
        // Check the explicit path before interacting with the process-global API.
        let _runtime = File::open(runtime_path)?;
        let environment = ort::init_from(runtime_path)
            .map_err(backend_error)?
            .with_name("whosthatpokemon")
            .with_telemetry(false)
            .build()
            .map_err(backend_error)?;
        let mut session = Session::builder(&environment)
            .map_err(backend_error)?
            .with_intra_threads(1)
            .map_err(backend_error)?
            .with_execution_providers([ort::ep::CPU::default().build()])
            .map_err(backend_error)?
            .commit_from_memory(&model_bytes)
            .map_err(backend_error)?;
        let probe =
            DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([127, 127, 127])));
        let embedding = run_encoder(&mut session, &preprocessing, &probe)?;
        let metadata = ClipEncoderMetadata {
            model_id: MODEL_SCHEME.into(),
            model_sha256: digest,
            preprocessing,
            dimensions: embedding.len(),
        };
        Ok(Self {
            inner: Arc::new(EncoderInner {
                metadata,
                session: Mutex::new(session),
            }),
        })
    }

    /// Return the immutable model hash, input transform and embedding dimension.
    #[must_use]
    pub fn metadata(&self) -> &ClipEncoderMetadata {
        &self.inner.metadata
    }

    /// Encode one cropped image as a unit-length image embedding.
    ///
    /// This method does not choose an orientation. [`ClipIndex`] searches four
    /// query orientations when it implements [`Matcher`].
    ///
    /// # Errors
    ///
    /// Returns an error for an empty image, poisoned session lock, model runtime
    /// failure, invalid model output, or an unexpected embedding dimension.
    pub fn encode(&self, image: &DynamicImage) -> Result<Vec<f32>, Error> {
        validate_image(image)?;
        let mut session = self.inner.session.lock().map_err(|_| {
            Error::Backend("CLIP inference session is unavailable after a thread failure".into())
        })?;
        let embedding = run_encoder(&mut session, &self.inner.metadata.preprocessing, image)?;
        if embedding.len() != self.inner.metadata.dimensions {
            return Err(Error::Backend(
                "CLIP embedding dimension changed during inference".into(),
            ));
        }
        Ok(embedding)
    }
}

/// One reference face and its normalized image embedding.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClipReference {
    /// Game-independent printing, face and semantic identity metadata.
    card: CardMetadata,
    /// Finite, unit-length embedding from the declared encoder.
    embedding: Vec<f32>,
}

/// Versioned on-disk CLIP index, independent of runtime library locations.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredClipIndex {
    /// Version of the serialized envelope.
    format_version: u32,
    /// Exact encoder/preprocessing identity required to query these vectors.
    encoder: ClipEncoderMetadata,
    /// Enrolled metadata and compact visual descriptors.
    references: Vec<ClipReference>,
}

impl StoredClipIndex {
    /// Validate the full index against the actual loaded encoder.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidIndex`] for incompatible encoder metadata, invalid
    /// version, duplicate faces, missing metadata or invalid embedding vectors.
    fn validate(&self, encoder: &ClipEncoderMetadata) -> Result<(), Error> {
        if self.format_version != FORMAT_VERSION {
            return Err(Error::InvalidIndex(
                "unsupported CLIP index format version".into(),
            ));
        }
        if &self.encoder != encoder {
            return Err(Error::InvalidIndex(
                "CLIP model hash, preprocessing, contract or dimensions differ from the index"
                    .into(),
            ));
        }
        let mut keys = HashSet::new();
        for reference in &self.references {
            validate_metadata(&reference.card).map_err(Error::InvalidIndex)?;
            let key = (
                &reference.card.game,
                &reference.card.id,
                reference.card.face_index,
            );
            if !keys.insert(key) {
                return Err(Error::InvalidIndex("duplicate CLIP reference face".into()));
            }
            validate_embedding(&reference.embedding, encoder.dimensions)?;
        }
        Ok(())
    }
}

/// A persisted CLIP reference gallery usable by [`crate::RecognitionPipeline`].
///
/// Enroll tight, upright faces. Query matching considers four right-angle
/// rotations and compares every reference before truncating ranked results.
/// The exact-print decision remains distinct from grouped Oracle identity.
#[derive(Clone, Debug)]
pub struct ClipIndex {
    /// Shared image encoder matching every stored embedding's contract.
    encoder: ClipEncoder,
    /// Unit embeddings and card metadata, preserving stable enrollment order.
    references: Vec<ClipReference>,
}

impl ClipIndex {
    /// Create an empty index using the supplied local encoder.
    #[must_use]
    pub fn new(encoder: ClipEncoder) -> Self {
        Self {
            encoder,
            references: Vec::new(),
        }
    }

    /// Return the number of enrolled physical faces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.references.len()
    }

    /// Return whether no reference faces are enrolled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.references.is_empty()
    }

    /// Return the exact encoder identity associated with this gallery.
    #[must_use]
    pub fn encoder_metadata(&self) -> &ClipEncoderMetadata {
        self.encoder.metadata()
    }

    /// Enroll an upright reference face using actual model inference.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or duplicate face metadata, an empty or
    /// nearly uniform image, or failed/incompatible model inference.
    pub fn add(&mut self, card: CardMetadata, image: &DynamicImage) -> Result<(), Error> {
        validate_metadata(&card).map_err(Error::InvalidCatalog)?;
        if self
            .references
            .iter()
            .any(|entry| same_face(&entry.card, &card))
        {
            return Err(Error::InvalidCatalog(
                "duplicate CLIP reference face".into(),
            ));
        }
        validate_image(image)?;
        if !has_texture(image) {
            return Err(Error::InvalidInput(
                "CLIP reference has insufficient luminance variation".into(),
            ));
        }
        let embedding = self.encoder.encode(image)?;
        self.references.push(ClipReference { card, embedding });
        Ok(())
    }

    /// Save embeddings, metadata and the encoder compatibility contract to JSON.
    ///
    /// Source pixels and model/runtime paths are not persisted. Existing files at
    /// `path` are replaced.
    ///
    /// # Errors
    ///
    /// Returns an error for an unwritable destination or failed serialization.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let stored = StoredClipIndex {
            format_version: FORMAT_VERSION,
            encoder: self.encoder.metadata().clone(),
            references: self.references.clone(),
        };
        let mut writer = BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut writer, &stored)?;
        writer.flush()?;
        Ok(())
    }

    /// Load an index and require an exactly compatible actual encoder.
    ///
    /// Matching model names or vector lengths alone are insufficient: the model
    /// SHA-256 and complete input transform must also match. Every persisted
    /// vector and face key is validated before the gallery becomes usable.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable files, invalid JSON, an unsupported format,
    /// encoder mismatch, malformed embeddings or inconsistent reference metadata.
    pub fn load(path: impl AsRef<Path>, encoder: ClipEncoder) -> Result<Self, Error> {
        let stored = read_stored_index(path.as_ref())?;
        stored.validate(encoder.metadata())?;
        Ok(Self {
            encoder,
            references: stored.references,
        })
    }

    /// Inspect the persisted input transform before loading a compatible encoder.
    ///
    /// This does not establish that the stored model hash is trustworthy; loading
    /// the index with [`Self::load`] still verifies it against actual model bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable or invalid JSON and unsupported versions.
    pub fn read_encoder_metadata(path: impl AsRef<Path>) -> Result<ClipEncoderMetadata, Error> {
        let stored = read_stored_index(path.as_ref())?;
        if stored.format_version != FORMAT_VERSION || stored.encoder.model_id != MODEL_SCHEME {
            return Err(Error::InvalidIndex(
                "unsupported CLIP encoder/index contract".into(),
            ));
        }
        stored
            .encoder
            .preprocessing
            .validate()
            .map_err(|error| Error::InvalidIndex(error.to_string()))?;
        Ok(stored.encoder)
    }

    /// Rank references for a cropped face, including four query orientations.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options, an empty image or gallery, or a model
    /// inference failure. Uniform images return an unknown result without inference.
    pub fn recognize(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error> {
        <Self as Matcher>::recognize(self, image, options)
    }
}

impl Matcher for ClipIndex {
    fn default_options(&self) -> MatchOptions {
        MatchOptions {
            top_k: 5,
            min_similarity: 0.90,
            min_margin: 0.04,
        }
    }

    fn recognize(
        &self,
        image: &DynamicImage,
        options: &MatchOptions,
    ) -> Result<Recognition, Error> {
        options.validate()?;
        validate_image(image)?;
        if self.references.is_empty() {
            return Err(Error::InvalidCatalog(
                "CLIP reference index is empty".into(),
            ));
        }
        if !has_texture(image) {
            return Ok(Recognition {
                status: MatchStatus::Unknown,
                identity_status: MatchStatus::Unknown,
                confidence: 0.0,
                identity_confidence: 0.0,
                candidates: Vec::new(),
            });
        }
        let rgb = image.to_rgb8();
        let orientations = [
            rgb.clone(),
            image::imageops::rotate90(&rgb),
            image::imageops::rotate180(&rgb),
            image::imageops::rotate270(&rgb),
        ];
        let queries = orientations
            .iter()
            .map(|rgb| self.encoder.encode(&DynamicImage::ImageRgb8(rgb.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        let candidates = self
            .references
            .iter()
            .map(|reference| {
                let mut similarity = 0.0_f32;
                let mut rotation_degrees = 0;
                for (rotation, query) in queries.iter().enumerate() {
                    let score = query
                        .iter()
                        .zip(&reference.embedding)
                        .map(|(left, right)| left * right)
                        .sum::<f32>()
                        .clamp(0.0, 1.0);
                    if score > similarity {
                        similarity = score;
                        rotation_degrees = rotation as u16 * 90;
                    }
                }
                MatchCandidate {
                    card: reference.card.clone(),
                    similarity,
                    confidence: 0.0,
                    rotation_degrees,
                }
            })
            .collect();
        finish_recognition(candidates, options)
    }
}

/// Map a runtime failure into the backend-independent error contract.
fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("CLIP ONNX Runtime: {error}"))
}

/// Read and decode the CLIP-specific index envelope.
fn read_stored_index(path: &Path) -> Result<StoredClipIndex, Error> {
    serde_json::from_reader(BufReader::new(File::open(path)?))
        .map_err(|error| Error::InvalidIndex(format!("could not decode CLIP index: {error}")))
}

/// Hash the self-contained model bytes actually passed to ONNX Runtime.
fn model_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Build and execute one `[1, 3, height, width]` tensor, normalizing its embedding.
fn run_encoder(
    session: &mut Session,
    preprocessing: &ClipPreprocessing,
    image: &DynamicImage,
) -> Result<Vec<f32>, Error> {
    let values = input_values(image, preprocessing)?;
    let side = preprocessing.image_size as usize;
    let tensor = Tensor::from_array(([1_usize, 3, side, side], values.into_boxed_slice()))
        .map_err(backend_error)?;
    let outputs = session
        .run(ort::inputs!["pixel_values" => tensor])
        .map_err(backend_error)?;
    let output = outputs
        .get("image_embeds")
        .ok_or_else(|| Error::Backend("CLIP model must expose an image_embeds output".into()))?;
    let (shape, values) = output.try_extract_tensor::<f32>().map_err(backend_error)?;
    if shape.len() != 2
        || shape[0] != 1
        || values.len() < 2
        || values.len() > MAX_EMBEDDING_DIMENSIONS
    {
        return Err(Error::Backend(
            "CLIP image_embeds must have shape [1, dimensions] with 2..65536 dimensions".into(),
        ));
    }
    normalized_embedding(values)
}

/// Convert a valid image to normalized channel-first pixels with bounded memory.
fn input_values(
    image: &DynamicImage,
    preprocessing: &ClipPreprocessing,
) -> Result<Vec<f32>, Error> {
    let side = preprocessing.image_size;
    let rgb = image.to_rgb8();
    let square = match preprocessing.resize {
        ClipResize::Stretch => image::imageops::resize(&rgb, side, side, FilterType::CatmullRom),
        ClipResize::CenterCrop => {
            let short = rgb.width().min(rgb.height());
            let width = u64::from(rgb.width()) * u64::from(side) / u64::from(short);
            let height = u64::from(rgb.height()) * u64::from(side) / u64::from(short);
            if width * height > MAX_RESIZED_PIXELS {
                return Err(Error::InvalidInput(
                    "CLIP image aspect ratio requires an excessive intermediate resize".into(),
                ));
            }
            let resized =
                image::imageops::resize(&rgb, width as u32, height as u32, FilterType::CatmullRom);
            let left = (resized.width() - side) / 2;
            let top = (resized.height() - side) / 2;
            image::imageops::crop_imm(&resized, left, top, side, side).to_image()
        }
    };
    let pixels = (side * side) as usize;
    let mut values = vec![0.0; 3 * pixels];
    for (index, pixel) in square.pixels().enumerate() {
        for channel in 0..3 {
            values[channel * pixels + index] = (f32::from(pixel[channel]) / 255.0
                - preprocessing.mean[channel])
                / preprocessing.std[channel];
        }
    }
    Ok(values)
}

/// Normalize a runtime-produced finite, nonzero vector using double precision.
fn normalized_embedding(values: &[f32]) -> Result<Vec<f32>, Error> {
    if values.iter().any(|value| !value.is_finite()) {
        return Err(Error::Backend(
            "CLIP embedding contains nonfinite values".into(),
        ));
    }
    let norm = values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= f64::EPSILON {
        return Err(Error::Backend(
            "CLIP embedding has no finite nonzero norm".into(),
        ));
    }
    Ok(values
        .iter()
        .map(|value| (f64::from(*value) / norm) as f32)
        .collect())
}

/// Verify a persisted embedding without silently repairing corrupt index values.
fn validate_embedding(values: &[f32], dimensions: usize) -> Result<(), Error> {
    if !(2..=MAX_EMBEDDING_DIMENSIONS).contains(&dimensions)
        || values.len() != dimensions
        || values.iter().any(|value| !value.is_finite())
    {
        return Err(Error::InvalidIndex(
            "invalid CLIP embedding dimensions or values".into(),
        ));
    }
    let norm = values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if (norm - 1.0).abs() > UNIT_NORM_TOLERANCE {
        return Err(Error::InvalidIndex(
            "CLIP index embedding must have unit length".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Contract and image-input tests that need no native runtime or downloaded model.

    use super::*;
    use image::Rgb;

    /// Make a small valid encoder identity for persisted-vector contract tests.
    fn metadata() -> ClipEncoderMetadata {
        ClipEncoderMetadata {
            model_id: MODEL_SCHEME.into(),
            model_sha256: "a".repeat(64),
            preprocessing: ClipPreprocessing::default(),
            dimensions: 3,
        }
    }

    /// Build one valid stored reference independent of any optional native library.
    fn stored() -> StoredClipIndex {
        StoredClipIndex {
            format_version: FORMAT_VERSION,
            encoder: metadata(),
            references: vec![ClipReference {
                card: CardMetadata {
                    game: "test".into(),
                    id: "printing".into(),
                    name: "Example".into(),
                    ..CardMetadata::default()
                },
                embedding: vec![0.6, 0.8, 0.0],
            }],
        }
    }

    /// Pixel normalization is channel-first, and center crops omit exterior borders.
    #[test]
    fn input_transform_preserves_contract_and_distinguishes_crop_policies() {
        let mut image = RgbImage::from_pixel(6, 2, Rgb([255, 0, 0]));
        for y in 0..2 {
            for x in 2..4 {
                image.put_pixel(x, y, Rgb([0, 255, 128]));
            }
        }
        let mut preprocessing = ClipPreprocessing {
            image_size: 2,
            mean: [0.0; 3],
            std: [1.0; 3],
            ..ClipPreprocessing::default()
        };
        let values = input_values(&DynamicImage::ImageRgb8(image.clone()), &preprocessing).unwrap();
        assert_eq!(&values[..4], &[0.0; 4]);
        assert_eq!(&values[4..8], &[1.0; 4]);
        assert!(
            values[8..]
                .iter()
                .all(|value| (*value - 128.0 / 255.0).abs() < 0.0001)
        );
        preprocessing.resize = ClipResize::Stretch;
        assert_ne!(
            values,
            input_values(&DynamicImage::ImageRgb8(image), &preprocessing).unwrap()
        );
    }

    /// Index compatibility depends on weights and transforms, not just vector length.
    #[test]
    fn persisted_index_rejects_different_model_or_preprocessing() {
        let stored = stored();
        assert!(stored.validate(&metadata()).is_ok());
        let mut different = metadata();
        different.model_sha256 = "b".repeat(64);
        assert!(stored.validate(&different).is_err());
        different = metadata();
        different.preprocessing.resize = ClipResize::Stretch;
        assert!(stored.validate(&different).is_err());
        different = metadata();
        different.dimensions = 4;
        assert!(stored.validate(&different).is_err());
    }

    /// Corrupt embeddings and duplicate physical faces fail closed when loading.
    #[test]
    fn persisted_index_rejects_nonunit_nonfinite_and_duplicate_vectors() {
        for embedding in [
            vec![0.0; 3],
            vec![1.0; 3],
            vec![f32::NAN, 0.0, 1.0],
            vec![1.0, 0.0],
        ] {
            let mut stored = stored();
            stored.references[0].embedding = embedding;
            assert!(stored.validate(&metadata()).is_err());
        }
        let mut stored = stored();
        stored.references.push(stored.references[0].clone());
        assert!(stored.validate(&metadata()).is_err());
    }

    /// Runtime normalization preserves cosine geometry and rejects unusable output.
    #[test]
    fn normalizes_real_vectors_without_accepting_empty_evidence() {
        assert_eq!(normalized_embedding(&[3.0, 4.0]).unwrap(), [0.6, 0.8]);
        assert!(normalized_embedding(&[0.0, 0.0]).is_err());
        assert!(normalized_embedding(&[f32::INFINITY, 1.0]).is_err());
    }

    /// Invalid configurations fail before native runtime initialization or allocation.
    #[test]
    fn rejects_invalid_preprocessing_without_loading_a_runtime() {
        for side in [0, MAX_IMAGE_SIZE + 1] {
            let invalid = ClipPreprocessing {
                image_size: side,
                ..ClipPreprocessing::default()
            };
            assert!(invalid.validate().is_err());
        }
        let invalid = ClipPreprocessing {
            std: [1.0, 0.0, 1.0],
            ..ClipPreprocessing::default()
        };
        assert!(invalid.validate().is_err());
        let oversized = ClipPreprocessing {
            image_size: MAX_IMAGE_SIZE,
            ..ClipPreprocessing::default()
        };
        let elongated = DynamicImage::ImageRgb8(RgbImage::new(20, 1));
        assert!(input_values(&elongated, &oversized).is_err());
    }

    /// The stored checksum is the standard lowercase SHA-256 of all model bytes.
    #[test]
    fn model_identity_hashes_exact_bytes_with_leading_zeroes() {
        assert_eq!(
            model_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
