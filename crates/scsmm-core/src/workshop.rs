//! Reads `versions.sii` from a Steam workshop content directory and picks
//! the one payload the game would actually load.
//!
//! Workshop mods that ship several game-version variants list their slot
//! names and compatible game versions there; the payload (a `.scs` file or
//! a directory) sits next to it under the slot name.

use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::MANIFEST_ENTRY;
use crate::sii::parse_sii;

const VERSIONS_SII: &str = "versions.sii";
const PACKAGE_VERSION_CLASS: &str = "package_version_info";
pub const ARCHIVE_SUFFIXES: &[&str] = &["scs", "zip"];

#[derive(Debug, Clone, PartialEq)]
pub struct VersionSlot {
    pub name: String,
    pub compatible_versions: Vec<String>,
}

impl VersionSlot {
    /// A slot without `compatible_versions` is the catch-all.
    pub fn is_default(&self) -> bool {
        self.compatible_versions.is_empty()
    }

    pub fn matches(&self, game_version: &str) -> bool {
        !game_version.is_empty() && self.compatible_versions.iter().any(|p| wildcard_match(p, game_version))
    }
}

/// Shell-style match supporting `*` and `?` (what `1.59.*` patterns need).
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Slots declared in versions.sii (empty if the file is missing or broken).
pub fn read_versions_sii(workshop_dir: &Path) -> Vec<VersionSlot> {
    let Ok(raw) = fs::read(workshop_dir.join(VERSIONS_SII)) else {
        return Vec::new();
    };
    let Ok(units) = parse_sii(&String::from_utf8_lossy(&raw)) else {
        return Vec::new();
    };
    units
        .iter()
        .filter(|u| u.class == PACKAGE_VERSION_CLASS)
        .map(|u| VersionSlot {
            name: match u.text("package_name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => u.name.trim_start_matches('.').to_string(),
            },
            compatible_versions: u.list("compatible_versions", false),
        })
        .collect()
}

/// The slot the game would load: a version match, else the first catch-all
/// slot, else the very first slot.
pub fn pick_active_slot<'a>(slots: &'a [VersionSlot], game_version: Option<&str>) -> Option<&'a str> {
    if let Some(v) = game_version.filter(|v| !v.is_empty()) {
        if let Some(slot) = slots.iter().find(|s| s.matches(v)) {
            return Some(&slot.name);
        }
    }
    slots.iter().find(|s| s.is_default()).or(slots.first()).map(|s| s.name.as_str())
}

/// True for helper names like `150`, `153_content`, `downgrade_info_package`.
pub fn is_helper_slot_name(name: &str) -> bool {
    if name == "downgrade_info_package" {
        return true;
    }
    let digits = name.strip_suffix("_content").unwrap_or(name);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ARCHIVE_SUFFIXES.iter().any(|s| e.eq_ignore_ascii_case(s)))
}

fn has_manifest(dir: &Path) -> bool {
    dir.join(MANIFEST_ENTRY).is_file()
}

/// Try the three layouts a slot is published under.
fn payload_for_slot(workshop_id_dir: &Path, slot_name: &str) -> Option<PathBuf> {
    [
        workshop_id_dir.join(format!("{slot_name}.scs")),
        workshop_id_dir.join(format!("{slot_name}.zip")),
        workshop_id_dir.join(slot_name),
    ]
    .into_iter()
    .find(|c| c.is_file() || (c.is_dir() && has_manifest(c)))
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let mut children: Vec<PathBuf> =
        fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    children.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
    children
}

/// Pick which payload inside a `<workshop-id>/` directory to scan.
pub fn resolve_workshop_payload(workshop_id_dir: &Path, game_version: Option<&str>) -> Option<PathBuf> {
    let slots = read_versions_sii(workshop_id_dir);
    if let Some(target) = pick_active_slot(&slots, game_version).and_then(|n| payload_for_slot(workshop_id_dir, n)) {
        return Some(target);
    }

    // versions.sii lists nothing we can find on disk: standard slot names
    for preferred in ["universal", "latest"] {
        if let Some(target) = payload_for_slot(workshop_id_dir, preferred) {
            return Some(target);
        }
    }
    let children = sorted_children(workshop_id_dir);
    let name_of = |p: &Path, stem: bool| {
        let part = if stem { p.file_stem() } else { p.file_name() };
        part.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    };
    // first non-helper archive at the root, then first non-helper mod dir
    children
        .iter()
        .find(|p| p.is_file() && is_archive(p) && !is_helper_slot_name(&name_of(p, true)))
        .or_else(|| children.iter().find(|p| p.is_dir() && !is_helper_slot_name(&name_of(p, false)) && has_manifest(p)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match("1.59.*", "1.59.1.3"));
        assert!(!wildcard_match("1.59.*", "1.58.2"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("1.5?.*", "1.58.2"));
        assert!(!wildcard_match("1.59", "1.59.1"));
    }

    #[test]
    fn helper_names() {
        assert!(is_helper_slot_name("150"));
        assert!(is_helper_slot_name("153_content"));
        assert!(is_helper_slot_name("downgrade_info_package"));
        assert!(!is_helper_slot_name("universal"));
        assert!(!is_helper_slot_name("_content"));
    }

    #[test]
    fn slot_selection() {
        let slots = vec![
            VersionSlot { name: "latest".into(), compatible_versions: vec![] },
            VersionSlot { name: "158_content".into(), compatible_versions: vec!["1.58.*".into()] },
        ];
        assert_eq!(pick_active_slot(&slots, Some("1.58.2")), Some("158_content"));
        assert_eq!(pick_active_slot(&slots, Some("1.60.0")), Some("latest"));
        assert_eq!(pick_active_slot(&slots, None), Some("latest"));
        assert_eq!(pick_active_slot(&[], Some("1.58")), None);
    }

    #[test]
    fn resolves_versioned_and_fallback_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("12345");
        fs::create_dir_all(&ws).unwrap();
        fs::write(
            ws.join("versions.sii"),
            r#"SiiNunit {
                package_version_info : .latest { package_name: "latest" }
                package_version_info : .158_content {
                    package_name: "158_content"
                    compatible_versions[]: "1.58.*"
                }
            }"#,
        )
        .unwrap();
        fs::write(ws.join("latest.scs"), b"x").unwrap();
        fs::write(ws.join("158_content.scs"), b"x").unwrap();
        assert_eq!(resolve_workshop_payload(&ws, Some("1.58.1")).unwrap().file_name().unwrap(), "158_content.scs");
        assert_eq!(resolve_workshop_payload(&ws, Some("1.61.0")).unwrap().file_name().unwrap(), "latest.scs");

        let bare = tmp.path().join("777");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join("150.scs"), b"x").unwrap();
        fs::write(bare.join("real_mod.scs"), b"x").unwrap();
        assert_eq!(resolve_workshop_payload(&bare, None).unwrap().file_name().unwrap(), "real_mod.scs");
    }
}
