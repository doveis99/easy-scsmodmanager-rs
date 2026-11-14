//! Tauri shell: exposes scsmm-core to the web UI as commands.
//!
//! All mod and profile logic lives in scsmm-core; this layer keeps the
//! last scan in memory, shapes data for the UI and persists the user's
//! choices (settings, category pins, favourites).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use serde::Serialize;
use tauri::State;

use scsmm_core::categories::{is_official, OFFICIAL_CATEGORIES};
use scsmm_core::compat::{compat_status, CompatStatus};
use scsmm_core::conflicts::{self, ConflictReport};
use scsmm_core::game::{self, Game, GameInstall};
use scsmm_core::identity::{workshop_url, ActiveModMatcher};
use scsmm_core::load_order::{self, Row, GROUPS, MAP_BASE};
use scsmm_core::manifest::sniff_image_mime;
use scsmm_core::profile::{self, ActiveMod};
use scsmm_core::scanner::{self, ScannedMod};
use scsmm_core::settings::{default_settings_path, Settings};
use scsmm_core::steam;
use scsmm_core::workshop_api::{self, WorkshopCache};

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Default)]
struct AppState {
    inner: Mutex<Inner>,
}

/// Per-game string sets / maps are keyed by the game's short name.
type PerGame<T> = HashMap<String, T>;

#[derive(Default)]
struct Inner {
    /// Last scan per game.
    scans: HashMap<Game, Vec<ScannedMod>>,
    /// Game version read at the last scan, per game.
    versions: HashMap<Game, Option<String>>,
    /// active name -> display name as the game wrote it into the profiles.
    profile_names: HashMap<Game, HashMap<String, String>>,
    /// mod name -> category token. Loaded lazily from disk.
    overrides: Option<PerGame<HashMap<String, String>>>,
    /// Favourited mod names. Loaded lazily from disk.
    favorites: Option<PerGame<BTreeSet<String>>>,
    /// Steam Workshop titles and previews. Loaded lazily from disk.
    workshop: Option<WorkshopCache>,
    /// Loaded lazily from disk.
    settings: Option<Settings>,
}

fn overrides_path() -> PathBuf {
    profile::default_data_dir().join("category_overrides.json")
}

fn favorites_path() -> PathBuf {
    profile::default_data_dir().join("favorites.json")
}

/// Backups are kept per game: profile directory names are only unique
/// within one game, and a backup must never be restorable into the other.
fn backup_root(game: Game) -> PathBuf {
    profile::default_backup_root().join(game.short_name().to_lowercase())
}

/// Cache, not data: everything in here can be fetched again.
fn workshop_cache_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) if cfg!(windows) => PathBuf::from(local).join("easy-scsmodmanager-rs").join("workshop"),
        _ => profile::default_data_dir().join("cache").join("workshop"),
    }
}

fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> CmdResult<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(err)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value).map_err(err)?).map_err(err)
}

impl Inner {
    fn settings(&mut self) -> &mut Settings {
        self.settings
            .get_or_insert_with(|| Settings::load(&default_settings_path()))
    }

    fn workshop(&mut self) -> &mut WorkshopCache {
        self.workshop
            .get_or_insert_with(|| WorkshopCache::open(&workshop_cache_dir()))
    }

    fn overrides(&mut self) -> &mut PerGame<HashMap<String, String>> {
        self.overrides
            .get_or_insert_with(|| load_json(&overrides_path()))
    }

    fn favorites(&mut self) -> &mut PerGame<BTreeSet<String>> {
        self.favorites
            .get_or_insert_with(|| load_json(&favorites_path()))
    }

    fn override_for(&mut self, game: Game, mod_name: &str) -> Option<String> {
        self.overrides()
            .get(game.short_name())
            .and_then(|m| m.get(mod_name))
            .cloned()
    }
}

fn parse_game(game: &str) -> CmdResult<Game> {
    Game::parse(game).ok_or_else(|| format!("unknown game '{game}'"))
}

fn lock<'a>(state: &'a State<'_, AppState>) -> CmdResult<std::sync::MutexGuard<'a, Inner>> {
    state.inner.lock().map_err(err)
}

