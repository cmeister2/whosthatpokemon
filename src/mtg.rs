//! Import Magic: The Gathering reference images and printing identities.
//!
//! Catalogues can be Scryfall JSON arrays, search/collection responses, current
//! JSON Lines bulk files, or local manifests containing a `cards` array. Gzip is
//! detected from its header. Importing and indexing never make network requests.

/// Portrait text regions and independent, fuzzy catalogue searches over OCR evidence.
pub mod evidence;

/// Explicit Scryfall metadata and reference-image downloads.
#[cfg(not(target_arch = "wasm32"))]
pub mod download;

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use serde_json::{Map, Value};

use crate::{CardMetadata, CatalogIndex, Error};

/// One physical card face and its local image or explicit download source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// Printing, semantic identity, and physical face represented by the image.
    pub card: CardMetadata,
    /// Explicit local image path, resolved against the catalogue's directory.
    ///
    /// Without this field, [`build_index`] searches the selected image directory
    /// using the printing ID and, for double-sided references, face index.
    pub file: Option<PathBuf>,
    /// Preferred full-card image URI supplied by the catalogue, when available.
    ///
    /// This URI is metadata only. Neither [`read_references`] nor [`build_index`]
    /// downloads it.
    pub image_url: Option<String>,
}

/// Read local catalogue metadata without loading or downloading images.
///
/// A local manifest uses entries such as
/// `{"id":"example","name":"Example","file":"images/example.png"}`.
/// Additional fields are ignored. Scryfall double-sided cards produce a
/// reference for each physical face; shared-image split/adventure cards produce
/// one reference, even when their metadata includes multiple card faces.
///
/// # Errors
///
/// Returns an error for unreadable files, malformed JSON or gzip, empty
/// catalogues, incomplete paginated responses, missing card IDs or names,
/// invalid metadata field types, or duplicate printing/face identities.
pub fn read_references(catalog_path: &Path) -> Result<Vec<Reference>, Error> {
    let file = File::open(catalog_path)?;
    let mut buffered = BufReader::new(file);
    let compressed = buffered.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    let reader: Box<dyn Read> = if compressed {
        Box::new(BufReader::new(MultiGzDecoder::new(buffered)))
    } else {
        Box::new(buffered)
    };
    let base = catalog_path.parent().unwrap_or_else(|| Path::new("."));
    let mut references = Vec::new();
    for value in serde_json::Deserializer::from_reader(reader).into_iter::<Value>() {
        let value = value.map_err(|error| {
            Error::InvalidCatalog(format!("cannot parse {}: {error}", catalog_path.display()))
        })?;
        append_value(value, base, &mut references)?;
    }
    if references.is_empty() {
        return Err(Error::InvalidCatalog("catalogue contains no cards".into()));
    }
    let mut identities = HashSet::new();
    for reference in &references {
        let identity = (&reference.card.id, reference.card.face_index);
        if !identities.insert(identity) {
            return Err(Error::InvalidCatalog(format!(
                "duplicate printing {} face {}",
                reference.card.id, reference.card.face_index
            )));
        }
    }
    Ok(references)
}

/// Build an offline visual index from a local Magic catalogue and image folder.
///
/// Explicit `file` paths are relative to the catalogue. Otherwise image files
/// are located as `<id>.<extension>` or `<id>-<face_index>.<extension>` beneath
/// `images`. Supported filename extensions are PNG, WebP, JPEG, and JPG. Face
/// zero also accepts the unsuffixed filename for single-sided catalogues.
///
/// # Errors
///
/// Returns an error for invalid catalogue metadata, a missing or undecodable
/// reference image, or an invalid reference rejected by the visual index. A
/// failure never returns a silently incomplete index.
pub fn build_index(catalog_path: &Path, images: &Path) -> Result<CatalogIndex, Error> {
    let references = read_references(catalog_path)?;
    let mut index = CatalogIndex::new();
    for reference in references {
        let path = reference_path(&reference, images)?;
        let image = crate::open_image(&path).map_err(|error| {
            Error::InvalidCatalog(format!(
                "cannot decode image for {} face {} at {}: {error}",
                reference.card.id,
                reference.card.face_index,
                path.display()
            ))
        })?;
        index.add(reference.card, &image)?;
    }
    Ok(index)
}

