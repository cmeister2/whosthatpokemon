//! Offline validation of explicit-download parameters and completed cache reuse.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use whosthatpokemon::mtg::download::{FetchReport, fetch};
use whosthatpokemon::{Error, mtg};

/// Unique directory suffixes for concurrently running tests in this process.
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Temporary local catalogues and image files removed after each test.
struct Fixture {
    /// Absolute directory selected as the explicit fetch destination.
    directory: PathBuf,
}

impl Fixture {
    /// Create a local test destination without contacting an external service.
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "whosthatpokemon-download-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create temporary destination");
        Self {
            directory: fs::canonicalize(directory).expect("resolve destination"),
        }
    }

    /// Seed the persisted shape of a completed download with genuine local PNGs.
    fn cache(
        &self,
        query: &str,
        limit: usize,
        cards: Value,
        physical_faces: &[(&str, u32)],
        truncated: bool,
    ) -> FetchReport {
        let images = self.directory.join("images");
        fs::create_dir_all(&images).expect("create image directory");
        for (id, face) in physical_faces {
            write_image(&images.join(format!("{id}-{face}.png")), *face);
        }
        let catalog = self.directory.join("cards.json");
        fs::write(
            &catalog,
            json!({
                "cards": cards,
                "selection": {"query": query, "limit": limit, "truncated": truncated},
            })
            .to_string(),
        )
        .expect("write cached catalogue");
        let report = FetchReport {
            catalog,
            images,
            cards: cards.as_array().expect("fixture card array").len(),
            faces: physical_faces.len(),
            cached: false,
            truncated,
        };
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after Unix epoch")
            .as_secs();
        fs::write(
            self.directory.join("fetch-cache.json"),
            json!({
                "query": query,
                "limit": limit,
                "created": created,
                "report": report,
            })
            .to_string(),
        )
        .expect("write completed cache record");
        report
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Write textured, decodable pixels to exercise actual offline index ingestion.
fn write_image(path: &Path, face: u32) {
    let image = image::RgbImage::from_fn(48, 68, |x, y| {
        image::Rgb([
            ((x * 17 + y * 3 + face * 37) % 256) as u8,
            ((x * 7 + y * 11 + face * 19) % 256) as u8,
            ((x * 5 + y * 13 + face * 71) % 256) as u8,
        ])
    });
    image.save(path).expect("write cached reference image");
}

/// Invalid requests fail before creating the selected output or image directories.
#[test]
fn invalid_parameters_do_not_create_files() {
    let fixture = Fixture::new();
    for (number, (query, limit)) in [("", 10), (" \n\t", 1), ("game:paper", 0)]
        .into_iter()
        .enumerate()
    {
        let output = fixture.directory.join(format!("invalid-{number}"));
        let error = fetch(query, &output, limit).expect_err("reject invalid parameters");
        assert!(matches!(error, Error::InvalidInput(_)));
        assert!(!output.exists(), "invalid request created output files");
    }
    assert_eq!(
        fs::read_dir(&fixture.directory)
            .expect("read destination")
            .count(),
        0
    );
}

/// A fresh identical completed request returns its local cache without network work.
#[test]
fn completed_cache_is_reused_without_rewriting_local_artifacts() {
    let fixture = Fixture::new();
    let query = "name:offline-cache-fixture";
    let expected = fixture.cache(
        query,
        1,
        json!([{"id":"local","name":"Local reference","oracle_id":"local-oracle"}]),
        &[("local", 0)],
        true,
    );
    let cache_path = fixture.directory.join("fetch-cache.json");
    let cache_before = fs::read(&cache_path).expect("read original cache");
    let catalog_before = fs::read(&expected.catalog).expect("read original catalogue");
    let image_path = expected.images.join("local-0.png");
    let image_before = fs::read(&image_path).expect("read original reference");

    let actual = fetch(query, &fixture.directory, 1).expect("reuse complete local cache");

    assert!(actual.cached);
    assert!(actual.truncated);
    assert_eq!(actual.catalog, expected.catalog);
    assert_eq!(actual.images, expected.images);
    assert_eq!((actual.cards, actual.faces), (1, 1));
    assert_eq!(
        fs::read(cache_path).expect("read unchanged cache"),
        cache_before
    );
    assert_eq!(
        fs::read(&actual.catalog).expect("read unchanged catalogue"),
        catalog_before
    );
    assert_eq!(
        fs::read(image_path).expect("read unchanged image"),
        image_before
    );
    assert!(!fixture.directory.join("cards.pending.json").exists());
    assert_eq!(
        mtg::build_index(&actual.catalog, &actual.images)
            .expect("index the cached reference")
            .len(),
        1
    );
}

/// A cached double-sided printing preserves both physical faces for offline scans.
#[test]
fn completed_double_sided_cache_preserves_face_counts_and_identities() {
    let fixture = Fixture::new();
    let query = "name:offline-double-sided-fixture";
    fixture.cache(
        query,
        3,
        json!([{
            "id":"double","name":"Front // Back","layout":"reversible_card",
            "card_faces":[
                {"name":"Front","oracle_id":"front-oracle"},
                {"name":"Back","oracle_id":"back-oracle"}
            ]
        }]),
        &[("double", 0), ("double", 1)],
        false,
    );

    let actual = fetch(query, &fixture.directory, 3).expect("reuse two-face cache");

    assert!(actual.cached);
    assert!(!actual.truncated);
    assert_eq!((actual.cards, actual.faces), (1, 2));
    let references = mtg::read_references(&actual.catalog).expect("read cached faces");
    assert_eq!(references[0].card.face_index, 0);
    assert_eq!(references[1].card.face_index, 1);
    assert_eq!(
        references[0].card.oracle_id.as_deref(),
        Some("front-oracle")
    );
    assert_eq!(references[1].card.oracle_id.as_deref(), Some("back-oracle"));
    assert_eq!(
        mtg::build_index(&actual.catalog, &actual.images)
            .expect("index both physical faces")
            .len(),
        2
    );
}

/// A complete cache with corrupted pixels fails explicitly without reporting success.
#[test]
fn corrupted_cached_image_is_reported_before_reuse() {
    let fixture = Fixture::new();
    let query = "name:offline-corrupt-cache-fixture";
    let report = fixture.cache(
        query,
        1,
        json!([{"id":"local","name":"Local reference"}]),
        &[("local", 0)],
        false,
    );
    fs::write(report.images.join("local-0.png"), b"invalid PNG").unwrap();
    assert!(matches!(
        fetch(query, &fixture.directory, 1),
        Err(Error::InvalidCatalog(_))
    ));
    assert!(!fixture.directory.join("cards.pending.json").exists());
}