fn settings_snapshot(state: &State<'_, AppState>) -> CmdResult<Settings> {
    Ok(lock(state)?.settings().clone())
}

/// The install to use for `game` (manual paths from the settings win over
/// detection), plus the Steam libraries that were found on the way.
fn install_for(game: Game, settings: &Settings) -> CmdResult<(GameInstall, Vec<PathBuf>)> {
    let libs = steam::discover_steam_libraries();
    let install = settings
        .resolve_install(game, &libs)
        .ok_or_else(|| format!("{} 데이터 폴더를 찾지 못했습니다.", game.short_name()))?;
    Ok((install, libs))
}

// ------------------------------------------------------------------- //
// DTOs
// ------------------------------------------------------------------- //

#[derive(Serialize)]
struct InstallInfo {
    game: Game,
    short_name: &'static str,
    documents_dir: String,
    workshop_dir: Option<String>,
    version: Option<String>,
    /// The documents dir comes from the settings, not from detection.
    manual: bool,
}

#[derive(Serialize)]
struct ModDto {
    /// The `active_mods[]` token - stable identity.
    id: String,
    path: String,
    file_name: String,
    source: &'static str,
    workshop_url: Option<String>,
    format: scsmm_core::scs::ScsFormat,
    display_name: String,
    author: String,
    version: String,
    /// Effective categories (override > content > manifest).
    categories: Vec<String>,
    manifest_categories: Vec<String>,
    category_override: Option<String>,
    /// Token deciding the load-order group (map_base aware).
    group_token: String,
    is_map: bool,
    error: Option<String>,
    installed_at: f64,
    description: Option<String>,
    compatible_versions: Vec<String>,
    /// The game's own verdict from `compatible_versions[]`.
    compat: CompatStatus,
    has_icon: bool,
    favorite: bool,
}

#[derive(Serialize)]
struct ProfileDto {
    path: String,
    profile_name: String,
    active_count: usize,
    error: Option<String>,
}

#[derive(Serialize)]
struct ActiveItem {
    name: String,
    display_name: String,
    /// Load-order group token.
    token: String,
    installed: bool,
    compat: CompatStatus,
}

#[derive(Serialize)]
struct Layout {
    rows: Vec<Row>,
    misplaced: usize,
}

#[derive(Serialize)]
struct Constants {
    categories: &'static [&'static str],
    groups: Vec<&'static str>,
}

#[derive(Serialize)]
struct DetectedPaths {
    documents_dir: Option<String>,
    workshop_dir: Option<String>,
}

#[derive(Serialize)]
struct SettingsView {
    settings: Settings,
    /// What auto-detection finds, shown as the hint for the manual fields.
    detected: HashMap<&'static str, DetectedPaths>,
    backup_root: String,
}

#[derive(Serialize)]
struct BackupDto {
    path: String,
    /// `YYYY-MM-DD_HH-MM-SS`, local time.
    timestamp: String,
    size_bytes: u64,
}

#[derive(Serialize)]
struct SaveResult {
    backup: String,
    /// Old backups removed to stay within the keep limit.
    pruned: usize,
}

#[derive(Serialize, Default)]
struct WorkshopFetch {
    /// Workshop ids looked up on Steam this time.
    asked: usize,
    /// Titles received.
    titles: usize,
    /// Preview images downloaded.
    previews: usize,
    error: Option<String>,
}

#[derive(Serialize)]
struct DeleteFailure {
    id: String,
    error: String,
}

#[derive(Serialize, Default)]
struct DeleteResult {
    deleted: Vec<String>,
    failed: Vec<DeleteFailure>,
    /// Workshop mods belong to Steam and are never deleted here.
    skipped_workshop: usize,
}

fn group_token(name: &str, display_name: &str, category: &str, map_base_names: &[String]) -> String {
    if load_order::is_map_base(name, display_name, map_base_names) {
        MAP_BASE.to_string()
    } else {
        category.to_string()
    }
}

/// The mod's own icon, when it is an image the webview can show.
fn own_icon(m: &ScannedMod) -> Option<&[u8]> {
    m.icon.as_deref().filter(|bytes| sniff_image_mime(bytes).is_some())
}

