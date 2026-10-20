//! Offline whole-catalogue streaming, language policy and resumable-image audits.

#![cfg(not(target_arch = "wasm32"))]

use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use flate2::{Compression, write::GzEncoder};
use serde_json::json;
use whosthatpokemon::{
    Error,
    mtg::{self, bulk},
};

/// Distinct temporary directories for parallel test workers.
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Filesystem-only fixture with automatic cleanup.
struct Fixture {
    /// Parent directory for sources, prepared metadata, and image caches.
    directory: PathBuf,
}

impl Fixture {
    /// Allocate an isolated local workspace.
    fn new() -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "whosthatpokemon-bulk-{}-{number}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        Self { directory }
    }

    /// Persist test metadata in the source format under test.
    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.directory.join(name);
        fs::write(&path, bytes).expect("write test source");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Every supported container streams the same printing objects through the callback.
#[test]
fn stream_plain_arrays_wrappers_jsonl_and_gzip() {
    let fixture = Fixture::new();
    let cards = json!([{"id":"one","name":"One"},{"id":"two","name":"Two"}]);
    let forms = [
        cards.to_string(),
        json!({"cards":cards}).to_string(),
        json!({"data":cards,"has_more":false}).to_string(),
        "\n {\"id\":\"one\",\"name\":\"One\"}\n{\"id\":\"two\",\"name\":\"Two\"}\n".into(),
    ];
    for (number, body) in forms.iter().enumerate() {
        for gzip in [false, true] {
            let bytes = if gzip {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(body.as_bytes()).expect("compress source");
                encoder.finish().expect("finish source")
            } else {
                body.as_bytes().to_vec()
            };
            let path = fixture.source(&format!("form-{number}-{gzip}"), &bytes);
            let mut ids = Vec::new();
            let count = bulk::for_each_card(&path, |card| {
                ids.push(card["id"].as_str().unwrap().to_owned());
                Ok(())
            })
            .expect("stream source");
            assert_eq!(count, 2);
            assert_eq!(ids, ["one", "two"]);
        }
    }
}

