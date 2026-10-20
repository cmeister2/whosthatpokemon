//! Streaming, resumable preparation of complete Scryfall catalogues.
//!
//! Metadata and image acquisition are explicit operations. Preparing metadata
//! never downloads images; image batches retain an honest remaining-face count.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fmt,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use flate2::read::MultiGzDecoder;
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};

use crate::{CardMetadata, Error};

/// Scryfall card exports, ordered from complete language coverage to smaller galleries.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BulkKind {
    /// Every printing in every language.
    AllCards,
    /// English printings plus cards that exist only in another language.
    DefaultCards,
    /// One representative printing per Oracle identity.
    OracleCards,
    /// Representative printings covering every distinct illustration.
    UniqueArtwork,
}

impl BulkKind {
    /// Return the exact Scryfall bulk type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllCards => "all_cards",
            Self::DefaultCards => "default_cards",
            Self::OracleCards => "oracle_cards",
            Self::UniqueArtwork => "unique_artwork",
        }
    }
}

/// Provenance and validated size of a completed metadata download.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BulkDownloadReport {
    /// Local gzipped JSONL or legacy JSON catalogue.
    pub catalog: PathBuf,
    /// Selected Scryfall export.
    pub kind: BulkKind,
    /// Timestamp supplied by Scryfall for this immutable snapshot.
    pub updated_at: String,
    /// Snapshot download URL; contains no observed/private card data.
    pub download_uri: String,
    /// Expected and verified compressed file length.
    pub compressed_bytes: u64,
    /// Printing records successfully parsed before publication.
    pub cards: usize,
    /// Whether the same completed local snapshot was reused.
    pub cached: bool,
}

/// Metadata/image separation and explicit catalogue coverage.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparationReport {
    /// Original local source, which can include every language.
    pub source: PathBuf,
    /// All source card records, streamed into uncompressed JSON Lines.
    pub metadata: PathBuf,
    /// Selected physical reference faces accepted by offline index builders.
    pub image_plan: PathBuf,
    /// Persisted coverage report.
    pub coverage_file: PathBuf,
    /// Counts covering the entire source and selected visual gallery.
    pub coverage: Coverage,
}

/// Coverage counts distinguish metadata availability from enrolled image availability.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Coverage {
    /// Number of source printing records, across every language.
    pub metadata_printings: usize,
    /// Number of physical faces described by source records.
    pub metadata_faces: usize,
    /// Distinct Oracle identities in the complete metadata.
    pub oracle_identities: usize,
    /// Printing counts grouped by Scryfall language code.
    pub languages: BTreeMap<String, usize>,
    /// Exact language selected for reference images.
    pub image_language: String,
    /// Printings with at least one selected downloadable face.
    pub selected_printings: usize,
    /// Physical faces in the image plan; not necessarily downloaded yet.
    pub selected_faces: usize,
    /// Distinct Oracle identities represented by the selected image plan.
    pub selected_oracle_identities: usize,
    /// Metadata Oracle identities lacking a selected image, including foreign-only cards.
    pub oracle_identities_without_selected_images: usize,
    /// Faces excluded by the exact image-language policy.
    pub excluded_language_faces: usize,
    /// Faces excluded because their card is not available in paper.
    pub excluded_nonpaper_faces: usize,
    /// Otherwise eligible faces lacking a supported full-card image URL.
    pub missing_image_faces: usize,
    /// Otherwise eligible faces represented only by Scryfall placeholder imagery.
    pub placeholder_image_faces: usize,
}

/// Progress from a bounded image batch or an entirely offline image audit.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImageDownloadReport {
    /// Complete selected physical-face catalogue.
    pub plan: PathBuf,
    /// Directory containing reusable `<printing>-<face>.jpg` images.
    pub images: PathBuf,
    /// Number of physical faces in the complete plan.
    pub total_faces: usize,
    /// Existing files whose pixels were successfully decoded.
    pub cached_faces: usize,
    /// New or repaired images downloaded during this invocation.
    pub downloaded_faces: usize,
    /// Faces not yet validated locally, or still unavailable after the bounded batch.
    pub remaining_faces: usize,
    /// True only when every planned face has a decodable local image.
    pub complete: bool,
    /// Network response bytes for image downloads in this invocation.
    pub bytes_downloaded: u64,
}

/// Limits for explicit static-CDN image acquisition.
#[derive(Clone, Copy, Debug)]
pub struct ImageDownloadOptions {
    /// Maximum missing images attempted by this invocation; zero is an offline audit.
    pub max_downloads: usize,
    /// Concurrent cache audits and CDN workers, from one to sixteen.
    ///
    /// Each CDN worker spaces request starts by 110 ms.
    pub workers: usize,
}

impl Default for ImageDownloadOptions {
    fn default() -> Self {
        Self {
            max_downloads: 100,
            workers: 4,
        }
    }
}

/// Convert a local plain/gzip JSONL, array, or cards/data wrapper one record at a time.
///
/// Memory usage is bounded by one card record and wrapper metadata. Callback
/// errors are preserved. Incomplete paginated wrappers fail, even when their
/// card array appeared before `has_more` in the input.
///
/// # Errors
///
/// Returns an error for unreadable, empty, malformed or incomplete catalogues,
/// gzip failures, invalid card containers, or a callback failure.
pub fn for_each_card(
    path: &Path,
    mut callback: impl FnMut(Value) -> Result<(), Error>,
) -> Result<usize, Error> {
    let mut original = BufReader::new(File::open(path)?);
    let compressed = original.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    let reader: Box<dyn Read> = if compressed {
        Box::new(MultiGzDecoder::new(original))
    } else {
        Box::new(original)
    };
    let mut reader = BufReader::new(reader);
    let mut count = 0;
    let mut callback_error = None;
    loop {
        while reader
            .fill_buf()?
            .first()
            .is_some_and(u8::is_ascii_whitespace)
        {
            reader.consume(1);
        }
        if reader.fill_buf()?.is_empty() {
            break;
        }
        let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
        if let Err(error) = (Records {
            callback: &mut callback,
            callback_error: &mut callback_error,
            count: &mut count,
        })
        .deserialize(&mut deserializer)
        {
            return Err(callback_error.unwrap_or_else(|| {
                Error::InvalidCatalog(format!("cannot stream {}: {error}", path.display()))
            }));
        }
    }
    if count == 0 {
        return Err(Error::InvalidCatalog("catalogue contains no cards".into()));
    }
    Ok(count)
}

