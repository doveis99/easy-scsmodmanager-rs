//! Named load orders the user switches between.
//!
//! One game is played in more than one way - trucks, a coach, a car - and
//! each wants its own set of mods. A preset is such a set: a name and the
//! active list as it stood when the preset was stored. Presets belong to a
//! game, not to a profile, so one can be applied to any profile of it.
//!
//! Applying a preset is the caller's business (it only replaces the list
//! being edited); this module keeps the presets and their names in order.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::game::Game;
use crate::profile::{default_data_dir, ActiveMod};
use crate::{format_err, Result};

pub const PRESETS_FORMAT: &str = "easy-scsmodmanager-presets";
pub const PRESETS_VERSION: u64 = 1;
/// Longest preset name, in characters.
pub const MAX_PRESET_NAME: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    /// Display order: the top of the load order first.
    pub mods: Vec<ActiveMod>,
}

/// Why a preset could not be stored or changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetError {
    /// The name is empty once trimmed.
    EmptyName,
    /// Another preset of the game already has the name.
    NameTaken,
    /// No preset of that name.
    NotFound,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetStore {
    format: String,
    version: u64,
    ets2: Vec<Preset>,
    ats: Vec<Preset>,
}

pub fn default_presets_path() -> PathBuf {
    default_data_dir().join("presets.json")
}

/// A name as it is stored: one line, trimmed, at most [`MAX_PRESET_NAME`]
/// characters.
pub fn clean_name(name: &str) -> std::result::Result<String, PresetError> {
    let one_line: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let cleaned: String = one_line.trim().chars().take(MAX_PRESET_NAME).collect();
    let cleaned = cleaned.trim_end().to_string();
    if cleaned.is_empty() {
        return Err(PresetError::EmptyName);
    }
    Ok(cleaned)
}

/// Names differing only in case are the same preset to the user.
fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

