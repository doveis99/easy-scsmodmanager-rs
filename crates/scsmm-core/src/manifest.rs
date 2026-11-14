//! A mod's `manifest.sii` plus the icon and description it points at.

use serde::Serialize;

use crate::scs::ModSource;
use crate::sii::crypto::sii_bytes_to_text;
use crate::sii::{parse_sii, SiiUnit};
use crate::{format_err, Error, Result};

pub const MANIFEST_ENTRY: &str = "manifest.sii";
const MOD_PACKAGE_CLASS: &str = "mod_package";
const DEFAULT_ICON_NAMES: &[&str] = &[
    "icon.jpg",
    "icon.png",
    "mod_icon.jpg",
    "mod_icon.png",
    "preview.jpg",
];
const DEFAULT_DESCRIPTION_NAMES: &[&str] =
    &["description.txt", "mod_description.txt", "readme.txt"];
/// 64 KiB is plenty for a mod readme; authors sometimes ship huge changelogs.
const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModManifest {
    pub display_name: String,
    pub package_version: String,
    pub author: String,
    pub categories: Vec<String>,
    pub description_file: String,
    pub icon: String,
    pub compatible_versions: Vec<String>,
}

impl ModManifest {
    pub fn from_units(units: &[SiiUnit]) -> Result<Self> {
        let unit = units
            .iter()
            .find(|u| u.class == MOD_PACKAGE_CLASS)
            .ok_or_else(|| format_err(format!("No '{MOD_PACKAGE_CLASS}' unit found in manifest")))?;
        let text = |key: &str| unit.text(key).unwrap_or_default().to_string();
        Ok(Self {
            display_name: text("display_name"),
            package_version: text("package_version"),
            author: text("author"),
            categories: unit.list("category", true),
            description_file: text("description_file"),
            icon: text("icon"),
            compatible_versions: unit.list("compatible_versions", true),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ManifestBundle {
    pub manifest: ModManifest,
    pub icon: Option<Vec<u8>>,
    pub description: Option<String>,
}

/// Parse `manifest.sii` from `source` and pull icon + description.
/// Returns `Error::NotFound` when the manifest is missing.
pub fn read_bundle(source: &mut dyn ModSource) -> Result<ManifestBundle> {
    if !source.has(MANIFEST_ENTRY) {
        return Err(Error::NotFound(format!("missing {MANIFEST_ENTRY}")));
    }
    // some authors scramble even the manifest
    let text = sii_bytes_to_text(&source.read_bytes(MANIFEST_ENTRY)?, false)?;
    let manifest = ModManifest::from_units(&parse_sii(&text)?)?;
    let icon = read_icon(source, &manifest);
    let description = read_description(source, &manifest);
    Ok(ManifestBundle {
        manifest,
        icon,
        description,
    })
}

fn candidates<'a>(named: &'a str, defaults: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    if !named.is_empty() {
        out.push(named);
    }
    out.extend(defaults.iter().filter(|d| **d != named));
    out
}

fn read_icon(source: &mut dyn ModSource, manifest: &ModManifest) -> Option<Vec<u8>> {
    // many older mods just drop icon.jpg at the root without referencing it
    for name in candidates(&manifest.icon, DEFAULT_ICON_NAMES) {
        if !source.has(name) {
            continue;
        }
        if let Ok(bytes) = source.read_bytes(name) {
            return Some(bytes);
        }
    }
    None
}

fn read_description(source: &mut dyn ModSource, manifest: &ModManifest) -> Option<String> {
    for name in candidates(&manifest.description_file, DEFAULT_DESCRIPTION_NAMES) {
        if !source.has(name) {
            continue;
        }
        let Ok(mut payload) = source.read_bytes(name) else {
            continue;
        };
        payload.truncate(MAX_DESCRIPTION_BYTES);
        // encodings in the wild range from UTF-8 to CP1252; lossy keeps going
        return Some(String::from_utf8_lossy(&payload).into_owned());
    }
    None
}

/// Guess an image MIME type from magic bytes (icons are jpg/png in practice).
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}
