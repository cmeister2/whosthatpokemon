//! Generate temporary public RGBA fixtures and full-catalogue native results for WASM parity.

/// Exercise the production resumable browser engine against the complete metadata catalogue.
#[cfg(all(feature = "web", feature = "ocr"))]
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::{
        collections::BTreeMap,
        fs,
        io::{self, Read},
        path::PathBuf,
    };
    use whosthatpokemon::{browser::RemoteBrowserEngine, image::imageops::FilterType};

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if !(2..=3).contains(&arguments.len()) {
        return Err(io::Error::other(
            "usage: camera_parity DEPLOYMENT NEW_TEMP_OUTPUT [PUBLIC_IMAGE_CACHE]",
        )
        .into());
    }
    let site = PathBuf::from(&arguments[0]).canonicalize()?;
    let output = PathBuf::from(&arguments[1]);
    let cache = arguments.get(2).map(PathBuf::from);
    fs::create_dir(&output)?;
    fs::create_dir(output.join("references"))?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(site.join("assets/camera/manifest.json"))?)?;
    let path = |value: &serde_json::Value| -> Result<PathBuf, io::Error> {
        let path = site
            .join(
                value
                    .as_str()
                    .ok_or_else(|| io::Error::other("missing public resource path"))?,
            )
            .canonicalize()?;
        if !path.starts_with(&site) {
            return Err(io::Error::other("public path escapes deployment"));
        }
        Ok(path)
    };
    let metadata_bytes = fs::read(path(&manifest["catalogue"]["metadata"]["path"])?)?;
    let mut metadata = String::new();
    if manifest["catalogue"]["metadata"]["compression"] == "gzip" {
        flate2::read::GzDecoder::new(metadata_bytes.as_slice()).read_to_string(&mut metadata)?;
    } else {
        metadata = String::from_utf8(metadata_bytes)?;
    }
    let samples = manifest["samples"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing public samples"))?;
    let mut inputs = samples
        .iter()
        .map(|sample| Ok((sample.clone(), path(&sample["image"])?)))
        .collect::<Result<Vec<_>, io::Error>>()?;
    // This canonical printing is physically inverted. Keep this regression query outside
    // the deployment's seven gallery samples and acquire it just like a lazy reference.
    let arvinox_id = "0fbaa2f0-9ad0-4ce2-8ce6-359d01bd524c";
    let arvinox = {
        let catalogue: serde_json::Value = serde_json::from_str(&metadata)?;
        catalogue["cards"]
            .as_array()
            .and_then(|cards| {
                cards.iter().find(|card| {
                    card["game"] == "mtg" && card["id"] == arvinox_id && card["face_index"] == 0
                })
            })
            .cloned()
            .ok_or_else(|| io::Error::other("missing inverted Arvinox public reference"))?
    };
    let (_, source) = acquire_reference(&arvinox, &output, cache.as_deref())?;
    inputs.push((
        serde_json::json!({
            "id":"inverted-reference-arvinox", "cropped":true, "layout":"modern",
        }),
        output.join(source),
    ));
    let mut engine = RemoteBrowserEngine::new(
        &metadata,
        &fs::read(path(&manifest["model"]["path"])?)?,
        &fs::read_to_string(path(&manifest["model"]["config_path"])?)?,
    )
    .map_err(io::Error::other)?;
    let mut fixtures = Vec::new();
    let mut references = BTreeMap::new();
    for (sample, source) in inputs {
        let source = whosthatpokemon::open_image(&source)?;
        let image = if source.width().max(source.height()) > 1280 {
            source.resize(1280, 1280, FilterType::Triangle)
        } else {
            source
        };
        let classic = sample["layout"] == "classic";
        let cropped = sample["cropped"].as_bool().unwrap_or(false);
        let id = sample["id"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing sample id"))?;
        let variants = if matches!(id, "classic-bolt" | "inverted-reference-arvinox") {
            2
        } else {
            1
        };
        for variant in 0..variants {
            let image = if variant == 1 {
                image.rotate180()
            } else {
                image.clone()
            };
            let rgba = image.to_rgba8();
            let filename = format!("frame-{}.rgba", fixtures.len());
            fs::write(output.join(&filename), rgba.as_raw())?;
            let mut progress: serde_json::Value = serde_json::from_str(
                &engine
                    .begin_rgba(rgba.width(), rgba.height(), rgba.as_raw(), cropped, classic)
                    .map_err(io::Error::other)?,
            )?;
            let mut requested = Vec::new();
            let mut rounds = 0;
            while progress["state"] == "needs_references" {
                rounds += 1;
                if rounds > 32 {
                    return Err(io::Error::other("unbounded reference continuation").into());
                }
                let requests = progress["requests"]
                    .as_array()
                    .ok_or_else(|| io::Error::other("missing reference requests"))?;
                if requests.is_empty() || requests.len() > 256 {
                    return Err(io::Error::other("invalid candidate reference budget").into());
                }
                for batch in requests.chunks(4) {
                    let downloaded = std::thread::scope(|scope| {
                        let tasks: Vec<_> = batch
                            .iter()
                            .map(|request| {
                                let output = &output;
                                let cache = cache.as_deref();
                                scope.spawn(move || acquire_reference(request, output, cache))
                            })
                            .collect();
                        tasks
                            .into_iter()
                            .map(|task| {
                                task.join().map_err(|_| {
                                    io::Error::other("reference download worker panicked")
                                })?
                            })
                            .collect::<Result<Vec<_>, Box<dyn std::error::Error + Send + Sync>>>()
                    })?;
                    for (request, (key, file)) in batch.iter().zip(downloaded) {
                        let bytes = fs::read(output.join(&file))?;
                        let digest = engine
                            .add_reference(
                                request["game"]
                                    .as_str()
                                    .ok_or_else(|| io::Error::other("missing game"))?,
                                request["id"]
                                    .as_str()
                                    .ok_or_else(|| io::Error::other("missing card id"))?,
                                u32::try_from(
                                    request["face_index"]
                                        .as_u64()
                                        .ok_or_else(|| io::Error::other("missing face index"))?,
                                )?,
                                &bytes,
                            )
                            .map_err(io::Error::other)?;
                        references.insert(
                            key.clone(),
                            serde_json::json!({"file":file,"sha256":digest,"bytes":bytes.len()}),
                        );
                        requested.push(key);
                    }
                }
                progress =
                    serde_json::from_str(&engine.continue_frame().map_err(io::Error::other)?)?;
            }
            if progress["state"] != "complete" || !progress["report"].is_object() {
                return Err(io::Error::other("incomplete native result").into());
            }
            if id == "inverted-reference-arvinox" {
                check_inverted_reference(&progress["report"], if variant == 0 { 180 } else { 0 })?;
            }
            fixtures.push(serde_json::json!({
                "id": if variant == 1 {format!("{id}-inverted")} else {id.to_string()},
                "width":rgba.width(),"height":rgba.height(),"rgba":filename,"cropped":cropped,"classic":classic,
                "expected":progress["report"],"reference_requests":requested,
            }));
            println!(
                "{id} variant {variant}: full-catalogue native result, {rounds} reference rounds"
            );
        }
    }
    fs::write(output.join("fixtures.json"), serde_json::to_vec(&fixtures)?)?;
    fs::write(
        output.join("references.json"),
        serde_json::to_vec(&references)?,
    )?;
    println!(
        "Recorded {} canonical reference images outside the deployment",
        references.len()
    );
    Ok(())
}

/// Require a broad reference-alignment retry to preserve both its failed fast pass and OCR.
#[cfg(all(feature = "web", feature = "ocr"))]
fn check_inverted_reference(report: &serde_json::Value, rotation: u32) -> std::io::Result<()> {
    use std::io;
    let cards = report["cards"]
        .as_array()
        .filter(|cards| cards.len() == 1)
        .ok_or_else(|| io::Error::other("Arvinox regression must contain one card"))?;
    let card = &cards[0];
    let selected = card["selected_attempt"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| card["attempts"].get(index))
        .ok_or_else(|| io::Error::other("missing selected Arvinox attempt"))?;
    let result = &selected["identification"];
    let narrow = &selected["restricted_reference_alignment"];
    if card["orientation_conflict"] != false
        || selected["ocr"]["rotation_degrees"] != rotation
        || result["identity"]["status"] != "verified_shortlist"
        || result["identity"]["card"]["name"] != "Arvinox, the Mind Flail"
        || narrow["identity"]["status"] != "unknown"
        || narrow["observations"] != result["observations"]
    {
        return Err(io::Error::other(format!(
            "Arvinox OCR {rotation}° must accept via a broad alignment retry and retain its failed narrow evidence"
        )));
    }
    Ok(())
}

/// Reuse a named public cache image or fetch one selected public canonical JPEG.
///
/// # Errors
///
/// Returns an error for invalid source metadata, failed downloads or invalid file sizes.
#[cfg(all(feature = "web", feature = "ocr"))]
fn acquire_reference(
    request: &serde_json::Value,
    output: &std::path::Path,
    cache: Option<&std::path::Path>,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    use std::{fs, io, process::Command};
    let game = request["game"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing game"))?;
    let id = request["id"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing card id"))?;
    let face = request["face_index"]
        .as_u64()
        .ok_or_else(|| io::Error::other("missing face"))?;
    if game != "mtg" || id.len() != 36 || !id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
        return Err(io::Error::other("invalid public card key").into());
    }
    let filename = format!("{id}-{face}.jpg");
    let relative = format!("references/{filename}");
    let destination = output.join(&relative);
    if !destination.exists() {
        if let Some(source) = cache
            .map(|root| root.join(&filename))
            .filter(|p| p.is_file())
        {
            fs::copy(source, &destination)?;
        } else {
            let url = request["image_url"]
                .as_str()
                .ok_or_else(|| io::Error::other("missing canonical URL"))?;
            if !url.starts_with("https://cards.scryfall.io/normal/")
                || !url.contains(&format!("/{id}.jpg"))
            {
                return Err(io::Error::other("invalid public canonical URL").into());
            }
            let temporary = destination.with_extension("download");
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--retry",
                    "3",
                    "--retry-all-errors",
                    "--retry-max-time",
                    "180",
                    "--connect-timeout",
                    "30",
                    "--max-time",
                    "120",
                    "--max-filesize",
                    "20971520",
                    "--user-agent",
                    "whosthatpokemon-public-wasm-parity/0.0.0",
                    "--output",
                ])
                .arg(&temporary)
                .arg(url)
                .status()?;
            if !status.success() {
                let _ = fs::remove_file(&temporary);
                return Err(io::Error::other("canonical image download failed").into());
            }
            fs::rename(temporary, &destination)?;
        }
    }
    let length = destination.metadata()?.len();
    if length == 0 || length > 20 * 1024 * 1024 {
        return Err(io::Error::other("invalid canonical image size").into());
    }
    Ok((format!("{game}:{id}:{face}"), relative))
}

/// Explain the feature requirement without adding optional OCR dependencies by default.
#[cfg(not(all(feature = "web", feature = "ocr")))]
fn main() {
    eprintln!("camera_parity requires --features web,ocr");
    std::process::exit(1);
}
