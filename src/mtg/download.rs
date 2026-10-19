//! Explicit, cached Scryfall downloads for subsequent offline indexing.

use crate::{Error, mtg::read_references};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A bounded reference collection saved for subsequent offline indexing.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FetchReport {
    /// Metadata file accepted by [`super::build_index`].
    pub catalog: PathBuf,
    /// Directory of canonical PNG reference faces.
    pub images: PathBuf,
    /// Number of selected printings, which can have multiple physical faces.
    pub cards: usize,
    /// Number of physical image faces.
    pub faces: usize,
    /// Whether this completed request was reused without network access.
    pub cached: bool,
    /// Whether the explicit limit selected only part of the search results.
    pub truncated: bool,
}

/// Parameters and timestamp of a completed, reusable download.
#[derive(Debug, Deserialize, Serialize)]
struct CacheRecord {
    /// Exact requested search expression.
    query: String,
    /// Requested upper limit on printing records.
    limit: usize,
    /// Completion time in Unix seconds.
    created: u64,
    /// Completed download locations and counts.
    report: FetchReport,
}

/// Fetch a bounded query and its physical reference faces from Scryfall.
///
/// Search requests are separated by at least 550 ms. HTTP errors, including 429,
/// stop without retrying. Completed identical requests are cached for 24 hours;
/// image files are reused. Indexing and scanning perform no network activity.
///
/// # Errors
///
/// Returns an error for an empty query, zero limit, invalid responses, missing
/// reference URLs, unsupported images, failed network requests, unavailable
/// system time, or inaccessible output files. Failed downloads do not replace
/// the last completed catalogue.
pub fn fetch(query: &str, output: &Path, limit: usize) -> Result<FetchReport, Error> {
    if query.trim().is_empty() || limit == 0 {
        return Err(Error::InvalidInput(
            "fetch needs a query and a positive limit".into(),
        ));
    }
    fs::create_dir_all(output)?;
    let output = fs::canonicalize(output)?;
    let images = output.join("images");
    fs::create_dir_all(&images)?;
    let cache_path = output.join("fetch-cache.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Download(error.to_string()))?
        .as_secs();
    if let Ok(file) = fs::File::open(&cache_path)
        && let Ok(cache) = serde_json::from_reader::<_, CacheRecord>(file)
        && cache.query == query
        && cache.limit == limit
        && cache.created <= now
        && now - cache.created < 86_400
        && cache.report.catalog == output.join("cards.json")
        && cache.report.images == images
        && cache.report.catalog.is_file()
        && cached_images_exist(&cache.report)?
    {
        return Ok(FetchReport {
            cached: true,
            ..cache.report
        });
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(45)))
        .user_agent(concat!(
            "whosthatpokemon/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/cmeister2/whosthatpokemon)"
        ))
        .build()
        .into();
    let mut page = agent
        .get("https://api.scryfall.com/cards/search")
        .query("q", query)
        .query("unique", "prints")
        .header("Accept", "application/json")
        .call()
        .map_err(download_error)?
        .body_mut()
        .read_json::<Value>()
        .map_err(download_error)?;
    let mut cards = Vec::new();
    let truncated;
    loop {
        let data = page["data"]
            .as_array()
            .ok_or_else(|| Error::Download("search response has no data array".into()))?;
        let remaining = limit - cards.len();
        cards.extend(data.iter().take(remaining).cloned());
        let has_more = page["has_more"].as_bool().unwrap_or(false);
        if cards.len() == limit || !has_more {
            truncated = data.len() > remaining || has_more;
            break;
        }
        let next = page["next_page"]
            .as_str()
            .ok_or_else(|| Error::Download("paginated response has no next_page".into()))?;
        if !next.starts_with("https://api.scryfall.com/") {
            return Err(Error::Download("unexpected pagination URL".into()));
        }
        thread::sleep(Duration::from_millis(550));
        page = agent
            .get(next)
            .header("Accept", "application/json")
            .call()
            .map_err(download_error)?
            .body_mut()
            .read_json::<Value>()
            .map_err(download_error)?;
    }
    if cards.is_empty() {
        return Err(Error::Download("the query selected no references".into()));
    }
    let pending = output.join("cards.pending.json");
    serde_json::to_writer(
        fs::File::create(&pending)?,
        &serde_json::json!({
            "cards": cards,
            "selection": {"query": query, "limit": limit, "truncated": truncated},
        }),
    )?;
    let references = read_references(&pending)?;
    for reference in &references {
        let path = images.join(format!(
            "{}-{}.png",
            reference.card.id, reference.card.face_index
        ));
        if path.is_file() {
            image::open(&path)?;
            continue;
        }
        let url = reference
            .image_url
            .as_deref()
            .ok_or_else(|| Error::Download(format!("no image URL for {}", reference.card.id)))?;
        if !url.starts_with("https://cards.scryfall.io/")
            && !url.starts_with("https://img.scryfall.com/")
        {
            return Err(Error::Download("unexpected Scryfall image URL".into()));
        }
        let bytes = agent
            .get(url)
            .header("Accept", "image/png,image/webp,image/jpeg")
            .call()
            .map_err(download_error)?
            .body_mut()
            .with_config()
            .limit(20 * 1024 * 1024)
            .read_to_vec()
            .map_err(download_error)?;
        let decoded = image::load_from_memory(&bytes)?;
        let temporary = path.with_extension("pending");
        decoded.save_with_format(&temporary, image::ImageFormat::Png)?;
        fs::rename(temporary, path)?;
    }
    let catalog = output.join("cards.json");
    fs::rename(&pending, &catalog)?;
    let report = FetchReport {
        catalog,
        images,
        cards: cards.len(),
        faces: references.len(),
        cached: false,
        truncated,
    };
    let cache = CacheRecord {
        query: query.into(),
        limit,
        created: now,
        report: report.clone(),
    };
    serde_json::to_writer_pretty(fs::File::create(cache_path)?, &cache)?;
    Ok(report)
}

/// Check that every cached physical face still exists.
///
/// # Errors
///
/// Returns an error for unreadable or invalid cached metadata.
fn cached_images_exist(report: &FetchReport) -> Result<bool, Error> {
    let references = read_references(&report.catalog)?;
    if references.len() != report.faces {
        return Ok(false);
    }
    for reference in references {
        let path = report.images.join(format!(
            "{}-{}.png",
            reference.card.id, reference.card.face_index
        ));
        if !path.is_file() {
            return Ok(false);
        }
        image::open(&path).map_err(|error| {
            Error::InvalidCatalog(format!(
                "cached reference {} is not a valid image: {error}",
                path.display()
            ))
        })?;
    }
    Ok(true)
}

/// Retain HTTP error details, including rate-limit status codes.
fn download_error(error: ureq::Error) -> Error {
    Error::Download(error.to_string())
}