/// DTOs for the last scan of `game`, with names and icons filled in from
/// the profile and the workshop cache where the mod itself has none.
fn build_dtos(inner: &mut Inner, game: Game) -> Vec<ModDto> {
    // force the lazy loads first so the fields can be borrowed side by side
    inner.overrides();
    inner.favorites();
    inner.workshop();
    inner.settings();
    let Inner {
        scans,
        versions,
        profile_names,
        overrides,
        favorites,
        workshop,
        settings,
    } = &*inner;
    let (Some(mods), Some(workshop), Some(settings)) = (scans.get(&game), workshop.as_ref(), settings.as_ref())
    else {
        return Vec::new();
    };
    let version = versions.get(&game).cloned().flatten();
    let pins = overrides.as_ref().and_then(|o| o.get(game.short_name()));
    let favorites = favorites.as_ref().and_then(|f| f.get(game.short_name()));
    let names = profile_names.get(&game);

    mods.iter()
        .map(|m| {
            let id = m.mod_name();
            let workshop_id = m.workshop_id();
            let manifest = m.manifest.clone().unwrap_or_default();
            // name chain: manifest -> the profile's active entry -> the
            // workshop title -> the file stem
            let borrowed = names
                .and_then(|n| n.get(&id))
                .map(String::as_str)
                .or_else(|| workshop_id.as_deref().and_then(|w| workshop.title(w)));
            let display_name = match borrowed {
                Some(name) if manifest.display_name.is_empty() => name.to_string(),
                _ => m.display_name(),
            };
            let category_override = pins.and_then(|o| o.get(&id)).cloned();
            let categories = m.effective_categories(category_override.as_deref());
            ModDto {
                group_token: group_token(&id, &display_name, &categories[0], &settings.map_base_names),
                path: m.path.display().to_string(),
                file_name: m
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                source: if workshop_id.is_some() { "workshop" } else { "local" },
                workshop_url: workshop_id.as_deref().map(workshop_url),
                format: m.format,
                display_name,
                author: manifest.author,
                version: manifest.package_version,
                categories,
                manifest_categories: manifest.categories,
                category_override,
                is_map: m.is_map,
                error: m.error.clone(),
                installed_at: m.installed_at,
                description: m.description.clone(),
                compat: compat_status(version.as_deref(), &manifest.compatible_versions),
                compatible_versions: manifest.compatible_versions,
                has_icon: own_icon(m).is_some()
                    || workshop_id.as_deref().is_some_and(|w| workshop.has_preview(w)),
                favorite: favorites.is_some_and(|f| f.contains(&id)),
                id,
            }
        })
        .collect()
}

/// Resolve active entries (display order) against the last scan: which
/// load-order group each belongs to and whether it is installed.
fn resolve(inner: &mut Inner, game: Game, entries: &[ActiveMod]) -> Vec<ActiveItem> {
    let scanned = inner.scans.get(&game).cloned().unwrap_or_default();
    let matcher = ActiveModMatcher::new(&scanned);
    let version = inner.versions.get(&game).cloned().flatten();
    let map_base_names = inner.settings().map_base_names.clone();
    entries
        .iter()
        .map(|a| {
            let hit = matcher.lookup(a).map(|i| &scanned[i]);
            let category = match hit {
                Some(m) => {
                    let pin = inner.override_for(game, &m.mod_name());
                    m.effective_categories(pin.as_deref())[0].clone()
                }
                None => "other".to_string(),
            };
            let compat = match hit.and_then(|m| m.manifest.as_ref()) {
                Some(mf) => compat_status(version.as_deref(), &mf.compatible_versions),
                None => CompatStatus::Unspecified,
            };
            ActiveItem {
                compat,
                token: group_token(&a.name, &a.display_name, &category, &map_base_names),
                name: a.name.clone(),
                display_name: a.display_name.clone(),
                installed: hit.is_some(),
            }
        })
        .collect()
}

/// Only paths the discovery itself returned may be read or written.
fn known_profile(game: Game, profile_path: &str, settings: &Settings) -> CmdResult<PathBuf> {
    let (install, _) = install_for(game, settings)?;
    profile::discover_profiles(&install)
        .into_iter()
        .find(|p| p.as_path() == Path::new(profile_path))
        .ok_or_else(|| "알 수 없는 프로필 경로입니다.".to_string())
}