/// State borrowed by the streaming Serde visitor.
struct Records<'a, F> {
    /// Consumer called once for each source printing.
    callback: &'a mut F,
    /// Original consumer error, retained instead of its Serde string representation.
    callback_error: &'a mut Option<Error>,
    /// Number of printing objects successfully consumed.
    count: &'a mut usize,
}

impl<F: FnMut(Value) -> Result<(), Error>> Records<'_, F> {
    /// Deliver one printing object, preserving any consumer error.
    fn emit<E: de::Error>(&mut self, value: Value) -> Result<(), E> {
        if !value.is_object() {
            return Err(E::custom("card entry must be an object"));
        }
        if value.get("object").and_then(Value::as_str) == Some("error") {
            return Err(E::custom(format!(
                "Scryfall error response: {}",
                value
                    .get("details")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified API error")
            )));
        }
        if let Err(error) = (self.callback)(value) {
            let message = error.to_string();
            *self.callback_error = Some(error);
            return Err(E::custom(message));
        }
        *self.count += 1;
        Ok(())
    }
}

impl<'de, F: FnMut(Value) -> Result<(), Error>> DeserializeSeed<'de> for Records<'_, F> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de, F: FnMut(Value) -> Result<(), Error>> Visitor<'de> for Records<'_, F> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("card objects, a card array, or a cards/data wrapper")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut sequence: A) -> Result<(), A::Error> {
        while let Some(value) = sequence.next_element::<Value>()? {
            self.emit(value)?;
        }
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut entries: A) -> Result<(), A::Error> {
        let mut object = Map::new();
        let mut collection = false;
        while let Some(key) = entries.next_key::<String>()? {
            if key == "cards" || key == "data" {
                if collection {
                    return Err(de::Error::custom(
                        "catalogue wrapper cannot contain both cards and data",
                    ));
                }
                collection = true;
                entries.next_value_seed(CardArray {
                    records: Records {
                        callback: self.callback,
                        callback_error: self.callback_error,
                        count: self.count,
                    },
                })?;
            } else {
                object.insert(key, entries.next_value::<Value>()?);
            }
        }
        if let Some(has_more) = object.get("has_more")
            && has_more.as_bool() != Some(false)
        {
            return Err(de::Error::custom(
                "has_more must be false; supply every page or a bulk catalogue",
            ));
        }
        if object.get("object").and_then(Value::as_str) == Some("error") || !collection {
            self.emit(Value::Object(object))?;
        }
        Ok(())
    }
}

/// Force wrapper `cards`/`data` values to be arrays rather than arbitrary objects.
struct CardArray<'a, F> {
    /// Consumer state shared with the surrounding wrapper.
    records: Records<'a, F>,
}

impl<'de, F: FnMut(Value) -> Result<(), Error>> DeserializeSeed<'de> for CardArray<'_, F> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self.records)
    }
}

/// Download the selected complete metadata snapshot, resuming a matching partial file.
///
/// Only the public bulk descriptor and its immutable static file URL are sent
/// over the network. HTTP errors stop immediately. The complete file is parsed
/// before replacing the previous snapshot. Existing matching completed snapshots
/// are reused after checking their compressed size.
///
/// # Errors
///
/// Returns an error for invalid descriptors/URLs, network or file failures,
/// inconsistent range responses, incorrect sizes, or malformed downloaded data.
pub fn download_metadata(kind: BulkKind, output: &Path) -> Result<BulkDownloadReport, Error> {
    fs::create_dir_all(output)?;
    let output = fs::canonicalize(output)?;
    let agent = agent(Duration::from_secs(1_800));
    let descriptor = agent
        .get(&format!(
            "https://api.scryfall.com/bulk-data/{}",
            kind.as_str()
        ))
        .header("Accept", "application/json")
        .call()
        .map_err(download_error)?
        .body_mut()
        .read_json::<Value>()
        .map_err(download_error)?;
    let (url, bytes) = descriptor_location(&descriptor)?;
    let updated_at = descriptor
        .get("updated_at")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Download("bulk descriptor has no updated_at".into()))?;
    let catalog = output.join(format!("{}.jsonl.gz", kind.as_str()));
    let report_path = output.join(format!("{}-download.json", kind.as_str()));
    if let Ok(file) = File::open(&report_path)
        && let Ok(report) = serde_json::from_reader::<_, BulkDownloadReport>(file)
        && report.kind == kind
        && report.updated_at == updated_at
        && report.download_uri == url
        && report.compressed_bytes == bytes
        && report.catalog == catalog
        && catalog
            .metadata()
            .is_ok_and(|metadata| metadata.len() == bytes)
    {
        return Ok(BulkDownloadReport {
            cached: true,
            ..report
        });
    }
    let pending = output.join(format!("{}.pending", kind.as_str()));
    let state = output.join(format!("{}.pending.json", kind.as_str()));
    let identity = serde_json::json!({ "url": url, "bytes": bytes });
    let resumable = File::open(&state)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, Value>(file).ok())
        .is_some_and(|previous| previous == identity);
    if !resumable {
        File::create(&pending)?;
        write_json_atomic(&state, &identity)?;
    }
    let mut existing = pending
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if existing > bytes {
        File::create(&pending)?;
        existing = 0;
    }
    if existing < bytes {
        let mut request = agent
            .get(&url)
            .header("Accept", "application/gzip,application/json")
            .header("Accept-Encoding", "identity");
        let range = format!("bytes={existing}-");
        if existing > 0 {
            request = request.header("Range", &range);
        }
        let mut response = request.call().map_err(download_error)?;
        match response.status().as_u16() {
            200 => existing = 0,
            206 => {
                let expected = format!("bytes {existing}-{}", bytes - 1);
                let actual = response
                    .headers()
                    .get("Content-Range")
                    .and_then(|header| header.to_str().ok())
                    .unwrap_or("");
                if actual != format!("{expected}/{bytes}") {
                    return Err(Error::Download(format!(
                        "unexpected bulk Content-Range: {actual}"
                    )));
                }
            }
            status => {
                return Err(Error::Download(format!(
                    "unexpected bulk HTTP status {status}"
                )));
            }
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(existing == 0)
            .append(existing > 0)
            .open(&pending)?;
        io::copy(
            &mut response.body_mut().as_reader().take(bytes - existing + 1),
            &mut file,
        )?;
        file.sync_all()?;
    }
    let actual = pending.metadata()?.len();
    if actual != bytes {
        return Err(Error::Download(format!(
            "bulk file has {actual} bytes, expected {bytes}; rerun to resume"
        )));
    }
    let cards = for_each_card(&pending, |_| Ok(()))?;
    fs::rename(&pending, &catalog)?;
    let report = BulkDownloadReport {
        catalog,
        kind,
        updated_at: updated_at.into(),
        download_uri: url,
        compressed_bytes: bytes,
        cards,
        cached: false,
    };
    write_json_atomic(&report_path, &report)?;
    fs::remove_file(state)?;
    Ok(report)
}

