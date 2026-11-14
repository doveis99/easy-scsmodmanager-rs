//! Canonical ETS2/ATS mod categories and the content-based signals that
//! override a (frequently wrong) manifest.
//!
//! The in-game mod manager only knows a fixed set of category tokens. A
//! manifest may carry any free-form `category[]` string, but the game files
//! it under one of these tokens or, when nothing matches, under `other`.

pub const OTHER: &str = "other";
pub const MAP: &str = "map";

/// Fixed order as the in-game mod manager presents them.
pub const OFFICIAL_CATEGORIES: &[&str] = &[
    "truck",
    "trailer",
    "interior",
    "tuning_parts",
    "ai_traffic",
    "sound",
    "paint_job",
    "cargo_pack",
    "map",
    "ui",
    "weather_setup",
    "physics",
    "graphics",
    "models",
    "movers",
    "walkers",
    "prefabs",
    OTHER,
];

pub fn is_official(token: &str) -> bool {
    OFFICIAL_CATEGORIES.contains(&token)
}

/// Map a mod's raw `category[]` tags onto official tokens.
///
/// Strict matching: a tag only counts when it equals an official token.
/// Anything else (mod-specific tags, plurals, placeholders, empties)
/// collapses to `other`. Order follows first appearance; duplicates drop.
pub fn canonical_categories<S: AsRef<str>>(raw: &[S]) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for tag in raw {
        let token = if is_official(tag.as_ref()) {
            tag.as_ref()
        } else {
            OTHER
        };
        if !result.iter().any(|t| t == token) {
            result.push(token.to_string());
        }
    }
    if result.is_empty() {
        result.push(OTHER.to_string());
    }
    result
}

/// Trustworthy category: override > content category > map-by-content >
/// manifest.
pub fn effective_categories<S: AsRef<str>>(
    manifest_categories: &[S],
    is_map: bool,
    override_token: Option<&str>,
    content_category: Option<&str>,
) -> Vec<String> {
    let single = |t: &str| vec![if is_official(t) { t } else { OTHER }.to_string()];
    if let Some(t) = override_token {
        return single(t);
    }
    if let Some(t) = content_category {
        return single(t);
    }
    if is_map {
        return vec![MAP.to_string()];
    }
    canonical_categories(manifest_categories)
}

/// A category token the archive content proves, or `None`.
///
/// Only the vehicle physics override is a clean enough signal. Truck,
/// trailer, sound and AI deliberately give no judgement: maps carry
/// trailer and AI defs, and sound/interior/tuning mods all overwrite
/// `def/vehicle/truck/` because they modify trucks.
pub fn content_category<S: AsRef<str>>(def_files: &[S]) -> Option<&'static str> {
    def_files
        .iter()
        .any(|p| {
            p.as_ref()
                .trim_start_matches('/')
                .starts_with("def/vehicle/physics")
        })
        .then_some("physics")
}

/// True if any path is a compiled map descriptor (`.mbd`) directly under
/// the top-level `map/` directory - what a playable map mod must ship.
pub fn contains_map<S: AsRef<str>>(paths: &[S]) -> bool {
    paths.iter().any(|p| {
        let p = p.as_ref();
        let p = p.strip_prefix('/').unwrap_or(p);
        let Some((dir, file)) = p.split_once('/') else {
            return false;
        };
        dir.eq_ignore_ascii_case("map")
            && !file.contains('/')
            && file.len() > 4
            && file
                .get(file.len() - 4..)
                .is_some_and(|ext| ext.eq_ignore_ascii_case(".mbd"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_is_strict() {
        assert_eq!(canonical_categories(&["sound", "ui", "sound"]), ["sound", "ui"]);
        assert_eq!(canonical_categories(&["maps", "_", ""]), ["other"]);
        assert_eq!(canonical_categories(&["donbass_map", "truck"]), ["other", "truck"]);
        assert_eq!(canonical_categories::<&str>(&[]), ["other"]);
    }

    #[test]
    fn effective_priority() {
        let m = ["truck"];
        assert_eq!(effective_categories(&m, false, None, None), ["truck"]);
        assert_eq!(effective_categories(&m, true, None, None), ["map"]);
        assert_eq!(effective_categories(&m, true, None, Some("physics")), ["physics"]);
        assert_eq!(effective_categories(&m, true, Some("sound"), Some("physics")), ["sound"]);
        assert_eq!(effective_categories(&m, false, Some("bogus"), None), ["other"]);
    }

    #[test]
    fn physics_signal() {
        assert_eq!(content_category(&["def/vehicle/physics.sii"]), Some("physics"));
        assert_eq!(content_category(&["/def/vehicle/physics/x.sii"]), Some("physics"));
        assert_eq!(content_category(&["def/vehicle/truck/a.sii"]), None);
    }

    #[test]
    fn map_signal() {
        assert!(contains_map(&["map/europe.mbd"]));
        assert!(contains_map(&["/MAP/Europe.MBD"]));
        assert!(!contains_map(&["map/europe/sec+0001+0001.base"]));
        assert!(!contains_map(&["def/map/x.mbd"]));
        assert!(!contains_map(&["map/.mbd", "map"]));
    }
}
