//! Command-line indexing and scanning for trading-card images.

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};
use whosthatpokemon::{
    CardMetadata, CatalogIndex, Error, ExtractorConfig, MatchOptions, Matcher, RecognitionPipeline,
    image, mtg,
};

/// Recognise trading cards using an offline visual catalogue.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Catalogue preparation or image scanning.
    #[command(subcommand)]
    command: Command,
}

/// Supported game catalogues and image operations.
#[derive(Debug, Subcommand)]
enum Command {
    /// Import or download Magic: The Gathering references.
    Mtg {
        /// Magic catalogue operation.
        #[command(subcommand)]
        command: MtgCommand,
    },
    /// Detect cards in scene images and return ranked matches as JSON.
    Scan(ScanArgs),
}

/// Magic-specific catalogue preparation operations.
#[derive(Debug, Subcommand)]
enum MtgCommand {
    /// Rank catalogue entries using independent title, footer, or rules OCR text.
    Search {
        /// Portable index file or shard directory containing searchable metadata.
        #[arg(long, required_unless_present_any = ["catalog", "text_index"], conflicts_with_all = ["catalog", "text_index"])]
        index: Option<PathBuf>,
        /// Metadata catalogue to search without enrolling reference images.
        #[arg(long, required_unless_present_any = ["index", "text_index"], conflicts_with = "text_index")]
        catalog: Option<PathBuf>,
        /// Reusable compact text index with cached aliases, rules and word weights.
        #[arg(long)]
        text_index: Option<PathBuf>,
        /// Observed or corrected text from the title bar.
        #[arg(long, default_value = "")]
        title: String,
        /// Observed collector footer; a set code and number are needed together.
        #[arg(long, default_value = "")]
        footer: String,
        /// Observed rules text; short generic phrases are ignored.
        #[arg(long, default_value = "")]
        rules: String,
    },
    /// Read Magic text locally with ocracle and independently search supplied metadata.
    #[cfg(feature = "ocr")]
    Ocr(OcrArgs),
    /// Combine local OCR with bounded visual and geometric matching.
    #[cfg(feature = "ocr")]
    Identify(IdentifyArgs),
    /// Build a compact all-language text index without acquiring reference images.
    TextIndex {
        /// Local metadata catalogue, including all desired printings and languages.
        #[arg(long)]
        catalog: PathBuf,
        /// Destination for the reusable text index.
        #[arg(long)]
        output: PathBuf,
    },
    /// Download an official bulk metadata snapshot without downloading card images.
    Bulk {
        /// Bulk snapshot type; all-cards retains every printed language.
        #[arg(long, value_enum, default_value = "all-cards")]
        kind: BulkKind,
        /// Directory for the snapshot and its provenance report.
        #[arg(long)]
        output: PathBuf,
    },
    /// Prepare all-language metadata and an English-first paper reference-image plan.
    Prepare {
        /// Local bulk JSONL, gzip, JSON array or complete manifest.
        #[arg(long)]
        catalog: PathBuf,
        /// Directory for metadata, image plan and explicit coverage counts.
        #[arg(long)]
        output: PathBuf,
        /// Printed language selected for visual references; metadata keeps all languages.
        #[arg(long, default_value = "en")]
        image_language: String,
    },
    /// Resume an image plan with a bounded number of new downloads.
    FetchImages {
        /// JSONL image plan produced by mtg prepare.
        #[arg(long)]
        plan: PathBuf,
        /// Reusable local reference-image directory.
        #[arg(long)]
        images: PathBuf,
        /// Maximum new image downloads (default 100); zero audits existing files offline.
        #[arg(long, conflicts_with = "all")]
        limit: Option<usize>,
        /// Acquire every remaining face in the prepared image plan.
        #[arg(long)]
        all: bool,
        /// Concurrent static-image downloads, from 1 through 16.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=16))]
        workers: u8,
    },
    /// Build an offline index from local images and metadata.
    Index {
        /// Scryfall JSON/JSONL/gzip or local reference manifest.
        #[arg(long)]
        catalog: PathBuf,
        /// Local reference directory, unless metadata declares explicit file paths.
        #[arg(long)]
        images: PathBuf,
        /// Destination for the versioned visual index.
        #[arg(long)]
        output: PathBuf,
        /// Build a bounded-memory portable shard directory instead of one JSON file.
        #[arg(long)]
        shard_size: Option<usize>,
        /// Resume completed shards from an interrupted portable index build.
        #[arg(long, requires = "shard_size")]
        resume: bool,
        /// Parallel descriptor workers for shard enrollment, from 1 through 16 (default 4).
        #[arg(long, requires = "shard_size", value_parser = clap::value_parser!(u8).range(1..=16))]
        workers: Option<u8>,
        /// Wait up to this many seconds per missing image while another process acquires it.
        #[arg(long, requires = "shard_size", default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=86_400))]
        wait_for_images: u32,
        /// Visual matching backend and any required model/runtime paths.
        #[command(flatten)]
        matching: BackendArgs,
    },
    /// Explicitly download a bounded Scryfall query and reference images.
    Fetch {
        /// Scryfall search expression, for example `set:lea`.
        #[arg(long)]
        query: String,
        /// Destination directory for cards.json and images.
        #[arg(long)]
        output: PathBuf,
        /// Maximum printings to fetch; subset selection is recorded in metadata.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

/// Official metadata snapshots, independent of which visual references are enrolled.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum BulkKind {
    /// Every printing and printed language.
    AllCards,
    /// Default language for every printing.
    DefaultCards,
    /// One default printing per semantic card identity.
    OracleCards,
    /// One default printing per artwork.
    UniqueArtwork,
}

/// Explicit portrait template for assigning observed text to fields.
#[cfg(feature = "ocr")]
#[derive(Clone, Copy, Debug, ValueEnum)]
enum OcrLayout {
    /// Conventional modern portrait card.
    Modern,
    /// Conventional classic portrait card.
    Classic,
}

/// Detector-free recognizer profile used by title-first identification.
#[cfg(feature = "ocr")]
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TitleModel {
    /// Faster PP-OCRv6 small recognizer; title errors remain possible.
    Small,
    /// The language registry's more accurate default recognizer.
    Default,
}

/// Native OCR input, reusable model assets and optional independent metadata search.
#[cfg(feature = "ocr")]
#[derive(Debug, Args)]
struct OcrArgs {
    /// Directory containing ocracle's models/ registry and model files.
    #[arg(long)]
    model_root: PathBuf,
    /// Printed language understood by the supplied ocracle registry.
    #[arg(long, default_value = "en")]
    language: String,
    /// Approximate portrait template; raw OCR is retained for other layouts.
    #[arg(long, value_enum, default_value = "modern")]
    layout: OcrLayout,
    /// Clockwise rotation of each extracted crop before OCR.
    #[arg(long, default_value_t = 0, value_parser = parse_rotation)]
    rotation: u16,
    /// Skip outline detection for tightly cropped card inputs.
    #[arg(long)]
    cropped: bool,
    /// Search metadata without needing a visual index or reference images.
    #[arg(long, conflicts_with_all = ["index", "text_index"])]
    catalog: Option<PathBuf>,
    /// Alternatively search metadata in an existing portable index or shard directory.
    #[arg(long, conflicts_with = "text_index")]
    index: Option<PathBuf>,
    /// Reusable compact metadata index, avoiding a raw catalogue parse per run.
    #[arg(long)]
    text_index: Option<PathBuf>,
    /// Destination for OCR and independent text candidates; otherwise standard output.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Scene photographs or already-cropped card images.
    #[arg(required = true)]
    images: Vec<PathBuf>,
}

/// Native card identification with reusable OCR, text and visual indexes.
#[cfg(feature = "ocr")]
#[derive(Debug, Args)]
struct IdentifyArgs {
    /// Completed portable visual shard directory.
    #[arg(long)]
    index: PathBuf,
    /// Optional all-language title source; defaults to enrolled visual metadata.
    #[arg(long)]
    text_index: Option<PathBuf>,
    /// Directory containing ocracle's local models registry and weights.
    #[arg(long)]
    model_root: PathBuf,
    /// Printed language supported by the local OCR registry.
    #[arg(long, default_value = "en")]
    language: String,
    /// Approximate portrait template for assigning observed text to fields.
    #[arg(long, value_enum, default_value = "modern")]
    layout: OcrLayout,
    /// Detector-free recognizer for title and collector rows; `small` is the live-path default.
    #[arg(long, value_enum, default_value = "small")]
    title_model: TitleModel,
    /// RTen inference workers retained by the shared text recognizer.
    #[arg(long, default_value_t = 4)]
    ocr_threads: usize,
    /// Expected card orientation; the opposite 180-degree orientation is tried on failure.
    #[arg(long, default_value_t = 0, value_parser = parse_title_rotation)]
    rotation: u16,
    /// Treat inputs as already cropped cards and bypass outline detection.
    #[arg(long)]
    cropped: bool,
    /// Maximum OCR title identities proposed before visual verification.
    #[arg(long, default_value_t = 5)]
    title_candidates: usize,
    /// Maximum title/footer-derived physical faces scored with compact descriptors.
    #[arg(long, default_value_t = 256)]
    max_candidates: usize,
    /// Maximum group-preserving candidate faces checked with local geometry.
    #[arg(long, default_value_t = 8)]
    max_geometry: usize,
    /// Maximum visual printing candidates returned per crop.
    #[arg(long, default_value_t = 5)]
    top_k: usize,
    /// Minimum absolute visual similarity for acceptance.
    #[arg(long, default_value_t = 0.80)]
    min_similarity: f32,
    /// Minimum visual lead over a competing printing or identity.
    #[arg(long, default_value_t = 0.03)]
    min_margin: f32,
    /// Write rectified crops and actual OCR input images to a new directory.
    ///
    /// PNG encoding and writing happen after processing and are excluded from timings.
    #[arg(long)]
    trace_dir: Option<PathBuf>,
    /// JSON destination; otherwise write to standard output.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Scene photographs or already-cropped card images.
    #[arg(required = true)]
    images: Vec<PathBuf>,
}

/// Reject unsupported rotations before loading expensive OCR model files.
#[cfg(feature = "ocr")]
fn parse_rotation(text: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(rotation @ (0 | 90 | 180 | 270)) => Ok(rotation),
        _ => Err("rotation must be 0, 90, 180 or 270".into()),
    }
}

