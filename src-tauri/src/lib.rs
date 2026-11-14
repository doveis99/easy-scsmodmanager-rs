//! Tauri shell: exposes scsmm-core to the web UI as commands.
//!
//! All mod and profile logic lives in scsmm-core; this layer keeps the
//! last scan in memory, shapes data for the UI and persists the user's
//! choices (settings, category pins, favourites).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use tauri::ipc::Channel;
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
use scsmm_core::extract::{extract_scs_with, ExtractOptions, ExtractResult};
use scsmm_core::gamepaths::GamePaths;
use scsmm_core::scs::ScsFormat;
use scsmm_core::settings::{default_settings_path, Settings, ViewSettings};
use scsmm_core::share::{self, check_share, MapComboEntry, ShareCheck, ShareList};
use scsmm_core::steam;
use scsmm_core::workshop_api::{self, WorkshopCache};

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// An error the interface shows in its own language: `@code` followed by
/// tab-separated values. The texts live in the UI's language files under
/// `err.<code>`, with `{0}`, `{1}`... standing for the values.
fn coded(code: &str, values: &[&str]) -> String {
    let mut out = format!("@{code}");
    for value in values {
        out.push('\t');
        out.push_str(value);
    }
    out
}

#[derive(Default)]
struct AppState {
    inner: Mutex<Inner>,
    /// One extraction at a time; the flags are outside the lock because
    /// the extraction runs for seconds and must stay cancellable.
    extract_busy: AtomicBool,
    extract_cancel: AtomicBool,
    /// Folder of the last extraction - the only one the UI may ask to open.
    last_extract: Mutex<Option<PathBuf>>,
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
    /// mod name -> pinned category tokens, the main one first. Loaded
    /// lazily from disk.
    overrides: Option<PerGame<HashMap<String, Vec<String>>>>,
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
    if profile::data_dir_is_overridden() {
        return profile::default_data_dir().join("cache").join("workshop");
    }
    system_workshop_cache_dir()
}

/// The list of the game's own file paths, which names the files of
/// locked archives. Cache as well: rebuilt from the game when missing.
fn game_paths_cache(game: Game) -> PathBuf {
    let base = workshop_cache_dir();
    base.parent()
        .unwrap_or(&base)
        .join("gamepaths")
        .join(format!("{}.bin", game.short_name().to_lowercase()))
}

/// Open (or build) the path list of `game`. `None` when the game's
/// install folder is not found - locked archives then stay as they were.
fn game_paths_for(game: Game, libs: &[PathBuf]) -> Option<GamePaths> {
    let install_dir = game::find_game_install_dir(game, libs)?;
    GamePaths::load_or_build(&install_dir, &game_paths_cache(game)).ok()
}

fn system_workshop_cache_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) if cfg!(windows) => PathBuf::from(local).join("easy-scsmodmanager-rs").join("workshop"),
        _ => profile::system_data_dir().join("cache").join("workshop"),
    }
}

/// The `data` folder next to the executable, if it can be written to.
///
/// The app is portable: settings, category pins, favourites, profile
/// backups, the workshop cache and the webview's own files all live there,
/// so the whole folder can be moved or put on a stick. A read-only
/// location (Program Files, a CD) falls back to the per-user folders.
fn portable_data_dir() -> Option<(PathBuf, bool)> {
    let dir = std::env::current_exe().ok()?.parent()?.join("data");
    let created = !dir.exists();
    fs::create_dir_all(&dir).ok()?;
    let probe = dir.join(".write-test");
    fs::write(&probe, b"").ok()?;
    let _ = fs::remove_file(&probe);
    Some((dir, created))
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if !target.exists() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// First start of a fresh portable folder: bring along what an earlier,
/// non-portable build kept in the per-user folders (settings, pins,
/// favourites, profile backups, workshop cache). Copies only - the old
/// folders are left as they are - and never overwrites.
fn adopt_system_data(data: &Path) {
    let old_data = profile::system_data_dir();
    if old_data.is_dir() {
        let _ = copy_dir(&old_data, data);
    }
    let old_cache = system_workshop_cache_dir();
    if old_cache.is_dir() {
        let _ = copy_dir(&old_cache, &data.join("cache").join("workshop"));
    }
}

/// Switch to portable storage. Must run before the first data access and
/// before the webview is created.
fn enter_portable_mode() {
    let Some((data, created)) = portable_data_dir() else {
        return;
    };
    if created {
        adopt_system_data(&data);
    }
    // WebView2 keeps its profile (cache, local storage) where this points
    std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", data.join("webview"));
    profile::set_data_dir(data);
}

fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

/// A pin as it may be on disk: older versions stored one token per mod.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum PinsOnDisk {
    One(String),
    Many(Vec<String>),
}