/// Append entries from a card, bulk array, or supported manifest wrapper.
fn append_value(value: Value, base: &Path, references: &mut Vec<Reference>) -> Result<(), Error> {
    match value {
        Value::Array(cards) => {
            for card in cards {
                append_card(card, base, references)?;
            }
        }
        Value::Object(mut object) => {
            if object.get("object").and_then(Value::as_str) == Some("error") {
                return Err(Error::InvalidCatalog(format!(
                    "Scryfall error response: {}",
                    object
                        .get("details")
                        .and_then(Value::as_str)
                        .unwrap_or("unspecified API error")
                )));
            }
            if let Some(has_more) = object.get("has_more") {
                match has_more.as_bool() {
                    Some(false) => {}
                    Some(true) => {
                        return Err(Error::InvalidCatalog(
                            "paginated response has_more is true; supply every page or a bulk catalogue"
                                .into(),
                        ));
                    }
                    None => {
                        return Err(Error::InvalidCatalog("has_more must be a boolean".into()));
                    }
                }
            }
            if object.contains_key("cards") && object.contains_key("data") {
                return Err(Error::InvalidCatalog(
                    "catalogue wrapper cannot contain both cards and data".into(),
                ));
            }
            if let Some(cards) = object.remove("cards").or_else(|| object.remove("data")) {
                let Value::Array(cards) = cards else {
                    return Err(Error::InvalidCatalog(
                        "catalogue cards/data field must be an array".into(),
                    ));
                };
                for card in cards {
                    append_card(card, base, references)?;
                }
            } else {
                append_card(Value::Object(object), base, references)?;
            }
        }
        _ => {
            return Err(Error::InvalidCatalog(
                "catalogue must contain card objects, an array, or a cards/data wrapper".into(),
            ));
        }
    }
    Ok(())
}

/// Convert one printing to either its shared image or its separate physical faces.
fn append_card(value: Value, base: &Path, references: &mut Vec<Reference>) -> Result<(), Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidCatalog("card entry must be an object".into()))?;
    let id = required_text(object, "id")?;
    let name = required_text(object, "name")?;
    if optional_text(object, "game")?.is_some_and(|game| game != "mtg") {
        return Err(Error::InvalidCatalog(format!(
            "printing {id} belongs to a game other than mtg"
        )));
    }
    let file = local_file(object, base)?;
    let preferred_url = image_url(object)?;
    let faces = match object.get("card_faces") {
        Some(Value::Array(faces)) => Some(faces),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(Error::InvalidCatalog(format!(
                "card_faces for {id} must be an array"
            )));
        }
    };
    let double_sided = optional_text(object, "layout")?.is_some_and(|layout| {
        matches!(
            layout.as_str(),
            "transform" | "modal_dfc" | "double_faced_token" | "reversible_card"
        )
    });
    let face_images = faces.is_some_and(|faces| {
        faces.iter().any(|face| {
            face.get("file").is_some_and(|file| !file.is_null())
                || face.get("image_url").is_some_and(|url| !url.is_null())
                || face
                    .get("image_uris")
                    .and_then(Value::as_object)
                    .is_some_and(|images| !images.is_empty())
        })
    });
    let separate_faces = file.is_none() && preferred_url.is_none() && (double_sided || face_images);
    if separate_faces {
        let faces = faces.filter(|faces| faces.len() >= 2).ok_or_else(|| {
            Error::InvalidCatalog(format!(
                "double-sided printing {id} needs at least two card_faces"
            ))
        })?;
        for (position, face) in faces.iter().enumerate() {
            let face = face.as_object().ok_or_else(|| {
                Error::InvalidCatalog(format!("face {position} for {id} must be an object"))
            })?;
            let face_index = u32::try_from(position)
                .map_err(|_| Error::InvalidCatalog(format!("too many faces for {id}")))?;
            references.push(Reference {
                card: metadata(object, Some(face), &id, &name, face_index)?,
                file: local_file(face, base)?,
                image_url: image_url(face)?,
            });
        }
    } else {
        let face_index = match object.get("face_index") {
            Some(value) => value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| {
                    Error::InvalidCatalog(format!("face_index for {id} must be a u32 integer"))
                })?,
            None => 0,
        };
        references.push(Reference {
            card: metadata(object, None, &id, &name, face_index)?,
            file,
            image_url: preferred_url,
        });
    }
    Ok(())
}