/// Restrict title-first identification to the two portrait orientations.
#[cfg(feature = "ocr")]
fn parse_title_rotation(text: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(rotation @ (0 | 180)) => Ok(rotation),
        _ => Err("title rotation must be 0 or 180".into()),
    }
}

/// Available visual matching implementations.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    /// Portable image-structure and colour matching without external model files.
    Portable,
    /// CPU CLIP embeddings, using explicitly supplied ONNX model and runtime files.
    #[cfg(feature = "clip")]
    Clip,
}

/// Select the matching implementation and its external assets.
#[derive(Debug, Args)]
struct BackendArgs {
    /// Visual backend used to build or read the index.
    #[arg(long, value_enum, default_value_t = Backend::Portable)]
    backend: Backend,
    /// CLIP vision ONNX file; must match the model used to build the index.
    #[cfg(feature = "clip")]
    #[arg(long)]
    model: Option<PathBuf>,
    /// ONNX Runtime shared library, version 1.24 or newer.
    #[cfg(feature = "clip")]
    #[arg(long)]
    runtime: Option<PathBuf>,
}

/// Scan inputs, result destination, and confidence thresholds.
#[derive(Debug, Args)]
struct ScanArgs {
    /// Previously built visual index.
    #[arg(long)]
    index: PathBuf,
    /// Visual matching backend and any required external model files.
    #[command(flatten)]
    matching: BackendArgs,
    /// PNG, JPEG, or WebP inputs.
    #[arg(required = true)]
    images: Vec<PathBuf>,
    /// Treat each input as an already cropped card; bypass outline detection.
    #[arg(long)]
    cropped: bool,
    /// Maximum candidates per card.
    #[arg(long, default_value_t = 5)]
    top_k: usize,
    /// Minimum absolute visual similarity for acceptance.
    #[arg(long)]
    min_similarity: Option<f32>,
    /// Minimum lead over a competing printing or card identity.
    #[arg(long)]
    min_margin: Option<f32>,
    /// JSON output file; otherwise use standard output.
    #[arg(long)]
    output: Option<PathBuf>,
}

/// Associate a recognition result with its source file.
#[derive(Debug, Serialize)]
struct ImageResult<T> {
    /// Caller-supplied source path.
    input: PathBuf,
    /// Scene or crop recognition evidence.
    result: T,
}

