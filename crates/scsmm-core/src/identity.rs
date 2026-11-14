//! Stable mod identity derived purely from a path, and matching profile
//! `active_mods[]` entries back to scanned mods.
//!
//! The internal mod name is the token the game writes into
//! `active_mods[]`. It is the only identity that survives across sessions
//! (display name and icon change).

use std::collections::HashMap;
use std::path::Path;

use crate::profile::ActiveMod;
use crate::scanner::ScannedMod;

pub const WORKSHOP_NAME_PREFIX: &str = "mod_workshop_package.";

/// Workshop published-file-id for `.../workshop/content/<appid>/<id>/...`.
pub fn workshop_id_for_path(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if !parts.iter().any(|p| p == "workshop") {
        return None;
    }
    let content_idx = parts.iter().position(|p| p == "content")?;
    let id = parts.get(content_idx + 2)?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.clone())
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The name this mod takes in `active_mods[]`. Workshop mods become
/// `mod_workshop_package.<16-char upper hex of the id>`; everything else
/// uses the file/directory stem.
pub fn mod_name_for_path(path: &Path) -> String {
    match workshop_id_for_path(path).and_then(|id| id.parse::<u64>().ok()) {
        Some(id) => format!("{WORKSHOP_NAME_PREFIX}{id:016X}"),
        None => stem(path),
    }
}

/// `mod_workshop_package.000000003A4B7C12` -> `"978025490"`.
pub fn workshop_id_from_active_name(name: &str) -> Option<String> {
    let hex = name.strip_prefix(WORKSHOP_NAME_PREFIX)?;
    u64::from_str_radix(hex, 16).ok().map(|id| id.to_string())
}

pub fn workshop_url(workshop_id: &str) -> String {
    format!("https://steamcommunity.com/sharedfiles/filedetails/?id={workshop_id}")
}

/// Index over scanned mods for resolving an active entry. Strategies, first
/// hit wins: stem, parent directory, workshop id, display name.
pub struct ActiveModMatcher {
    by_stem: HashMap<String, usize>,
    by_parent: HashMap<String, usize>,
    by_workshop_id: HashMap<String, usize>,
    by_display_name: HashMap<String, usize>,
}

impl ActiveModMatcher {
    pub fn new(scanned: &[ScannedMod]) -> Self {
        let mut m = Self {
            by_stem: HashMap::new(),
            by_parent: HashMap::new(),
            by_workshop_id: HashMap::new(),
            by_display_name: HashMap::new(),
        };
        for (i, sm) in scanned.iter().enumerate() {
            m.by_stem.entry(stem(&sm.path).to_lowercase()).or_insert(i);
            if let Some(parent) = sm.path.parent().and_then(|p| p.file_name()) {
                m.by_parent
                    .entry(parent.to_string_lossy().to_lowercase())
                    .or_insert(i);
            }
            if let Some(id) = workshop_id_for_path(&sm.path) {
                m.by_workshop_id.entry(id).or_insert(i);
            }
            if let Some(name) = sm.manifest.as_ref().map(|mf| &mf.display_name) {
                if !name.is_empty() {
                    m.by_display_name.entry(name.to_lowercase()).or_insert(i);
                }
            }
        }
        m
    }

    /// Index into the scanned list for this active entry, if installed.
    pub fn lookup(&self, active: &ActiveMod) -> Option<usize> {
        let name = active.name.to_lowercase();
        if let Some(&i) = self.by_stem.get(&name).or_else(|| self.by_parent.get(&name)) {
            return Some(i);
        }
        if let Some(&i) = workshop_id_from_active_name(&active.name)
            .and_then(|id| self.by_workshop_id.get(&id))
        {
            return Some(i);
        }
        if active.display_name.is_empty() {
            return None;
        }
        self.by_display_name
            .get(&active.display_name.to_lowercase())
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn workshop_paths() {
        let p = PathBuf::from("D:/Steam/steamapps/workshop/content/227300/978025490/universal.scs");
        assert_eq!(workshop_id_for_path(&p).as_deref(), Some("978025490"));
        assert_eq!(mod_name_for_path(&p), "mod_workshop_package.000000003A4B7C12");
        assert_eq!(
            workshop_id_from_active_name("mod_workshop_package.000000003A4B7C12").as_deref(),
            Some("978025490")
        );
    }

    #[test]
    fn local_paths_use_the_stem() {
        let p = PathBuf::from("C:/Users/x/Documents/Euro Truck Simulator 2/mod/promods-def-v270.scs");
        assert_eq!(workshop_id_for_path(&p), None);
        assert_eq!(mod_name_for_path(&p), "promods-def-v270");
        assert_eq!(workshop_id_from_active_name("promods-def-v270"), None);
        assert_eq!(workshop_id_from_active_name("mod_workshop_package.zz"), None);
    }
}