// ------------------------------------------------------------------- //
// commands: setup
// ------------------------------------------------------------------- //

#[tauri::command]
fn constants() -> Constants {
    Constants {
        categories: OFFICIAL_CATEGORIES,
        groups: GROUPS.iter().map(|g| g.id).collect(),
    }
}

#[tauri::command]
fn get_installs(state: State<'_, AppState>) -> CmdResult<Vec<InstallInfo>> {
    let settings = settings_snapshot(&state)?;
    let libs = steam::discover_steam_libraries();
    Ok(Game::ALL
        .into_iter()
        .filter_map(|g| settings.resolve_install(g, &libs))
        .map(|i| InstallInfo {
            game: i.game,
            short_name: i.game.short_name(),
            version: game::read_game_version(&i.documents_dir),
            manual: settings.game(i.game).documents_dir.is_some(),
            documents_dir: i.documents_dir.display().to_string(),
            workshop_dir: i.workshop_dir.map(|p| p.display().to_string()),
        })
        .collect())
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> CmdResult<SettingsView> {
    let settings = settings_snapshot(&state)?;
    let libs = steam::discover_steam_libraries();
    let detected = Game::ALL
        .into_iter()
        .map(|g| {
            let found = game::detect_game_installs(g, &libs).into_iter().next();
            let paths = DetectedPaths {
                documents_dir: found.as_ref().map(|i| i.documents_dir.display().to_string()),
                workshop_dir: found
                    .and_then(|i| i.workshop_dir)
                    .or_else(|| game::detect_workshop_dir(g, &libs))
                    .map(|p| p.display().to_string()),
            };
            (if g == Game::Ets2 { "ets2" } else { "ats" }, paths)
        })
        .collect();
    Ok(SettingsView {
        settings,
        detected,
        backup_root: profile::default_backup_root().display().to_string(),
    })
}

/// Replace the settings. Manual paths must be existing directories.
#[tauri::command]
fn save_settings(mut settings: Settings, state: State<'_, AppState>) -> CmdResult<()> {
    settings.normalize();
    settings.backup_keep = settings.backup_keep.min(1000);
    for game in Game::ALL {
        let gs = settings.game(game);
        for (label, path) in [("문서 폴더", &gs.documents_dir), ("창작마당 폴더", &gs.workshop_dir)] {
            if let Some(p) = path {
                if !p.is_dir() {
                    return Err(format!(
                        "{} {label}가 존재하지 않습니다: {}",
                        game.short_name(),
                        p.display()
                    ));
                }
            }
        }
    }
    let mut inner = lock(&state)?;
    // where the user last was is not part of the form; keep what we have
    let current = inner.settings().clone();
    settings.last_game = current.last_game;
    for game in Game::ALL {
        settings.game_mut(game).last_profile = current.game(game).last_profile.clone();
    }
    settings.save(&default_settings_path()).map_err(err)?;
    *inner.settings() = settings;
    Ok(())
}

/// Remember the game and profile the user is on, for the next start.
#[tauri::command]
fn remember_place(game: String, profile_path: Option<String>, state: State<'_, AppState>) -> CmdResult<()> {
    let game = parse_game(&game)?;
    let mut inner = lock(&state)?;
    let settings = inner.settings();
    settings.last_game = Some(game);
    if let Some(p) = profile_path.filter(|p| !p.is_empty()) {
        settings.game_mut(game).last_profile = Some(PathBuf::from(p));
    }
    settings.save(&default_settings_path()).map_err(err)
}

// ------------------------------------------------------------------- //
// commands: mods
// ------------------------------------------------------------------- //

/// Scan the game's mod folder and workshop tree. Runs off the UI thread.
#[tauri::command(async)]
fn scan_mods(game: String, state: State<'_, AppState>) -> CmdResult<Vec<ModDto>> {
    let game = parse_game(&game)?;
    let (install, libs) = install_for(game, &settings_snapshot(&state)?)?;
    let version = game::read_game_version(&install.documents_dir);
    let install_dir = game::find_game_install_dir(game, &libs);
    let owned = game::owned_dlc_tokens(install_dir.as_deref());
    let mods = scanner::scan_game_install(&install, version.as_deref(), owned.as_ref());

    // many workshop manifests carry no display_name; the game mirrors the
    // real name into the profile's active_mods entry, so borrow it there
    let mut profile_names: HashMap<String, String> = HashMap::new();
    for sii in profile::discover_profiles(&install) {
        if let Ok(p) = profile::read_profile(&sii) {
            for a in p.active_mods {
                if !a.display_name.is_empty() {
                    profile_names.entry(a.name).or_insert(a.display_name);
                }
            }
        }
    }

    let mut inner = lock(&state)?;
    inner.scans.insert(game, mods);
    inner.versions.insert(game, version);
    inner.profile_names.insert(game, profile_names);
    Ok(build_dtos(&mut inner, game))
}

/// The last scan again, without rescanning - used after workshop names
/// arrived or a favourite / pin changed.
#[tauri::command]
fn mods_snapshot(game: String, state: State<'_, AppState>) -> CmdResult<Vec<ModDto>> {
    let game = parse_game(&game)?;
    let mut inner = lock(&state)?;
    Ok(build_dtos(&mut inner, game))
}

/// Look up names and preview images of the scanned workshop mods on Steam.
///
/// Only ids that are not cached yet (or are stale) are sent, and previews
/// are downloaded only for mods without an icon of their own. No lock is
/// held while the network is in use. Does nothing when the lookup is
/// switched off in the settings.
#[tauri::command(async)]
fn fetch_workshop_meta(game: String, state: State<'_, AppState>) -> CmdResult<WorkshopFetch> {
    let game = parse_game(&game)?;
    let mut result = WorkshopFetch::default();

    // workshop id -> whether the mod ships its own icon
    let (ids, to_fetch): (Vec<(String, bool)>, Vec<String>) = {
        let mut inner = lock(&state)?;
        if !inner.settings().workshop_lookup {
            return Ok(result);
        }
        let ids: Vec<(String, bool)> = inner
            .scans
            .get(&game)
            .map(|mods| {
                mods.iter()
                    .filter_map(|m| Some((m.workshop_id()?, own_icon(m).is_some())))
                    .collect()
            })
            .unwrap_or_default();
        let to_fetch = inner.workshop().missing_or_stale(ids.iter().map(|(id, _)| id.as_str()));
        (ids, to_fetch)
    };

    let agent = workshop_api::agent();
    if !to_fetch.is_empty() {
        let outcome = workshop_api::fetch_metadata(&agent, &to_fetch);
        result.asked = outcome.answered.len();
        result.titles = outcome.found.values().filter(|m| m.title.is_some()).count();
        result.error = outcome.error;
        lock(&state)?
            .workshop()
            .store(&outcome.answered, outcome.found)
            .map_err(err)?;
    }

    // previews: only where the mod has no icon and none is stored yet
    let wanted: Vec<(String, String)> = {
        let mut inner = lock(&state)?;
        let cache = inner.workshop();
        ids.iter()
            .filter(|(id, has_own_icon)| !has_own_icon && !cache.has_preview(id))
            .filter_map(|(id, _)| Some((id.clone(), cache.get(id)?.preview_url.clone()?)))
            .collect()
    };
    let writer = WorkshopCache::open(&workshop_cache_dir());
    for (id, url) in wanted {
        match workshop_api::fetch_preview(&agent, &url).and_then(|bytes| writer.write_preview(&id, &bytes)) {
            Ok(()) => result.previews += 1,
            Err(e) => result.error = Some(e.to_string()),
        }
    }
    Ok(result)
}

/// The mod's icon as a data URL, or null. Falls back to the cached
/// workshop preview when the mod ships no icon.
#[tauri::command]
fn mod_icon(game: String, id: String, state: State<'_, AppState>) -> CmdResult<Option<String>> {
    let game = parse_game(&game)?;
    let mut inner = lock(&state)?;
    inner.workshop();
    let Some(m) = inner
        .scans
        .get(&game)
        .and_then(|mods| mods.iter().find(|m| m.mod_name() == id))
    else {
        return Ok(None);
    };
    let bytes = match own_icon(m) {
        Some(own) => Some(own.to_vec()),
        None => m
            .workshop_id()
            .and_then(|w| inner.workshop.as_ref()?.read_preview(&w)),
    };
    Ok(bytes.and_then(|bytes| {
        let mime = sniff_image_mime(&bytes)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(format!("data:{mime};base64,{b64}"))
    }))
}

/// Pin a mod to a category (or clear the pin with `token: null`).
#[tauri::command]
fn set_category_override(
    game: String,
    id: String,
    token: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let game = parse_game(&game)?;
    if let Some(t) = &token {
        if !is_official(t) {
            return Err(format!("unknown category '{t}'"));
        }
    }
    let mut inner = lock(&state)?;
    let per_game = inner.overrides().entry(game.short_name().to_string()).or_default();
    match token {
        Some(t) => per_game.insert(id, t),
        None => per_game.remove(&id),
    };
    save_json(&overrides_path(), inner.overrides())
}

#[tauri::command]
fn set_favorite(game: String, id: String, favorite: bool, state: State<'_, AppState>) -> CmdResult<()> {
    let game = parse_game(&game)?;
    let mut inner = lock(&state)?;
    let per_game = inner.favorites().entry(game.short_name().to_string()).or_default();
    if favorite {
        per_game.insert(id);
    } else {
        per_game.remove(&id);
    }
    save_json(&favorites_path(), inner.favorites())
}

/// For each given mod, the names of the profiles whose load order lists it.
#[tauri::command(async)]
fn mod_usage(game: String, ids: Vec<String>, state: State<'_, AppState>) -> CmdResult<HashMap<String, Vec<String>>> {
    let game = parse_game(&game)?;
    let (install, _) = install_for(game, &settings_snapshot(&state)?)?;
    let profiles: Vec<profile::Profile> = profile::discover_profiles(&install)
        .iter()
        .filter_map(|sii| profile::read_profile(sii).ok())
        .collect();
    let inner = lock(&state)?;
    let empty = Vec::new();
    let scanned = inner.scans.get(&game).unwrap_or(&empty);
    Ok(ids
        .into_iter()
        .filter_map(|id| {
            let m = scanned.iter().find(|m| m.mod_name() == id)?;
            // the game's tokens can differ from the mod name, so go through
            // the same matcher the load order uses
            let matcher = ActiveModMatcher::new(std::slice::from_ref(m));
            let users = profiles
                .iter()
                .filter(|p| p.active_mods.iter().any(|a| matcher.lookup(a).is_some()))
                .map(|p| p.profile_name.clone())
                .collect();
            Some((id, users))
        })
        .collect())
}

/// Move local mods to the OS trash. There is no hard-delete fallback: a
/// failed move is reported, never turned into destroying the file.
/// Workshop mods are skipped - they belong to Steam.
#[tauri::command(async)]
fn delete_mods(game: String, ids: Vec<String>, state: State<'_, AppState>) -> CmdResult<DeleteResult> {
    let game = parse_game(&game)?;
    let (install, _) = install_for(game, &settings_snapshot(&state)?)?;
    let mod_dir = install.mod_dir();

    // decide what to delete under the lock, move files without it
    let mut result = DeleteResult::default();
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    {
        let inner = lock(&state)?;
        let empty = Vec::new();
        let scanned = inner.scans.get(&game).unwrap_or(&empty);
        for id in ids {
            let Some(m) = scanned.iter().find(|m| m.mod_name() == id) else {
                result.failed.push(DeleteFailure { id, error: "스캔 목록에 없는 모드입니다.".into() });
                continue;
            };
            if m.workshop_id().is_some() {
                result.skipped_workshop += 1;
            } else if m.path.parent() != Some(mod_dir.as_path()) {
                // only direct children of the game's own mod folder
                result.failed.push(DeleteFailure { id, error: "모드 폴더 밖의 경로입니다.".into() });
            } else {
                targets.push((id, m.path.clone()));
            }
        }
    }
    for (id, path) in targets {
        match trash::delete(&path) {
            Ok(()) => result.deleted.push(id),
            Err(e) => result.failed.push(DeleteFailure { id, error: e.to_string() }),
        }
    }
    if !result.deleted.is_empty() {
        let mut inner = lock(&state)?;
        if let Some(scanned) = inner.scans.get_mut(&game) {
            scanned.retain(|m| !result.deleted.contains(&m.mod_name()));
        }
    }
    Ok(result)
}

fn scanned_path(state: &State<'_, AppState>, game: Game, id: &str) -> CmdResult<(PathBuf, Option<String>)> {
    let inner = lock(state)?;
    inner
        .scans
        .get(&game)
        .and_then(|mods| mods.iter().find(|m| m.mod_name() == id))
        .map(|m| (m.path.clone(), m.workshop_id()))
        .ok_or_else(|| "스캔 목록에 없는 모드입니다.".to_string())
}

/// Show the mod's file in the system file manager.
#[tauri::command]
fn reveal_mod(game: String, id: String, state: State<'_, AppState>) -> CmdResult<()> {
    let (path, _) = scanned_path(&state, parse_game(&game)?, &id)?;
    if !path.exists() {
        return Err("파일이 더 이상 존재하지 않습니다.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer wants the path quoted on its own, right after the comma
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
            .map_err(err)?;
    }
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg("-R").arg(&path).spawn().map_err(err)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(&path))
        .spawn()
        .map_err(err)?;
    Ok(())
}

/// Open the mod's Steam Workshop page in the default browser.
#[tauri::command]
fn open_workshop_page(game: String, id: String, state: State<'_, AppState>) -> CmdResult<()> {
    let (_, workshop_id) = scanned_path(&state, parse_game(&game)?, &id)?;
    // the URL is built here from a numeric id, never taken from the UI
    let url = workshop_url(&workshop_id.ok_or("창작마당 모드가 아닙니다.")?);
    #[cfg(windows)]
    std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .spawn()
        .map_err(err)?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&url).spawn().map_err(err)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(&url).spawn().map_err(err)?;
    Ok(())
}