/// Resolve current JSONL and legacy JSON bulk descriptors with strict public-host checks.
fn descriptor_location(descriptor: &Value) -> Result<(String, u64), Error> {
    let url = descriptor
        .get("jsonl_download_uri")
        .or_else(|| descriptor.get("download_uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Download("bulk descriptor has no download URI".into()))?;
    if !url.starts_with("https://data.scryfall.io/")
        && !url.starts_with("https://data.scryfall.com/")
    {
        return Err(Error::Download("unexpected Scryfall bulk host".into()));
    }
    let size = descriptor
        .get("compressed_size")
        .or_else(|| descriptor.get("size"))
        .and_then(Value::as_u64)
        .filter(|size| *size > 0 && *size <= 4 * 1024 * 1024 * 1024)
        .ok_or_else(|| {
            Error::Download("bulk descriptor has an invalid or unsupported size".into())
        })?;
    Ok((url.into(), size))
}

/// Retain every metadata record while selecting full-card paper references in one language.
///
/// Normal/display images are selected ahead of large PNGs to bound storage.
/// Foreign-only cards remain in metadata and are counted as lacking an English
/// visual reference. Nothing is silently promoted into the selected language.
/// No network requests are made.
///
/// # Errors
///
/// Returns an error for invalid/duplicate metadata, an empty language, unsafe
/// printing identifiers, or file failures. Invalid source data never replaces
/// the previous complete metadata and image-plan files.
pub fn prepare(
    catalog: &Path,
    output: &Path,
    image_language: &str,
) -> Result<PreparationReport, Error> {
    if image_language.trim().is_empty() {
        return Err(Error::InvalidInput(
            "image language must not be empty".into(),
        ));
    }
    fs::create_dir_all(output)?;
    let output = fs::canonicalize(output)?;
    let source = fs::canonicalize(catalog)?;
    let metadata = output.join("metadata.jsonl");
    let image_plan = output.join("images.jsonl");
    let metadata_pending = output.join("metadata.pending.jsonl");
    let images_pending = output.join("images.pending.jsonl");
    let coverage_file = output.join("coverage.json");
    if [
        &metadata,
        &image_plan,
        &metadata_pending,
        &images_pending,
        &coverage_file,
        &coverage_file.with_extension("json.pending"),
    ]
    .into_iter()
    .any(|path| overlaps_source(path, &source))
    {
        return Err(Error::InvalidInput(
            "preparation output must differ from its source".into(),
        ));
    }
    let mut metadata_writer = BufWriter::new(File::create(&metadata_pending)?);
    let mut images_writer = BufWriter::new(File::create(&images_pending)?);
    let mut coverage = Coverage {
        image_language: image_language.into(),
        ..Coverage::default()
    };
    let mut printings = HashSet::new();
    let mut oracle_ids = HashSet::new();
    let mut selected_ids = HashSet::new();
    let base = source.parent().unwrap_or_else(|| Path::new("."));
    for_each_card(&source, |mut value| {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidCatalog("card entry requires an id".into()))?
            .to_owned();
        safe_id(&id)?;
        if !printings.insert(id.clone()) {
            return Err(Error::InvalidCatalog(format!("duplicate printing {id}")));
        }
        let language = value
            .get("lang")
            .or_else(|| value.get("language"))
            .and_then(Value::as_str)
            .unwrap_or("und")
            .to_owned();
        let paper = match value.get("games") {
            Some(Value::Array(games)) => games.iter().any(|game| game.as_str() == Some("paper")),
            None | Some(Value::Null) => value.get("digital").and_then(Value::as_bool) != Some(true),
            Some(_) => {
                return Err(Error::InvalidCatalog(format!(
                    "games for {id} must be an array"
                )));
            }
        };
        let placeholder = value.get("image_status").and_then(Value::as_str) == Some("placeholder");
        let image_missing = value.get("image_status").and_then(Value::as_str) == Some("missing");
        serde_json::to_writer(&mut metadata_writer, &value)?;
        metadata_writer.write_all(b"\n")?;
        select_image_urls(&mut value);
        let mut references = Vec::new();
        super::append_value(value, base, &mut references)?;
        coverage.metadata_printings += 1;
        *coverage.languages.entry(language.clone()).or_default() += 1;
        coverage.metadata_faces += references.len();
        let mut selected_printing = false;
        for reference in references {
            if let Some(oracle) = &reference.card.oracle_id {
                oracle_ids.insert(oracle.clone());
            }
            if !paper {
                coverage.excluded_nonpaper_faces += 1;
                continue;
            }
            if language != image_language {
                coverage.excluded_language_faces += 1;
                continue;
            }
            if placeholder {
                coverage.placeholder_image_faces += 1;
                continue;
            }
            if image_missing {
                coverage.missing_image_faces += 1;
                continue;
            }
            let Some(url) = reference.image_url.filter(|url| valid_image_url(url)) else {
                coverage.missing_image_faces += 1;
                continue;
            };
            let entry = ImageEntry {
                card: reference.card,
                image_url: url,
            };
            if let Some(oracle) = &entry.card.oracle_id {
                selected_ids.insert(oracle.clone());
            }
            serde_json::to_writer(&mut images_writer, &entry)?;
            images_writer.write_all(b"\n")?;
            coverage.selected_faces += 1;
            selected_printing = true;
        }
        coverage.selected_printings += usize::from(selected_printing);
        Ok(())
    })?;
    metadata_writer.flush()?;
    images_writer.flush()?;
    coverage.oracle_identities = oracle_ids.len();
    coverage.selected_oracle_identities = selected_ids.len();
    coverage.oracle_identities_without_selected_images =
        oracle_ids.difference(&selected_ids).count();
    fs::rename(metadata_pending, &metadata)?;
    fs::rename(images_pending, &image_plan)?;
    let report = PreparationReport {
        source,
        metadata,
        image_plan,
        coverage_file,
        coverage,
    };
    write_json_atomic(&report.coverage_file, &report)?;
    Ok(report)
}

/// A selected physical face; flattened metadata also makes the plan an index input.
#[derive(Debug, Deserialize, Serialize)]
struct ImageEntry {
    /// Printing, language, Oracle identity and physical-face metadata.
    #[serde(flatten)]
    card: CardMetadata,
    /// Full-card image chosen from the original metadata.
    image_url: String,
}

/// Select economical full-card images, including each separately imaged back face.
fn select_image_urls(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        let url = object
            .get("image_uris")
            .and_then(Value::as_object)
            .and_then(|uris| {
                ["normal", "grid", "display", "large", "png"]
                    .iter()
                    .find_map(|key| uris.get(*key).and_then(Value::as_str))
            })
            .map(str::to_owned);
        if let Some(url) = url {
            object.insert("image_url".into(), Value::String(url));
        }
        if let Some(Value::Object(uris)) = object.get_mut("image_uris") {
            uris.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "normal" | "grid" | "display" | "large" | "png"
                )
            });
        }
        if let Some(Value::Array(faces)) = object.get_mut("card_faces") {
            for face in faces {
                select_image_urls(face);
            }
        }
    }
}