/// Preserve card-level identity and override face-specific fields when supplied.
fn metadata(
    object: &Map<String, Value>,
    face: Option<&Map<String, Value>>,
    id: &str,
    name: &str,
    face_index: u32,
) -> Result<CardMetadata, Error> {
    let face_text = |key: &str| -> Result<Option<String>, Error> {
        match face {
            Some(face) => optional_text(face, key),
            None => Ok(None),
        }
    };
    let face_name = face_text("name")?
        .or(optional_text(object, "face_name")?)
        .filter(|face_name| face_name != name);
    // A physical face must not borrow rules or titles from its other face.
    let physical_text = |key: &str| -> Result<Option<String>, Error> {
        if face.is_some() {
            face_text(key)
        } else {
            optional_text(object, key)
        }
    };
    Ok(CardMetadata {
        game: "mtg".into(),
        id: id.into(),
        name: name.into(),
        oracle_id: face_text("oracle_id")?.or(optional_text(object, "oracle_id")?),
        set: optional_text(object, "set")?,
        collector_number: optional_text(object, "collector_number")?,
        language: optional_text(object, "lang")?.or(optional_text(object, "language")?),
        face_index,
        face_name,
        illustration_id: face_text("illustration_id")?
            .or(optional_text(object, "illustration_id")?),
        source: optional_text(object, "source")?
            .or(optional_text(object, "scryfall_uri")?)
            .or(optional_text(object, "uri")?),
        oracle_text: physical_text("oracle_text")?,
        printed_text: physical_text("printed_text")?,
        printed_name: physical_text("printed_name")?,
        type_line: physical_text("type_line")?,
        layout: optional_text(object, "layout")?,
    })
}

/// Read a required, nonempty metadata string.
fn required_text(object: &Map<String, Value>, key: &str) -> Result<String, Error> {
    optional_text(object, key)?
        .ok_or_else(|| Error::InvalidCatalog(format!("card entry requires a nonempty {key}")))
}

/// Read a nullable metadata string while rejecting unexpected field types.
fn optional_text(object: &Map<String, Value>, key: &str) -> Result<Option<String>, Error> {
    match object.get(key) {
        Some(Value::String(value)) => {
            let value = value.trim();
            Ok((!value.is_empty()).then(|| value.to_owned()))
        }
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(Error::InvalidCatalog(format!("{key} must be a string"))),
    }
}

/// Resolve an explicitly selected image relative to the catalogue's directory.
fn local_file(object: &Map<String, Value>, base: &Path) -> Result<Option<PathBuf>, Error> {
    if let Some(Value::String(file)) = object.get("file")
        && file.trim().is_empty()
    {
        return Err(Error::InvalidCatalog("file path must not be empty".into()));
    }
    Ok(optional_text(object, "file")?.map(|file| base.join(file)))
}

/// Select a full-card image URL from explicit metadata or Scryfall image formats.
fn image_url(object: &Map<String, Value>) -> Result<Option<String>, Error> {
    if let Some(url) = optional_text(object, "image_url")? {
        return Ok(Some(url));
    }
    match object.get("image_uris") {
        Some(Value::Object(uris)) => {
            for format in [
                "png",
                "display",
                "large",
                "grid",
                "normal",
                "crop",
                "border_crop",
                "thumb",
                "small",
            ] {
                if let Some(url) = optional_text(uris, format)? {
                    return Ok(Some(url));
                }
            }
            Ok(None)
        }
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(Error::InvalidCatalog("image_uris must be an object".into())),
    }
}