// ------------------------------------------------------------------- //
// commands: profiles and load order
// ------------------------------------------------------------------- //

#[tauri::command]
fn list_profiles(game: String, state: State<'_, AppState>) -> CmdResult<Vec<ProfileDto>> {
    let (install, _) = install_for(parse_game(&game)?, &settings_snapshot(&state)?)?;
    Ok(profile::discover_profiles(&install)
        .into_iter()
        .map(|sii| {
            let path = sii.display().to_string();
            match profile::read_profile(&sii) {
                Ok(p) => ProfileDto {
                    path,
                    profile_name: p.profile_name,
                    active_count: p.active_mods.len(),
                    error: None,
                },
                Err(e) => ProfileDto {
                    path,
                    profile_name: "?".into(),
                    active_count: 0,
                    error: Some(e.to_string()),
                },
            }
        })
        .collect())
}

/// The profile's active mods in display order (top = loaded last).
#[tauri::command]
fn read_load_order(game: String, profile_path: String, state: State<'_, AppState>) -> CmdResult<Vec<ActiveItem>> {
    let game = parse_game(&game)?;
    let sii = known_profile(game, &profile_path, &settings_snapshot(&state)?)?;
    let p = profile::read_profile(&sii).map_err(err)?;
    let display_order: Vec<ActiveMod> = p.active_mods.into_iter().rev().collect();
    let mut inner = lock(&state)?;
    Ok(resolve(&mut inner, game, &display_order))
}

