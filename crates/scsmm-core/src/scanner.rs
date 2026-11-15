//! Walks the filesystem to enumerate installed mods.
//!
//! A mod payload can land on disk as:
//!
//! * `mod/<name>.scs`                         - ZIP, HashFS or AEM archive
//! * `mod/<name>.zip`                         - ZIP with a .zip suffix
//! * `mod/<name>/manifest.sii`                - unpacked directory
//! * `workshop/content/<appid>/<id>/<slot>`   - workshop archive or dir,
//!   with `versions.sii` selecting the active slot

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::Serialize;

use crate::categories::{contains_map, content_category, effective_categories};
use crate::classify::{self, Evidence, EvidenceBuilder, Label, Verdict};
use crate::game::GameInstall;
use crate::gamepaths::GamePaths;
use crate::identity::{mod_name_for_path, workshop_id_for_path};
use crate::manifest::{read_bundle, ManifestBundle, ModManifest, MANIFEST_ENTRY};
use crate::scs::aem::AemSource;
use crate::scs::cityhash::city_hash64;
use crate::scs::dir::DirSource;
use crate::scs::hashfs::HashFsReader;
use crate::scs::zip::{is_zipfile, RawZipSource, ZipSource};
use crate::sii::crypto::sii_bytes_to_text;
use crate::scs::{detect_format, ModSource, ScsFormat};
use crate::workshop::{resolve_workshop_payload, ARCHIVE_SUFFIXES};
use crate::worldmap::{self, MapFacts};
use crate::Error;

#[derive(Debug, Clone, Serialize)]
pub struct ScannedMod {
    pub path: PathBuf,
    pub format: ScsFormat,
    pub manifest: Option<ModManifest>,
    pub error: Option<String>,
    pub description: Option<String>,
    pub is_map: bool,
    /// The archive's `def/` file paths, captured once at scan time. Powers
    /// the physics content signal (and later, conflict detection).
    #[serde(skip)]
    pub def_files: Vec<String>,
    /// Fingerprint of every file the mod mounts: sorted, de-duplicated
    /// hashes of the normalised paths, without the root-level files
    /// (manifest, icon, readme) every mod has. Two mods that share a
    /// fingerprint entry overwrite each other's file, so their relative
    /// load order matters. Shared, because scans are cloned around.
    #[serde(skip)]
    pub file_hashes: Arc<[u64]>,
    /// Fingerprint of the files this mod brings on its own: the entries of
    /// `file_hashes` whose path the game does not have, without the
    /// `automat/` materials (exporters name those by content, so unrelated
    /// mods carry identical ones). Two mods sharing an entry here do more
    /// than replace the same file of the game: one overwrites a file only
    /// the other introduces - a patch or add-on and the mod it is made
    /// for. Without the game's path list every file counts.
    #[serde(skip)]
    pub own_hashes: Arc<[u64]>,
    /// Hashes of the files the archive holds without listing them - a
    /// "locked" archive. Empty for everything that lists its files.
    #[serde(skip)]
    pub hidden: Arc<[u64]>,
    /// How many of the hidden files could be named after all (they share
    /// a path with a file of the game or of another mod).
    pub hidden_named: usize,
    /// Fingerprint of every file the mod can hand to another one: like
    /// `file_hashes`, with the paths in lower case, the way definitions
    /// spell them when they point at a file.
    #[serde(skip)]
    pub provides: Arc<[u64]>,
    /// Top-level folders of the listed files, in lower case.
    #[serde(skip)]
    pub top_dirs: Vec<String>,
    /// The files this mod needs someone else to bring: every path its
    /// definitions and materials name that is neither one of its own files
    /// nor one of the game's. Lower case, sorted. Empty without the game's
    /// path list - then nothing tells a missing file from a game file.
    #[serde(skip)]
    pub wants: Arc<[String]>,
    /// How many different paths the definitions and materials name in all.
    pub named_paths: usize,
    /// What the mod has to do with the world map: the placement of the
    /// map picture, the picture itself, the sectors of its map.
    #[serde(skip)]
    pub world_map: MapFacts,
    /// When the payload landed on this filesystem (seconds since the Unix
    /// epoch; creation time where available). 0 when unreadable.
    pub installed_at: f64,
    #[serde(skip)]
    pub icon: Option<Vec<u8>>,
    /// What the files say the mod is: every named file labelled by its
    /// path, corrected by a sample of what the files contain.
    #[serde(skip)]
    pub evidence: Evidence,
}

impl ScannedMod {
    /// Stable identity - the `active_mods[]` token, derived from the path.
    pub fn mod_name(&self) -> String {
        mod_name_for_path(&self.path)
    }

    pub fn workshop_id(&self) -> Option<String> {
        workshop_id_for_path(&self.path)
    }

    /// Best human name: manifest name, else the file stem.
    pub fn display_name(&self) -> String {
        match &self.manifest {
            Some(m) if !m.display_name.is_empty() => m.display_name.clone(),
            _ => self
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    fn manifest_categories(&self) -> &[String] {
        self.manifest
            .as_ref()
            .map(|m| m.categories.as_slice())
            .unwrap_or_default()
    }

    /// What the content says the mod is, where that differs from the
    /// manifest. Applied or only a suggestion - see [`Verdict::applied`].
    pub fn verdict(&self) -> Option<Verdict> {
        classify::judge(&self.evidence, self.manifest_categories())
    }

    /// Categories: the user's pins (main one first), else what the
    /// content proves, else the manifest.
    pub fn effective_categories(&self, pins: &[String]) -> Vec<String> {
        let judged = self
            .verdict()
            .filter(|v| v.applied)
            .map(|v| v.categories)
            .unwrap_or_default();
        effective_categories(
            self.manifest_categories(),
            self.is_map,
            pins,
            content_category(&self.def_files),
            &judged,
        )
    }

    /// A category the content points to without being sure enough to
    /// overrule the manifest - for the user to accept or not. `None` once
    /// the user pinned the mod, and where a map descriptor or the physics
    /// file settles the category anyway.
    pub fn suggested_category(&self, pins: &[String]) -> Option<&'static str> {
        if !pins.is_empty() || self.is_map || content_category(&self.def_files).is_some() {
            return None;
        }
        self.verdict().filter(|v| !v.applied).map(|v| v.category)
    }

    fn failed(path: &Path, format: ScsFormat, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            format,
            manifest: None,
            error: Some(message.into()),
            description: None,
            is_map: false,
            def_files: Vec::new(),
            file_hashes: Arc::from([]),
            own_hashes: Arc::from([]),
            hidden: Arc::from([]),
            hidden_named: 0,
            provides: Arc::from([]),
            top_dirs: Vec::new(),
            wants: Arc::from([]),
            named_paths: 0,
            world_map: MapFacts::default(),
            installed_at: 0.0,
            icon: None,
            evidence: Evidence::default(),
        }
    }