/// Callback failures keep their original error category, and malformed tails never pass.
#[test]
fn stream_preserves_callback_errors_and_rejects_partial_input() {
    let fixture = Fixture::new();
    let source = fixture.source("callback", br#"[{"id":"one","name":"One"}]"#);
    assert!(
        matches!(bulk::for_each_card(&source, |_| Err(Error::Backend("sentinel".into()))), Err(Error::Backend(message)) if message == "sentinel")
    );
    for (number, body) in [
        "",
        "[]",
        "[{}",
        "{}\n{",
        "{\"data\":{},\"has_more\":false}",
        "{\"data\":[{}],\"has_more\":true}",
        "{\"cards\":[{}],\"data\":[{}]}",
        "{\"object\":\"error\",\"details\":\"bad request\"}",
        "[1]",
    ]
    .iter()
    .enumerate()
    {
        let source = fixture.source(&format!("invalid-{number}"), body.as_bytes());
        assert!(
            bulk::for_each_card(&source, |_| Ok(())).is_err(),
            "accepted malformed source {body}"
        );
    }
}

/// All-language metadata is retained while English paper faces are selected and counted.
#[test]
fn prepare_retains_languages_and_expands_physical_faces() {
    let fixture = Fixture::new();
    let source = fixture.source("cards.json", json!([
        {"id":"english","name":"One","oracle_id":"one","lang":"en","games":["paper"],"image_uris":{"normal":"https://cards.scryfall.io/normal/en.jpg","png":"https://cards.scryfall.io/png/en.png"}},
        {"id":"french","name":"One","printed_name":"Un","oracle_id":"one","lang":"fr","games":["paper"],"image_uris":{"normal":"https://cards.scryfall.io/normal/fr.jpg"}},
        {"id":"foreign-only","name":"Foreign","oracle_id":"foreign","lang":"ja","games":["paper"],"image_uris":{"normal":"https://cards.scryfall.io/normal/ja.jpg"}},
        {"id":"digital","name":"Digital","oracle_id":"digital","lang":"en","games":["arena"],"image_uris":{"normal":"https://cards.scryfall.io/normal/digital.jpg"}},
        {"id":"missing","name":"Missing","oracle_id":"missing","lang":"en","games":["paper"]},
        {"id":"double","name":"Front // Back","oracle_id":"double","lang":"en","games":["paper"],"layout":"transform","card_faces":[
            {"name":"Front","image_uris":{"normal":"https://cards.scryfall.io/normal/front.jpg"}},
            {"name":"Back","image_uris":{"normal":"https://cards.scryfall.io/normal/back.jpg"}}
        ]}
    ]).to_string().as_bytes());
    let prepared = bulk::prepare(&source, &fixture.directory.join("prepared"), "en")
        .expect("prepare whole metadata");
    let stats = &prepared.coverage;
    assert_eq!(
        (
            stats.metadata_printings,
            stats.metadata_faces,
            stats.oracle_identities
        ),
        (6, 7, 5)
    );
    assert_eq!(
        (
            stats.selected_printings,
            stats.selected_faces,
            stats.selected_oracle_identities
        ),
        (2, 3, 2)
    );
    assert_eq!(
        (
            stats.excluded_language_faces,
            stats.excluded_nonpaper_faces,
            stats.missing_image_faces
        ),
        (2, 1, 1)
    );
    assert_eq!(stats.oracle_identities_without_selected_images, 3);
    assert_eq!(stats.languages["en"], 4);
    let mut retained = Vec::new();
    bulk::for_each_card(&prepared.metadata, |card| {
        retained.push(card);
        Ok(())
    })
    .expect("read retained metadata");
    assert_eq!(retained[1]["printed_name"], "Un");
    let references =
        mtg::read_references(&prepared.image_plan).expect("image plan is an indexing catalogue");
    assert_eq!(references.len(), 3);
    assert!(
        references[0]
            .image_url
            .as_deref()
            .unwrap()
            .ends_with("/normal/en.jpg")
    );
    assert_eq!(
        (references[1].card.face_index, references[2].card.face_index),
        (0, 1)
    );
    assert_eq!(references[2].card.face_name.as_deref(), Some("Back"));
    assert_eq!(references[0].card.language.as_deref(), Some("en"));

    let images = fixture.directory.join("images");
    let audit = bulk::download_images(&prepared.image_plan, &images, 0).expect("offline audit");
    assert_eq!(
        (
            audit.total_faces,
            audit.cached_faces,
            audit.downloaded_faces,
            audit.remaining_faces
        ),
        (3, 0, 0, 3)
    );
    assert!(!audit.complete);
    let image = image::RgbImage::from_fn(48, 68, |x, y| {
        image::Rgb([
            ((x * 7 + y * 11) % 255) as u8,
            ((x * 13 + y * 19) % 255) as u8,
            ((x * 23 + y * 29) % 255) as u8,
        ])
    });
    for reference in &references {
        image
            .save(images.join(format!(
                "{}-{}.jpg",
                reference.card.id, reference.card.face_index
            )))
            .expect("seed canonical pixels");
    }
    fs::write(images.join("english-0.jpg"), b"corrupted JPEG").expect("seed corrupt file");
    let corrupt_audit = bulk::download_images(&prepared.image_plan, &images, 0)
        .expect("audit corrupt cache without network");
    assert_eq!(
        (corrupt_audit.cached_faces, corrupt_audit.remaining_faces),
        (2, 1)
    );
    assert!(!corrupt_audit.complete);
    image
        .save(images.join("english-0.jpg"))
        .expect("repair cached pixels");
    let audit = bulk::download_images(&prepared.image_plan, &images, 0)
        .expect("reuse decoded cached images without network");
    assert!(audit.complete);
    assert_eq!((audit.cached_faces, audit.remaining_faces), (3, 0));
    assert_eq!(
        mtg::build_index(&prepared.image_plan, &images)
            .expect("index selected faces")
            .len(),
        3
    );
}

/// Deprecated JPEG imagery can fall back to full-card WebP without enrolling placeholders.
#[test]
fn prepare_selects_full_card_grid_and_excludes_placeholder_images() {
    let fixture = Fixture::new();
    let source = fixture.source("formats.json", json!([
        {"id":"webp","name":"WebP card","lang":"en","image_uris":{"grid":"https://cards.scryfall.io/grid/card.webp","display":"https://cards.scryfall.io/display/card.webp","art":"https://cards.scryfall.io/art/card.webp"}},
        {"id":"placeholder","name":"Placeholder","lang":"en","image_status":"placeholder","image_uris":{"normal":"https://cards.scryfall.io/normal/placeholder.jpg"}},
        {"id":"art-only","name":"Art only","lang":"en","image_uris":{"art":"https://cards.scryfall.io/art/card.webp"}},
        {"id":"border-only","name":"Border crop only","lang":"en","image_uris":{"crop":"https://cards.scryfall.io/crop/card.webp","border_crop":"https://cards.scryfall.io/border_crop/card.jpg"}}
    ]).to_string().as_bytes());
    let report = bulk::prepare(&source, &fixture.directory.join("prepared"), "en")
        .expect("select genuine full-card imagery");
    assert_eq!(
        (
            report.coverage.selected_faces,
            report.coverage.placeholder_image_faces,
            report.coverage.missing_image_faces
        ),
        (1, 1, 2)
    );
    let references = mtg::read_references(&report.image_plan).expect("read full-card plan");
    assert_eq!(
        references[0].image_url.as_deref(),
        Some("https://cards.scryfall.io/grid/card.webp")
    );
}

/// A late invalid record cannot replace either previously completed output file.
#[test]
fn failed_preparation_preserves_previous_catalogues() {
    let fixture = Fixture::new();
    let output = fixture.directory.join("prepared");
    fs::create_dir_all(&output).expect("create output");
    fs::write(output.join("metadata.jsonl"), b"previous metadata").unwrap();
    fs::write(output.join("images.jsonl"), b"previous image plan").unwrap();
    for (number, cards) in [
        json!([{"id":"safe","name":"Safe","lang":"en"},{"id":"safe","name":"Duplicate","lang":"en"}]),
        json!([{"id":"../escape","name":"Unsafe","lang":"en"}]),
        json!([{"id":"invalid","lang":"en"}]),
    ].iter().enumerate() {
        let source = fixture.source(&format!("invalid-{number}"),cards.to_string().as_bytes());
        assert!(bulk::prepare(&source,&output,"en").is_err());
        assert_eq!(fs::read(output.join("metadata.jsonl")).unwrap(), b"previous metadata");
        assert_eq!(fs::read(output.join("images.jsonl")).unwrap(), b"previous image plan");
    }
}

/// Empty language policy is rejected without creating a preparation output directory.
#[test]
fn empty_language_is_rejected_before_output_creation() {
    let fixture = Fixture::new();
    let output = fixture.directory.join("must-not-exist");
    assert!(matches!(
        bulk::prepare(&fixture.directory.join("absent"), &output, " \t"),
        Err(Error::InvalidInput(_))
    ));
    assert!(!output.exists());
}

/// Worker limits reject invalid concurrency without creating a destination or using the network.
#[test]
fn invalid_image_worker_counts_are_rejected_before_output_creation() {
    let fixture = Fixture::new();
    let images = fixture.directory.join("must-not-exist");
    for workers in [0, 17] {
        let options = bulk::ImageDownloadOptions {
            max_downloads: 1,
            workers,
        };
        assert!(matches!(
            bulk::download_images_with_options(
                &fixture.directory.join("absent"),
                &images,
                &options
            ),
            Err(Error::InvalidInput(_))
        ));
        assert!(!images.exists());
    }
}

/// Staging files and progress-report paths cannot alias and overwrite source metadata.
#[test]
fn preparation_and_image_audit_preserve_sources_at_output_paths() {
    let fixture = Fixture::new();
    let output = fixture.directory.join("prepared");
    fs::create_dir_all(&output).expect("create preparation directory");
    let body = br#"[{"id":"safe","name":"Safe","lang":"en","image_uris":{"normal":"https://cards.scryfall.io/normal/safe.jpg"}}]"#;
    for name in [
        "metadata.pending.jsonl",
        "images.pending.jsonl",
        "coverage.json",
        "coverage.json.pending",
    ] {
        let source = output.join(name);
        fs::write(&source, body).expect("seed source at reserved output path");
        assert!(matches!(
            bulk::prepare(&source, &output, "en"),
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(fs::read(&source).unwrap(), body);
    }
    let source = fixture.source("source.json", body);
    let prepared = bulk::prepare(&source, &output, "en").expect("prepare safe source");
    let images = fixture.directory.join("images");
    fs::create_dir_all(&images).expect("create image directory");
    let plan_bytes = fs::read(&prepared.image_plan).expect("read image plan");
    for name in [
        "image-progress.json",
        "image-progress.json.pending",
        "safe-0.jpg",
        "safe-0.pending",
    ] {
        let plan = images.join(name);
        fs::write(&plan, &plan_bytes).expect("seed plan at output path");
        assert!(matches!(
            bulk::download_images(&plan, &images, 0),
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(fs::read(&plan).unwrap(), plan_bytes);
        fs::remove_file(plan).expect("remove conflicting source");
    }
}
