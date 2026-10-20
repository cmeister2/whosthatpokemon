//! Context-preserving errors for reference input and recognition.

/// An input, configuration, catalogue, or recognition failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A catalogue, image, or index file could not be accessed.
    #[error("file operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// Image bytes could not be decoded or encoded.
    #[error("image operation failed: {0}")]
    Image(#[from] image::ImageError),
    /// Metadata or index JSON could not be decoded or encoded.
    #[error("JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    /// The extraction library rejected its input or configuration.
    #[error("card extraction failed: {0}")]
    Extraction(#[from] magicextract::Error),
    /// A reference catalogue is incomplete, empty, or inconsistent.
    #[error("invalid catalogue: {0}")]
    InvalidCatalog(String),
    /// A persisted index is unsupported, incomplete, or corrupt.
    #[error("invalid index: {0}")]
    InvalidIndex(String),
    /// An input image or requested operation is unsupported.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Matching settings are invalid or nonfinite.
    #[error("invalid matching options: {0}")]
    InvalidOptions(String),
    /// An explicit reference download failed.
    #[error("download failed: {0}")]
    Download(String),
    /// Selected candidate images must be supplied before sparse browser matching can resume.
    #[cfg(feature = "web")]
    #[error("selected reference images are not loaded")]
    MissingReferenceImages(Vec<usize>),
    /// A configured inference backend could not load or run its model.
    #[error("recognition backend failed: {0}")]
    Backend(String),
}
