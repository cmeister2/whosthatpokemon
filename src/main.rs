//! Command-line indexing and scanning for trading-card images.

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};
use whosthatpokemon::{
    CatalogIndex, Error, ExtractorConfig, MatchOptions, Matcher, RecognitionPipeline, image, mtg,
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
        /// Portable visual index containing searchable card metadata.
        #[arg(long)]
        index: PathBuf,
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
                title,
                footer,
                rules,
            } => {
                let index = CatalogIndex::load(index)?;
                let observations = mtg::evidence::TextObservations {
                    title,
                    footer,
                    rules,
                };
                write_json(
                    &mtg::evidence::search(index.metadata(), &observations),
                    None,
                )
            }
            MtgCommand::Index {
                catalog,
                images,
                output,
                matching,
            } => {
                let references = build_index(&catalog, &images, &output, &matching)?;
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
) -> Result<usize, Error> {
    match matching.backend {
        Backend::Portable => {
            validate_portable(matching)?;
            let index = mtg::build_index(catalog, images)?;
            index.save(output)?;
            Ok(index.len())
        }
        #[cfg(feature = "clip")]
        Backend::Clip => {
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
            Ok(Box::new(CatalogIndex::load(path)?))
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
        serde_json::to_writer_pretty(fs::File::create(path)?, value)?;
    } else {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, value)?;
        stdout.write_all(b"\n")?;
    }
    Ok(())
}