/// Download at most `max_downloads` missing faces, or audit entirely offline when zero.
///
/// Completed images are decoded before reuse; interrupted files never replace a
/// completed image. A single failed request stops the batch, retaining previous
/// successes. Responses are bounded to 20 MiB, requests are sequential with a
/// 110 ms minimum interval, and HTTP errors are never retried automatically.
///
/// # Errors
///
/// Returns an error for invalid/duplicate plans, unsafe identifiers/URLs, failed
/// requests, unsupported images, or file failures. Repeating the operation
/// resumes using completed local images.
pub fn download_images(
    plan: &Path,
    images: &Path,
    max_downloads: usize,
) -> Result<ImageDownloadReport, Error> {
    download_images_with_options(
        plan,
        images,
        &ImageDownloadOptions {
            max_downloads,
            workers: 1,
        },
    )
}

/// Acquire reference images with bounded concurrency against Scryfall's static CDN.
///
/// The entire plan is validated before publishing progress or auditing cached
/// pixels. Cache decoding uses the bounded worker count, and every face is audited
/// before any request. Only compact image tasks are retained. Each worker spaces its request
/// starts by at least 110 ms; no card API calls are made. A first request failure
/// stops new work while already started requests settle. Completed files remain
/// reusable, and `image-progress.json` records counts, worker activity and errors
/// during the run as well as at completion. HTTP errors are never retried.
///
/// # Errors
///
/// Returns an error for an invalid worker count, invalid/duplicate plans, unsafe
/// paths or URLs, failed requests, unsupported images, or file failures. Progress
/// retained on error counts every successfully published image from this run.
pub fn download_images_with_options(
    plan: &Path,
    images: &Path,
    options: &ImageDownloadOptions,
) -> Result<ImageDownloadReport, Error> {
    let agent = agent(Duration::from_secs(45));
    download_images_using(plan, images, options, |url| {
        agent
            .get(url)
            .header("Accept", "image/jpeg,image/png,image/webp")
            .call()
            .map_err(download_error)?
            .body_mut()
            .with_config()
            .limit(20 * 1024 * 1024)
            .read_to_vec()
            .map_err(download_error)
    })
}

/// A compact, prevalidated static-CDN request and its local publication path.
struct ImageTask {
    /// Printing UUID retained for precise failure reports.
    id: String,
    /// Physical face retained for precise failure reports.
    face_index: u32,
    /// Public canonical full-card image, independent of private input photos.
    url: String,
    /// Final deterministic local filename.
    path: PathBuf,
}

/// Worker events are aggregated by one coordinator, which owns progress reporting.
enum ImageEvent {
    /// A worker is about to make one network request.
    Started(usize),
    /// A request finished, with successful byte count or its original error.
    Finished(usize, Result<u64, Error>, Option<FailedImage>),
}

/// Indexed audit results keep missing-image selection independent of worker timing.
enum AuditEvent {
    /// One worker is about to inspect a local image file.
    Started(usize),
    /// Worker number, source-plan index, and whether pixels decoded successfully.
    Finished(usize, usize, bool),
}

/// One unavailable face with enough context to investigate a large-gallery failure.
#[derive(Serialize)]
struct FailedImage {
    /// Printing UUID from the selected image plan.
    id: String,
    /// Physical face index from the selected image plan.
    face_index: u32,
    /// Exact public image URL attempted.
    url: String,
    /// Original transport, image-decode or file-publication error.
    error: String,
}

/// Live worker counters persisted beside the public image-coverage counts.
#[derive(Serialize)]
struct ImageRunStats {
    /// Number of workers selected for this invocation.
    workers: usize,
    /// Physical faces whose existing-file audit has completed.
    audited_faces: usize,
    /// Local image decodes currently being audited; no network requests are involved.
    active_audits: usize,
    /// Completed cache audits per worker, in worker-number order.
    worker_audited_faces: Vec<usize>,
    /// Requests started in this invocation, including currently active requests.
    requests_started: usize,
    /// Requests currently awaiting bytes, decoding or local publication.
    active_requests: usize,
    /// Requests that failed during this invocation.
    failed_requests: usize,
    /// Failed physical faces, including settled failures from already active peers.
    failed_images: Vec<FailedImage>,
    /// Request count for each worker, in worker-number order.
    worker_requests: Vec<usize>,
    /// Successfully published images for each worker.
    worker_downloaded_faces: Vec<usize>,
}

/// Persisted progress keeps the existing flat report shape and adds live-run details.
#[derive(Serialize)]
struct ImageProgress<'a> {
    /// Honest complete-plan coverage, including unfinished and unattempted faces.
    #[serde(flatten)]
    report: &'a ImageDownloadReport,
    /// Per-worker activity for observing a long-running acquisition.
    #[serde(flatten)]
    stats: &'a ImageRunStats,
    /// `auditing`, `downloading`, `complete`, `bounded`, `audited`, or `failed`.
    state: &'static str,
    /// First failure, absent for successful runs and ongoing progress updates.
    error: Option<&'a str>,
}