fn load_overrides(path: &Path) -> PerGame<HashMap<String, Vec<String>>> {
    let on_disk: PerGame<HashMap<String, PinsOnDisk>> = load_json(path);
    on_disk
        .into_iter()
        .map(|(game, pins)| {
            let pins = pins
                .into_iter()
                .map(|(name, pin)| match pin {
                    PinsOnDisk::One(token) => (name, vec![token]),
                    PinsOnDisk::Many(tokens) => (name, tokens),
                })
                .filter(|(_, tokens)| !tokens.is_empty())
                .collect();
            (game, pins)
        })
        .collect()
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

    fn overrides(&mut self) -> &mut PerGame<HashMap<String, Vec<String>>> {
        self.overrides
            .get_or_insert_with(|| load_overrides(&overrides_path()))
    }

    fn favorites(&mut self) -> &mut PerGame<BTreeSet<String>> {
        self.favorites
            .get_or_insert_with(|| load_json(&favorites_path()))
    }

    /// The categories the user pinned the mod to; empty when automatic.
    fn pins_for(&mut self, game: Game, mod_name: &str) -> Vec<String> {
        self.overrides()
            .get(game.short_name())
            .and_then(|m| m.get(mod_name))
            .cloned()
            .unwrap_or_default()
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
        .ok_or_else(|| coded("no_install", &[game.short_name()]))?;
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
    /// Effective categories (pins > content > manifest), main one first.
    categories: Vec<String>,
    manifest_categories: Vec<String>,
    /// The categories the user pinned by hand; empty when automatic.
    category_pins: Vec<String>,
    /// What the categories would be without pins.
    auto_categories: Vec<String>,
    /// The automatic categories come from the mod's files, which overrule
    /// what its manifest declares (or fill in for an empty one).
    judged_from_content: bool,
    /// A category the files point to without being sure enough to overrule
    /// the manifest: offered to the user, never applied by itself.
    suggested_category: Option<&'static str>,
    /// What the analysis saw in the files, the strongest signals first.
    evidence: Vec<scsmm_core::classify::Signal>,
    /// Files the archive holds without listing them (a locked archive),
    /// and how many of those could be named all the same.
    hidden_files: usize,
    hidden_named: usize,
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
    /// category token -> id of the load-order group it sorts into.
    category_groups: HashMap<&'static str, &'static str>,
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
            let category_pins = pins.and_then(|o| o.get(&id)).cloned().unwrap_or_default();
            let categories = m.effective_categories(&category_pins);
            let auto_categories = m.effective_categories(&[]);
            // a map descriptor or the physics file may have decided instead
            let judged_from_content = m
                .verdict()
                .is_some_and(|v| v.applied && auto_categories.first().map(String::as_str) == Some(v.category));
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
                auto_categories,
                judged_from_content,
                suggested_category: m.suggested_category(&category_pins),
                evidence: m.evidence.signals.clone(),
                category_pins,
                hidden_files: m.hidden.len(),
                hidden_named: m.hidden_named,
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
                    let pins = inner.pins_for(game, &m.mod_name());
                    m.effective_categories(&pins)[0].clone()
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
        .ok_or_else(|| coded("unknown_profile", &[]))
}

// ------------------------------------------------------------------- //
// commands: setup
// ------------------------------------------------------------------- //

#[tauri::command]
fn constants() -> Constants {
    Constants {
        categories: OFFICIAL_CATEGORIES,
        groups: GROUPS.iter().map(|g| g.id).collect(),
        category_groups: OFFICIAL_CATEGORIES
            .iter()
            .map(|c| (*c, GROUPS[load_order::group_index_for_token(c)].id))
            .collect(),
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
        for (code, path) in [("docs_missing", &gs.documents_dir), ("workshop_missing", &gs.workshop_dir)] {
            if let Some(p) = path {
                if !p.is_dir() {
                    return Err(coded(code, &[game.short_name(), &p.display().to_string()]));
                }
            }
        }
    }
    let mut inner = lock(&state)?;
    // where the user last was is not part of the form; keep what we have
    let current = inner.settings().clone();
    settings.last_game = current.last_game;
    settings.view = current.view.clone();
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

/// Remember the library's sort order and filters, for the next start.
#[tauri::command]
fn remember_view(mut view: ViewSettings, state: State<'_, AppState>) -> CmdResult<()> {
    view.normalize();
    let mut inner = lock(&state)?;
    let settings = inner.settings();
    if settings.view == view {
        return Ok(());
    }
    settings.view = view;
    settings.save(&default_settings_path()).map_err(err)
}

// ------------------------------------------------------------------- //
// commands: mods
// ------------------------------------------------------------------- //

/// Everything one scan produces, before it is stored in the app state.
struct ScanOutcome {
    mods: Vec<ScannedMod>,
    version: Option<String>,
    profile_names: HashMap<String, String>,
}

/// Scan the game's mod folder and workshop tree.
fn scan_game(game: Game, settings: &Settings) -> CmdResult<ScanOutcome> {
    let (install, libs) = install_for(game, settings)?;
    let version = game::read_game_version(&install.documents_dir);
    let install_dir = game::find_game_install_dir(game, &libs);
    let owned = game::owned_dlc_tokens(install_dir.as_deref());
    let mut game_paths = game_paths_for(game, &libs);
    let mods = scanner::scan_game_install(&install, version.as_deref(), owned.as_ref(), game_paths.as_mut());

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

    Ok(ScanOutcome {
        mods,
        version,
        profile_names,
    })
}

type Prewarm = (Game, Settings, std::thread::JoinHandle<CmdResult<ScanOutcome>>);

/// The scan started at launch. The webview needs a few hundred ms to come
/// up; scanning during that time means the first screen has its mods the
/// moment it asks for them.
static PREWARM: Mutex<Option<Prewarm>> = Mutex::new(None);

fn start_prewarm() {
    let settings = Settings::load(&default_settings_path());
    // the game the UI opens first: where the user left off, else ETS2
    let game = settings.last_game.unwrap_or(Game::Ets2);
    let for_scan = settings.clone();
    let handle = std::thread::spawn(move || scan_game(game, &for_scan));
    if let Ok(mut slot) = PREWARM.lock() {
        *slot = Some((game, settings, handle));
    }
}

/// The launch scan, if it was for this game under these settings. It is
/// handed out once; every later scan is a fresh one.
fn take_prewarm(game: Game, settings: &Settings) -> Option<CmdResult<ScanOutcome>> {
    let (prewarmed_game, prewarmed_settings, handle) = PREWARM.lock().ok()?.take()?;
    if prewarmed_game != game || prewarmed_settings != *settings {
        return None;
    }
    handle.join().ok()
}

/// Scan the game's mods and return them. Runs off the UI thread.
#[tauri::command(async)]
fn scan_mods(game: String, state: State<'_, AppState>) -> CmdResult<Vec<ModDto>> {
    let game = parse_game(&game)?;
    let settings = settings_snapshot(&state)?;
    let outcome = match take_prewarm(game, &settings) {
        Some(prewarmed) => prewarmed?,
        None => scan_game(game, &settings)?,
    };
    let mut inner = lock(&state)?;
    inner.scans.insert(game, outcome.mods);
    inner.versions.insert(game, outcome.version);
    inner.profile_names.insert(game, outcome.profile_names);
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

/// Pin a mod to one or more categories - the first is the main one and
/// decides the load-order group. An empty list clears the pin.
#[tauri::command]
fn set_category_override(
    game: String,
    id: String,
    tokens: Vec<String>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let game = parse_game(&game)?;
    let mut pins: Vec<String> = Vec::new();
    for t in tokens {
        if !is_official(&t) {
            return Err(format!("unknown category '{t}'"));
        }
        if !pins.contains(&t) {
            pins.push(t);
        }
    }
    let mut inner = lock(&state)?;
    let per_game = inner.overrides().entry(game.short_name().to_string()).or_default();
    if pins.is_empty() {
        per_game.remove(&id);
    } else {
        per_game.insert(id, pins);
    }
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
                result.failed.push(DeleteFailure { id, error: coded("not_scanned", &[]) });
                continue;
            };
            if m.workshop_id().is_some() {
                result.skipped_workshop += 1;
            } else if m.path.parent() != Some(mod_dir.as_path()) {
                // only direct children of the game's own mod folder
                result.failed.push(DeleteFailure { id, error: coded("outside_mod_dir", &[]) });
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
        .ok_or_else(|| coded("not_scanned", &[]))
}

/// Show the mod's file in the system file manager.
#[tauri::command]
fn reveal_mod(game: String, id: String, state: State<'_, AppState>) -> CmdResult<()> {
    let (path, _) = scanned_path(&state, parse_game(&game)?, &id)?;
    if !path.exists() {
        return Err(coded("file_gone", &[]));
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

#[derive(Serialize, Clone)]
struct ExtractProgress {
    done: usize,
    total: usize,
}

#[derive(Serialize)]
struct ExtractOutcome {
    /// The folder the files went to. `None` when nothing could be written
    /// (no file in the archive has a name) and no folder was left behind.
    dest: Option<String>,
    #[serde(flatten)]
    result: ExtractResult,
}

/// Turn a mod's display name into one safe folder name. Display names come
/// from manifests and from Steam, so they are untrusted: path separators,
/// reserved characters and reserved device names must not get through.
fn folder_name_for(wanted: &str, fallback: &str) -> String {
    // Windows drops trailing dots and spaces from names, so they go too
    let edge = |c: char| c == '.' || c.is_whitespace();
    let clean = |raw: &str| -> String {
        let replaced: String = raw
            .chars()
            .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
            .collect();
        let short: String = replaced.trim_matches(edge).chars().take(80).collect();
        short.trim_end_matches(edge).to_string()
    };
    let mut name = clean(wanted);
    if name.is_empty() {
        name = clean(fallback);
    }
    if name.is_empty() {
        name = "extracted".to_string();
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| c.is_ascii_digit()));
    if reserved {
        name.insert(0, '_');
    }
    name
}

/// `<parent>/<name>`, or `<name> (2)`, `(3)`... - the first that does not
/// exist yet or is an empty folder. An extraction never mixes into files
/// that are already there.
fn free_folder(parent: &Path, name: &str) -> CmdResult<PathBuf> {
    for n in 1..1000 {
        let candidate = parent.join(if n == 1 { name.to_string() } else { format!("{name} ({n})") });
        let usable = match fs::read_dir(&candidate) {
            Ok(mut entries) => entries.next().is_none(),
            Err(_) => !candidate.exists(),
        };
        if usable {
            return Ok(candidate);
        }
    }
    Err(coded("no_folder_name", &[]))
}

/// Clears the busy flag however the extraction ends.
struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Unpack a scanned mod's archive into a new folder under `dest_parent`
/// (the folder the user picked). The mod itself is only read.
#[tauri::command(async)]
fn extract_mod(
    game: String,
    id: String,
    dest_parent: String,
    folder_name: String,
    on_progress: Channel<ExtractProgress>,
    state: State<'_, AppState>,
) -> CmdResult<ExtractOutcome> {
    let game = parse_game(&game)?;
    let (path, _) = scanned_path(&state, game, &id)?;
    let parent = Path::new(&dest_parent);
    if !parent.is_absolute() || !parent.is_dir() {
        return Err(coded("bad_dest", &[]));
    }
    if !path.is_file() {
        return Err(coded("is_folder_mod", &[]));
    }
    match scsmm_core::scs::detect_format(&path).map_err(err)? {
        ScsFormat::Zip | ScsFormat::HashfsV1 | ScsFormat::HashfsV2 => {}
        _ => return Err(coded("not_extractable", &[])),
    }

    if state.extract_busy.swap(true, Ordering::SeqCst) {
        return Err(coded("extract_busy", &[]));
    }
    let _busy = BusyGuard(&state.extract_busy);
    state.extract_cancel.store(false, Ordering::SeqCst);

    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dest = free_folder(parent, &folder_name_for(&folder_name, &stem))?;

    // the game's own paths give the files of a locked archive their names
    let mut game_paths = game_paths_for(game, &steam::discover_steam_libraries());
    let options = ExtractOptions {
        game_paths: game_paths.as_mut(),
    };

    // the screen needs a steady trickle, not one message per file
    let mut last_sent = Instant::now() - Duration::from_secs(1);
    let result = extract_scs_with(
        &path,
        &dest,
        options,
        &mut |done, total| {
            if done == total || last_sent.elapsed() >= Duration::from_millis(40) {
                last_sent = Instant::now();
                let _ = on_progress.send(ExtractProgress { done, total });
            }
        },
        &|| state.extract_cancel.load(Ordering::SeqCst),
    )
    .map_err(err)?;

    // nothing written: do not leave an empty folder behind (remove_dir
    // refuses anything that is not empty)
    let kept = result.extracted > 0 || fs::remove_dir(&dest).is_err() && dest.exists();
    let dest = kept.then_some(dest);
    if let Ok(mut slot) = state.last_extract.lock() {
        *slot = dest.clone();
    }
    Ok(ExtractOutcome {
        dest: dest.map(|d| d.display().to_string()),
        result,
    })
}

/// Ask the running extraction to stop after the file it is on.
#[tauri::command]
fn cancel_extract(state: State<'_, AppState>) {
    state.extract_cancel.store(true, Ordering::SeqCst);
}

/// Open the folder of the last extraction in the system file manager.
#[tauri::command]
fn open_extracted(state: State<'_, AppState>) -> CmdResult<()> {
    let dest = state
        .last_extract
        .lock()
        .map_err(err)?
        .clone()
        .filter(|d| d.is_dir())
        .ok_or_else(|| coded("extract_gone", &[]))?;
    #[cfg(windows)]
    std::process::Command::new("explorer.exe").arg(&dest).spawn().map_err(err)?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&dest).spawn().map_err(err)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(&dest).spawn().map_err(err)?;
    Ok(())
}

/// Open the mod's Steam Workshop page in the default browser.
#[tauri::command]
fn open_workshop_page(game: String, id: String, state: State<'_, AppState>) -> CmdResult<()> {
    let (_, workshop_id) = scanned_path(&state, parse_game(&game)?, &id)?;
    // the URL is built here from a numeric id, never taken from the UI
    open_url(&workshop_url(&workshop_id.ok_or_else(|| coded("not_workshop", &[]))?))
}

/// Hand a URL to the default browser. Callers only pass workshop URLs
/// built here from a validated numeric id.
fn open_url(url: &str) -> CmdResult<()> {
    #[cfg(windows)]
    std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map_err(err)?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(url).spawn().map_err(err)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(url).spawn().map_err(err)?;
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

#[derive(Serialize)]
struct SortPlan {
    /// Permutation of the list (display order) that sorts mods into their
    /// groups without changing which mod wins any file.
    order: Vec<usize>,
    /// The list is already in that order.
    sorted: bool,
    /// Mods that stay outside their group in the sorted list because a mod
    /// they share files with has to stay above (or below) them.
    held: Vec<String>,
    /// For each held mod, the mods it shares files with.
    partners: HashMap<String, Vec<String>>,
}

/// Work out the automatic sort for the list (display order, top first).
///
/// Two active mods that carry a file at the same path keep their current
/// order: the upper one wins that file today, and a sort by category alone
/// would silently hand it to the other (a patch declared as "truck" above
/// a fix declared as "other" is the typical case).
#[tauri::command]
fn sort_plan(game: String, items: Vec<ActiveMod>, state: State<'_, AppState>) -> CmdResult<SortPlan> {
    let game = parse_game(&game)?;
    let mut inner = lock(&state)?;
    let tokens: Vec<String> = resolve(&mut inner, game, &items).into_iter().map(|i| i.token).collect();

    let empty = Vec::new();
    let scanned = inner.scans.get(&game).unwrap_or(&empty);
    let matcher = ActiveModMatcher::new(scanned);
    let mods: Vec<Option<&ScannedMod>> = items.iter().map(|a| matcher.lookup(a).map(|i| &scanned[i])).collect();
    let mut keep_above: Vec<(usize, usize)> = Vec::new();
    for upper in 0..mods.len() {
        for lower in upper + 1..mods.len() {
            if let (Some(a), Some(b)) = (mods[upper], mods[lower]) {
                if a.overlaps(b) {
                    keep_above.push((upper, lower));
                }
            }
        }
    }

    let order = load_order::auto_sort_preserving(&tokens, &keep_above);
    let sorted_tokens: Vec<&str> = order.iter().map(|&i| tokens[i].as_str()).collect();
    let held_indices: Vec<usize> = load_order::build_rows(&sorted_tokens)
        .into_iter()
        .filter_map(|row| match row {
            Row::Mod { index, misplaced: true, .. } => Some(order[index]),
            _ => None,
        })
        .collect();
    let partners = held_indices
        .iter()
        .map(|&k| {
            let names = keep_above
                .iter()
                .filter_map(|&(u, l)| if u == k { Some(l) } else if l == k { Some(u) } else { None })
                .map(|i| items[i].name.clone())
                .collect();
            (items[k].name.clone(), names)
        })
        .collect();
    Ok(SortPlan {
        sorted: order.iter().enumerate().all(|(position, &i)| position == i),
        held: held_indices.iter().map(|&k| items[k].name.clone()).collect(),
        partners,
        order,
    })
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

// ------------------------------------------------------------------- //
// commands: sharing
// ------------------------------------------------------------------- //

/// A share file is a few kilobytes; anything far beyond that is not one.
const MAX_SHARE_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Serialize)]
struct SharePreview {
    /// `modshare` for a share file, `profile` for someone's profile.sii.
    kind: &'static str,
    profile_name: String,
    /// Display order: top of the load order first.
    entries: Vec<ShareCheck>,
}

#[derive(Serialize)]
struct OutdatedMap {
    name: String,
    local: String,
    combo: String,
}

#[derive(Serialize)]
struct MapComboPreview {
    /// Maps named in the combo.
    count: usize,
    /// Combo maps that are not in the Maps group of the current list.
    missing: Vec<String>,
    /// Maps installed in an older version than the combo was built with.
    outdated: Vec<OutdatedMap>,
    /// The whole list (display order) with the maps block rearranged.
    order: Vec<String>,
}

/// Exports only ever write `.json` files into an existing folder; the path
/// comes from the save dialog.
fn write_share_file(path: &str, text: &str) -> CmdResult<()> {
    let path = Path::new(path);
    let is_json = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if !is_json || !path.parent().is_some_and(|p| p.is_dir()) {
        return Err(coded("share_json_only", &[]));
    }
    fs::write(path, text).map_err(err)
}

fn read_share_file(path: &Path) -> CmdResult<Vec<u8>> {
    let size = fs::metadata(path).map_err(err)?.len();
    if size > MAX_SHARE_FILE_BYTES {
        return Err(coded("share_too_big", &[]));
    }
    fs::read(path).map_err(err)
}

/// Installed version of an active entry: `None` when not installed, an
/// empty string when installed with an unknown version.
fn local_version(scanned: &[ScannedMod], matcher: &ActiveModMatcher, name: &str, display_name: &str) -> Option<String> {
    let probe = ActiveMod {
        name: name.to_string(),
        display_name: display_name.to_string(),
    };
    let m = &scanned[matcher.lookup(&probe)?];
    Some(m.manifest.as_ref().map(|mf| mf.package_version.clone()).unwrap_or_default())
}

/// Names (in `items`) of the mods that sit in the Maps group.
fn maps_block(inner: &mut Inner, game: Game, items: &[ActiveMod]) -> Vec<String> {
    let maps = load_order::group_index_for_token(scsmm_core::categories::MAP);
    resolve(inner, game, items)
        .into_iter()
        .filter(|i| load_order::group_index_for_token(&i.token) == maps)
        .map(|i| i.name)
        .collect()
}

/// Write the list (display order) as a ModShare file. Returns the number
/// of mods written.
#[tauri::command(async)]
fn export_share(
    game: String,
    profile_name: String,
    items: Vec<ActiveMod>,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<usize> {
    let game = parse_game(&game)?;
    let share = {
        let inner = lock(&state)?;
        let empty = Vec::new();
        let scanned = inner.scans.get(&game).unwrap_or(&empty);
        let matcher = ActiveModMatcher::new(scanned);
        // the file is in profile order: index 0 = bottom of the load order
        let profile = profile::Profile {
            dir_name: String::new(),
            profile_name,
            active_mods: items.into_iter().rev().collect(),
        };
        ShareList::from_profile(&profile, game, |a| {
            local_version(scanned, &matcher, &a.name, &a.display_name).filter(|v| !v.is_empty())
        })
    };
    write_share_file(&path, &share.serialize())?;
    Ok(share.entries.len())
}

/// Read a ModShare file - or someone's profile.sii - and check every mod
/// in it against what is installed here. Nothing is changed.
#[tauri::command(async)]
fn preview_share(game: String, path: String, state: State<'_, AppState>) -> CmdResult<SharePreview> {
    let game = parse_game(&game)?;
    let path = Path::new(&path);
    let is_profile = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sii"));
    let (kind, share) = if is_profile {
        read_share_file(path)?; // size check only
        let foreign = profile::read_profile(path).map_err(err)?;
        ("profile", ShareList::from_profile(&foreign, game, |_| None))
    } else {
        let raw = read_share_file(path)?;
        let share = ShareList::parse(&String::from_utf8_lossy(&raw)).map_err(err)?;
        if share.game != game {
            return Err(coded("share_wrong_game", &[share.game.short_name()]));
        }
        ("modshare", share)
    };

    let inner = lock(&state)?;
    let empty = Vec::new();
    let scanned = inner.scans.get(&game).unwrap_or(&empty);
    let matcher = ActiveModMatcher::new(scanned);
    let mut entries = check_share(&share, |e| local_version(scanned, &matcher, &e.name, &e.display_name));
    entries.reverse(); // display order
    Ok(SharePreview {
        kind,
        profile_name: share.profile_name,
        entries,
    })
}

/// Write the Maps group of the list (display order) as a MapCombo file.
#[tauri::command(async)]
fn export_map_combo(game: String, items: Vec<ActiveMod>, path: String, state: State<'_, AppState>) -> CmdResult<usize> {
    let game = parse_game(&game)?;
    let combo: Vec<MapComboEntry> = {
        let mut inner = lock(&state)?;
        let block = maps_block(&mut inner, game, &items);
        let empty = Vec::new();
        let scanned = inner.scans.get(&game).unwrap_or(&empty);
        let matcher = ActiveModMatcher::new(scanned);
        items
            .iter()
            .filter(|a| block.contains(&a.name))
            .map(|a| MapComboEntry {
                name: a.name.clone(),
                display_name: a.display_name.clone(),
                package_version: local_version(scanned, &matcher, &a.name, &a.display_name).unwrap_or_default(),
            })
            .collect()
    };
    if combo.is_empty() {
        return Err(coded("no_maps", &[]));
    }
    write_share_file(&path, &share::serialize_map_combo(&combo))?;
    Ok(combo.len())
}

/// Read a MapCombo file and work out the reordered list. Nothing is
/// changed; the UI applies `order` only when no map is missing.
#[tauri::command(async)]
fn preview_map_combo(game: String, items: Vec<ActiveMod>, path: String, state: State<'_, AppState>) -> CmdResult<MapComboPreview> {
    let game = parse_game(&game)?;
    let raw = read_share_file(Path::new(&path))?;
    let combo = share::parse_map_combo(&String::from_utf8_lossy(&raw)).map_err(err)?;

    let mut inner = lock(&state)?;
    let block: std::collections::HashSet<String> = maps_block(&mut inner, game, &items).into_iter().collect();
    let empty = Vec::new();
    let scanned = inner.scans.get(&game).unwrap_or(&empty);
    let matcher = ActiveModMatcher::new(scanned);
    let label = |e: &MapComboEntry| if e.display_name.is_empty() { e.name.clone() } else { e.display_name.clone() };

    let missing = combo.iter().filter(|e| !block.contains(&e.name)).map(label).collect();
    let outdated = combo
        .iter()
        .filter_map(|e| {
            let local = local_version(scanned, &matcher, &e.name, &e.display_name).filter(|v| !v.is_empty())?;
            (share::compare_versions(&local, &e.package_version) == Some(std::cmp::Ordering::Less)).then(|| OutdatedMap {
                name: label(e),
                local,
                combo: e.package_version.clone(),
            })
        })
        .collect();
    let order: Vec<String> = items.iter().map(|a| a.name.clone()).collect();
    Ok(MapComboPreview {
        count: combo.len(),
        missing,
        outdated,
        order: share::apply_map_combo(&order, &block, &combo),
    })
}

/// Open a workshop item's Steam page by its numeric id (the subscribe
/// link for a mod that a shared list needs but is not installed).
#[tauri::command]
fn open_workshop_item(workshop_id: String) -> CmdResult<()> {
    if !workshop_api::is_workshop_id(&workshop_id) {
        return Err(coded("bad_workshop_id", &[]));
    }
    open_url(&workshop_url(&workshop_id))
}

pub fn run() {
    enter_portable_mode();
    start_prewarm();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            constants,
            get_installs,
            get_settings,
            save_settings,
            remember_place,
            remember_view,
            scan_mods,
            mods_snapshot,
            fetch_workshop_meta,
            mod_icon,
            set_category_override,
            set_favorite,
            mod_usage,
            delete_mods,
            reveal_mod,
            extract_mod,
            cancel_extract,
            open_extracted,
            open_workshop_page,
            list_profiles,
            read_load_order,
            resolve_items,
            analyze_conflicts,
            layout_rows,
            sort_plan,
            save_load_order,
            list_backups,
            restore_backup,
            delete_backup,
            export_share,
            preview_share,
            export_map_combo,
            preview_map_combo,
            open_workshop_item,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_one_safe_component() {
        assert_eq!(folder_name_for("ProMods Europe v2.80", "x"), "ProMods Europe v2.80");
        assert_eq!(folder_name_for("a/b\\c:d*e?f", "x"), "a_b_c_d_e_f");
        assert_eq!(folder_name_for("..", "my_mod"), "my_mod");
        assert_eq!(folder_name_for("  name. ", "x"), "name");
        assert_eq!(folder_name_for("", ""), "extracted");
        assert_eq!(folder_name_for("nul", "x"), "_nul");
        assert_eq!(folder_name_for("COM1.old", "x"), "_COM1.old");
        assert_eq!(folder_name_for("combo", "x"), "combo");
        assert_eq!(folder_name_for(&"가".repeat(200), "x").chars().count(), 80);
        for name in ["../../evil", "C:\\Windows", "tab\there"] {
            let safe = folder_name_for(name, "x");
            assert_eq!(Path::new(&safe).components().count(), 1, "{safe:?}");
            assert!(!safe.contains(['/', '\\', ':', '\t']), "{safe:?}");
        }
    }

    #[test]
    fn coded_errors_carry_their_values() {
        assert_eq!(coded("unknown_profile", &[]), "@unknown_profile");
        assert_eq!(coded("docs_missing", &["ETS2", "X:\\gone"]), "@docs_missing\tETS2\tX:\\gone");
    }

    #[test]
    fn pins_from_older_versions_are_still_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("category_overrides.json");
        fs::write(
            &path,
            br#"{"ETS2": {"old_style": "sound", "new_style": ["truck", "sound"], "empty": []}}"#,
        )
        .unwrap();
        let pins = load_overrides(&path);
        let ets2 = &pins["ETS2"];
        assert_eq!(ets2["old_style"], ["sound"]);
        assert_eq!(ets2["new_style"], ["truck", "sound"]);
        assert!(!ets2.contains_key("empty"));
        // and what is written now reads back the same
        save_json(&path, &pins).unwrap();
        assert_eq!(load_overrides(&path), pins);
        assert!(load_overrides(&tmp.path().join("missing.json")).is_empty());
    }

    #[test]
    fn extraction_never_reuses_a_folder_with_files() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        assert_eq!(free_folder(parent, "mod").unwrap(), parent.join("mod"));
        // an empty folder of that name is fine to use
        fs::create_dir(parent.join("mod")).unwrap();
        assert_eq!(free_folder(parent, "mod").unwrap(), parent.join("mod"));
        // one with something in it is not, nor is a file of that name
        fs::write(parent.join("mod").join("a.txt"), b"x").unwrap();
        assert_eq!(free_folder(parent, "mod").unwrap(), parent.join("mod (2)"));
        fs::write(parent.join("mod (2)"), b"a file").unwrap();
        assert_eq!(free_folder(parent, "mod").unwrap(), parent.join("mod (3)"));
    }
}
