//! Persistent user settings, stored as one small JSON file.
//!
//! Kept small on purpose: optional manual path overrides for when
//! auto-detection misses the game, a few behaviour switches, and where the
//! user last was. A missing or unreadable file means defaults - settings
//! are never a reason to refuse to start.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::categories::is_official;
use crate::game::{detect_game_installs, install_for_overrides, Game, GameInstall};
use crate::load_order::DEFAULT_MAP_BASE_NAMES;
use crate::profile::default_data_dir;
use crate::{format_err, Result};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    /// Manual documents dir (holds `mod/` and `profiles/`); `None` = detect.
    pub documents_dir: Option<PathBuf>,
    /// Manual workshop content dir; `None` = detect.
    pub workshop_dir: Option<PathBuf>,
    /// The profile.sii last open for this game.
    pub last_profile: Option<PathBuf>,
}

/// How the mod library was last looked at: its sort order and filters.
/// The search text is deliberately not part of it - a forgotten search
/// from last time would hide mods for no visible reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    /// `name` or `date`.
    pub sort: String,
    /// Category token to show, or empty for all.
    pub category: String,
    /// `local`, `workshop`, or empty for both.
    pub source: String,
    /// Show favourites only.
    pub favorites: bool,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            sort: "name".to_string(),
            category: String::new(),
            source: String::new(),
            favorites: false,
        }
    }
}

impl ViewSettings {
    /// Anything this version does not know falls back to the default, so
    /// a hand-edited or newer file can never leave the library filtered by
    /// something the screen has no control for.
    pub fn normalize(&mut self) {
        if !matches!(self.sort.as_str(), "name" | "date") {
            self.sort = "name".to_string();
        }
        if !self.category.is_empty() && !is_official(&self.category) {
            self.category.clear();
        }
        if !matches!(self.source.as_str(), "" | "local" | "workshop") {
            self.source.clear();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Ask Steam for titles and previews of workshop mods after a scan.
    pub workshop_lookup: bool,
    /// How many backups to keep per profile; 0 keeps all of them.
    pub backup_keep: u32,
    /// Name fragments of mods that belong at the very top of the load
    /// order (matched case-insensitively against name and display name).
    pub map_base_names: Vec<String>,
    pub last_game: Option<Game>,
    /// Sort order and filters of the mod library.
    pub view: ViewSettings,
    pub ets2: GameSettings,
    pub ats: GameSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            workshop_lookup: true,
            backup_keep: 10,
            map_base_names: DEFAULT_MAP_BASE_NAMES.iter().map(|s| s.to_string()).collect(),
            last_game: None,
            view: ViewSettings::default(),
            ets2: GameSettings::default(),
            ats: GameSettings::default(),
        }
    }
}

pub fn default_settings_path() -> PathBuf {
    default_data_dir().join("settings.json")
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let mut settings: Settings = fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        settings.view.normalize();
        settings
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| format_err(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn game(&self, game: Game) -> &GameSettings {
        match game {
            Game::Ets2 => &self.ets2,
            Game::Ats => &self.ats,
        }
    }

    pub fn game_mut(&mut self, game: Game) -> &mut GameSettings {
        match game {
            Game::Ets2 => &mut self.ets2,
            Game::Ats => &mut self.ats,
        }
    }

    /// Tidy user input: trimmed, non-empty, de-duplicated fragments; an
    /// empty list falls back to the defaults.
    pub fn normalize(&mut self) {
        let mut names: Vec<String> = Vec::new();
        for name in &self.map_base_names {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                names.push(name.to_string());
            }
        }
        if names.is_empty() {
            names = Settings::default().map_base_names;
        }
        self.map_base_names = names;
        for game in Game::ALL {
            let gs = self.game_mut(game);
            for slot in [&mut gs.documents_dir, &mut gs.workshop_dir] {
                if slot.as_ref().is_some_and(|p| p.as_os_str().is_empty()) {
                    *slot = None;
                }
            }
        }
    }

    /// The install to use for `game`: the manual documents dir when set,
    /// otherwise the first detected one. A manual workshop dir replaces
    /// the detected one in both cases.
    pub fn resolve_install(&self, game: Game, steam_libraries: &[PathBuf]) -> Option<GameInstall> {
        let gs = self.game(game);
        if let Some(documents) = &gs.documents_dir {
            return Some(install_for_overrides(
                game,
                documents.clone(),
                gs.workshop_dir.clone(),
                steam_libraries,
            ));
        }
        let mut install = detect_game_installs(game, steam_libraries).into_iter().next()?;
        if let Some(workshop) = &gs.workshop_dir {
            install.workshop_dir = Some(workshop.clone());
        }
        Some(install)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::InstallKind;

    #[test]
    fn missing_or_corrupt_file_gives_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn round_trip_and_forward_compatibility() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("settings.json");
        let mut s = Settings::default();
        s.workshop_lookup = false;
        s.backup_keep = 3;
        s.last_game = Some(Game::Ats);
        s.ats.documents_dir = Some(PathBuf::from("X:/ats"));
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);

        // a file from an older or newer version: unknown keys ignored,
        // missing keys defaulted
        fs::write(&path, br#"{"backup_keep": 2, "some_future_key": true}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.backup_keep, 2);
        assert!(loaded.workshop_lookup);
        assert!(!loaded.map_base_names.is_empty());
    }

    #[test]
    fn view_survives_a_restart_and_unknown_values_fall_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut s = Settings::default();
        s.view = ViewSettings {
            sort: "date".into(),
            category: "truck".into(),
            source: "workshop".into(),
            favorites: true,
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).view, s.view);

        // a file written before the view was stored
        fs::write(&path, br#"{"backup_keep": 2}"#).unwrap();
        assert_eq!(Settings::load(&path).view, ViewSettings::default());

        // values this version has no control for
        fs::write(
            &path,
            br#"{"view": {"sort": "size", "category": "no_such", "source": "ftp", "favorites": true}}"#,
        )
        .unwrap();
        let view = Settings::load(&path).view;
        assert_eq!((view.sort.as_str(), view.category.as_str(), view.source.as_str()), ("name", "", ""));
        assert!(view.favorites);
    }

    #[test]
    fn normalize_cleans_names_and_empty_paths() {
        let mut s = Settings::default();
        s.map_base_names = vec!["  Loading Screen ".into(), "".into(), "loading screen".into(), "X".into()];
        s.ets2.documents_dir = Some(PathBuf::new());
        s.normalize();
        assert_eq!(s.map_base_names, ["Loading Screen", "X"]);
        assert_eq!(s.ets2.documents_dir, None);

        s.map_base_names = vec!["  ".into()];
        s.normalize();
        assert_eq!(s.map_base_names, Settings::default().map_base_names);
    }

    #[test]
    fn manual_documents_dir_wins() {
        let mut s = Settings::default();
        s.ets2.documents_dir = Some(PathBuf::from("X:/docs"));
        s.ets2.workshop_dir = Some(PathBuf::from("Y:/ws"));
        let install = s.resolve_install(Game::Ets2, &[]).unwrap();
        assert_eq!(install.kind, InstallKind::Manual);
        assert_eq!(install.documents_dir, PathBuf::from("X:/docs"));
        assert_eq!(install.workshop_dir, Some(PathBuf::from("Y:/ws")));
    }
}
