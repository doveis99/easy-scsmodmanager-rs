//! Tauri shell: exposes scsmm-core to the web UI as commands.
//!
//! All mod and profile logic lives in scsmm-core; this layer only keeps
//! the last scan in memory, shapes data for the UI and persists the few
//! user choices (category overrides).

use std::collections::HashMap;
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
use scsmm_core::load_order::{self, Row, DEFAULT_MAP_BASE_NAMES, GROUPS, MAP_BASE};
use scsmm_core::manifest::sniff_image_mime;
use scsmm_core::profile::{self, ActiveMod};
use scsmm_core::scanner::{self, ScannedMod};
use scsmm_core::steam;
use scsmm_core::workshop_api::{self, WorkshopCache};

type CmdResult<T> = Result<T, String>;

#[derive(Default)]
struct AppState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Last scan per game.
    scans: HashMap<Game, Vec<ScannedMod>>,
    /// Game version read at the last scan, per game.
    versions: HashMap<Game, Option<String>>,
    /// mod name -> category token, per game. Loaded lazily from disk.
    overrides: Option<HashMap<String, HashMap<String, String>>>,
    /// active name -> display name as the game wrote it into the profiles.
    profile_names: HashMap<Game, HashMap<String, String>>,
    /// Steam Workshop titles and previews. Loaded lazily from disk.
    workshop: Option<WorkshopCache>,
}

fn overrides_path() -> PathBuf {
    profile::default_data_dir().join("category_overrides.json")
}

/// Cache, not data: everything in here can be fetched again.
fn workshop_cache_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) if cfg!(windows) => PathBuf::from(local).join("easy-scsmodmanager-rs").join("workshop"),
        _ => profile::default_data_dir().join("cache").join("workshop"),
    }
}

impl Inner {
    fn workshop(&mut self) -> &mut WorkshopCache {
        self.workshop
            .get_or_insert_with(|| WorkshopCache::open(&workshop_cache_dir()))
    }

    fn overrides(&mut self) -> &mut HashMap<String, HashMap<String, String>> {
        self.overrides.get_or_insert_with(|| {
            fs::read(overrides_path())
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .unwrap_or_default()
        })
    }

    fn override_for(&mut self, game: Game, mod_name: &str) -> Option<String> {
        self.overrides()
            .get(game.short_name())
            .and_then(|m| m.get(mod_name))
            .cloned()
    }

    fn save_overrides(&mut self) -> CmdResult<()> {
        let path = overrides_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(self.overrides()).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| e.to_string())
    }
}

fn parse_game(game: &str) -> CmdResult<Game> {
    Game::parse(game).ok_or_else(|| format!("unknown game '{game}'"))
}

