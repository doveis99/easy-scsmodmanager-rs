//! Looks workshop items up on Steam and downloads their preview images
//! into a throw-away cache, to check the network path end to end.
//!
//!     cargo run -p scsmm-core --features net --example workshop_lookup -- <id> [<id> ...]

use std::process::ExitCode;

use scsmm_core::manifest::sniff_image_mime;
use scsmm_core::workshop_api::{self, WorkshopCache};

fn main() -> ExitCode {
    let ids: Vec<String> = std::env::args().skip(1).collect();
    if ids.is_empty() {
        eprintln!("usage: workshop_lookup <workshop id> [<workshop id> ...]");
        return ExitCode::FAILURE;
    }
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut cache = WorkshopCache::open(tmp.path());
    let asked = cache.missing_or_stale(ids.iter().map(String::as_str));
    let agent = workshop_api::agent();
    let outcome = workshop_api::fetch_metadata(&agent, &asked);
    if let Some(e) = &outcome.error {
        println!("metadata error: {e}");
    }
    println!(
        "asked {} id(s), Steam answered for {}, found {}",
        asked.len(),
        outcome.answered.len(),
        outcome.found.len()
    );
    if let Err(e) = cache.store(&outcome.answered, outcome.found) {
        println!("cache error: {e}");
        return ExitCode::FAILURE;
    }

    let mut failures = 0;
    for id in &asked {
        let Some(meta) = cache.get(id).cloned() else {
            println!("{id}: no answer");
            continue;
        };
        println!("{id}: {}", meta.title.as_deref().unwrap_or("(not found or private)"));
        let Some(url) = meta.preview_url else {
            continue;
        };
        match workshop_api::fetch_preview(&agent, &url).and_then(|bytes| {
            cache.write_preview(id, &bytes)?;
            Ok(bytes)
        }) {
            Ok(bytes) => println!(
                "  preview: {} bytes, {}, stored: {}",
                bytes.len(),
                sniff_image_mime(&bytes).unwrap_or("?"),
                cache.has_preview(id)
            ),
            Err(e) => {
                failures += 1;
                println!("  preview failed: {e}");
            }
        }
    }
    println!(
        "second pass asks for {} id(s) (expected 0)",
        cache.missing_or_stale(ids.iter().map(String::as_str)).len()
    );
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