    /// True when both mods carry a file at the same path - whichever sits
    /// higher in the load order wins it.
    pub fn overlaps(&self, other: &ScannedMod) -> bool {
        intersects(&self.file_hashes, &other.file_hashes)
    }

    /// True when one of the two overwrites a file that only the other
    /// brings (see `own_hashes`): the one higher in the load order is a
    /// patch or add-on for the one below it.
    pub fn shares_own_files(&self, other: &ScannedMod) -> bool {
        intersects(&self.own_hashes, &other.own_hashes)
    }
}

/// Whether two sorted lists have an element in common.
fn intersects(a: &[u64], b: &[u64]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

/// Scan the install's `mod/` directory and workshop tree.
///
/// `owned_dlc` gates the 1.48 `dlc_<name>/` normalisation; `None` means the
/// game directory was not found, so those folders are ungated.
/// `game_paths` (the paths of the game's own files) lets locked archives
/// be seen for what they replace; without it they are only compared with
/// the other mods.
pub fn scan_game_install(
    install: &GameInstall,
    game_version: Option<&str>,
    owned_dlc: Option<&BTreeSet<String>>,
    game_paths: Option<&mut GamePaths>,
) -> Vec<ScannedMod> {
    let mut payloads = mod_candidates(&install.mod_dir());
    if let Some(ws) = &install.workshop_dir {
        payloads.extend(workshop_payloads(ws, game_version));
    }
    scan_paths(&payloads, owned_dlc, game_paths)
}

pub fn scan_mod_directory(directory: &Path, owned_dlc: Option<&BTreeSet<String>>) -> Vec<ScannedMod> {
    scan_paths(&mod_candidates(directory), owned_dlc, None)
}

fn scan_paths(
    paths: &[PathBuf],
    owned_dlc: Option<&BTreeSet<String>>,
    game_paths: Option<&mut GamePaths>,
) -> Vec<ScannedMod> {
    // read-only while the archives are scanned in parallel
    let ctx = ScanContext { owned_dlc, game: game_paths.as_deref() };
    let mut mods: Vec<ScannedMod> = paths.par_iter().map(|p| scan_with(p, ctx)).collect();
    name_hidden_files(&mut mods, game_paths);
    worldmap::find_hidden_sectors(&mut mods);
    mods
}

/// Paths probed in every archive regardless of what other mods list.
const PROBED_DEFS: &[&str] = &["def/vehicle/physics.sii"];
const PROBED_MAPS: &[&str] = &["map/europe.mbd", "map/usa.mbd"];

/// Put names to files that archives carry without listing them.
///
/// A "locked" HashFS archive has its directory listings stripped: its
/// files are there (and in the fingerprint, by hash) but nameless. A path
/// that the game itself has (`game_paths`), that any other mod lists, or
/// one of the few standard ones can still be tested against those hashes -
/// so the def files such an archive replaces become visible to conflict
/// detection, and a map or physics mod is recognised by content again.
fn name_hidden_files(mods: &mut [ScannedMod], mut game_paths: Option<&mut GamePaths>) {
    let mut known: Vec<(u64, String)> = mods
        .iter()
        .flat_map(|m| m.def_files.iter())
        .map(String::as_str)
        .chain(PROBED_DEFS.iter().copied())
        .chain(PROBED_MAPS.iter().copied())
        .map(|p| (city_hash64(p.as_bytes()), p.to_string()))
        .collect();
    known.sort_unstable();
    known.dedup();

    for m in mods.iter_mut().filter(|m| !m.hidden.is_empty()) {
        let names: Vec<String> = m
            .hidden
            .iter()
            .filter_map(|hash| {
                let among_mods = known.binary_search_by_key(hash, |(h, _)| *h).ok().map(|i| known[i].1.clone());
                among_mods.or_else(|| game_paths.as_deref_mut()?.name_of(*hash))
            })
            .collect();
        m.hidden_named = names.len();
        if !m.is_map && contains_map(&names) {
            m.is_map = true;
        }
        let listed: std::collections::HashSet<String> = m.def_files.iter().cloned().collect();
        m.def_files.extend(
            names
                .into_iter()
                .filter(|name| name.starts_with("def/") && !listed.contains(name)),
        );
    }
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let mut children: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    children.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    children
}

fn has_archive_suffix(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ARCHIVE_SUFFIXES.iter().any(|s| e.eq_ignore_ascii_case(s)))
}

/// Archive files and unpacked mod directories worth scanning.
fn mod_candidates(directory: &Path) -> Vec<PathBuf> {
    sorted_children(directory)
        .into_iter()
        .filter(|child| {
            (child.is_file() && has_archive_suffix(child))
                || (child.is_dir() && child.join(MANIFEST_ENTRY).is_file())
        })
        .collect()
}

/// One payload per `<workshop-id>/` directory - the version the game loads.
fn workshop_payloads(directory: &Path, game_version: Option<&str>) -> Vec<PathBuf> {
    sorted_children(directory)
        .into_iter()
        .filter(|p| p.is_dir())
        .filter_map(|id_dir| resolve_workshop_payload(&id_dir, game_version))
        .collect()
}

/// What a scan knows besides the archive itself.
#[derive(Clone, Copy)]
struct ScanContext<'a> {
    owned_dlc: Option<&'a BTreeSet<String>>,
    /// The game's own file paths: tells a replaced file from a new one.
    game: Option<&'a GamePaths>,
}

pub fn scan_one(path: &Path, owned_dlc: Option<&BTreeSet<String>>) -> ScannedMod {
    scan_with(path, ScanContext { owned_dlc, game: None })
}

fn scan_with(path: &Path, ctx: ScanContext) -> ScannedMod {
    let mut result = if path.is_dir() {
        scan_directory(path, ctx)
    } else if path.is_file() {
        scan_archive(path, ctx)
    } else {
        ScannedMod::failed(path, ScsFormat::Unknown, "path does not exist")
    };
    result.installed_at = installed_at_for(path);
    result
}

/// Install timestamp: creation time (falls back to mtime where the
/// filesystem has no birth time), or 0 if unreadable.
fn installed_at_for(path: &Path) -> f64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0.0;
    };
    meta.created()
        .or_else(|_| meta.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Drop a leading `base/` package segment (SCS 1.48 layout). Segment
/// match, never substring: `base_vehicle/...` is left untouched.
fn strip_base_prefix(path: &str) -> &str {
    let p = path.trim_start_matches('/');
    if p == "base" {
        return "";
    }
    p.strip_prefix("base/").unwrap_or(p)
}

/// Drop a leading `dlc_<name>/` segment when that DLC is owned (or when
/// ownership is unknown).
fn strip_dlc_prefix<'a>(path: &'a str, owned_dlc: Option<&BTreeSet<String>>) -> &'a str {
    match path.split_once('/') {
        Some((first, rest))
            if first.starts_with("dlc_") && owned_dlc.is_none_or(|o| o.contains(first)) =>
        {
            rest
        }
        _ => path,
    }
}