fn install_for(game: Game) -> CmdResult<(GameInstall, Vec<PathBuf>)> {
    let libs = steam::discover_steam_libraries();
    let install = game::detect_game_installs(game, &libs)
        .into_iter()
        .next()
        .ok_or_else(|| format!("{} 설치를 찾지 못했습니다.", game.short_name()))?;
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

fn group_token(name: &str, display_name: &str, category: &str) -> String {
    if load_order::is_map_base(name, display_name, DEFAULT_MAP_BASE_NAMES) {
        MAP_BASE.to_string()
    } else {
        category.to_string()
    }
}

fn mod_dto(
    m: &ScannedMod,
    display_name: String,
    category_override: Option<String>,
    game_version: Option<&str>,
    has_icon: bool,
) -> ModDto {
    let id = m.mod_name();
    let categories = m.effective_categories(category_override.as_deref());
    let manifest = m.manifest.clone().unwrap_or_default();
    let workshop_id = m.workshop_id();
    ModDto {
        group_token: group_token(&id, &display_name, &categories[0]),
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
        compat: compat_status(game_version, &manifest.compatible_versions),
        compatible_versions: manifest.compatible_versions,
        has_icon,
        id,
    }
}

// ------------------------------------------------------------------- //
// commands
// ------------------------------------------------------------------- //

#[tauri::command]
fn constants() -> Constants {
    Constants {
        categories: OFFICIAL_CATEGORIES,
        groups: GROUPS.iter().map(|g| g.id).collect(),
    }
}

#[tauri::command]
fn get_installs() -> Vec<InstallInfo> {
    game::detect_all_installs()
        .into_iter()
        .map(|i| InstallInfo {
            game: i.game,
            short_name: i.game.short_name(),
            version: game::read_game_version(&i.documents_dir),
            documents_dir: i.documents_dir.display().to_string(),
            workshop_dir: i.workshop_dir.map(|p| p.display().to_string()),
        })
        .collect()
}

/// DTOs for the last scan of `game`, with names and icons filled in from
/// the profile and the workshop cache where the mod itself has none.
fn build_dtos(inner: &mut Inner, game: Game) -> Vec<ModDto> {
    // force the lazy loads first so the fields can be borrowed side by side
    inner.overrides();
    inner.workshop();
    let Inner {
        scans,
        versions,
        overrides,
        profile_names,
        workshop,
    } = &*inner;
    let (Some(mods), Some(workshop)) = (scans.get(&game), workshop.as_ref()) else {
        return Vec::new();
    };
    let version = versions.get(&game).cloned().flatten();
    let per_game_overrides = overrides.as_ref().and_then(|o| o.get(game.short_name()));
    let names = profile_names.get(&game);
    mods.iter()
        .map(|m| {
            let id = m.mod_name();
            let workshop_id = m.workshop_id();
            // name chain: manifest -> the profile's active entry -> the
            // workshop title -> the file stem
            let has_name = m.manifest.as_ref().is_some_and(|mf| !mf.display_name.is_empty());
            let borrowed = names
                .and_then(|n| n.get(&id))
                .map(String::as_str)
                .or_else(|| workshop_id.as_deref().and_then(|w| workshop.title(w)));
            let display = match borrowed {
                Some(name) if !has_name => name.to_string(),
                _ => m.display_name(),
            };
            let has_icon = own_icon(m).is_some()
                || workshop_id.as_deref().is_some_and(|w| workshop.has_preview(w));
            let pinned = per_game_overrides.and_then(|o| o.get(&id)).cloned();
            mod_dto(m, display, pinned, version.as_deref(), has_icon)
        })
        .collect()
}

/// The mod's own icon, when it is an image the webview can show.
fn own_icon(m: &ScannedMod) -> Option<&[u8]> {
    m.icon.as_deref().filter(|bytes| sniff_image_mime(bytes).is_some())
}

/// Scan the game's mod folder and workshop tree. Runs off the UI thread.
#[tauri::command(async)]
fn scan_mods(game: String, state: State<'_, AppState>) -> CmdResult<Vec<ModDto>> {
    let game = parse_game(&game)?;
    let (install, libs) = install_for(game)?;
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

    let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
    inner.scans.insert(game, mods);
    inner.versions.insert(game, version);
    inner.profile_names.insert(game, profile_names);
    Ok(build_dtos(&mut inner, game))
}

/// The last scan again, without rescanning - used after workshop names or
/// previews arrived.
#[tauri::command]
fn mods_snapshot(game: String, state: State<'_, AppState>) -> CmdResult<Vec<ModDto>> {
    let game = parse_game(&game)?;
    let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
    Ok(build_dtos(&mut inner, game))
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

/// Look up names and preview images of the scanned workshop mods on Steam.
///
/// Only ids that are not cached yet (or are stale) are sent, and previews
/// are downloaded only for mods without an icon of their own. No lock is
/// held while the network is in use.
#[tauri::command(async)]
fn fetch_workshop_meta(game: String, state: State<'_, AppState>) -> CmdResult<WorkshopFetch> {
    let game = parse_game(&game)?;
    let mut result = WorkshopFetch::default();

    // workshop id -> whether the mod ships its own icon
    let (ids, to_fetch): (Vec<(String, bool)>, Vec<String>) = {
        let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
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
        let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
        inner
            .workshop()
            .store(&outcome.answered, outcome.found)
            .map_err(|e| e.to_string())?;
    }

    // previews: only where the mod has no icon and none is stored yet
    let wanted: Vec<(String, String)> = {
        let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
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
    let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
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

#[tauri::command]
fn list_profiles(game: String) -> CmdResult<Vec<ProfileDto>> {
    let (install, _) = install_for(parse_game(&game)?)?;
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

/// Only paths the discovery itself returned may be read or written.
fn known_profile(game: Game, profile_path: &str) -> CmdResult<PathBuf> {
    let (install, _) = install_for(game)?;
    profile::discover_profiles(&install)
        .into_iter()
        .find(|p| p.as_path() == Path::new(profile_path))
        .ok_or_else(|| "알 수 없는 프로필 경로입니다.".to_string())
}

/// Resolve active entries (display order) against the last scan: which
/// load-order group each belongs to and whether it is installed.
fn resolve(inner: &mut Inner, game: Game, entries: &[ActiveMod]) -> Vec<ActiveItem> {
    let scanned = inner.scans.get(&game).cloned().unwrap_or_default();
    let matcher = ActiveModMatcher::new(&scanned);
    let version = inner.versions.get(&game).cloned().flatten();
    entries
        .iter()
        .map(|a| {
            let hit = matcher.lookup(a).map(|i| &scanned[i]);
            let category = match hit {
                Some(m) => {
                    let ov = inner.override_for(game, &m.mod_name());
                    m.effective_categories(ov.as_deref())[0].clone()
                }
                None => "other".to_string(),
            };
            let compat = match hit.and_then(|m| m.manifest.as_ref()) {
                Some(mf) => compat_status(version.as_deref(), &mf.compatible_versions),
                None => CompatStatus::Unspecified,
            };
            ActiveItem {
                compat,
                token: group_token(&a.name, &a.display_name, &category),
                name: a.name.clone(),
                display_name: a.display_name.clone(),
                installed: hit.is_some(),
            }
        })
        .collect()
}

/// The profile's active mods in display order (top = loaded last).
#[tauri::command]
fn read_load_order(game: String, profile_path: String, state: State<'_, AppState>) -> CmdResult<Vec<ActiveItem>> {
    let game = parse_game(&game)?;
    let sii = known_profile(game, &profile_path)?;
    let p = profile::read_profile(&sii).map_err(|e| e.to_string())?;
    let display_order: Vec<ActiveMod> = p.active_mods.into_iter().rev().collect();
    let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
    Ok(resolve(&mut inner, game, &display_order))
}

/// Re-resolve an edited list (display order) after a rescan or a category
/// change, without touching the profile on disk.
#[tauri::command]
fn resolve_items(game: String, items: Vec<ActiveMod>, state: State<'_, AppState>) -> CmdResult<Vec<ActiveItem>> {
    let game = parse_game(&game)?;
    let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
    Ok(resolve(&mut inner, game, &items))
}

/// Which active mods (display order, top first) lose `def/` files to a
/// mod above them, and which files are shared too widely to matter.
#[tauri::command]
fn analyze_conflicts(game: String, items: Vec<ActiveMod>, state: State<'_, AppState>) -> CmdResult<ConflictReport> {
    let game = parse_game(&game)?;
    let inner = state.inner.lock().map_err(|e| e.to_string())?;
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
/// profile directory up. Returns the backup path.
#[tauri::command(async)]
fn save_load_order(game: String, profile_path: String, items: Vec<ActiveMod>) -> CmdResult<String> {
    let sii = known_profile(parse_game(&game)?, &profile_path)?;
    // file order is the reverse of display order: index 0 = bottom
    let mods: Vec<ActiveMod> = items.into_iter().rev().collect();
    let backup = profile::save_active_mods(&sii, &mods, &profile::default_backup_root())
        .map_err(|e| e.to_string())?;
    Ok(backup.path.display().to_string())
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
    let mut inner = state.inner.lock().map_err(|e| e.to_string())?;
    let per_game = inner.overrides().entry(game.short_name().to_string()).or_default();
    match token {
        Some(t) => per_game.insert(id, t),
        None => per_game.remove(&id),
    };
    inner.save_overrides()
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            constants,
            get_installs,
            scan_mods,
            mod_icon,
            mods_snapshot,
            fetch_workshop_meta,
            list_profiles,
            read_load_order,
            resolve_items,
            analyze_conflicts,
            layout_rows,
            auto_sort,
            save_load_order,
            set_category_override,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the application");
}
