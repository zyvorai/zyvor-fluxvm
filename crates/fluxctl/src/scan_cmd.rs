//! `fluxctl scan` — check an image against a local advisory list.
//!
//! Usage: fluxctl scan IMAGE [--advisories FILE]
//!
//! Reads a simple JSON advisory file (default: ~/.fluxvm/advisories.json)
//! and reports matches. This is a local-only check; no network is used.

use std::path::PathBuf;

pub fn run(image: &str, advisories: Option<PathBuf>) -> Result<(), String> {
    let path = advisories.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".fluxvm/advisories.json")
    });
    if !path.exists() {
        println!("no advisory file at {}; nothing to check", path.display());
        return Ok(());
    }
    let data = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let advisories: Vec<serde_json::Value> = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    let mut hits = 0;
    for adv in &advisories {
        if let Some(pkg) = adv.get("package").and_then(|v| v.as_str()) {
            if image.contains(pkg) {
                println!("HIT {} {}", adv.get("id").and_then(|v| v.as_str()).unwrap_or("?"), pkg);
                hits += 1;
            }
        }
    }
    if hits == 0 {
        println!("{}: no advisory matches", image);
    } else {
        println!("{}: {} advisory matches", image, hits);
    }
    Ok(())
}