/// Re-resolve an edited list (display order) after a rescan or a category
/// change, without touching the profile on disk.
#[tauri::command]
fn resolve_items(game: String, items: Vec<ActiveMod>, state: State<'_, AppState>) -> CmdResult<Vec<ActiveItem>> {
    let game = parse_game(&game)?;
    let mut inner = lock(&state)?;
    Ok(resolve(&mut inner, game, &items))
}

/// Which active mods (display order, top first) lose `def/` files to a
/// mod above them, and which files are shared too widely to matter.
#[tauri::command]
fn analyze_conflicts(game: String, items: Vec<ActiveMod>, state: State<'_, AppState>) -> CmdResult<ConflictReport> {
    let game = parse_game(&game)?;
    let inner = lock(&state)?;
    let empty = Vec::new();
    let scanned = inner.scans.get(&game).unwrap_or(&empty);
    let matcher = ActiveModMatcher::new(scanned);
    // an active entry that is not installed has no files to compare
    let active: Vec<(&str, &[String])> = items
        .iter()
        .filter_map(|a| {
            let m = &scanned[matcher.lookup(a)?];
            Some((a.name.as_str(), m.def_files.as_slice()))
        })
        .collect();
    Ok(conflicts::analyze(&active))
}

/// Group headers and misplacement flags for tokens in display order.
#[tauri::command]
fn layout_rows(tokens: Vec<String>) -> Layout {
    Layout {
        misplaced: load_order::misplaced_count(&tokens),
        rows: load_order::build_rows(&tokens),
    }
}