/// Execute one command and report failures to standard error.
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Prepare catalogues or recognise inputs, emitting machine-readable results.
///
/// # Errors
///
/// Returns an error for invalid settings, failed metadata/image/index I/O,
/// failed extraction/matching, download errors, or unwritable output.
fn run(cli: Cli) -> Result<(), Error> {
    match cli.command {
        Command::Mtg { command } => match command {
            MtgCommand::Search {
                index,
                catalog,
                text_index,
                title,
                footer,
                rules,
            } => {
                let metadata =
                    read_text_search(index.as_deref(), catalog.as_deref(), text_index.as_deref())?;
                let observations = mtg::evidence::TextObservations {
                    title,
                    footer,
                    rules,
                };
                write_json(&metadata.search(&observations), None)
            }
            #[cfg(feature = "ocr")]
            MtgCommand::Ocr(arguments) => run_ocr(arguments),
            #[cfg(feature = "ocr")]
            MtgCommand::Identify(arguments) => run_identify(arguments),
            MtgCommand::TextIndex { catalog, output } => {
                reject_source_overwrite(&catalog, &output)?;
                let index = mtg::evidence::TextIndex::build(&catalog)?;
                index.save(&output)?;
                write_json(
                    &serde_json::json!({"text_index":output,"references":index.len()}),
                    None,
                )
            }
            MtgCommand::Bulk { kind, output } => {
                let kind = match kind {
                    BulkKind::AllCards => mtg::bulk::BulkKind::AllCards,
                    BulkKind::DefaultCards => mtg::bulk::BulkKind::DefaultCards,
                    BulkKind::OracleCards => mtg::bulk::BulkKind::OracleCards,
                    BulkKind::UniqueArtwork => mtg::bulk::BulkKind::UniqueArtwork,
                };
                write_json(&mtg::bulk::download_metadata(kind, &output)?, None)
            }
            MtgCommand::Prepare {
                catalog,
                output,
                image_language,
            } => write_json(
                &mtg::bulk::prepare(&catalog, &output, &image_language)?,
                None,
            ),
            MtgCommand::FetchImages {
                plan,
                images,
                limit,
                all,
                workers,
            } => write_json(
                &mtg::bulk::download_images_with_options(
                    &plan,
                    &images,
                    &mtg::bulk::ImageDownloadOptions {
                        max_downloads: if all {
                            usize::MAX
                        } else {
                            limit.unwrap_or(100)
                        },
                        workers: usize::from(workers),
                    },
                )?,
                None,
            ),
            MtgCommand::Index {
                catalog,
                images,
                output,
                shard_size,
                resume,
                workers,
                wait_for_images,
                matching,
            } => {
                let work = IndexWork {
                    shard_size,
                    resume,
                    workers: usize::from(workers.unwrap_or(4)),
                    wait_for_images: Duration::from_secs(u64::from(wait_for_images)),
                };
                let references = build_index(&catalog, &images, &output, &matching, work)?;
                write_json(
                    &serde_json::json!({"index": output, "references": references}),
                    None,
                )
            }
            MtgCommand::Fetch {
                query,
                output,
                limit,
            } => write_json(&mtg::download::fetch(&query, &output, limit)?, None),
        },
        Command::Scan(arguments) => {
            let matcher = load_index(&arguments.index, &arguments.matching)?;
            let defaults = matcher.default_options();
            let options = MatchOptions {
                top_k: arguments.top_k,
                min_similarity: arguments.min_similarity.unwrap_or(defaults.min_similarity),
                min_margin: arguments.min_margin.unwrap_or(defaults.min_margin),
            };
            let pipeline =
                RecognitionPipeline::with_config(matcher, ExtractorConfig::default(), options)?;
            if arguments.cropped {
                let results = arguments
                    .images
                    .into_iter()
                    .map(|input| {
                        let result = pipeline.recognize_crop(&open_image(&input)?)?;
                        Ok(ImageResult { input, result })
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                write_json(&results, arguments.output.as_deref())
            } else {
                let results = arguments
                    .images
                    .into_iter()
                    .map(|input| {
                        let result = pipeline.scan(&open_image(&input)?)?;
                        Ok(ImageResult { input, result })
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                write_json(&results, arguments.output.as_deref())
            }
        }
    }
}

/// Reject identical or symlink-aliased source and output paths before index creation.
///
/// # Errors
///
/// Returns an error for an unavailable source or an output resolving to that source.
fn reject_source_overwrite(source: &Path, output: &Path) -> Result<(), Error> {
    let source = fs::canonicalize(source)?;
    let same = fs::canonicalize(output).is_ok_and(|output| output == source);
    #[cfg(unix)]
    let same = same
        || if let Ok(output) = fs::metadata(output) {
            use std::os::unix::fs::MetadataExt;
            let original = fs::metadata(&source)?;
            original.dev() == output.dev() && original.ino() == output.ino()
        } else {
            false
        };
    if same {
        return Err(Error::InvalidInput(
            "output must differ from its source files, including aliases".into(),
        ));
    }
    Ok(())
}

/// Reject result paths inside an active asset directory, including symlink aliases.
///
/// # Errors
///
/// Returns an error for invalid paths or an output inside the protected directory.
#[cfg(feature = "ocr")]
fn reject_output_in_directory(directory: &Path, output: &Path) -> Result<(), Error> {
    let directory = fs::canonicalize(directory)?;
    let resolved = if output.exists() {
        fs::canonicalize(output)?
    } else {
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::canonicalize(parent)?.join(
            output
                .file_name()
                .ok_or_else(|| Error::InvalidInput("output must name a file".into()))?,
        )
    };
    if resolved.starts_with(directory) {
        return Err(Error::InvalidInput(
            "identification output must be outside the active index and model assets".into(),
        ));
    }
    Ok(())
}

/// Protect standard OCR assets and any existing model/configuration registry paths.
///
/// # Errors
///
/// Returns an error for an output aliasing a protected file or an invalid registry.
#[cfg(feature = "ocr")]
fn protect_ocr_output(model_root: &Path, output: &Path) -> Result<(), Error> {
    let models = model_root.join("models");
    if models.is_dir() {
        reject_output_in_directory(&models, output)?;
    }
    let registry = models.join("profiles.json");
    if registry.is_file() {
        reject_source_overwrite(&registry, output)?;
        let registry: serde_json::Value = serde_json::from_reader(fs::File::open(registry)?)?;
        if let Some(profiles) = registry
            .get("profiles")
            .and_then(serde_json::Value::as_array)
        {
            for profile in profiles {
                for key in ["model", "configuration"] {
                    if let Some(path) = profile.get(key).and_then(serde_json::Value::as_str) {
                        let path = model_root.join(path);
                        if path.is_file() {
                            reject_source_overwrite(&path, output)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Read searchable card metadata independently of source reference images.
///
/// # Errors
///
/// Returns an error for an invalid catalogue, index, shard manifest or metadata I/O.
fn read_metadata(index: Option<&Path>, catalog: Option<&Path>) -> Result<Vec<CardMetadata>, Error> {
    let mut cards = Vec::new();
    if let Some(catalog) = catalog {
        mtg::for_each_reference(catalog, |reference| {
            cards.push(reference.card);
            Ok(())
        })?;
    } else if let Some(index) = index {
        if index.is_dir() {
            whosthatpokemon::matching::ShardedIndex::load(index)?
                .for_each_metadata(|card| cards.push(card.clone()))?;
        } else {
            cards.extend(CatalogIndex::load(index)?.metadata().cloned());
        }
    }
    Ok(cards)
}

/// Search either a prepared compact text index or raw legacy catalogue metadata.
enum TextSearch {
    /// Existing reference metadata, retaining legacy search semantics.
    Raw(Vec<CardMetadata>),
    /// Cached all-language metadata and retrieval evidence.
    Indexed(Box<mtg::evidence::TextIndex>),
}

impl TextSearch {
    /// Rank observed text without replacing it or claiming visual acceptance.
    fn search(
        &self,
        observations: &mtg::evidence::TextObservations,
    ) -> Vec<mtg::evidence::TextMatch> {
        match self {
            Self::Raw(cards) => mtg::evidence::search(cards, observations),
            Self::Indexed(index) => index.search(observations),
        }
    }
}

/// Open the selected metadata source once for successive OCR readings.
///
/// # Errors
///
/// Returns an error for unreadable, invalid or incompatible metadata/index files.
fn read_text_search(
    index: Option<&Path>,
    catalog: Option<&Path>,
    text_index: Option<&Path>,
) -> Result<TextSearch, Error> {
    if let Some(path) = text_index {
        Ok(TextSearch::Indexed(Box::new(
            mtg::evidence::TextIndex::load(path)?,
        )))
    } else {
        Ok(TextSearch::Raw(read_metadata(index, catalog)?))
    }
}

/// OCR and independent text ranking for one extracted scene region.
#[cfg(feature = "ocr")]
#[derive(Serialize)]
struct OcrCard {
    /// Outline in source-image pixels after EXIF orientation.
    corners: [[f32; 2]; 4],
    /// Outline area in source-image square pixels.
    area: f32,
    /// Unmodified reading and approximate field assignments in the rotated crop.
    ocr: mtg::ocr::OcrEvidence,
    /// Independent text evidence; this implies no visual acceptance decision.
    text_matches: Vec<mtg::evidence::TextMatch>,
}

/// Image dimensions and per-card OCR evidence, without an implied visual match.
#[cfg(feature = "ocr")]
#[derive(Serialize)]
struct OcrImageReport {
    /// Source width after applying EXIF orientation.
    width: u32,
    /// Source height after applying EXIF orientation.
    height: u32,
    /// Extracted card regions, or the whole image for an already-cropped input.
    cards: Vec<OcrCard>,
}

/// Load one OCR engine and reuse it across all input images and detected cards.
///
/// # Errors
///
/// Returns an error for missing/incompatible models, unavailable catalogues or
/// input images, invalid extraction configuration, failed inference or output I/O.
#[cfg(feature = "ocr")]
fn run_ocr(arguments: OcrArgs) -> Result<(), Error> {
    let metadata = read_text_search(
        arguments.index.as_deref(),
        arguments.catalog.as_deref(),
        arguments.text_index.as_deref(),
    )?;
    let engine = mtg::ocr::MagicOcr::open(&arguments.model_root, &arguments.language)?;
    let layout = match arguments.layout {
        OcrLayout::Modern => mtg::evidence::FrameLayout::Modern,
        OcrLayout::Classic => mtg::evidence::FrameLayout::Classic,
    };
    let extractor = whosthatpokemon::extraction::CardExtractor::new(ExtractorConfig {
        output_width: 500,
        output_height: 700,
        ..ExtractorConfig::default()
    })?;
    let recognize = |image: &image::DynamicImage, corners, area| -> Result<OcrCard, Error> {
        let ocr = engine.read_crop(image, layout, arguments.rotation)?;
        let text_matches = metadata.search(&ocr.observations);
        Ok(OcrCard {
            corners,
            area,
            ocr,
            text_matches,
        })
    };
    let mut reports = Vec::new();
    for input in arguments.images {
        let image = open_image(&input)?;
        let (width, height) = (image.width(), image.height());
        let cards = if arguments.cropped {
            vec![recognize(
                &image,
                [
                    [0.0, 0.0],
                    [width as f32, 0.0],
                    [width as f32, height as f32],
                    [0.0, height as f32],
                ],
                width as f32 * height as f32,
            )?]
        } else {
            extractor
                .extract(&image)?
                .into_iter()
                .map(|card| {
                    recognize(
                        &image::DynamicImage::ImageRgb8(card.image),
                        card.detection.corners.map(|point| [point.x, point.y]),
                        card.detection.area,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        reports.push(ImageResult {
            input,
            result: OcrImageReport {
                width,
                height,
                cards,
            },
        });
    }
    write_json(&reports, arguments.output.as_deref())
}

#[cfg(feature = "ocr")]
use mtg::recognition::IdentifiedCard;

/// One complete image's detections, identifications and elapsed processing times.
#[cfg(feature = "ocr")]
#[derive(Serialize)]
struct IdentifiedImage {
    /// Source width after EXIF orientation.
    width: u32,
    /// Source height after EXIF orientation.
    height: u32,
    /// Image loading and orientation time in milliseconds.
    decode_ms: f64,
    /// Outline detection and perspective extraction time; zero for cropped input.
    extraction_ms: f64,
    /// Decode, extraction, OCR and matching elapsed time in milliseconds.
    total_ms: f64,
    /// Identified outlines; an empty list means no outline was detected.
    cards: Vec<IdentifiedCard>,
}

/// Reuse the visual metadata, text postings and OCR engine across a batch of images.
///
/// # Errors
///
/// Returns an error for invalid matching limits, unavailable indexes or models,
/// failed image loading/extraction/OCR/matching, or unwritable output.
#[cfg(feature = "ocr")]
fn run_identify(arguments: IdentifyArgs) -> Result<(), Error> {
    let options = MatchOptions {
        top_k: arguments.top_k,
        min_similarity: arguments.min_similarity,
        min_margin: arguments.min_margin,
    };
    options.validate()?;
    if arguments.ocr_threads == 0
        || arguments.title_candidates == 0
        || arguments.max_candidates == 0
        || arguments.max_geometry == 0
    {
        return Err(Error::InvalidOptions(
            "OCR threads and title, descriptor and geometry candidate budgets must be positive"
                .into(),
        ));
    }
    if let Some(output) = &arguments.output {
        for input in arguments.images.iter().chain(arguments.text_index.iter()) {
            reject_source_overwrite(input, output)?;
        }
        reject_output_in_directory(&arguments.index, output)?;
        protect_ocr_output(&arguments.model_root, output)?;
    }
    let started = Instant::now();
    let index = whosthatpokemon::matching::ShardedIndex::load(&arguments.index)?;
    let visual_load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let (title_source, text): (&str, Box<dyn mtg::evidence::TitleProposer>) =
        if let Some(path) = &arguments.text_index {
            (
                "text_index",
                Box::new(mtg::evidence::TextIndex::load(path)?),
            )
        } else {
            (
                "visual_metadata",
                Box::new(mtg::evidence::TitleIndex::from_visual_index(&index)?),
            )
        };
    let title_index_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let footer_index = mtg::evidence::FooterIndex::from_visual_index(&index)?;
    let footer_index_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let engine = match arguments.title_model {
        TitleModel::Small => mtg::ocr::MagicOcr::open_title_small_with_threads(
            &arguments.model_root,
            &arguments.language,
            Some(arguments.ocr_threads),
        )?,
        TitleModel::Default => mtg::ocr::MagicOcr::open_title_with_threads(
            &arguments.model_root,
            &arguments.language,
            Some(arguments.ocr_threads),
        )?,
    };
    let ocr_load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let layout = match arguments.layout {
        OcrLayout::Modern => mtg::evidence::FrameLayout::Modern,
        OcrLayout::Classic => mtg::evidence::FrameLayout::Classic,
    };
    let extractor = whosthatpokemon::extraction::CardExtractor::new(ExtractorConfig {
        output_width: 500,
        output_height: 700,
        ..ExtractorConfig::default()
    })?;
    if let Some(directory) = &arguments.trace_dir {
        fs::create_dir(directory)?;
    }
    let recognize = |image: &image::DynamicImage, corners, area| {
        mtg::recognition::identify_crop(
            &engine,
            image,
            layout,
            arguments.rotation,
            corners,
            area,
            &whosthatpokemon::matching::CandidateOptions {
                max_descriptors: arguments.max_candidates,
                max_geometry: arguments.max_geometry,
                ..Default::default()
            },
            |observations, candidate_options| {
                mtg::identify::identify_fields_with(
                    &index,
                    text.as_ref(),
                    &footer_index,
                    image,
                    observations,
                    &options,
                    arguments.title_candidates,
                    candidate_options,
                )
            },
        )
    };
    let mut reports = Vec::new();
    for (image_number, input) in arguments.images.into_iter().enumerate() {
        let started = Instant::now();
        let image = open_image(&input)?;
        let decode_ms = started.elapsed().as_secs_f64() * 1000.0;
        let (width, height) = (image.width(), image.height());
        let extraction_started = Instant::now();
        let mut trace_crops = Vec::new();
        let (cards, extraction_ms) = if arguments.cropped {
            if arguments.trace_dir.is_some() {
                trace_crops.push(image.clone());
            }
            (
                vec![recognize(
                    &image,
                    [
                        [0.0, 0.0],
                        [width as f32, 0.0],
                        [width as f32, height as f32],
                        [0.0, height as f32],
                    ],
                    width as f32 * height as f32,
                )?],
                0.0,
            )
        } else {
            let extracted = extractor.extract(&image)?;
            let extraction_ms = extraction_started.elapsed().as_secs_f64() * 1000.0;
            let cards = extracted
                .into_iter()
                .map(|card| {
                    let crop = image::DynamicImage::ImageRgb8(card.image);
                    if arguments.trace_dir.is_some() {
                        trace_crops.push(crop.clone());
                    }
                    recognize(
                        &crop,
                        card.detection.corners.map(|point| [point.x, point.y]),
                        card.detection.area,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            (cards, extraction_ms)
        };
        let result = IdentifiedImage {
            width,
            height,
            decode_ms,
            extraction_ms,
            total_ms: started.elapsed().as_secs_f64() * 1000.0,
            cards,
        };
        if let Some(directory) = &arguments.trace_dir {
            write_identification_trace(directory, image_number, &trace_crops, &result)?;
        }
        reports.push(ImageResult { input, result });
    }
    write_json(
        &serde_json::json!({
            "build": {
                "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "crate_version": env!("CARGO_PKG_VERSION"),
            },
            "initialization_ms": {
                "visual_index": visual_load_ms,
                "title_index": title_index_ms,
                "footer_index": footer_index_ms,
                "ocr": ocr_load_ms,
            },
            "title_source": title_source,
            "images": reports,
        }),
        arguments.output.as_deref(),
    )
}

/// Save rectified pixels, oriented field crops and the exact footer recognizer input.
///
/// The caller supplies a newly created directory. Files are created exclusively,
/// preserving existing inputs even if a generated filename aliases one of them.
/// This performs no extraction, recognition, matching or orientation selection.
///
/// # Errors
///
/// Returns an error if a trace file already exists or PNG encoding/writing fails.
#[cfg(feature = "ocr")]
fn write_identification_trace(
    directory: &Path,
    image_number: usize,
    crops: &[image::DynamicImage],
    result: &IdentifiedImage,
) -> Result<(), Error> {
    for (card_number, (crop, card)) in crops.iter().zip(&result.cards).enumerate() {
        let prefix = format!("image-{image_number}-card-{card_number}");
        save_trace_png(crop, &directory.join(format!("{prefix}-crop.png")))?;
        for (attempt_number, attempt) in card.attempts.iter().enumerate() {
            write_identification_attempt_trace(
                directory,
                &format!("{prefix}-attempt-{attempt_number}"),
                crop,
                attempt.ocr.rotation_degrees,
                attempt.ocr.crop_bounds,
            )?;
            let oriented = if attempt.footer_ocr.rotation_degrees == 180 {
                crop.rotate180()
            } else {
                crop.clone()
            };
            let [x, y, width, height] = attempt.footer_ocr.crop_bounds;
            save_trace_png(
                &oriented.crop_imm(x, y, width, height),
                &directory.join(format!("{prefix}-attempt-{attempt_number}-footer.png")),
            )?;
            save_trace_png(
                &mtg::ocr::render_footer_input(crop, &attempt.footer_ocr)?,
                &directory.join(format!(
                    "{prefix}-attempt-{attempt_number}-footer-input.png"
                )),
            )?;
            for (row_number, row) in attempt.footer_ocr.rows.iter().enumerate() {
                let [x, y, width, height] = row.crop_bounds;
                save_trace_png(
                    &oriented.crop_imm(x, y, width, height),
                    &directory.join(format!(
                        "{prefix}-attempt-{attempt_number}-footer-row-{row_number}.png"
                    )),
                )?;
            }
        }
    }
    Ok(())
}

/// Export a recorded orientation and its actual title rectangle without choosing an orientation.
///
/// # Errors
///
/// Returns an error for an existing destination or failed PNG encoding/writing.
#[cfg(feature = "ocr")]
fn write_identification_attempt_trace(
    directory: &Path,
    prefix: &str,
    crop: &image::DynamicImage,
    rotation: u16,
    bounds: [u32; 4],
) -> Result<(), Error> {
    let oriented = if rotation == 180 {
        crop.rotate180()
    } else {
        crop.clone()
    };
    let [x, y, width, height] = bounds;
    save_trace_png(
        &oriented.crop_imm(x, y, width, height),
        &directory.join(format!("{prefix}-title.png")),
    )?;
    save_trace_png(&oriented, &directory.join(format!("{prefix}-oriented.png")))
}

/// Encode one trace PNG, creating exclusively and reporting buffered write errors.
///
/// # Errors
///
/// Returns an error for an existing destination or failed PNG encoding/writing.
#[cfg(feature = "ocr")]
fn save_trace_png(image: &image::DynamicImage, path: &Path) -> Result<(), Error> {
    let file = fs::File::create_new(path)?;
    let mut writer = io::BufWriter::new(file);
    image.write_to(&mut writer, image::ImageFormat::Png)?;
    writer.flush()?;
    Ok(())
}

/// Resource and recovery choices for native reference enrollment.
struct IndexWork {
    /// Maximum physical faces retained in each descriptor shard.
    shard_size: Option<usize>,
    /// Recover completed shards from an interrupted enrollment.
    resume: bool,
    /// Concurrent independent descriptor-extraction workers.
    workers: usize,
    /// Per-face deadline for a missing image to appear in the local cache.
    wait_for_images: Duration,
}

/// Resolve a local reference, optionally waiting for another process to publish it.
///
/// Path discovery is retried; image decoding remains a single separate operation.
/// The helper performs no downloads and never waits for an existing corrupt image.
///
/// # Errors
///
/// Returns the original missing-image error when waiting is disabled, or a
/// contextual timeout error when the per-face deadline expires.
fn reference_path_when_ready(
    reference: &mtg::Reference,
    images: &Path,
    timeout: Duration,
) -> Result<PathBuf, Error> {
    let started = Instant::now();
    loop {
        match mtg::reference_path(reference, images) {
            Ok(path) => return Ok(path),
            Err(error @ Error::InvalidCatalog(_)) if !timeout.is_zero() => {
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(Error::InvalidCatalog(format!(
                        "timed out after {} seconds waiting for a local reference image: {error}; completed shards can be resumed with --resume",
                        timeout.as_secs_f64()
                    )));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(250)));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Build a game-specific index using the selected visual implementation.
///
/// # Errors
///
/// Returns an error for invalid metadata, unreadable images, a failed model or
/// runtime, unsupported options, or an unwritable index destination.
fn build_index(
    catalog: &Path,
    images: &Path,
    output: &Path,
    matching: &BackendArgs,
    work: IndexWork,
) -> Result<usize, Error> {
    match matching.backend {
        Backend::Portable => {
            validate_portable(matching)?;
            if let Some(shard_size) = work.shard_size {
                let mut builder = if work.resume {
                    whosthatpokemon::matching::ShardedIndexBuilder::resume(output)?
                } else {
                    whosthatpokemon::matching::ShardedIndexBuilder::new(output, shard_size)?
                };
                let mut batch = Vec::with_capacity(work.workers * 2);
                mtg::for_each_reference(catalog, |reference| {
                    if builder.contains(&reference.card) {
                        return Ok(());
                    }
                    let path = reference_path_when_ready(&reference, images, work.wait_for_images)?;
                    let image = open_image(&path)?;
                    batch.push((reference.card, image));
                    if batch.len() >= work.workers * 2 {
                        builder.add_batch(std::mem::take(&mut batch), work.workers)?;
                    }
                    Ok(())
                })?;
                builder.add_batch(batch, work.workers)?;
                return Ok(builder.finish()?.len());
            }
            let index = mtg::build_index(catalog, images)?;
            index.save(output)?;
            Ok(index.len())
        }
        #[cfg(feature = "clip")]
        Backend::Clip => {
            if work.shard_size.is_some() {
                return Err(Error::InvalidOptions(
                    "--shard-size currently requires the portable backend".into(),
                ));
            }
            let encoder = clip_encoder(matching)?;
            let mut index = whosthatpokemon::clip::ClipIndex::new(encoder);
            for reference in mtg::read_references(catalog)? {
                let image = open_image(&mtg::reference_path(&reference, images)?)?;
                index.add(reference.card, &image)?;
            }
            index.save(output)?;
            Ok(index.len())
        }
    }
}

/// Load the selected matcher and validate any model/index compatibility contract.
///
/// # Errors
///
/// Returns an error for corrupt/incompatible indexes, invalid options, missing
/// model/runtime files, or failed model initialization.
fn load_index(path: &Path, matching: &BackendArgs) -> Result<Box<dyn Matcher>, Error> {
    match matching.backend {
        Backend::Portable => {
            validate_portable(matching)?;
            if path.is_dir() {
                Ok(Box::new(whosthatpokemon::matching::ShardedIndex::load(
                    path,
                )?))
            } else {
                Ok(Box::new(CatalogIndex::load(path)?))
            }
        }
        #[cfg(feature = "clip")]
        Backend::Clip => Ok(Box::new(whosthatpokemon::clip::ClipIndex::load(
            path,
            clip_encoder(matching)?,
        )?)),
    }
}

/// Reject external asset options that have no meaning for the portable matcher.
///
/// # Errors
///
/// Returns [`Error::InvalidOptions`] if model/runtime paths accompany a portable index.
fn validate_portable(_matching: &BackendArgs) -> Result<(), Error> {
    #[cfg(feature = "clip")]
    if _matching.model.is_some() || _matching.runtime.is_some() {
        return Err(Error::InvalidOptions(
            "--model and --runtime require --backend clip".into(),
        ));
    }
    Ok(())
}

/// Initialize the explicitly selected CLIP encoder and dynamic runtime.
///
/// # Errors
///
/// Returns an error for missing paths or model/runtime incompatibility.
#[cfg(feature = "clip")]
fn clip_encoder(matching: &BackendArgs) -> Result<whosthatpokemon::clip::ClipEncoder, Error> {
    let model = matching
        .model
        .as_deref()
        .ok_or_else(|| Error::InvalidOptions("--backend clip requires --model".into()))?;
    let runtime = matching
        .runtime
        .as_deref()
        .ok_or_else(|| Error::InvalidOptions("--backend clip requires --runtime".into()))?;
    whosthatpokemon::clip::ClipEncoder::load(model, runtime, Default::default())
}

/// Decode and honor EXIF orientation before reporting coordinates.
///
/// # Errors
///
/// Returns an error for inaccessible input, unsupported codecs, or invalid bytes.
fn open_image(path: &Path) -> Result<image::DynamicImage, Error> {
    whosthatpokemon::open_image(path)
}

/// Write a complete JSON document to a file or standard output.
///
/// # Errors
///
/// Returns an error for serialization failures or inaccessible output streams.
fn write_json(value: &impl Serialize, output: Option<&Path>) -> Result<(), Error> {
    if let Some(path) = output {
        // Replacing a directory entry also preserves a source inode if a result
        // destination was accidentally hard-linked to an otherwise unlisted asset.
        use std::sync::atomic::{AtomicU64, Ordering};
        /// Process-local discriminator for sibling output temporary files.
        static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(0);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let temporary = parent.join(format!(
            ".whosthatpokemon-result-{}-{}.tmp",
            std::process::id(),
            NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result: Result<(), Error> = (|| {
            let mut writer = io::BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, value)?;
            writer.flush()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result?;
    } else {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, value)?;
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

/// CLI contracts use synthetic inputs without model downloads or network requests.
#[cfg(test)]
mod tests {
    use super::*;

    /// Trace images preserve upright input and show the recorded opposite orientation exactly.
    #[cfg(feature = "ocr")]
    #[test]
    fn identification_trace_exports_actual_orientation_and_title_pixels() {
        let directory = TemporaryImages::new();
        let crop = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(2, 3, |x, y| {
            image::Rgb([(y * 2 + x) as u8, 0, 0])
        }));
        write_identification_attempt_trace(&directory.0, "upright", &crop, 0, [0, 0, 2, 1])
            .unwrap();
        write_identification_attempt_trace(&directory.0, "inverted", &crop, 180, [0, 0, 2, 1])
            .unwrap();
        let upright = image::open(directory.0.join("upright-oriented.png")).unwrap();
        assert_eq!(upright.to_rgb8(), crop.to_rgb8());
        let title = image::open(directory.0.join("inverted-title.png"))
            .unwrap()
            .to_rgb8();
        assert_eq!(title.dimensions(), (2, 1));
        assert_eq!(title.get_pixel(0, 0).0, [5, 0, 0]);
        assert_eq!(title.get_pixel(1, 0).0, [4, 0, 0]);
        assert!(
            write_identification_attempt_trace(&directory.0, "upright", &crop, 180, [0, 0, 2, 1])
                .is_err()
        );
        assert_eq!(
            image::open(directory.0.join("upright-oriented.png"))
                .unwrap()
                .to_rgb8(),
            crop.to_rgb8()
        );
    }

    /// Asset-directory aliases and custom registry paths cannot be result destinations.
    #[cfg(feature = "ocr")]
    #[test]
    fn identify_protects_gallery_and_model_assets() {
        let directory = TemporaryImages::new();
        let models = directory.0.join("models");
        let weights = directory.0.join("custom.onnx");
        fs::create_dir_all(&models).unwrap();
        fs::write(&weights, b"model").unwrap();
        fs::write(
            models.join("profiles.json"),
            br#"{"profiles":[{"model":"custom.onnx"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            protect_ocr_output(&directory.0, &weights),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            protect_ocr_output(&directory.0, &models.join("new.json")),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            reject_output_in_directory(&models, &models.join("../models/new.json")),
            Err(Error::InvalidInput(_))
        ));
        protect_ocr_output(&directory.0, &directory.0.join("result.json")).unwrap();
        #[cfg(unix)]
        {
            let alias = directory.0.join("alias");
            std::os::unix::fs::symlink(&models, &alias).unwrap();
            assert!(matches!(
                reject_output_in_directory(&models, &alias.join("new.json")),
                Err(Error::InvalidInput(_))
            ));
        }
        assert_eq!(fs::read(weights).unwrap(), b"model");
    }

    /// Failed serialization preserves the previous result rather than publishing partial JSON.
    #[test]
    fn output_failure_keeps_previous_result() {
        let directory = TemporaryImages::new();
        let output = directory.0.join("result.json");
        fs::write(&output, b"previous result").unwrap();
        let invalid = std::collections::BTreeMap::from([((1_u8, 2_u8), 3_u8)]);
        assert!(write_json(&invalid, Some(&output)).is_err());
        assert_eq!(fs::read(output).unwrap(), b"previous result");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    /// Hard-linked inputs are rejected; replacing another alias preserves its source inode.
    #[cfg(unix)]
    #[test]
    fn output_does_not_truncate_hardlinked_sources() {
        let directory = TemporaryImages::new();
        let source = directory.0.join("source");
        let alias = directory.0.join("alias.json");
        fs::write(&source, b"original bytes").unwrap();
        fs::hard_link(&source, &alias).unwrap();
        assert!(matches!(
            reject_source_overwrite(&source, &alias),
            Err(Error::InvalidInput(_))
        ));
        write_json(&serde_json::json!({"result": true}), Some(&alias)).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"original bytes");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(alias).unwrap()).unwrap(),
            serde_json::json!({"result": true})
        );
    }

    /// Invalid budgets and thresholds fail before opening indexes or OCR models.
    #[cfg(feature = "ocr")]
    #[test]
    fn identify_validates_limits_before_loading_assets() {
        let base = [
            "app",
            "mtg",
            "identify",
            "--index",
            "missing-gallery",
            "--model-root",
            "missing-models",
            "image.png",
        ];
        for (flag, invalid) in [
            ("--title-candidates", "0"),
            ("--max-candidates", "0"),
            ("--max-geometry", "0"),
            ("--top-k", "0"),
            ("--min-similarity", "NaN"),
            ("--min-margin", "1.1"),
        ] {
            let cli = Cli::try_parse_from(base.into_iter().chain([flag, invalid])).unwrap();
            assert!(matches!(run(cli), Err(Error::InvalidOptions(_))));
        }
        assert!(Cli::try_parse_from(base.into_iter().chain(["--rotation", "45"])).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--rotation", "90"])).is_err());
    }

    /// A requested result destination cannot overwrite its source photograph.
    #[cfg(feature = "ocr")]
    #[test]
    fn identify_output_preserves_input_before_loading_models() {
        let directory = TemporaryImages::new();
        let input = directory.0.join("input.png");
        let original = b"source bytes are not an output document";
        fs::write(&input, original).unwrap();
        let cli = Cli::try_parse_from([
            std::ffi::OsString::from("app"),
            "mtg".into(),
            "identify".into(),
            "--index".into(),
            "missing-gallery".into(),
            "--text-index".into(),
            "missing-text".into(),
            "--model-root".into(),
            "missing-models".into(),
            "--output".into(),
            input.as_os_str().to_owned(),
            input.as_os_str().to_owned(),
        ])
        .unwrap();
        assert!(matches!(run(cli), Err(Error::InvalidInput(_))));
        assert_eq!(fs::read(&input).unwrap(), original);
    }

    /// Exactly one of three metadata sources is required for independent text search.
    #[test]
    fn text_search_selects_one_metadata_source() {
        for source in ["--index", "--catalog", "--text-index"] {
            assert!(
                Cli::try_parse_from(["app", "mtg", "search", source, "local", "--title", "Bolt"])
                    .is_ok()
            );
        }
        assert!(Cli::try_parse_from(["app", "mtg", "search", "--title", "Bolt"]).is_err());
        for sources in [
            ["--index", "--catalog"],
            ["--index", "--text-index"],
            ["--catalog", "--text-index"],
        ] {
            assert!(
                Cli::try_parse_from(["app", "mtg", "search", sources[0], "one", sources[1], "two"])
                    .is_err()
            );
        }
    }

    /// An accidental text-index output cannot destroy the source catalogue.
    #[test]
    fn text_index_preserves_source_at_output_path() {
        let directory =
            std::env::temp_dir().join(format!("whosthatpokemon-cli-source-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("cards.jsonl");
        let original = b"{\"id\":\"a\",\"name\":\"A\"}\n";
        fs::write(&source, original).unwrap();
        let cli = Cli::try_parse_from([
            std::ffi::OsString::from("app"),
            "mtg".into(),
            "text-index".into(),
            "--catalog".into(),
            source.as_os_str().to_owned(),
            "--output".into(),
            source.as_os_str().to_owned(),
        ])
        .unwrap();
        assert!(matches!(run(cli), Err(Error::InvalidInput(_))));
        assert_eq!(fs::read(&source).unwrap(), original);
        fs::remove_dir_all(directory).unwrap();
    }

    /// Waiting is explicit, bounded, and restricted to resumable sharded enrollment.
    #[test]
    fn image_wait_bounds_require_a_sharded_build_and_default_to_zero() {
        let base = [
            "app",
            "mtg",
            "index",
            "--catalog",
            "cards",
            "--images",
            "images",
            "--output",
            "index",
        ];
        let default = Cli::try_parse_from(base).unwrap();
        let Command::Mtg {
            command: MtgCommand::Index {
                wait_for_images, ..
            },
        } = default.command
        else {
            panic!("expected index command");
        };
        assert_eq!(wait_for_images, 0);
        assert!(Cli::try_parse_from(base.into_iter().chain(["--wait-for-images", "900"])).is_err());
        for invalid in ["-1", "86401", "4294967296", "unbounded"] {
            assert!(
                Cli::try_parse_from(base.into_iter().chain([
                    "--shard-size",
                    "256",
                    "--wait-for-images",
                    invalid
                ]))
                .is_err()
            );
        }
        for valid in ["0", "900", "86400"] {
            assert!(
                Cli::try_parse_from(base.into_iter().chain([
                    "--shard-size",
                    "256",
                    "--wait-for-images",
                    valid
                ]))
                .is_ok()
            );
        }
    }

    /// Unique, generic local artifacts are removed even if an image-wait test fails.
    struct TemporaryImages(PathBuf);

    impl TemporaryImages {
        /// Create a private temporary directory without relying on user photographs.
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "whosthatpokemon-cli-wait-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryImages {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Atomic publication resolves missing paths; existing invalid bytes fail decoding once.
    #[test]
    fn image_wait_handles_publication_timeout_and_existing_corrupt_files() {
        let directory = TemporaryImages::new();
        let reference = mtg::Reference {
            card: CardMetadata {
                game: "mtg".into(),
                id: "synthetic".into(),
                name: "Synthetic reference".into(),
                face_index: 1,
                ..CardMetadata::default()
            },
            file: None,
            image_url: None,
        };
        let original = mtg::reference_path(&reference, &directory.0)
            .unwrap_err()
            .to_string();
        assert_eq!(
            reference_path_when_ready(&reference, &directory.0, Duration::ZERO)
                .unwrap_err()
                .to_string(),
            original
        );
        let timeout =
            reference_path_when_ready(&reference, &directory.0, Duration::from_millis(10))
                .unwrap_err()
                .to_string();
        assert!(timeout.contains("timed out"));
        assert!(timeout.contains("synthetic face 1"));
        assert!(timeout.contains("--resume"));
        let path = directory.0.join("synthetic-1.png");
        let published = path.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            let temporary = published.with_extension("png.tmp");
            fs::write(&temporary, b"generic invalid image bytes").unwrap();
            fs::rename(temporary, published).unwrap();
        });
        assert_eq!(
            reference_path_when_ready(&reference, &directory.0, Duration::from_secs(2)).unwrap(),
            path
        );
        worker.join().unwrap();
        assert!(matches!(open_image(&path), Err(Error::Image(_))));
        // A second resolution also immediately returns that existing corrupt file;
        // path waiting neither decodes it nor treats corruption as a pending download.
        assert_eq!(
            reference_path_when_ready(&reference, &directory.0, Duration::from_secs(2)).unwrap(),
            path
        );
    }

    /// A deadline failure leaves earlier completed shards available for explicit resume.
    #[test]
    fn image_wait_timeout_preserves_completed_enrollment_checkpoint() {
        let directory = TemporaryImages::new();
        let output = directory.0.join("index");
        let catalog = directory.0.join("catalog.json");
        let mut builder = whosthatpokemon::matching::ShardedIndexBuilder::new(&output, 1).unwrap();
        let pixels = image::RgbImage::from_fn(64, 88, |x, y| {
            image::Rgb([
                (x.wrapping_mul(17) ^ y.wrapping_mul(31)) as u8,
                (x.wrapping_mul(53) ^ y.wrapping_mul(7)) as u8,
                (x.wrapping_mul(11) ^ y.wrapping_mul(47)) as u8,
            ])
        });
        builder
            .add(
                CardMetadata {
                    game: "mtg".into(),
                    id: "complete".into(),
                    name: "Synthetic complete".into(),
                    ..CardMetadata::default()
                },
                &image::DynamicImage::ImageRgb8(pixels),
            )
            .unwrap();
        drop(builder);
        fs::write(&catalog, br#"[{"id":"complete","name":"Synthetic complete"},{"id":"missing","name":"Synthetic missing"}]"#).unwrap();
        let before = fs::read(output.join("checkpoint.json")).unwrap();
        let work = IndexWork {
            shard_size: Some(1),
            resume: true,
            workers: 1,
            wait_for_images: Duration::from_millis(10),
        };
        let cli = Cli::try_parse_from([
            "app",
            "mtg",
            "index",
            "--catalog",
            "unused",
            "--images",
            "unused",
            "--output",
            "unused",
        ])
        .unwrap();
        let Command::Mtg {
            command: MtgCommand::Index { matching, .. },
        } = cli.command
        else {
            panic!("expected index command");
        };
        let error = build_index(&catalog, &directory.0, &output, &matching, work)
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("missing face 0"), "{error}");
        assert_eq!(fs::read(output.join("checkpoint.json")).unwrap(), before);
        assert!(!output.join("manifest.json").exists());
        assert_eq!(
            whosthatpokemon::matching::ShardedIndexBuilder::resume(&output)
                .unwrap()
                .len(),
            1
        );
    }

    /// Parallel acquisition and resumable enrollment validate work boundaries at parsing.
    #[test]
    fn worker_bounds_and_resume_require_a_sharded_build() {
        for workers in ["0", "17"] {
            assert!(
                Cli::try_parse_from([
                    "app",
                    "mtg",
                    "fetch-images",
                    "--plan",
                    "plan",
                    "--images",
                    "images",
                    "--workers",
                    workers
                ])
                .is_err()
            );
        }
        assert!(
            Cli::try_parse_from([
                "app",
                "mtg",
                "fetch-images",
                "--plan",
                "plan",
                "--images",
                "images",
                "--workers",
                "16",
                "--limit",
                "0"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "app",
                "mtg",
                "fetch-images",
                "--plan",
                "plan",
                "--images",
                "images",
                "--workers",
                "8",
                "--all"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "app",
                "mtg",
                "fetch-images",
                "--plan",
                "plan",
                "--images",
                "images",
                "--all",
                "--limit",
                "1"
            ])
            .is_err()
        );
        let base = [
            "app",
            "mtg",
            "index",
            "--catalog",
            "cards",
            "--images",
            "images",
            "--output",
            "index",
        ];
        assert!(Cli::try_parse_from(base.into_iter().chain(["--resume"])).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--workers", "4"])).is_err());
        assert!(
            Cli::try_parse_from(base.into_iter().chain([
                "--shard-size",
                "256",
                "--resume",
                "--workers",
                "4"
            ]))
            .is_ok()
        );
    }
}
