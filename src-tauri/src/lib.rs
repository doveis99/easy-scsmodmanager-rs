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
use serde::{Deserialize, Serialize};
use tauri::State;

use scsmm_core::categories::{is_official, OFFICIAL_CATEGORIES};
use scsmm_core::game::{self, Game, GameInstall};
use scsmm_core::identity::{workshop_url, ActiveModMatcher};
use scsmm_core::load_order::{self, Row, DEFAULT_MAP_BASE_NAMES, GROUPS, MAP_BASE};
use scsmm_core::manifest::sniff_image_mime;
use scsmm_core::profile::{self, ActiveMod};
use scsmm_core::scanner::{self, ScannedMod};
use scsmm_core::steam;

type CmdResult<T> = Result<T, String>;

#[derive(Default)]
struct AppState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Last scan per game.
    scans: HashMap<Game, Vec<ScannedMod>>,
    /// mod name -> category token, per game. Loaded lazily from disk.
    overrides: Option<HashMap<String, HashMap<String, String>>>,
}

fn overrides_path() -> PathBuf {
    profile::default_data_dir().join("category_overrides.json")
}

impl Inner {
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
    has_icon: bool,
}

#[derive(Serialize)]
struct ProfileDto {
    path: String,
    profile_name: String,
    active_count: usize,
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ActiveItem {
    name: String,
    display_name: String,
    /// Load-order group token.
    token: String,
    installed: bool,
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

fn mod_dto(m: &ScannedMod, display_name: String, category_override: Option<String>) -> ModDto {
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
        compatible_versions: manifest.compatible_versions,
        has_icon: m.icon.as_deref().and_then(sniff_image_mime).is_some(),
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
    let dtos = mods
        .iter()
        .map(|m| {
            let id = m.mod_name();
            let has_name = m.manifest.as_ref().is_some_and(|mf| !mf.display_name.is_empty());
            let display = match profile_names.get(&id) {
                Some(name) if !has_name => name.clone(),
                _ => m.display_name(),
            };
            mod_dto(m, display, inner.override_for(game, &id))
        })
        .collect();
    inner.scans.insert(game, mods);
    Ok(dtos)
}

/// The mod's icon as a data URL, or null.
#[tauri::command]
fn mod_icon(game: String, id: String, state: State<'_, AppState>) -> CmdResult<Option<String>> {
    let game = parse_game(&game)?;
    let inner = state.inner.lock().map_err(|e| e.to_string())?;
    let icon = inner
        .scans
        .get(&game)
        .and_then(|mods| mods.iter().find(|m| m.mod_name() == id))
        .and_then(|m| m.icon.as_deref());
    Ok(icon.and_then(|bytes| {
        let mime = sniff_image_mime(bytes)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
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
            ActiveItem {
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
fn save_load_order(game: String, profile_path: String, items: Vec<ActiveItem>) -> CmdResult<String> {
    let sii = known_profile(parse_game(&game)?, &profile_path)?;
    // file order is the reverse of display order: index 0 = bottom
    let mods: Vec<ActiveMod> = items
        .into_iter()
        .rev()
        .map(|i| ActiveMod {
            name: i.name,
            display_name: i.display_name,
        })
        .collect();
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
            list_profiles,
            read_load_order,
            resolve_items,
            layout_rows,
            auto_sort,
            save_load_order,
            set_category_override,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the application");
}
