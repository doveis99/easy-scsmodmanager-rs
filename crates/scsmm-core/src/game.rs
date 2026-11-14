//! Discovery of ETS2 / ATS data locations.
//!
//! A user can have a Windows install (`Documents/<game>/`), a Linux native
//! install (`~/.local/share/<game>/`), a Proton install (inside the Wine
//! prefix of a Steam library) or a macOS install. Workshop mods live under
//! `steamapps/workshop/content/<app-id>` of whichever Steam library holds
//! them, regardless of how the game itself is installed.

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::steam::{discover_steam_libraries, read_registry_string};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Game {
    Ets2,
    Ats,
}

impl Game {
    pub const ALL: [Game; 2] = [Game::Ets2, Game::Ats];

    pub fn app_id(self) -> u32 {
        match self {
            Game::Ets2 => 227300,
            Game::Ats => 270880,
        }
    }

    pub fn directory_name(self) -> &'static str {
        match self {
            Game::Ets2 => "Euro Truck Simulator 2",
            Game::Ats => "American Truck Simulator",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Game::Ets2 => "ETS2",
            Game::Ats => "ATS",
        }
    }

    pub fn parse(s: &str) -> Option<Game> {
        match s.to_ascii_lowercase().as_str() {
            "ets2" => Some(Game::Ets2),
            "ats" => Some(Game::Ats),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    LinuxNative,
    Proton,
    Windows,
    Macos,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GameInstall {
    pub game: Game,
    pub kind: InstallKind,
    /// Holds `mod/`, `profiles/`, `game.log.txt`.
    pub documents_dir: PathBuf,
    pub workshop_dir: Option<PathBuf>,
}

impl GameInstall {
    pub fn mod_dir(&self) -> PathBuf {
        self.documents_dir.join("mod")
    }
}

fn home_dir() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default())
}

/// The user's Documents folder, honouring a redirected location (Known
/// Folder redirection, OneDrive).
pub fn windows_documents_root() -> PathBuf {
    if let Some(personal) = read_registry_string(
        "HKCU",
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders",
        "Personal",
        true,
    ) {
        return PathBuf::from(personal);
    }
    if let Some(onedrive) = env::var_os("OneDrive") {
        return PathBuf::from(onedrive).join("Documents");
    }
    PathBuf::from(env::var_os("USERPROFILE").unwrap_or_default()).join("Documents")
}

fn native_documents(game: Game) -> (PathBuf, InstallKind) {
    if cfg!(windows) {
        (
            windows_documents_root().join(game.directory_name()),
            InstallKind::Windows,
        )
    } else if cfg!(target_os = "macos") {
        (
            home_dir()
                .join("Library/Application Support")
                .join(game.directory_name()),
            InstallKind::Macos,
        )
    } else {
        let base = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local/share"));
        (base.join(game.directory_name()), InstallKind::LinuxNative)
    }
}

/// Join a `/`-separated relative path one segment at a time, so the result
/// uses the platform separator throughout.
fn sub(base: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(base.to_path_buf(), |p, seg| p.join(seg))
}

fn workshop_dir_path(steam_library: &Path, game: Game) -> PathBuf {
    sub(steam_library, "steamapps/workshop/content").join(game.app_id().to_string())
}

/// First existing Workshop content dir for the game across Steam libraries.
pub fn detect_workshop_dir(game: Game, steam_libraries: &[PathBuf]) -> Option<PathBuf> {
    steam_libraries
        .iter()
        .map(|lib| workshop_dir_path(lib, game))
        .find(|p| p.is_dir())
}

/// The game's own install directory (`steamapps/common/<game>`, the one
/// holding base.scs and the `dlc_*.scs` files).
pub fn find_game_install_dir(game: Game, steam_libraries: &[PathBuf]) -> Option<PathBuf> {
    steam_libraries
        .iter()
        .map(|lib| sub(lib, "steamapps/common").join(game.directory_name()))
        .find(|p| p.is_dir())
}