/// Run the same bounded coordinator with either HTTP or an offline test transport.
fn download_images_using<F>(
    plan: &Path,
    images: &Path,
    options: &ImageDownloadOptions,
    fetch: F,
) -> Result<ImageDownloadReport, Error>
where
    F: Fn(&str) -> Result<Vec<u8>, Error> + Sync,
{
    download_images_with_auditor(plan, images, options, fetch, |path| {
        path.is_file() && image::open(path).is_ok()
    })
}

/// Validate the complete plan before running bounded cache audits and acquisition.
fn download_images_with_auditor<F, A>(
    plan: &Path,
    images: &Path,
    options: &ImageDownloadOptions,
    fetch: F,
    audit: A,
) -> Result<ImageDownloadReport, Error>
where
    F: Fn(&str) -> Result<Vec<u8>, Error> + Sync,
    A: Fn(&Path) -> bool + Sync,
{
    if !(1..=16).contains(&options.workers) {
        return Err(Error::InvalidInput(
            "image workers must be from 1 to 16".into(),
        ));
    }
    let plan = fs::canonicalize(plan)?;
    fs::create_dir_all(images)?;
    let images = fs::canonicalize(images)?;
    let progress = images.join("image-progress.json");
    if overlaps_source(&progress, &plan)
        || overlaps_source(&progress.with_extension("json.pending"), &plan)
    {
        return Err(Error::InvalidInput(
            "image progress output must differ from its source plan".into(),
        ));
    }
    let mut report = ImageDownloadReport {
        plan: plan.clone(),
        images: images.clone(),
        total_faces: 0,
        cached_faces: 0,
        downloaded_faces: 0,
        remaining_faces: 0,
        complete: false,
        bytes_downloaded: 0,
    };
    let mut tasks = Vec::new();
    let mut identities = HashSet::new();
    for_each_card(&plan, |value| {
        let entry: ImageEntry = serde_json::from_value(value)?;
        safe_id(&entry.card.id)?;
        if !identities.insert((entry.card.id.clone(), entry.card.face_index)) {
            return Err(Error::InvalidCatalog(
                "duplicate physical face in image plan".into(),
            ));
        }
        if !valid_image_url(&entry.image_url) {
            return Err(Error::InvalidInput("unexpected Scryfall image URL".into()));
        }
        report.total_faces += 1;
        let path = images.join(format!("{}-{}.jpg", entry.card.id, entry.card.face_index));
        if overlaps_source(&path, &plan) || overlaps_source(&path.with_extension("pending"), &plan)
        {
            return Err(Error::InvalidInput(
                "reference image output must differ from its source plan".into(),
            ));
        }
        tasks.push(ImageTask {
            id: entry.card.id,
            face_index: entry.card.face_index,
            url: entry.image_url,
            path,
        });
        Ok(())
    })?;
    drop(identities);
    report.remaining_faces = report.total_faces;
    let mut stats = ImageRunStats {
        workers: options.workers,
        audited_faces: 0,
        active_audits: 0,
        worker_audited_faces: vec![0; options.workers],
        requests_started: 0,
        active_requests: 0,
        failed_requests: 0,
        failed_images: Vec::new(),
        worker_requests: vec![0; options.workers],
        worker_downloaded_faces: vec![0; options.workers],
    };
    persist_image_progress(&progress, &report, &stats, "auditing", None)?;
    let cached = audit_images(&tasks, &progress, &mut report, &mut stats, &audit)?;
    let tasks: VecDeque<_> = tasks
        .into_iter()
        .zip(cached)
        .filter_map(|(task, cached)| (!cached).then_some(task))
        .take(options.max_downloads)
        .collect();
    report.complete = report.remaining_faces == 0;
    if tasks.is_empty() {
        persist_image_progress(
            &progress,
            &report,
            &stats,
            if report.complete {
                "complete"
            } else {
                "audited"
            },
            None,
        )?;
        return Ok(report);
    }
    persist_image_progress(&progress, &report, &stats, "downloading", None)?;
    let tasks = Mutex::new(tasks);
    let stopped = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();
    let mut failure = None;
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(options.workers);
        for worker in 0..options.workers {
            let sender = sender.clone();
            let (tasks, stopped, fetch) = (&tasks, &stopped, &fetch);
            handles.push(
                scope.spawn(move || run_image_worker(worker, tasks, stopped, &sender, fetch)),
            );
        }
        drop(sender);
        let mut last_progress = Instant::now();
        loop {
            match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(ImageEvent::Started(worker)) => {
                    stats.requests_started += 1;
                    stats.active_requests += 1;
                    stats.worker_requests[worker] += 1;
                }
                Ok(ImageEvent::Finished(worker, result, failed_image)) => {
                    stats.active_requests = stats.active_requests.saturating_sub(1);
                    match result {
                        Ok(bytes) => {
                            report.downloaded_faces += 1;
                            report.remaining_faces -= 1;
                            report.bytes_downloaded += bytes;
                            stats.worker_downloaded_faces[worker] += 1;
                        }
                        Err(error) => {
                            stats.failed_requests += 1;
                            if let Some(failed_image) = failed_image {
                                stats.failed_images.push(failed_image);
                            }
                            stopped.store(true, Ordering::Release);
                            if failure.is_none() {
                                failure = Some(error);
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if last_progress.elapsed() >= Duration::from_secs(1) || failure.is_some() {
                if let Err(error) = persist_image_progress(
                    &progress,
                    &report,
                    &stats,
                    if failure.is_some() {
                        "failed"
                    } else {
                        "downloading"
                    },
                    failure.as_ref(),
                ) {
                    stopped.store(true, Ordering::Release);
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
                last_progress = Instant::now();
            }
        }
        for handle in handles {
            if handle.join().is_err() && failure.is_none() {
                failure = Some(Error::Download(
                    "image worker terminated unexpectedly".into(),
                ));
            }
        }
    });
    stats.active_requests = 0;
    report.complete = report.remaining_faces == 0;
    let state = if failure.is_some() {
        "failed"
    } else if report.complete {
        "complete"
    } else {
        "bounded"
    };
    if let Err(error) = persist_image_progress(&progress, &report, &stats, state, failure.as_ref())
        && failure.is_none()
    {
        failure = Some(error);
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(report),
    }
}

/// Audit decoded pixels with bounded workers, retaining one result bit per source face.
fn audit_images<A>(
    tasks: &[ImageTask],
    progress: &Path,
    report: &mut ImageDownloadReport,
    stats: &mut ImageRunStats,
    audit: &A,
) -> Result<Vec<bool>, Error>
where
    A: Fn(&Path) -> bool + Sync,
{
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let (sender, receiver) = mpsc::sync_channel(stats.workers * 2);
    let mut cached = vec![false; tasks.len()];
    let mut failure = None;
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(stats.workers);
        for worker in 0..stats.workers {
            let sender = sender.clone();
            let (next, stopped) = (&next, &stopped);
            handles.push(scope.spawn(move || {
                run_audit_worker(worker, tasks, next, stopped, &sender, audit);
            }));
        }
        drop(sender);
        let mut last_progress = Instant::now();
        loop {
            match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(AuditEvent::Started(_worker)) => stats.active_audits += 1,
                Ok(AuditEvent::Finished(worker, index, valid)) => {
                    stats.active_audits = stats.active_audits.saturating_sub(1);
                    stats.audited_faces += 1;
                    stats.worker_audited_faces[worker] += 1;
                    cached[index] = valid;
                    if valid {
                        report.cached_faces += 1;
                        report.remaining_faces -= 1;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if last_progress.elapsed() >= Duration::from_secs(1) || failure.is_some() {
                if let Err(error) = persist_image_progress(
                    progress,
                    report,
                    stats,
                    if failure.is_some() {
                        "failed"
                    } else {
                        "auditing"
                    },
                    failure.as_ref(),
                ) {
                    stopped.store(true, Ordering::Release);
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
                last_progress = Instant::now();
            }
        }
        for handle in handles {
            if handle.join().is_err() && failure.is_none() {
                failure = Some(Error::Download(
                    "cache-audit worker terminated unexpectedly".into(),
                ));
            }
        }
    });
    stats.active_audits = 0;
    if failure.is_none() && stats.audited_faces != tasks.len() {
        failure = Some(Error::Download(
            "cache audit ended before every face was checked".into(),
        ));
    }
    if let Err(error) = persist_image_progress(
        progress,
        report,
        stats,
        if failure.is_some() {
            "failed"
        } else {
            "auditing"
        },
        failure.as_ref(),
    ) && failure.is_none()
    {
        failure = Some(error);
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(cached),
    }
}

/// Decode at most one cache image per worker and bound outstanding coordinator events.
fn run_audit_worker<A>(
    worker: usize,
    tasks: &[ImageTask],
    next: &AtomicUsize,
    stopped: &AtomicBool,
    sender: &mpsc::SyncSender<AuditEvent>,
    audit: &A,
) where
    A: Fn(&Path) -> bool + Sync,
{
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        let index = next.fetch_add(1, Ordering::Relaxed);
        let Some(task) = tasks.get(index) else {
            break;
        };
        if sender.send(AuditEvent::Started(worker)).is_err() {
            break;
        }
        let valid = audit(&task.path);
        if sender
            .send(AuditEvent::Finished(worker, index, valid))
            .is_err()
        {
            break;
        }
    }
}

/// Consume compact tasks until exhausted or any worker has observed a failure.
fn run_image_worker<F>(
    worker: usize,
    tasks: &Mutex<VecDeque<ImageTask>>,
    stopped: &AtomicBool,
    sender: &mpsc::Sender<ImageEvent>,
    fetch: &F,
) where
    F: Fn(&str) -> Result<Vec<u8>, Error> + Sync,
{
    let mut previous_request: Option<Instant> = None;
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        let task = match tasks.lock() {
            Ok(mut tasks) => tasks.pop_front(),
            Err(_) => {
                stopped.store(true, Ordering::Release);
                let _ = sender.send(ImageEvent::Finished(
                    worker,
                    Err(Error::Download("image task queue was poisoned".into())),
                    None,
                ));
                break;
            }
        };
        let Some(task) = task else {
            break;
        };
        if let Some(previous) = previous_request {
            thread::sleep(Duration::from_millis(110).saturating_sub(previous.elapsed()));
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }
        previous_request = Some(Instant::now());
        if sender.send(ImageEvent::Started(worker)).is_err() {
            break;
        }
        let result = fetch(&task.url).and_then(|bytes| publish_image(&task.path, &bytes));
        let failed_image = result.as_ref().err().map(|error| FailedImage {
            id: task.id.clone(),
            face_index: task.face_index,
            url: task.url.clone(),
            error: error.to_string(),
        });
        let result = result.map_err(|error| {
            Error::Download(format!(
                "printing {} face {} from {} failed: {error}",
                task.id, task.face_index, task.url
            ))
        });
        if result.is_err() {
            stopped.store(true, Ordering::Release);
        }
        if sender
            .send(ImageEvent::Finished(worker, result, failed_image))
            .is_err()
        {
            break;
        }
    }
}

/// Decode an entire response and atomically publish only its completed pixels.
fn publish_image(path: &Path, bytes: &[u8]) -> Result<u64, Error> {
    let decoded = image::load_from_memory(bytes)?;
    let temporary = path.with_extension("pending");
    if image::guess_format(bytes)? == image::ImageFormat::Jpeg {
        fs::write(&temporary, bytes)?;
    } else {
        decoded
            .to_rgb8()
            .save_with_format(&temporary, image::ImageFormat::Jpeg)?;
    }
    fs::rename(temporary, path)?;
    Ok(bytes.len() as u64)
}

/// Persist a coherent progress snapshot while retaining the original first error.
fn persist_image_progress(
    path: &Path,
    report: &ImageDownloadReport,
    stats: &ImageRunStats,
    state: &'static str,
    error: Option<&Error>,
) -> Result<(), Error> {
    let message = error.map(ToString::to_string);
    write_json_atomic(
        path,
        &ImageProgress {
            report,
            stats,
            state,
            error: message.as_deref(),
        },
    )
}

/// Compare source/output aliases before a staging writer could truncate its own input.
fn overlaps_source(output: &Path, source: &Path) -> bool {
    output == source || fs::canonicalize(output).is_ok_and(|resolved| resolved == source)
}

/// Restrict derived filenames to a single safe path component.
fn safe_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::InvalidCatalog(format!(
            "unsafe printing identifier {id:?}"
        )));
    }
    Ok(())
}

