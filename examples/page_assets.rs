//! Rebuild the public page index and sample reports with the production engine.
//!
//! Run through `scripts/prepare-page-assets.py`, or provide the enrollment
//! catalogue, output index and page sample manifest as three positional paths.

/// Generate reports when the optional browser diagnostics implementation is built.
#[cfg(feature = "web")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs;
    use std::io;
    use std::path::Path;

    use whosthatpokemon::browser::BrowserEngine;

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: page_assets CATALOG_JSON OUTPUT_INDEX SAMPLES_JSON",
        )
        .into());
    }
    let index = whosthatpokemon::mtg::build_index(Path::new(&arguments[0]), Path::new("."))?;
    index.save(&arguments[1])?;
    let engine =
        BrowserEngine::new(&fs::read_to_string(&arguments[1])?).map_err(io::Error::other)?;
    let manifest_path = Path::new(&arguments[2]);
    let site = manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            io::Error::other("sample manifest must be under the page assets directory")
        })?;
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
    let samples = manifest["samples"]
        .as_array()
        .ok_or_else(|| io::Error::other("sample manifest requires a samples array"))?;
    for sample in samples {
        let input = sample["image"]
            .as_str()
            .ok_or_else(|| io::Error::other("sample requires an image path"))?;
        // Samples without a report path belong only to the native gallery.
        let Some(output) = sample["report"].as_str() else {
            continue;
        };
        let cropped = sample["cropped"].as_bool().unwrap_or(false);
        let report = engine.scan_report(&fs::read(site.join(input))?, cropped)?;
        let output = site.join(output);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, serde_json::to_vec(&report)?)?;
        println!("{input}: {} extracted card(s)", report.cards.len());
    }
    Ok(())
}

/// Explain the required build feature without introducing a default dependency.
#[cfg(not(feature = "web"))]
fn main() {
    eprintln!("page_assets requires --features web");
    std::process::exit(1);
}