/// Permutation that sorts every mod into its group (stable).
#[tauri::command]
fn auto_sort(tokens: Vec<String>) -> Vec<usize> {
    load_order::auto_sort_order(&tokens)
}

/// Write the list (display order) to profile.sii after backing the whole
/// profile directory up, then trim old backups to the keep limit.
#[tauri::command(async)]
fn save_load_order(
    game: String,
    profile_path: String,
    items: Vec<ActiveMod>,
    state: State<'_, AppState>,
) -> CmdResult<SaveResult> {
    let settings = settings_snapshot(&state)?;
    let game = parse_game(&game)?;
    let sii = known_profile(game, &profile_path, &settings)?;
    // file order is the reverse of display order: index 0 = bottom
    let mods: Vec<ActiveMod> = items.into_iter().rev().collect();
    let root = backup_root(game);
    let backup = profile::save_active_mods(&sii, &mods, &root).map_err(err)?;
    Ok(SaveResult {
        backup: backup.path.display().to_string(),
        pruned: profile::prune_backups(&sii, &root, settings.backup_keep),
    })
}

// ------------------------------------------------------------------- //
// commands: backups
// ------------------------------------------------------------------- //

#[tauri::command]
fn list_backups(game: String, profile_path: String, state: State<'_, AppState>) -> CmdResult<Vec<BackupDto>> {
    let game = parse_game(&game)?;
    let sii = known_profile(game, &profile_path, &settings_snapshot(&state)?)?;
    Ok(profile::list_backups(&sii, &backup_root(game))
        .into_iter()
        .map(|b| BackupDto {
            path: b.path.display().to_string(),
            timestamp: b.timestamp,
            size_bytes: b.size_bytes,
        })
        .collect())
}