/// Resolve an explicitly named reference or its printing/face filename.
///
/// # Errors
///
/// Returns [`Error::InvalidCatalog`] if the selected local reference is missing.
pub fn reference_path(reference: &Reference, images: &Path) -> Result<PathBuf, Error> {
    if let Some(file) = &reference.file {
        if file.is_file() {
            return Ok(file.clone());
        }
        return Err(Error::InvalidCatalog(format!(
            "missing image for {} face {}: {}",
            reference.card.id,
            reference.card.face_index,
            file.display()
        )));
    }
    let face_name = format!("{}-{}", reference.card.id, reference.card.face_index);
    let mut names = vec![face_name];
    if reference.card.face_index == 0 {
        names.push(reference.card.id.clone());
    }
    for name in names {
        for extension in ["png", "webp", "jpg", "jpeg"] {
            let path = images.join(format!("{name}.{extension}"));
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(Error::InvalidCatalog(format!(
        "missing local image for {} face {} under {}",
        reference.card.id,
        reference.card.face_index,
        images.display()
    )))
}

/// Catalogue format, identity, and offline image-integration checks.
#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;

    use super::*;

    /// Unique suffixes for test directories within one process.
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    /// Temporary catalogue files removed when a test finishes.
    struct Fixture {
        /// Unique directory holding this test's catalogue and images.
        directory: PathBuf,
    }

    impl Fixture {
        /// Create an isolated directory without depending on third-party test tools.
        fn new() -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "whosthatpokemon-mtg-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).expect("create fixture directory");
            Self { directory }
        }

        /// Write a catalogue and return its selected path.
        fn catalog(&self, content: &str) -> PathBuf {
            let path = self.directory.join("catalog.json");
            fs::write(&path, content).expect("write catalogue");
            path
        }

        /// Save a small textured image suitable for actual descriptor indexing.
        fn image(&self, filename: &str) {
            let image = image::RgbImage::from_fn(48, 68, |x, y| {
                image::Rgb([
                    ((x * 17 + y * 3) % 256) as u8,
                    ((x * 7 + y * 11) % 256) as u8,
                    ((x * 5 + y * 13) % 256) as u8,
                ])
            });
            image
                .save(self.directory.join(filename))
                .expect("save reference image");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    /// All supported uncompressed formats preserve the same card identities.
    #[test]
    fn imports_arrays_wrappers_single_objects_and_json_lines() {
        let fixture = Fixture::new();
        let first = json!({"id":"first","name":"First"});
        let second = json!({"id":"second","name":"Second"});
        let variants = [
            json!([first, second]).to_string(),
            json!({"data":[first,second],"has_more":false}).to_string(),
            json!({"cards":[first,second],"selection":{"truncated":true}}).to_string(),
            format!("{first}\n\n{second}\n"),
        ];
        for content in variants {
            let references = read_references(&fixture.catalog(&content)).expect("supported format");
            assert_eq!(references.len(), 2);
            assert_eq!(references[0].card.id, "first");
            assert_eq!(references[1].card.name, "Second");
        }
        let references =
            read_references(&fixture.catalog(&first.to_string())).expect("single card");
        assert_eq!(references.len(), 1);
    }

    /// Current gzip JSONL is recognized by bytes even with a non-gzip filename.
    #[test]
    fn streams_gzip_json_lines_and_rejects_corrupt_archives() {
        let fixture = Fixture::new();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(
                b"{\"id\":\"first\",\"name\":\"First\"}\n{\"id\":\"second\",\"name\":\"Second\"}\n",
            )
            .expect("compress JSONL");
        let encoded = encoder.finish().expect("finish gzip");
        let path = fixture.directory.join("bulk.unusual");
        fs::write(&path, &encoded).expect("write gzip");
        assert_eq!(read_references(&path).expect("gzip JSONL").len(), 2);
        fs::write(&path, &encoded[..encoded.len() - 5]).expect("write truncated gzip");
        assert!(read_references(&path).is_err());
    }

    /// Shared images remain one reference while physical faces retain their own IDs.
    #[test]
    fn distinguishes_shared_images_from_double_sided_faces() {
        let fixture = Fixture::new();
        let content = json!([
            {
                "id":"split","name":"Left // Right","layout":"split",
                "image_uris":{"grid":"https://example.invalid/split.webp"},
                "card_faces":[{"name":"Left"},{"name":"Right"}]
            },
            {
                "id":"double","name":"Front // Back","layout":"reversible_card",
                "set":"example","collector_number":"42a","lang":"ja",
                "scryfall_uri":"https://example.invalid/card",
                "card_faces":[
                    {"name":"Front","oracle_id":"front-oracle","illustration_id":"front-art",
                     "image_uris":{"normal":"https://example.invalid/front.jpg"}},
                    {"name":"Back","oracle_id":"back-oracle","illustration_id":"back-art",
                     "image_uris":{"png":"https://example.invalid/back.png"}}
                ]
            }
        ]);
        let references =
            read_references(&fixture.catalog(&content.to_string())).expect("face import");
        assert_eq!(references.len(), 3);
        assert_eq!(references[0].card.face_name, None);
        assert_eq!(
            references[0].image_url.as_deref(),
            Some("https://example.invalid/split.webp")
        );
        let back = &references[2];
        assert_eq!(back.card.id, "double");
        assert_eq!(back.card.face_index, 1);
        assert_eq!(back.card.face_name.as_deref(), Some("Back"));
        assert_eq!(back.card.oracle_id.as_deref(), Some("back-oracle"));
        assert_eq!(back.card.illustration_id.as_deref(), Some("back-art"));
        assert_eq!(back.card.language.as_deref(), Some("ja"));
        assert_eq!(back.card.collector_number.as_deref(), Some("42a"));
    }

    /// Text evidence remains attached to its physical face, including absent text.
    #[test]
    fn imports_text_fields_without_borrowing_another_faces_rules() {
        let fixture = Fixture::new();
        let content = json!([
            {"id":"normal","name":"Bolt","oracle_text":"Deal three damage.",
             "printed_text":"Three damage to target creature or player.",
             "printed_name":"Éclair","type_line":"Instant","layout":"normal"},
            {"id":"double","name":"Front // Back","layout":"transform",
             "oracle_text":"Whole card summary must not be inherited.",
             "card_faces":[
                 {"name":"Front","oracle_text":"Front rules.","printed_name":"Face avant"},
                 {"name":"Back","oracle_text":""}
             ]}
        ]);
        let references =
            read_references(&fixture.catalog(&content.to_string())).expect("text metadata");
        assert_eq!(
            references[0].card.oracle_text.as_deref(),
            Some("Deal three damage.")
        );
        assert_eq!(references[0].card.printed_name.as_deref(), Some("Éclair"));
        assert_eq!(references[0].card.type_line.as_deref(), Some("Instant"));
        assert_eq!(
            references[1].card.oracle_text.as_deref(),
            Some("Front rules.")
        );
        assert_eq!(
            references[1].card.printed_name.as_deref(),
            Some("Face avant")
        );
        assert_eq!(references[2].card.oracle_text, None);
        assert_eq!(references[2].card.printed_name, None);
    }

    /// Bad formats, duplicated faces, and partial search pages cannot pass as complete.
    #[test]
    fn rejects_incomplete_or_inconsistent_catalogues() {
        let fixture = Fixture::new();
        let invalid = [
            "",
            "[]",
            "{broken",
            "null",
            "{\"cards\":{}}",
            "{\"id\":\"missing-name\"}",
            "{\"id\":\"\",\"name\":\"Empty ID\"}",
            "{\"id\":\"bad\",\"name\":\"Bad\",\"oracle_id\":17}",
            "{\"id\":\"bad\",\"name\":\"Bad\",\"file\":\"\"}",
            "{\"id\":\"bad\",\"name\":\"Bad\",\"face_index\":-1}",
            "{\"id\":\"bad\",\"name\":\"Bad\",\"layout\":\"transform\"}",
            "{\"data\":[{\"id\":\"first\",\"name\":\"First\"}],\"has_more\":true}",
            "{\"object\":\"error\",\"details\":\"Not found\"}",
            "[{\"id\":\"duplicate\",\"name\":\"One\"},{\"id\":\"duplicate\",\"name\":\"Two\"}]",
            "{\"id\":\"duplicate\",\"name\":\"One\"}\n{\"id\":\"duplicate\",\"name\":\"Two\"}",
        ];
        for content in invalid {
            assert!(
                read_references(&fixture.catalog(content)).is_err(),
                "unexpected success for {content}"
            );
        }
    }

    /// Relative explicit paths and generated image filenames both index real pixels.
    #[test]
    fn builds_index_with_explicit_and_fallback_local_images() {
        let fixture = Fixture::new();
        fixture.image("explicit.png");
        fixture.image("fallback-0.png");
        let content = json!({"cards":[
            {"id":"explicit","name":"Explicit","file":"explicit.png"},
            {"id":"fallback","name":"Fallback"}
        ]});
        let path = fixture.catalog(&content.to_string());
        let references = read_references(&path).expect("local manifest");
        assert_eq!(
            references[0].file,
            Some(fixture.directory.join("explicit.png"))
        );
        let index = build_index(&path, &fixture.directory).expect("offline index");
        assert_eq!(index.len(), 2);
    }

    /// Each double-sided local reference resolves its own indexed image filename.
    #[test]
    fn indexes_both_faces_and_fails_on_any_missing_or_invalid_image() {
        let fixture = Fixture::new();
        fixture.image("double-0.png");
        fixture.image("double-1.png");
        let content = json!({
            "id":"double","name":"Front // Back","layout":"transform",
            "card_faces":[{"name":"Front"},{"name":"Back"}]
        });
        let path = fixture.catalog(&content.to_string());
        assert_eq!(
            build_index(&path, &fixture.directory)
                .expect("both faces")
                .len(),
            2
        );
        fs::remove_file(fixture.directory.join("double-1.png")).expect("remove back");
        let error = build_index(&path, &fixture.directory).expect_err("missing back must fail");
        assert!(error.to_string().contains("double face 1"));
        fs::write(fixture.directory.join("double-1.png"), b"not an image").expect("corrupt back");
        let error = build_index(&path, &fixture.directory).expect_err("invalid back must fail");
        assert!(
            error
                .to_string()
                .contains("cannot decode image for double face 1")
        );
    }
}