/// Accept only Scryfall's public canonical-image hosts.
fn valid_image_url(url: &str) -> bool {
    url.starts_with("https://cards.scryfall.io/") || url.starts_with("https://img.scryfall.com/")
}

/// Configure identified, finite-time HTTP access for explicit downloads.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!(
            "whosthatpokemon/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/cmeister2/whosthatpokemon)"
        ))
        .build()
        .into()
}

/// Preserve HTTP status and transport details without retrying.
fn download_error(error: ureq::Error) -> Error {
    Error::Download(error.to_string())
}

/// Publish a complete JSON report without truncating its prior completed version.
fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    let pending = path.with_extension("json.pending");
    let mut writer = BufWriter::new(File::create(&pending)?);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    fs::rename(pending, path)?;
    Ok(())
}

/// Offline transport tests exercise concurrency and publication independently of HTTP.
#[cfg(test)]
mod acquisition_tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{
            Barrier,
            atomic::{AtomicU64, AtomicUsize},
        },
    };

    /// Unique temporary directories across concurrently running test cases.
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    /// A complete, local image plan and an initially empty image directory.
    struct Fixture {
        /// Parent directory removed when the test ends.
        directory: PathBuf,
        /// Flattened physical-face image plan accepted by the public API.
        plan: PathBuf,
        /// Canonical-image destination.
        images: PathBuf,
    }

    impl Fixture {
        /// Write a plan containing the requested number of distinct physical faces.
        fn new(count: usize) -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "whosthatpokemon-image-workers-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).expect("create test directory");
            let plan = directory.join("images.jsonl");
            let mut writer = File::create(&plan).expect("create image plan");
            for number in 0..count {
                let entry = ImageEntry {
                    card: CardMetadata {
                        game: "mtg".into(),
                        id: format!("card-{number}"),
                        name: format!("Card {number}"),
                        ..CardMetadata::default()
                    },
                    image_url: format!("https://cards.scryfall.io/normal/{number}.jpg"),
                };
                serde_json::to_writer(&mut writer, &entry).expect("write image task");
                writer.write_all(b"\n").expect("terminate image task");
            }
            let images = directory.join("images");
            Self {
                directory,
                plan,
                images,
            }
        }

        /// Read the last atomically published progress snapshot.
        fn progress(&self) -> Value {
            serde_json::from_reader(
                File::open(self.images.join("image-progress.json")).expect("open progress"),
            )
            .expect("decode progress")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    /// Produce decodable JPEG bytes without a network service or external fixture.
    fn jpeg() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            12,
            image::Rgb([20, 40, 60]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .expect("encode test JPEG");
        bytes.into_inner()
    }

    /// Two workers overlap, respect the batch bound, and expose success before a slow peer ends.
    #[test]
    fn workers_are_bounded_and_publish_live_progress() {
        let fixture = Fixture::new(5);
        let bytes = jpeg();
        let barrier = Barrier::new(2);
        let calls = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let options = ImageDownloadOptions {
            max_downloads: 2,
            workers: 2,
        };
        let report = download_images_using(&fixture.plan, &fixture.images, &options, |url| {
            calls.fetch_add(1, Ordering::SeqCst);
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            barrier.wait();
            if url.ends_with("/1.jpg") {
                let started = Instant::now();
                while fixture.progress()["downloaded_faces"] != 1 {
                    assert!(
                        started.elapsed() < Duration::from_secs(5),
                        "successful peer never reached live progress"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                let progress = fixture.progress();
                assert_eq!(progress["state"], "downloading");
                assert_eq!(progress["remaining_faces"], 4);
                assert_eq!(progress["active_requests"], 1);
            }
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(bytes.clone())
        })
        .expect("complete bounded parallel batch");
        assert_eq!(
            (calls.load(Ordering::SeqCst), peak.load(Ordering::SeqCst)),
            (2, 2)
        );
        assert_eq!(
            (
                report.total_faces,
                report.downloaded_faces,
                report.remaining_faces
            ),
            (5, 2, 3)
        );
        let progress = fixture.progress();
        assert_eq!(progress["state"], "bounded");
        assert_eq!(progress["worker_requests"], serde_json::json!([1, 1]));
        assert_eq!(progress["active_requests"], 0);
    }

    /// An HTTP-like failure stops new requests, retains in-flight success, and resumes offline.
    #[test]
    fn first_failure_is_persisted_and_completed_peers_resume() {
        let fixture = Fixture::new(4);
        let bytes = jpeg();
        let barrier = Barrier::new(2);
        let calls = AtomicUsize::new(0);
        let options = ImageDownloadOptions {
            max_downloads: 4,
            workers: 2,
        };
        let result = download_images_using(&fixture.plan, &fixture.images, &options, |url| {
            calls.fetch_add(1, Ordering::SeqCst);
            barrier.wait();
            if url.ends_with("/0.jpg") {
                return Err(Error::Download("HTTP 429 sentinel".into()));
            }
            thread::sleep(Duration::from_millis(40));
            Ok(bytes.clone())
        });
        assert!(
            matches!(result, Err(Error::Download(message)) if message.contains("HTTP 429 sentinel") && message.contains("printing card-0 face 0") && message.contains("/normal/0.jpg"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let progress = fixture.progress();
        assert_eq!(progress["state"], "failed");
        assert_eq!(progress["failed_requests"], 1);
        assert_eq!(progress["failed_images"][0]["id"], "card-0");
        assert_eq!(progress["failed_images"][0]["face_index"], 0);
        assert_eq!(
            progress["failed_images"][0]["url"],
            "https://cards.scryfall.io/normal/0.jpg"
        );
        assert_eq!(progress["downloaded_faces"], 1);
        assert_eq!(progress["remaining_faces"], 3);
        assert!(progress["error"].as_str().unwrap().contains("429 sentinel"));
        let resumed = download_images_using(&fixture.plan, &fixture.images, &options, |_| {
            Ok(bytes.clone())
        })
        .expect("resume retained completed peer");
        assert_eq!(
            (
                resumed.cached_faces,
                resumed.downloaded_faces,
                resumed.remaining_faces
            ),
            (1, 3, 0)
        );
        assert!(resumed.complete);
        assert_eq!(fixture.progress()["state"], "complete");
    }

    /// A late invalid task fails preflight before any request or image publication.
    #[test]
    fn whole_plan_is_preflighted_before_parallel_requests() {
        let fixture = Fixture::new(2);
        fs::create_dir_all(&fixture.images).expect("create cache directory");
        let previous = br#"{"state":"failed","error":"previous run"}"#;
        fs::write(fixture.images.join("image-progress.json"), previous)
            .expect("seed previous progress");
        let source = fs::read_to_string(&fixture.plan).expect("read source plan");
        fs::write(
            &fixture.plan,
            source.replace(
                "https://cards.scryfall.io/normal/1.jpg",
                "https://example.invalid/not-scryfall",
            ),
        )
        .expect("seed invalid late URL");
        let calls = AtomicUsize::new(0);
        let audits = AtomicUsize::new(0);
        let result = download_images_with_auditor(
            &fixture.plan,
            &fixture.images,
            &ImageDownloadOptions::default(),
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(jpeg())
            },
            |_| {
                audits.fetch_add(1, Ordering::SeqCst);
                true
            },
        );
        assert!(matches!(result, Err(Error::InvalidInput(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(audits.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read(fixture.images.join("image-progress.json")).unwrap(),
            previous
        );
        assert!(!fixture.images.join("card-0-0.jpg").exists());
    }

    /// Parallel offline audits clear stale errors and expose validated caches while a peer is active.
    #[test]
    fn cache_audits_overlap_and_publish_truthful_offline_progress() {
        let fixture = Fixture::new(4);
        fs::create_dir_all(&fixture.images).expect("create cache directory");
        let bytes = jpeg();
        for index in 0..3 {
            fs::write(fixture.images.join(format!("card-{index}-0.jpg")), &bytes)
                .expect("seed valid cached JPEG");
        }
        fs::write(fixture.images.join("card-3-0.jpg"), b"corrupt JPEG")
            .expect("seed undecodable existing file");
        fs::write(
            fixture.images.join("image-progress.json"),
            br#"{"state":"failed","error":"previous run"}"#,
        )
        .expect("seed stale progress");
        let barrier = Barrier::new(2);
        let calls = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let requests = AtomicUsize::new(0);
        let report = download_images_with_auditor(
            &fixture.plan,
            &fixture.images,
            &ImageDownloadOptions {
                max_downloads: 0,
                workers: 2,
            },
            |_| {
                requests.fetch_add(1, Ordering::SeqCst);
                Ok(bytes.clone())
            },
            |path| {
                let number = calls.fetch_add(1, Ordering::SeqCst);
                let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(concurrent, Ordering::SeqCst);
                let initial = fixture.progress();
                assert_eq!(initial["state"], "auditing");
                assert_eq!(initial["total_faces"], 4);
                assert_eq!(initial["active_requests"], 0);
                assert_eq!(initial["complete"], false);
                assert!(initial["error"].is_null());
                if number < 2 {
                    barrier.wait();
                }
                if path.ends_with("card-1-0.jpg") {
                    let started = Instant::now();
                    while fixture.progress()["cached_faces"].as_u64().unwrap() == 0 {
                        assert!(
                            started.elapsed() < Duration::from_secs(5),
                            "cache audit never reached live progress"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                    let progress = fixture.progress();
                    assert_eq!(progress["state"], "auditing");
                    assert!(progress["audited_faces"].as_u64().unwrap() < 4);
                    assert!(progress["active_audits"].as_u64().unwrap() > 0);
                    assert_eq!(
                        progress["cached_faces"].as_u64().unwrap()
                            + progress["remaining_faces"].as_u64().unwrap(),
                        4
                    );
                    assert_eq!(progress["requests_started"], 0);
                }
                let valid = path.is_file() && image::open(path).is_ok();
                active.fetch_sub(1, Ordering::SeqCst);
                valid
            },
        )
        .expect("audit existing pixels in parallel without HTTP");
        assert_eq!(
            (
                calls.load(Ordering::SeqCst),
                peak.load(Ordering::SeqCst),
                requests.load(Ordering::SeqCst)
            ),
            (4, 2, 0)
        );
        assert_eq!(
            (
                report.total_faces,
                report.cached_faces,
                report.downloaded_faces,
                report.remaining_faces
            ),
            (4, 3, 0, 1)
        );
        assert!(!report.complete);
        let progress = fixture.progress();
        assert_eq!(progress["state"], "audited");
        assert_eq!(progress["audited_faces"], 4);
        assert_eq!(progress["active_audits"], 0);
        assert_eq!(
            progress["worker_audited_faces"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_u64().unwrap())
                .sum::<u64>(),
            4
        );
    }

    /// Out-of-order audit results do not change the first missing faces selected by the limit.
    #[test]
    fn audited_missing_selection_preserves_plan_order_and_batch_bound() {
        let fixture = Fixture::new(5);
        fs::create_dir_all(&fixture.images).expect("create cache directory");
        let bytes = jpeg();
        for index in [0, 2] {
            fs::write(fixture.images.join(format!("card-{index}-0.jpg")), &bytes)
                .expect("seed cached image");
        }
        let audited = AtomicUsize::new(0);
        let requested = Mutex::new(Vec::new());
        let report = download_images_with_auditor(
            &fixture.plan,
            &fixture.images,
            &ImageDownloadOptions {
                max_downloads: 2,
                workers: 2,
            },
            |url| {
                assert_eq!(
                    audited.load(Ordering::SeqCst),
                    5,
                    "HTTP started before the complete cache audit"
                );
                assert_eq!(fixture.progress()["audited_faces"], 5);
                requested.lock().unwrap().push(url.to_owned());
                Ok(bytes.clone())
            },
            |path| {
                if path.ends_with("card-1-0.jpg") {
                    thread::sleep(Duration::from_millis(50));
                }
                let valid = path.is_file() && image::open(path).is_ok();
                audited.fetch_add(1, Ordering::SeqCst);
                valid
            },
        )
        .expect("acquire the first two missing faces in source order");
        let mut requested = requested.into_inner().unwrap();
        requested.sort();
        assert_eq!(
            requested,
            [
                "https://cards.scryfall.io/normal/1.jpg",
                "https://cards.scryfall.io/normal/3.jpg"
            ]
        );
        assert!(!fixture.images.join("card-4-0.jpg").exists());
        assert_eq!(
            (
                report.cached_faces,
                report.downloaded_faces,
                report.remaining_faces
            ),
            (2, 2, 1)
        );
    }
}