/// What one listing tells about a mod: the `is_map` flag, the `def/` file
/// paths and the fingerprint of all its files.
struct ListingFacts {
    is_map: bool,
    def_files: Vec<String>,
    file_hashes: Arc<[u64]>,
    own_hashes: Arc<[u64]>,
    hidden: Arc<[u64]>,
    provides: Arc<[u64]>,
    top_dirs: Vec<String>,
}

/// Materials the exporter generates and names by their content: the same
/// file turns up in mods that have nothing to do with each other.
const GENERATED_PREFIX: &str = "automat/";

/// Root-level files a mod may carry besides the ones its manifest names.
/// They exist in every mod and say nothing about load order.
const ROOT_FILE_NAMES: &[&str] = &[
    "manifest.sii",
    "version.sii",
    "version.txt",
    "description.txt",
    "mod_description.txt",
    "readme.txt",
    "icon.jpg",
    "icon.png",
    "mod_icon.jpg",
    "mod_icon.png",
    "mod_workshop_icon.jpg",
    "preview.jpg",
    "preview.png",
    "thumbnail.jpg",
];

/// `files` is the listing by name; `table_hashes` the container's own
/// path hashes when it has them (complete even where the listing is not);
/// `root_names` the root-level files the manifest points at; `game` the
/// game's own paths, to tell a file the mod brings from one it replaces.
fn listing_facts(
    files: &[String],
    table_hashes: Option<Vec<u64>>,
    root_names: &[&str],
    owned_dlc: Option<&BTreeSet<String>>,
    game: Option<&GamePaths>,
) -> ListingFacts {
    // normalise the 1.48 package layers once, before both signals derive
    // from them; dedupe keeping first-seen order
    let mut seen = std::collections::HashSet::new();
    let normalized: Vec<&str> = files
        .iter()
        .map(|f| strip_dlc_prefix(strip_base_prefix(f), owned_dlc))
        .filter(|s| !s.is_empty() && seen.insert(*s))
        .collect();
    let is_map = contains_map(&normalized);
    // drop directory entries ("def/", "def/vehicle/")
    let def_files = normalized
        .iter()
        .filter(|p| p.starts_with("def/") && !p.ends_with('/'))
        .map(|p| p.to_string())
        .collect();
    // every mounted file below the root; root-level files (manifest.sii,
    // icon, description) exist in every mod and never conflict meaningfully
    let mut hashes: Vec<u64> = Vec::new();
    // the ones among them that the game does not have (it spells its
    // paths in lower case)
    let mut own: Vec<u64> = Vec::new();
    // the same files as another mod's definitions would name them
    let mut provides: Vec<u64> = Vec::new();
    let mut top_dirs: BTreeSet<String> = BTreeSet::new();
    for path in normalized.iter().filter(|p| p.contains('/') && !p.ends_with('/')) {
        let hash = city_hash64(path.as_bytes());
        hashes.push(hash);
        let lower = path.to_ascii_lowercase();
        let lower_hash = city_hash64(lower.as_bytes());
        provides.push(lower_hash);
        if let Some((top, _)) = lower.split_once('/') {
            if !top_dirs.contains(top) {
                top_dirs.insert(top.to_string());
            }
        }
        if !lower.starts_with(GENERATED_PREFIX) && game.is_none_or(|g| !g.contains(lower_hash)) {
            own.push(hash);
        }
    }
    let mut hidden: Vec<u64> = Vec::new();
    if let Some(table) = table_hashes {
        // the table also holds the root-level files; drop the ones we can
        // name (standard names, the manifest's, and whatever is listed)
        let root: std::collections::HashSet<u64> = ROOT_FILE_NAMES
            .iter()
            .chain(root_names)
            .copied()
            .chain(files.iter().map(|f| f.trim_start_matches('/')).filter(|f| !f.contains('/')))
            .map(|name| city_hash64(name.as_bytes()))
            .collect();
        // the table stores the paths as the archive spells them, before
        // the package-layer normalisation above
        let listed: std::collections::HashSet<u64> = files
            .iter()
            .map(|f| city_hash64(f.trim_start_matches('/').as_bytes()))
            .collect();
        for hash in table.into_iter().filter(|h| !root.contains(h)) {
            if !listed.contains(&hash) {
                hidden.push(hash);
                // nameless, so only the game can tell whose file it is
                if game.is_none_or(|g| !g.contains(hash)) {
                    own.push(hash);
                }
            }
            hashes.push(hash);
            provides.push(hash);
        }
    }
    for list in [&mut hashes, &mut own, &mut hidden, &mut provides] {
        list.sort_unstable();
        list.dedup();
    }
    let file_hashes: Arc<[u64]> = Arc::from(hashes);
    // nearly every mod spells its paths in lower case: one list for both
    let provides = if provides[..] == file_hashes[..] { Arc::clone(&file_hashes) } else { Arc::from(provides) };
    ListingFacts {
        is_map,
        def_files,
        file_hashes,
        own_hashes: Arc::from(own),
        hidden: Arc::from(hidden),
        provides,
        top_dirs: top_dirs.into_iter().collect(),
    }
}

/// Nothing a definition or material file should be is larger.
const MAX_REFERENCE_BYTES: usize = 8 * 1024 * 1024;

/// The quoted strings of a definition or material file that name a file.
///
/// A path with a leading slash counts from the root. One without is taken
/// to sit next to the file that names it where that is how the format
/// works: an `@include` in a definition, a texture in a material.
fn named_paths(text: &str, dir: &str, is_material: bool, out: &mut HashSet<String>) {
    for line in text.lines() {
        if !line.contains('"') {
            continue;
        }
        let is_include = line.trim_start().starts_with("@include");
        for part in line.split('"').skip(1).step_by(2) {
            if part.len() < 5 || part.len() > 260 {
                continue;
            }
            // templates and the like: "<base_name>.<id>.sii", "a|b"
            if part.bytes().any(|b| matches!(b, b'<' | b'>' | b'|' | b':' | b'*' | b'?') || b < 0x20) {
                continue;
            }
            let Some((_, ext)) = part.rsplit_once('.') else { continue };
            if ext.is_empty() || ext.len() > 5 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            let part = part.replace('\\', "/").to_ascii_lowercase();
            let resolved = if part.starts_with('/') {
                resolve_path("", &part).filter(|path| path.contains('/'))
            } else if (is_include || (is_material && ext.eq_ignore_ascii_case("tobj"))) && !part.contains(' ') {
                // a shader name ("eut2.dif.spec") has a dot too: only what
                // the format looks up beside the file counts
                resolve_path(dir, &part)
            } else {
                None
            };
            out.extend(resolved);
        }
    }
}