/// Every existing install for the game.
pub fn detect_game_installs(game: Game, steam_libraries: &[PathBuf]) -> Vec<GameInstall> {
    let mut installs = Vec::new();
    let (documents, kind) = native_documents(game);
    if documents.is_dir() {
        installs.push(GameInstall {
            game,
            kind,
            documents_dir: documents,
            workshop_dir: detect_workshop_dir(game, steam_libraries),
        });
    }
    if cfg!(target_os = "linux") {
        for lib in steam_libraries {
            let prefix = sub(lib, "steamapps/compatdata").join(game.app_id().to_string());
            let proton_docs =
                sub(&prefix, "pfx/drive_c/users/steamuser/Documents").join(game.directory_name());
            if !proton_docs.is_dir() {
                continue;
            }
            let workshop = workshop_dir_path(lib, game);
            installs.push(GameInstall {
                game,
                kind: InstallKind::Proton,
                documents_dir: proton_docs,
                workshop_dir: workshop.is_dir().then_some(workshop),
            });
        }
    }
    installs
}

/// Convenience: discover Steam libraries, then the installs of every game.
pub fn detect_all_installs() -> Vec<GameInstall> {
    let libs = discover_steam_libraries();
    Game::ALL
        .into_iter()
        .flat_map(|g| detect_game_installs(g, &libs))
        .collect()
}

/// A manual install built from a user-set documents dir. Workshop is still
/// auto-detected unless pinned, so a manual path never switches it off.
pub fn install_for_overrides(
    game: Game,
    documents_dir: PathBuf,
    workshop_override: Option<PathBuf>,
    steam_libraries: &[PathBuf],
) -> GameInstall {
    GameInstall {
        game,
        kind: InstallKind::Manual,
        documents_dir,
        workshop_dir: workshop_override.or_else(|| detect_workshop_dir(game, steam_libraries)),
    }
}

/// Game version from `<documents_dir>/game.log.txt`, e.g. `1.59.1.3`.
///
/// The line `[ufs] Loaded pack set version 1.59.1.3 created at ...` sits
/// near the top of the log; the read is capped for huge logs.
pub fn read_game_version(documents_dir: &Path) -> Option<String> {
    const MARKER: &str = "Loaded pack set version ";
    const MAX_LINES: usize = 300;
    let file = File::open(documents_dir.join("game.log.txt")).ok()?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    for _ in 0..MAX_LINES {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).ok()? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        if let Some(pos) = line.find(MARKER) {
            let version: String = line[pos + MARKER.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if version.starts_with(|c: char| c.is_ascii_digit()) {
                return Some(version);
            }
        }
    }
    None
}

/// Owned DLC tokens (`dlc_mighty_griffin` ...): the stems of the
/// `dlc_*.scs` files in the game directory. `None` when the install dir is
/// unknown, which tells the scanner to ungate `dlc_` normalisation.
pub fn owned_dlc_tokens(install_dir: Option<&Path>) -> Option<BTreeSet<String>> {
    let entries = fs::read_dir(install_dir?).ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(".scs")?;
                stem.starts_with("dlc_").then(|| stem.to_string())
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_from_log() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("game.log.txt"),
            "************ : log created on : Saturday\n\
             00:00:00.061 : [ufs] Loaded pack set version 1.59.1.3 created at Wed\n",
        )
        .unwrap();
        assert_eq!(read_game_version(tmp.path()).as_deref(), Some("1.59.1.3"));
        assert_eq!(read_game_version(&tmp.path().join("missing")), None);
    }

    #[test]
    fn collects_owned_dlc() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["dlc_east.scs", "dlc_north.scs", "base.scs", "dlc_readme.txt"] {
            fs::write(tmp.path().join(name), b"").unwrap();
        }
        let owned = owned_dlc_tokens(Some(tmp.path())).unwrap();
        assert_eq!(owned.into_iter().collect::<Vec<_>>(), ["dlc_east", "dlc_north"]);
        assert_eq!(owned_dlc_tokens(None), None);
    }

    #[test]
    fn manual_install_keeps_pinned_workshop() {
        let inst = install_for_overrides(
            Game::Ats,
            PathBuf::from("X:/docs"),
            Some(PathBuf::from("Y:/ws")),
            &[],
        );
        assert_eq!(inst.kind, InstallKind::Manual);
        assert_eq!(inst.workshop_dir, Some(PathBuf::from("Y:/ws")));
        assert_eq!(inst.mod_dir(), PathBuf::from("X:/docs").join("mod"));
    }
}