impl PresetStore {
    /// A missing file is an empty store. A file that cannot be read as
    /// presets is an error rather than an empty store: the next save would
    /// otherwise replace what may still be recoverable by hand.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let store: PresetStore = serde_json::from_slice(&raw).map_err(|e| format_err(e.to_string()))?;
        if store.format != PRESETS_FORMAT {
            return Err(format_err("not a presets file"));
        }
        if store.version > PRESETS_VERSION {
            return Err(format_err("presets file is from a newer version"));
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let stamped = PresetStore { format: PRESETS_FORMAT.to_string(), version: PRESETS_VERSION, ..self.clone() };
        let json = serde_json::to_vec_pretty(&stamped).map_err(|e| format_err(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The game's presets, in the order they were first stored.
    pub fn list(&self, game: Game) -> &[Preset] {
        match game {
            Game::Ets2 => &self.ets2,
            Game::Ats => &self.ats,
        }
    }

    fn list_mut(&mut self, game: Game) -> &mut Vec<Preset> {
        match game {
            Game::Ets2 => &mut self.ets2,
            Game::Ats => &mut self.ats,
        }
    }

    /// Store `mods` under `name`. A preset of that name keeps its place in
    /// the list and gets the new mods when `overwrite` is set; otherwise
    /// the name counts as taken. Returns the name as stored.
    pub fn put(
        &mut self,
        game: Game,
        name: &str,
        mods: Vec<ActiveMod>,
        overwrite: bool,
    ) -> std::result::Result<String, PresetError> {
        let name = clean_name(name)?;
        let list = self.list_mut(game);
        match list.iter_mut().find(|p| same_name(&p.name, &name)) {
            Some(_) if !overwrite => Err(PresetError::NameTaken),
            Some(existing) => {
                existing.mods = mods;
                Ok(existing.name.clone())
            }
            None => {
                list.push(Preset { name: name.clone(), mods });
                Ok(name)
            }
        }
    }

    /// Give the preset `name` the name `new_name`; changing only the case
    /// of its own name is fine. Returns the name as stored.
    pub fn rename(&mut self, game: Game, name: &str, new_name: &str) -> std::result::Result<String, PresetError> {
        let new_name = clean_name(new_name)?;
        let list = self.list_mut(game);
        let index = list.iter().position(|p| same_name(&p.name, name)).ok_or(PresetError::NotFound)?;
        if list.iter().enumerate().any(|(i, p)| i != index && same_name(&p.name, &new_name)) {
            return Err(PresetError::NameTaken);
        }
        list[index].name = new_name.clone();
        Ok(new_name)
    }

    pub fn remove(&mut self, game: Game, name: &str) -> std::result::Result<(), PresetError> {
        let list = self.list_mut(game);
        let index = list.iter().position(|p| same_name(&p.name, name)).ok_or(PresetError::NotFound)?;
        list.remove(index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(names: &[&str]) -> Vec<ActiveMod> {
        names.iter().map(|n| ActiveMod { name: n.to_string(), display_name: format!("{n} (shown)") }).collect()
    }

    #[test]
    fn names_are_trimmed_single_line_and_bounded() {
        assert_eq!(clean_name("  Road Trip \n").unwrap(), "Road Trip");
        assert_eq!(clean_name("a\tb\nc").unwrap(), "a b c");
        assert_eq!(clean_name("   "), Err(PresetError::EmptyName));
        assert_eq!(clean_name("\n\t"), Err(PresetError::EmptyName));
        assert_eq!(clean_name(&"버".repeat(200)).unwrap().chars().count(), MAX_PRESET_NAME);
        // a cut that lands on a space leaves no trailing blank
        let cut = format!("{} tail", "x".repeat(MAX_PRESET_NAME - 1));
        assert_eq!(clean_name(&cut).unwrap(), "x".repeat(MAX_PRESET_NAME - 1));
    }

    #[test]
    fn put_adds_keeps_order_and_overwrites_only_when_asked() {
        let mut store = PresetStore::default();
        assert_eq!(store.put(Game::Ets2, " Trucks ", mods(&["a", "b"]), false).unwrap(), "Trucks");
        store.put(Game::Ets2, "Coach", mods(&[]), false).unwrap();
        assert_eq!(store.put(Game::Ets2, "trucks", mods(&["c"]), false), Err(PresetError::NameTaken));
        assert_eq!(store.list(Game::Ets2)[0].mods, mods(&["a", "b"]));

        // overwriting keeps the place and the name as first written
        assert_eq!(store.put(Game::Ets2, "TRUCKS", mods(&["c"]), true).unwrap(), "Trucks");
        let names: Vec<&str> = store.list(Game::Ets2).iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Trucks", "Coach"]);
        assert_eq!(store.list(Game::Ets2)[0].mods, mods(&["c"]));

        // the other game has its own names
        assert!(store.list(Game::Ats).is_empty());
        store.put(Game::Ats, "Trucks", mods(&["z"]), false).unwrap();
        assert_eq!(store.list(Game::Ets2)[0].mods, mods(&["c"]));
        assert_eq!(store.put(Game::Ets2, "  ", mods(&[]), true), Err(PresetError::EmptyName));
    }

    #[test]
    fn rename_and_remove() {
        let mut store = PresetStore::default();
        store.put(Game::Ets2, "Trucks", mods(&["a"]), false).unwrap();
        store.put(Game::Ets2, "Coach", mods(&["b"]), false).unwrap();

        assert_eq!(store.rename(Game::Ets2, "coach", "Trucks"), Err(PresetError::NameTaken));
        assert_eq!(store.rename(Game::Ets2, "gone", "X"), Err(PresetError::NotFound));
        assert_eq!(store.rename(Game::Ets2, "Coach", " "), Err(PresetError::EmptyName));
        // its own name in another case is not "taken"
        assert_eq!(store.rename(Game::Ets2, "coach", "COACH").unwrap(), "COACH");
        assert_eq!(store.rename(Game::Ets2, "COACH", "Euro Coach").unwrap(), "Euro Coach");
        assert_eq!(store.list(Game::Ets2)[1].mods, mods(&["b"]));

        assert_eq!(store.remove(Game::Ets2, "gone"), Err(PresetError::NotFound));
        assert_eq!(store.remove(Game::Ats, "Trucks"), Err(PresetError::NotFound));
        store.remove(Game::Ets2, "trucks").unwrap();
        let names: Vec<&str> = store.list(Game::Ets2).iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Euro Coach"]);
    }

    #[test]
    fn round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("presets.json");
        assert_eq!(PresetStore::load(&path).unwrap(), PresetStore::default());

        let mut store = PresetStore::default();
        store.put(Game::Ets2, "Trucks", mods(&["a", "b"]), false).unwrap();
        store.put(Game::Ats, "Road Trip", mods(&[]), false).unwrap();
        store.save(&path).unwrap();

        let loaded = PresetStore::load(&path).unwrap();
        assert_eq!(loaded.list(Game::Ets2), store.list(Game::Ets2));
        assert_eq!(loaded.list(Game::Ats), store.list(Game::Ats));
        // saving what was loaded changes nothing
        loaded.save(&path).unwrap();
        assert_eq!(PresetStore::load(&path).unwrap(), loaded);
    }

    #[test]
    fn a_file_that_is_not_presets_is_refused_not_emptied() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("presets.json");
        for bad in [
            &b"{ not json"[..],
            br#"{"ets2": []}"#,
            br#"{"format": "something-else", "version": 1}"#,
            br#"{"format": "easy-scsmodmanager-presets", "version": 99}"#,
        ] {
            fs::write(&path, bad).unwrap();
            assert!(PresetStore::load(&path).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // unknown keys from a later minor change are fine
        fs::write(
            &path,
            br#"{"format": "easy-scsmodmanager-presets", "version": 1, "future": true,
                "ets2": [{"name": "Trucks", "mods": [{"name": "a", "display_name": "A"}], "note": "x"}]}"#,
        )
        .unwrap();
        let store = PresetStore::load(&path).unwrap();
        assert_eq!(store.list(Game::Ets2)[0].name, "Trucks");
        assert_eq!(store.list(Game::Ets2)[0].mods[0].display_name, "A");
    }
}