/// `path` as seen from the folder `dir`, without empty, `.` and `..`
/// segments. `None` when it climbs out of the root.
fn resolve_path(dir: &str, path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in dir.split('/').chain(path.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

/// The files a mod points at without having them (see `ScannedMod::wants`)
/// and how many paths it names in all. `files` is the listing as the
/// source spells it. Needs the game's paths: without them every reference
/// to a file of the game would look like one to a missing file.
fn gather_wants(
    source: &mut dyn ModSource,
    files: &[String],
    provides: &[u64],
    ctx: ScanContext,
) -> (Arc<[String]>, usize) {
    let Some(game) = ctx.game else {
        return (Arc::from([]), 0);
    };
    let mut named: HashSet<String> = HashSet::new();
    for file in files {
        let path = strip_dlc_prefix(strip_base_prefix(file), ctx.owned_dlc).to_ascii_lowercase();
        let is_material = path.ends_with(".mat");
        let is_definition = path.starts_with("def/") && (path.ends_with(".sii") || path.ends_with(".sui"));
        if !is_material && !is_definition {
            continue;
        }
        let Ok(data) = source.read_bytes(file) else {
            continue;
        };
        if data.len() > MAX_REFERENCE_BYTES {
            continue;
        }
        let text = if is_material {
            String::from_utf8_lossy(&data).into_owned()
        } else {
            match sii_bytes_to_text(&data, false) {
                Ok(text) => text,
                Err(_) => continue,
            }
        };
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        named_paths(&text, dir, is_material, &mut named);
    }
    let total = named.len();
    let mut wants: Vec<String> = named
        .into_iter()
        .filter(|path| {
            let hash = city_hash64(path.as_bytes());
            provides.binary_search(&hash).is_err() && !game.contains(hash)
        })
        .collect();
    wants.sort_unstable();
    (Arc::from(wants), total)
}

/// Files of one path label whose content is looked into.
const SAMPLES_PER_LABEL: usize = 16;
/// Unnamed files of a locked archive that are looked into.
const HIDDEN_SAMPLES: usize = 160;
/// Definitions are small; nothing larger is read for its content.
const MAX_SAMPLE_BYTES: usize = 256 * 1024;

/// About `count` of `items`, spread evenly over all of them.
fn evenly<T>(items: &[T], count: usize) -> impl Iterator<Item = &T> {
    let step = items.len().div_ceil(count.max(1)).max(1);
    items.iter().step_by(step)
}

/// Label every file of a mod and look into a sample of its definitions.
/// `files` is the listing as the source spells it, `hidden` the hashes of
/// the files it does not name.
fn gather_evidence(source: &mut dyn ModSource, files: &[String], hidden: &[u64], ctx: ScanContext) -> Evidence {
    let mut builder = EvidenceBuilder::default();
    // per label: the definition files a sample is drawn from
    let mut candidates: HashMap<Label, Vec<&String>> = HashMap::new();
    let mut seen = HashSet::new();
    for file in files {
        let path = strip_dlc_prefix(strip_base_prefix(file), ctx.owned_dlc);
        // directory entries and the root-level files say nothing
        if path.ends_with('/') || !path.contains('/') {
            continue;
        }
        let path = path.to_ascii_lowercase();
        if !seen.insert(path.clone()) {
            continue;
        }
        let replaces = ctx.game.map(|game| game.contains(city_hash64(path.as_bytes())));
        let label = builder.add_path(&path, replaces);
        if path.starts_with("def/") && (path.ends_with(".sii") || path.ends_with(".sui")) {
            candidates.entry(label).or_default().push(file);
        }
    }
    for (label, files) in candidates {
        for file in evenly(&files, SAMPLES_PER_LABEL) {
            match source.read_bytes(file) {
                Ok(data) if data.len() <= MAX_SAMPLE_BYTES => builder.add_sample(label, &data),
                _ => {}
            }
        }
    }
    builder.set_hidden(hidden.len());
    for hash in evenly(hidden, HIDDEN_SAMPLES) {
        builder.add_hidden_sample(source.read_hashed(*hash, MAX_SAMPLE_BYTES).as_deref());
    }
    builder.finish()
}

/// What the mod brings for the world map (see [`MapFacts`]). `files` is
/// the listing as the source spells it, `provides` the fingerprint of all
/// its files, the unlisted ones of a locked archive among them.
fn gather_world_map(source: &mut dyn ModSource, files: &[String], provides: &[u64], ctx: ScanContext) -> MapFacts {
    let mut facts = MapFacts::default();
    let mut data_file: Option<&str> = None;
    let mut sectors: BTreeSet<(i16, i16)> = BTreeSet::new();
    let mut maps: BTreeSet<String> = BTreeSet::new();
    for file in files {
        let path = strip_dlc_prefix(strip_base_prefix(file), ctx.owned_dlc).to_ascii_lowercase();
        if path == worldmap::DATA_FILE {
            data_file = Some(file);
        } else if let Some((map, x, z)) = worldmap::sector_of(&path) {
            sectors.insert((x, z));
            if !maps.contains(map) {
                maps.insert(map.to_string());
            }
        }
    }
    // a locked archive holds the file without listing it
    let data_file = data_file.or_else(|| source.has(worldmap::DATA_FILE).then_some(worldmap::DATA_FILE));
    if let Some(path) = data_file {
        facts.has_data = true;
        facts.data = worldmap::read_map_data(source, path);
    }
    facts.background = worldmap::BACKGROUND_FILES
        .iter()
        .any(|file| provides.binary_search(&city_hash64(file.as_bytes())).is_ok());
    facts.sectors = sectors.into_iter().collect();
    facts.maps = maps.into_iter().collect();
    facts
}

fn from_bundle(
    path: &Path,
    format: ScsFormat,
    bundle: ManifestBundle,
    source: &mut dyn ModSource,
    ctx: ScanContext,
) -> ScannedMod {
    let root_names = [bundle.manifest.icon.as_str(), bundle.manifest.description_file.as_str()];
    let files = source.list_files();
    let facts = listing_facts(&files, source.path_hashes(), &root_names, ctx.owned_dlc, ctx.game);
    let evidence = gather_evidence(source, &files, &facts.hidden, ctx);
    let (wants, named_paths) = gather_wants(source, &files, &facts.provides, ctx);
    let world_map = gather_world_map(source, &files, &facts.provides, ctx);
    ScannedMod {
        path: path.to_path_buf(),
        format,
        manifest: Some(bundle.manifest),
        error: None,
        description: bundle.description,
        is_map: facts.is_map,
        def_files: facts.def_files,
        file_hashes: facts.file_hashes,
        own_hashes: facts.own_hashes,
        hidden: facts.hidden,
        hidden_named: 0,
        provides: facts.provides,
        top_dirs: facts.top_dirs,
        wants,
        named_paths,
        world_map,
        installed_at: 0.0,
        icon: bundle.icon,
        evidence,
    }
}

fn scan_source(
    path: &Path,
    format: ScsFormat,
    source: &mut dyn ModSource,
    ctx: ScanContext,
) -> std::result::Result<ScannedMod, Error> {
    let bundle = read_bundle(source)?;
    Ok(from_bundle(path, format, bundle, source, ctx))
}

fn scan_directory(directory: &Path, ctx: ScanContext) -> ScannedMod {
    DirSource::open(directory)
        .and_then(|mut s| scan_source(directory, ScsFormat::Directory, &mut s, ctx))
        .unwrap_or_else(|e| ScannedMod::failed(directory, ScsFormat::Directory, e.to_string()))
}

fn scan_archive(path: &Path, ctx: ScanContext) -> ScannedMod {
    let mut format = match detect_format(path) {
        Ok(f) => f,
        Err(e) => return ScannedMod::failed(path, ScsFormat::Unknown, e.to_string()),
    };
    // some workshop mods ship a .zip with leading junk before the first
    // local header; a valid central directory still makes it a ZIP
    let is_zip_ext = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if format == ScsFormat::Unknown && is_zip_ext && is_zipfile(path) {
        format = ScsFormat::Zip;
    }

    match format {
        ScsFormat::Zip => scan_zip(path, ctx),
        ScsFormat::HashfsV1 | ScsFormat::HashfsV2 => HashFsReader::open(path)
            .and_then(|mut r| scan_source(path, format, &mut r, ctx))
            .unwrap_or_else(|e| ScannedMod::failed(path, format, e.to_string())),
        ScsFormat::Aem => AemSource::open(path)
            .and_then(|mut r| scan_source(path, format, &mut r, ctx))
            .unwrap_or_else(|e| ScannedMod::failed(path, format, e.to_string())),
        _ => ScannedMod::failed(path, format, "Unknown SCS container format"),
    }
}

fn scan_zip(path: &Path, ctx: ScanContext) -> ScannedMod {
    let fmt = ScsFormat::Zip;
    let first = ZipSource::open(path).and_then(|mut z| scan_source(path, fmt, &mut z, ctx));
    let prior_error = match first {
        Ok(found) => return found,
        Err(e) => e.to_string(),
    };

    // second pass for zips the normal reader refused: a set encryption flag
    // and an odd method label over an ordinary raw-deflate payload
    let second = RawZipSource::open(path).and_then(|mut r| scan_source(path, fmt, &mut r, ctx));
    match second {
        Ok(found) => found,
        Err(e) => {
            let message = match e {
                Error::NotFound(_) => format!("missing {MANIFEST_ENTRY}"),
                _ => prior_error,
            };
            let mut failed = ScannedMod::failed(path, fmt, message);
            failed.icon = scavenge_zip_icon(path);
            failed
        }
    }
}

const ZIP_ICON_CANDIDATES: &[&str] = &[
    "icon.jpg",
    "icon.png",
    "mod_icon.jpg",
    "mod_icon.png",
    "preview.jpg",
    "preview.png",
    "thumbnail.jpg",
];

/// Pull an icon out of a ZIP even when the manifest is unreadable. Map
/// mods that encrypt manifest.sii usually leave the icon in the clear.
fn scavenge_zip_icon(path: &Path) -> Option<Vec<u8>> {
    let mut z = ZipSource::open(path).ok()?;
    for name in ZIP_ICON_CANDIDATES {
        if z.has(name) {
            if let Ok(bytes) = z.read_bytes(name) {
                return Some(bytes);
            }
        }
    }
    // last resort: any image at the archive root
    z.list_files()
        .into_iter()
        .filter(|entry| !entry.contains('/'))
        .filter(|entry| {
            let lower = entry.to_lowercase();
            [".jpg", ".jpeg", ".png"].iter().any(|ext| lower.ends_with(ext))
        })
        .find_map(|entry| z.read_bytes(&entry).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scs::hashfs::testutil::build_v1;
    use std::io::Write;

    const MANIFEST: &[u8] = br#"SiiNunit {
        mod_package: .package {
            package_version: "2.0"
            display_name: "Test Truck"
            author: "tester"
            category[]: "truck"
            icon: "pic.jpg"
            description_file: "desc.txt"
        }
    }"#;
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 1, 2];

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn prefix_normalisation() {
        assert_eq!(strip_base_prefix("/base/def/a.sii"), "def/a.sii");
        assert_eq!(strip_base_prefix("base_vehicle/x"), "base_vehicle/x");
        assert_eq!(strip_base_prefix("base"), "");
        let owned: BTreeSet<String> = ["dlc_east".to_string()].into();
        assert_eq!(strip_dlc_prefix("dlc_east/def/a", Some(&owned)), "def/a");
        assert_eq!(strip_dlc_prefix("dlc_west/def/a", Some(&owned)), "dlc_west/def/a");
        assert_eq!(strip_dlc_prefix("dlc_west/def/a", None), "def/a");
        assert_eq!(strip_dlc_prefix("dlcfoo/def/a", None), "dlcfoo/def/a");
    }

    #[test]
    fn map_and_defs_dedupe_and_skip_directories() {
        let files: Vec<String> = ["def/", "def/a.sii", "base/def/a.sii", "/map/x.mbd", "ui/b.mat"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let facts = listing_facts(&files, None, &[], None, None);
        assert!(facts.is_map);
        assert_eq!(facts.def_files, vec!["def/a.sii"]);
        // def/a.sii (once, though listed twice), map/x.mbd and ui/b.mat
        assert_eq!(facts.file_hashes.len(), 3);
    }

    /// A HashFS v1 archive without any directory listing - the way
    /// "locked" mods ship: every file is addressable by hash only.
    fn build_locked_v1(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"SCS#");
        out.extend_from_slice(&1u16.to_le_bytes()); // version
        out.extend_from_slice(&0u16.to_le_bytes()); // salt
        out.extend_from_slice(b"CITY");
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        let table_offset_pos = out.len();
        out.extend_from_slice(&0u64.to_le_bytes());
        let mut rows = Vec::new();
        for (path, content) in files {
            rows.push((city_hash64(path.as_bytes()), out.len() as u64, content.len() as u32));
            out.extend_from_slice(content);
        }
        let table_offset = out.len() as u64;
        out[table_offset_pos..table_offset_pos + 8].copy_from_slice(&table_offset.to_le_bytes());
        for (hash, offset, size) in rows {
            out.extend_from_slice(&hash.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // flags: plain file
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
        }
        out
    }

    #[test]
    fn locked_archives_are_seen_through_their_hash_table() {
        let tmp = tempfile::tempdir().unwrap();
        let mod_dir = tmp.path().join("mod");
        fs::create_dir_all(&mod_dir).unwrap();

        // a locked physics pack: nothing in it can be listed by name
        fs::write(
            mod_dir.join("a_locked.scs"),
            build_locked_v1(&[
                ("manifest.sii", MANIFEST),
                ("pic.jpg", JPEG),
                ("def/vehicle/physics.sii", b"."),
                ("def/vehicle/truck/x/chassis.sii", b"."),
                ("sound/engine.bank", b"."),
            ]),
        )
        .unwrap();
        // an ordinary patch that overrides one of its def files
        make_zip(
            &mod_dir.join("b_patch.scs"),
            &[("manifest.sii", MANIFEST), ("pic.jpg", JPEG), ("def/vehicle/truck/x/chassis.sii", b"patched")],
        );
        // an unrelated mod that only shares the root-level files
        make_zip(&mod_dir.join("c_other.scs"), &[("manifest.sii", MANIFEST), ("pic.jpg", JPEG), ("ui/a.mat", b".")]);
        // another locked archive with a sound file the first one also has
        fs::write(
            mod_dir.join("d_locked_sound.scs"),
            build_locked_v1(&[("manifest.sii", MANIFEST), ("sound/engine.bank", b"."), ("map/europe.mbd", b".")]),
        )
        .unwrap();

        let mods = scan_mod_directory(&mod_dir, None);
        let (locked, patch, other, locked_sound) = (&mods[0], &mods[1], &mods[2], &mods[3]);
        assert!(locked.error.is_none(), "{:?}", locked.error);
        assert_eq!(locked.icon.as_deref(), Some(JPEG));

        // overlap is seen although the locked archive lists nothing
        assert!(locked.overlaps(patch) && patch.overlaps(locked));
        // two locked archives are compared hash to hash
        assert!(locked.overlaps(locked_sound));
        // manifest and icon are in every table and must not count
        assert!(!locked.overlaps(other) && !patch.overlaps(other) && !locked_sound.overlaps(other));

        // hidden def files get their names back: the one the patch lists and
        // the standard physics file
        let mut defs = locked.def_files.clone();
        defs.sort();
        assert_eq!(defs, ["def/vehicle/physics.sii", "def/vehicle/truck/x/chassis.sii"]);
        assert_eq!(locked.effective_categories(&[]), ["physics"]);
        // a locked map is recognised by its standard descriptor
        assert!(locked_sound.is_map && !locked.is_map);
        // nothing was invented for the ordinary mods
        assert_eq!(patch.def_files, ["def/vehicle/truck/x/chassis.sii"]);
        assert!(other.def_files.is_empty());
        // what stays hidden is known, and how much of it got a name
        assert_eq!((locked.hidden.len(), locked.hidden_named), (3, 2));
        assert_eq!((locked_sound.hidden.len(), locked_sound.hidden_named), (2, 1));
        assert!(patch.hidden.is_empty() && other.hidden.is_empty());
    }

    #[test]
    fn the_games_paths_name_what_no_other_mod_lists() {
        use crate::scs::hashfs::testutil::build_locked_v1;
        let tmp = tempfile::tempdir().unwrap();
        let mod_dir = tmp.path().join("mod");
        fs::create_dir_all(&mod_dir).unwrap();
        // two locked archives replacing the same file of the game: neither
        // lists it, and no other mod does
        for name in ["a_locked.scs", "b_locked.scs"] {
            fs::write(
                mod_dir.join(name),
                build_locked_v1(
                    0,
                    &[
                        ("manifest.sii", MANIFEST),
                        ("pic.jpg", JPEG),
                        ("def/vehicle/truck/x/engine/big.sii", b"."),
                        ("def/private/only_mine.sii", b"."),
                        ("map/europe/extra.mbd", b"."),
                    ],
                ),
            )
            .unwrap();
        }
        let payloads = mod_candidates(&mod_dir);

        // without the game's paths the shared file has no name
        let blind = scan_paths(&payloads, None, None);
        assert!(blind[0].def_files.is_empty());
        assert_eq!((blind[0].hidden.len(), blind[0].hidden_named), (3, 0));
        assert!(blind[0].overlaps(&blind[1]), "the hashes still give the overlap away");

        let mut game = GamePaths::build(
            ["def/vehicle/truck/x/engine/big.sii", "def/vehicle/truck/x/engine/small.sii", "map/europe.mbd"].into_iter(),
            "sig",
            &tmp.path().join("paths.bin"),
        )
        .unwrap();
        let mods = scan_paths(&payloads, None, Some(&mut game));
        for m in &mods {
            assert_eq!(m.def_files, ["def/vehicle/truck/x/engine/big.sii"]);
            assert_eq!((m.hidden.len(), m.hidden_named), (3, 1));
            assert!(!m.is_map, "a file below map/ is not a map descriptor");
        }
    }

    #[test]
    fn the_files_of_a_mod_can_overrule_its_manifest() {
        use crate::sii::crypto::testutil::scramble_3nk;
        const BARE: &[u8] = b"SiiNunit {\n mod_package: .package {\n display_name: \"Bare\"\n }\n}";
        let tmp = tempfile::tempdir().unwrap();
        let mod_dir = tmp.path().join("mod");
        fs::create_dir_all(&mod_dir).unwrap();

        // dashboard layouts whose manifest (MANIFEST) says "truck"
        make_zip(
            &mod_dir.join("a_dash.scs"),
            &[
                ("manifest.sii", MANIFEST),
                ("ui/dashboard/x.sii", b"."),
                ("ui/dashboard/x_gps.sii", b"."),
                ("ui/template/dashboard_text.x.sii", b"."),
                ("material/ui/dashboard/x.dds", b"."),
            ],
        );
        // a locked archive of scrambled paint jobs that declares nothing:
        // no file name and no readable byte, yet the units give it away
        let paint = scramble_3nk(b"SiiNunit\n{\naccessory_paint_job_data : red.x.paint_job\n{\n}\n}\n", 0x10);
        fs::write(
            mod_dir.join("b_locked_paint.scs"),
            build_locked_v1(&[
                ("manifest.sii", BARE),
                ("def/vehicle/truck/x/paint_job/a.sii", paint.as_slice()),
                ("def/vehicle/truck/x/paint_job/b.sii", paint.as_slice()),
                ("def/vehicle/truck/x/paint_job/c.sii", paint.as_slice()),
                ("vehicle/truck/x/paint/a.dds", b"DDS "),
            ]),
        )
        .unwrap();
        // one stray definition against the manifest: too little to act on
        make_zip(&mod_dir.join("c_camera.scs"), &[("manifest.sii", MANIFEST), ("def/camera/units/x.sii", b".")]);
        // a truck part declared as a truck: at home there, nothing to say
        make_zip(
            &mod_dir.join("d_part.scs"),
            &[("manifest.sii", MANIFEST), ("def/vehicle/truck/x/accessory/mirror/a.sii", b".")],
        );

        let mods = scan_mod_directory(&mod_dir, None);
        let (dash, locked, camera, part) = (&mods[0], &mods[1], &mods[2], &mods[3]);
        let none: [String; 0] = [];

        assert_eq!(dash.effective_categories(&none), ["interior"]);
        assert_eq!(dash.suggested_category(&none), None);
        // the user's pin still wins
        assert_eq!(dash.effective_categories(&["truck".to_string()]), ["truck"]);

        assert!(locked.error.is_none(), "{:?}", locked.error);
        assert_eq!((locked.evidence.hidden, locked.evidence.hidden_tried, locked.evidence.hidden_read), (4, 4, 3));
        assert_eq!(locked.effective_categories(&none), ["paint_job"]);
        assert!(locked.evidence.signals.iter().any(|s| s.from_content && s.key == "accessory_paint_job_data"));

        assert_eq!(camera.effective_categories(&none), ["truck"]);
        assert_eq!(camera.suggested_category(&none), Some("other"));
        assert_eq!(camera.suggested_category(&["truck".to_string()]), None);

        assert_eq!(part.effective_categories(&none), ["truck"]);
        assert_eq!((part.verdict(), part.suggested_category(&none)), (None, None));
    }

    #[test]
    fn overlap_ignores_root_files_and_sees_any_shared_path() {
        let mk = |files: &[&str]| {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            let mut m = ScannedMod::failed(Path::new("x.scs"), ScsFormat::Zip, "");
            m.file_hashes = listing_facts(&files, None, &[], None, None).file_hashes;
            m
        };
        let sound = mk(&["manifest.sii", "icon.jpg", "sound/truck/engine.bank"]);
        let patch = mk(&["manifest.sii", "icon.jpg", "base/sound/truck/engine.bank", "def/x.sii"]);
        let other = mk(&["manifest.sii", "icon.jpg", "material/ui/a.mat"]);
        assert!(sound.overlaps(&patch) && patch.overlaps(&sound)); // through the base/ layer
        assert!(!sound.overlaps(&other)); // manifest and icon do not count
        assert!(!other.overlaps(&mk(&[])));
    }

    #[test]
    fn own_files_tell_a_patch_from_a_mod_replacing_the_same_game_file() {
        let tmp = tempfile::tempdir().unwrap();
        let game = GamePaths::build(
            ["def/vehicle/truck/stock/engine.sii", "automat/aa/1.mat"].into_iter(),
            "sig",
            &tmp.path().join("paths.bin"),
        )
        .unwrap();
        let mk = |files: &[&str], game: Option<&GamePaths>| {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            let facts = listing_facts(&files, None, &[], None, game);
            let mut m = ScannedMod::failed(Path::new("x.scs"), ScsFormat::Zip, "");
            (m.file_hashes, m.own_hashes) = (facts.file_hashes, facts.own_hashes);
            m
        };
        let truck_files =
            ["def/vehicle/truck/new/data.sii", "def/vehicle/truck/stock/engine.sii", "automat/bb/2.mat"];
        let truck = mk(&truck_files, Some(&game));
        // only the truck's own definition is a file it brings
        assert_eq!(truck.own_hashes.as_ref(), [city_hash64(b"def/vehicle/truck/new/data.sii")]);

        // a patch replaces a file that exists only because of the truck
        let patch = mk(&["base/def/vehicle/truck/new/data.sii"], Some(&game));
        assert!(patch.shares_own_files(&truck) && truck.shares_own_files(&patch));
        // an engine tweak replaces the same file of the game: a conflict
        // to order, but neither needs the other (however it spells the path)
        let tweak = mk(&["Def/Vehicle/truck/stock/engine.sii", "def/vehicle/truck/stock/engine.sii"], Some(&game));
        assert!(tweak.overlaps(&truck) && !tweak.shares_own_files(&truck));
        assert!(tweak.own_hashes.is_empty());
        // the same generated material in two models means nothing
        let model = mk(&["automat/bb/2.mat", "model/thing.pmd"], Some(&game));
        assert!(model.overlaps(&truck) && !model.shares_own_files(&truck));

        // a locked archive: the game's paths still tell which nameless
        // files are its own
        let table = truck_files.iter().map(|p| city_hash64(p.as_bytes())).collect();
        let locked = listing_facts(&[], Some(table), &[], None, Some(&game));
        assert_eq!(locked.own_hashes.len(), 2, "its definition and the material it cannot name");
        assert_eq!(locked.file_hashes.len(), 3);

        // without the game's paths any shared file counts, generated ones aside
        let (truck, tweak, model) = (
            mk(&truck_files, None),
            mk(&["def/vehicle/truck/stock/engine.sii"], None),
            mk(&["automat/bb/2.mat", "model/thing.pmd"], None),
        );
        assert!(tweak.shares_own_files(&truck) && !model.shares_own_files(&truck));
    }

    #[test]
    fn the_files_a_mod_names_without_having_them_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mod_dir = tmp.path().join("mod");
        fs::create_dir_all(&mod_dir).unwrap();
        let world: &[u8] = br#"SiiNunit
{
@include "sub/part.sui"
@include "sub/gone.sui"
model_def : model.a {
	model_desc: "/model/pack/house.pmd"	# another mod's
	look: "/model/Own/Shed.PMD"
	icon: "volvo_fh"
	template: "/def/world/<base_name>.<id>.sii"
}
prefab_model : prefab.b {
	prefab_desc: "/prefab/cross/x.ppd"	# the game's
	name: "not a file.txt"
}
}"#;
        let material: &[u8] = br#"effect : "eut2.dif.spec" {
	texture : "texture_base" {
		source : "wall.tobj"
	}
	texture : "texture_over" {
		source : "//material/shared/./dirt.tobj"
	}
	texture : "texture_mask" {
		source : "../own/roof.tobj"
	}
	texture : "texture_none" {
		source : "../../../outside.tobj"
	}
}"#;
        make_zip(
            &mod_dir.join("def.scs"),
            &[
                ("manifest.sii", MANIFEST),
                ("def/world/model.pack.sii", world),
                ("def/world/sub/part.sui", b"# nothing"),
                ("model/own/shed.pmd", b"."),
                ("model/own/wall.mat", material),
                ("model/own/roof.mat", br#"material : "eut2.dif" { texture : "roof.tobj" }"#),
                ("model/own/roof.tobj", b"."),
            ],
        );
        let mut game = GamePaths::build(["prefab/cross/x.ppd"].into_iter(), "sig", &tmp.path().join("paths.bin")).unwrap();

        let mods = scan_paths(&mod_candidates(&mod_dir), None, Some(&mut game));
        let m = &mods[0];
        assert_eq!(m.error, None);
        // what it names besides its own files and the game's, lower case;
        // names without a file ending, templates and shaders are no paths
        assert_eq!(
            m.wants.as_ref(),
            ["def/world/sub/gone.sui", "material/shared/dirt.tobj", "model/own/wall.tobj", "model/pack/house.pmd"]
        );
        // those four, its own shed, include and roof texture, the game's prefab
        assert_eq!(m.named_paths, 8);
        assert_eq!(m.top_dirs, ["def", "model"]);
        assert!(m.provides.binary_search(&city_hash64(b"model/own/shed.pmd")).is_ok());

        // without the game's paths a file of the game cannot be told from
        // a missing one: nothing is kept
        let blind = scan_mod_directory(&mod_dir, None);
        assert!(blind[0].wants.is_empty());
        assert_eq!(blind[0].named_paths, 0);
    }

    #[test]
    fn what_a_mod_brings_for_the_world_map_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mod_dir = tmp.path().join("mod");
        fs::create_dir_all(&mod_dir).unwrap();
        make_zip(
            &mod_dir.join("a_def.scs"),
            &[
                ("manifest.sii", MANIFEST),
                ("def/map_data.sii", b"map_data : .map.data {\n ui_map_center: (66700, -24700)\n ui_map_size: (8127, 7928)\n}"),
                ("map/europe/sec+0010-0002.base", b"."),
                ("map/europe/sec+0010-0002.aux", b"."),
                ("Map/Europe/sec-0034+0021.base", b"."),
                ("map/europe.mbd", b"."),
            ],
        );
        make_zip(
            &mod_dir.join("b_assets.scs"),
            &[("manifest.sii", MANIFEST), ("material/ui/map/map0.dds", b"."), ("material/ui/map/road/a1.mat", b".")],
        );
        // a locked archive: values, picture and a sector, none of them listed
        fs::write(
            mod_dir.join("c_locked.scs"),
            build_locked_v1(&[
                ("manifest.sii", MANIFEST),
                ("def/map_data.sii", b"ui_map_center: (1, 2)\nui_map_size: (3, 4)"),
                ("material/ui/map/map.tobj", b"."),
                ("map/europe/sec+0035-0004.base", b"."),
            ]),
        )
        .unwrap();
        make_zip(&mod_dir.join("d_truck.scs"), &[("manifest.sii", MANIFEST), ("def/vehicle/truck/x/data.sii", b".")]);

        let mods = scan_mod_directory(&mod_dir, None);
        let facts = |i: usize| &mods[i].world_map;
        assert!(facts(0).has_data && !facts(0).background);
        assert_eq!(facts(0).data.map(|d| (d.center, d.size_km)), Some(((66700.0, -24700.0), (8127.0, 7928.0))));
        assert_eq!(facts(0).sectors, [(-34, 21), (10, -2)]);
        assert_eq!(facts(0).maps, ["europe"]);
        assert!(facts(1).background && !facts(1).has_data && facts(1).sectors.is_empty());
        assert!(facts(2).has_data && facts(2).background);
        assert_eq!(facts(2).data.map(|d| d.center), Some((1.0, 2.0)));
        assert_eq!(facts(2).sectors, [(35, -4)]);
        assert!(facts(3).is_empty());
    }

    #[test]
    fn scans_every_container_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let mod_dir = tmp.path().join("mod");
        fs::create_dir_all(&mod_dir).unwrap();

        // zip .scs with icon + description named by the manifest
        make_zip(
            &mod_dir.join("a_truck.scs"),
            &[
                ("manifest.sii", MANIFEST),
                ("pic.jpg", JPEG),
                ("desc.txt", b"A fine truck."),
                ("def/vehicle/truck/x.sii", b"."),
            ],
        );
        // hashfs physics mod labelled as a truck by its manifest
        fs::write(
            mod_dir.join("b_physics.scs"),
            build_v1(0, &[("manifest.sii", MANIFEST, true), ("def/vehicle/physics.sii", b".", false)]),
        )
        .unwrap();
        // unpacked map mod
        let unpacked = mod_dir.join("c_map");
        fs::create_dir_all(unpacked.join("map")).unwrap();
        fs::write(unpacked.join("manifest.sii"), MANIFEST).unwrap();
        fs::write(unpacked.join("map/europe.mbd"), b".").unwrap();
        // junk
        fs::write(mod_dir.join("d_broken.scs"), b"garbage that is no archive").unwrap();
        fs::write(mod_dir.join("readme.txt"), b"ignored").unwrap();
        fs::create_dir_all(mod_dir.join("not_a_mod")).unwrap();

        let mods = scan_mod_directory(&mod_dir, None);
        let names: Vec<String> = mods.iter().map(|m| m.mod_name()).collect();
        assert_eq!(names, ["a_truck", "b_physics", "c_map", "d_broken"]);

        let truck = &mods[0];
        assert_eq!(truck.format, ScsFormat::Zip);
        assert_eq!(truck.display_name(), "Test Truck");
        assert_eq!(truck.icon.as_deref(), Some(JPEG));
        assert_eq!(truck.description.as_deref(), Some("A fine truck."));
        assert_eq!(truck.effective_categories(&[]), ["truck"]);
        assert!(truck.installed_at > 0.0);

        let physics = &mods[1];
        assert_eq!(physics.format, ScsFormat::HashfsV1);
        assert_eq!(physics.effective_categories(&[]), ["physics"]);
        assert_eq!(physics.effective_categories(&["sound".to_string()]), ["sound"]);

        let map = &mods[2];
        assert_eq!(map.format, ScsFormat::Directory);
        assert!(map.is_map);
        assert_eq!(map.effective_categories(&[]), ["map"]);

        let broken = &mods[3];
        assert!(broken.manifest.is_none());
        assert!(broken.error.is_some());
        assert_eq!(broken.display_name(), "d_broken");
        assert_eq!(broken.effective_categories(&[]), ["other"]);
    }
}