/// Put a backup back over the profile. The current state is backed up
/// first; returns that safety backup's path.
#[tauri::command(async)]
fn restore_backup(
    game: String,
    profile_path: String,
    backup_path: String,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let settings = settings_snapshot(&state)?;
    let game = parse_game(&game)?;
    let sii = known_profile(game, &profile_path, &settings)?;
    let root = backup_root(game);
    let safety = profile::restore_backup(&sii, &root, Path::new(&backup_path)).map_err(err)?;
    // never trim below two here: the one just restored and the safety copy
    profile::prune_backups(&sii, &root, settings.backup_keep.max(2));
    Ok(safety.path.display().to_string())
}

#[tauri::command]
fn delete_backup(
    game: String,
    profile_path: String,
    backup_path: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let game = parse_game(&game)?;
    let sii = known_profile(game, &profile_path, &settings_snapshot(&state)?)?;
    profile::delete_backup(&sii, &backup_root(game), Path::new(&backup_path)).map_err(err)
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            constants,
            get_installs,
            get_settings,
            save_settings,
            remember_place,
            scan_mods,
            mods_snapshot,
            fetch_workshop_meta,
            mod_icon,
            set_category_override,
            set_favorite,
            mod_usage,
            delete_mods,
            reveal_mod,
            open_workshop_page,
            list_profiles,
            read_load_order,
            resolve_items,
            analyze_conflicts,
            layout_rows,
            auto_sort,
            save_load_order,
            list_backups,
            restore_backup,
            delete_backup,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the application");
}
