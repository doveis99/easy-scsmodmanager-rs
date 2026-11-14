//! Walks the filesystem to enumerate installed mods.
//!
//! A mod payload can land on disk as:
//!
//! * `mod/<name>.scs`                         - ZIP, HashFS or AEM archive
//! * `mod/<name>.zip`                         - ZIP with a .zip suffix
//! * `mod/<name>/manifest.sii`                - unpacked directory
//! * `workshop/content/<appid>/<id>/<slot>`   - workshop archive or dir,
//!   with `versions.sii` selecting the active slot

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::Serialize;

use crate::categories::{contains_map, content_category, effective_categories};
use crate::game::GameInstall;
use crate::identity::{mod_name_for_path, workshop_id_for_path};
use crate::manifest::{read_bundle, ManifestBundle, ModManifest, MANIFEST_ENTRY};
use crate::scs::aem::AemSource;
use crate::scs::cityhash::city_hash64;
use crate::scs::dir::DirSource;
use crate::scs::hashfs::HashFsReader;
use crate::scs::zip::{is_zipfile, RawZipSource, ZipSource};
use crate::scs::{detect_format, ModSource, ScsFormat};
use crate::workshop::{resolve_workshop_payload, ARCHIVE_SUFFIXES};
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
    /// When the payload landed on this filesystem (seconds since the Unix
    /// epoch; creation time where available). 0 when unreadable.
    pub installed_at: f64,
    #[serde(skip)]
    pub icon: Option<Vec<u8>>,
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

    /// Category the content proves, falling back to the manifest.
    pub fn effective_categories(&self, override_token: Option<&str>) -> Vec<String> {
        let manifest_cats = self
            .manifest
            .as_ref()
            .map(|m| m.categories.as_slice())
            .unwrap_or_default();
        effective_categories(
            manifest_cats,
            self.is_map,
            override_token,
            content_category(&self.def_files),
        )
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
            installed_at: 0.0,
            icon: None,
        }
    }

    /// True when both mods carry a file at the same path - whichever sits
    /// higher in the load order wins it.
    pub fn overlaps(&self, other: &ScannedMod) -> bool {
        // both sides are sorted: walk them together
        let (a, b) = (&self.file_hashes, &other.file_hashes);
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
}

/// Scan the install's `mod/` directory and workshop tree.
///
/// `owned_dlc` gates the 1.48 `dlc_<name>/` normalisation; `None` means the
/// game directory was not found, so those folders are ungated.
pub fn scan_game_install(
    install: &GameInstall,
    game_version: Option<&str>,
    owned_dlc: Option<&BTreeSet<String>>,
) -> Vec<ScannedMod> {
    let mut payloads = mod_candidates(&install.mod_dir());
    if let Some(ws) = &install.workshop_dir {
        payloads.extend(workshop_payloads(ws, game_version));
    }
    scan_paths(&payloads, owned_dlc)
}

pub fn scan_mod_directory(directory: &Path, owned_dlc: Option<&BTreeSet<String>>) -> Vec<ScannedMod> {
    scan_paths(&mod_candidates(directory), owned_dlc)
}

fn scan_paths(paths: &[PathBuf], owned_dlc: Option<&BTreeSet<String>>) -> Vec<ScannedMod> {
    let mut mods: Vec<ScannedMod> = paths
        .par_iter()
        .map(|p| scan_one(p, owned_dlc))
        .collect();
    name_hidden_files(&mut mods);
    mods
}

/// Paths probed in every archive regardless of what other mods list.
const PROBED_DEFS: &[&str] = &["def/vehicle/physics.sii"];
const PROBED_MAPS: &[&str] = &["map/europe.mbd", "map/usa.mbd"];

/// Put names to files that archives carry without listing them.
///
/// A "locked" HashFS archive has its directory listings stripped: its
/// files are there (and in the fingerprint, by hash) but nameless. A path
/// that any other mod lists, or one of the few standard ones, can still be
/// tested against the fingerprint - so the def files such an archive
/// shares with the rest become visible to conflict detection, and a map or
/// physics mod is recognised by content again.
fn name_hidden_files(mods: &mut [ScannedMod]) {
    let mut known: Vec<(u64, String)> = mods
        .iter()
        .flat_map(|m| m.def_files.iter())
        .map(String::as_str)
        .chain(PROBED_DEFS.iter().copied())
        .map(|p| (city_hash64(p.as_bytes()), p.to_string()))
        .collect();
    known.sort_unstable();
    known.dedup();
    let map_hashes: Vec<u64> = PROBED_MAPS.iter().map(|p| city_hash64(p.as_bytes())).collect();

    for m in mods.iter_mut() {
        let carries = |hash: &u64| m.file_hashes.binary_search(hash).is_ok();
        let listed: std::collections::HashSet<&str> = m.def_files.iter().map(String::as_str).collect();
        let hidden: Vec<String> = known
            .iter()
            .filter(|(hash, path)| !listed.contains(path.as_str()) && carries(hash))
            .map(|(_, path)| path.clone())
            .collect();
        if !m.is_map && map_hashes.iter().any(carries) {
            m.is_map = true;
        }
        m.def_files.extend(hidden);
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

pub fn scan_one(path: &Path, owned_dlc: Option<&BTreeSet<String>>) -> ScannedMod {
    let mut result = if path.is_dir() {
        scan_directory(path, owned_dlc)
    } else if path.is_file() {
        scan_archive(path, owned_dlc)
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
}

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
/// `root_names` the root-level files the manifest points at.
fn listing_facts(
    files: &[String],
    table_hashes: Option<Vec<u64>>,
    root_names: &[&str],
    owned_dlc: Option<&BTreeSet<String>>,
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
    let mut hashes: Vec<u64> = normalized
        .iter()
        .filter(|p| p.contains('/') && !p.ends_with('/'))
        .map(|p| city_hash64(p.as_bytes()))
        .collect();
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
        hashes.extend(table.into_iter().filter(|h| !root.contains(h)));
    }
    hashes.sort_unstable();
    hashes.dedup();
    ListingFacts {
        is_map,
        def_files,
        file_hashes: Arc::from(hashes),
    }
}

fn from_bundle(
    path: &Path,
    format: ScsFormat,
    bundle: ManifestBundle,
    source: &mut dyn ModSource,
    owned_dlc: Option<&BTreeSet<String>>,
) -> ScannedMod {
    let root_names = [bundle.manifest.icon.as_str(), bundle.manifest.description_file.as_str()];
    let facts = listing_facts(&source.list_files(), source.path_hashes(), &root_names, owned_dlc);
    ScannedMod {
        path: path.to_path_buf(),
        format,
        manifest: Some(bundle.manifest),
        error: None,
        description: bundle.description,
        is_map: facts.is_map,
        def_files: facts.def_files,
        file_hashes: facts.file_hashes,
        installed_at: 0.0,
        icon: bundle.icon,
    }
}

fn scan_source(
    path: &Path,
    format: ScsFormat,
    source: &mut dyn ModSource,
    owned_dlc: Option<&BTreeSet<String>>,
) -> std::result::Result<ScannedMod, Error> {
    let bundle = read_bundle(source)?;
    Ok(from_bundle(path, format, bundle, source, owned_dlc))
}

fn scan_directory(directory: &Path, owned_dlc: Option<&BTreeSet<String>>) -> ScannedMod {
    DirSource::open(directory)
        .and_then(|mut s| scan_source(directory, ScsFormat::Directory, &mut s, owned_dlc))
        .unwrap_or_else(|e| ScannedMod::failed(directory, ScsFormat::Directory, e.to_string()))
}

fn scan_archive(path: &Path, owned_dlc: Option<&BTreeSet<String>>) -> ScannedMod {
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
        ScsFormat::Zip => scan_zip(path, owned_dlc),
        ScsFormat::HashfsV1 | ScsFormat::HashfsV2 => HashFsReader::open(path)
            .and_then(|mut r| scan_source(path, format, &mut r, owned_dlc))
            .unwrap_or_else(|e| ScannedMod::failed(path, format, e.to_string())),
        ScsFormat::Aem => AemSource::open(path)
            .and_then(|mut r| scan_source(path, format, &mut r, owned_dlc))
            .unwrap_or_else(|e| ScannedMod::failed(path, format, e.to_string())),
        _ => ScannedMod::failed(path, format, "Unknown SCS container format"),
    }
}

fn scan_zip(path: &Path, owned_dlc: Option<&BTreeSet<String>>) -> ScannedMod {
    let fmt = ScsFormat::Zip;
    let first = ZipSource::open(path).and_then(|mut z| scan_source(path, fmt, &mut z, owned_dlc));
    let prior_error = match first {
        Ok(found) => return found,
        Err(e) => e.to_string(),
    };

    // second pass for zips the normal reader refused: a set encryption flag
    // and an odd method label over an ordinary raw-deflate payload
    let second = RawZipSource::open(path).and_then(|mut r| scan_source(path, fmt, &mut r, owned_dlc));
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
        let facts = listing_facts(&files, None, &[], None);
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
        assert_eq!(locked.effective_categories(None), ["physics"]);
        // a locked map is recognised by its standard descriptor
        assert!(locked_sound.is_map && !locked.is_map);
        // nothing was invented for the ordinary mods
        assert_eq!(patch.def_files, ["def/vehicle/truck/x/chassis.sii"]);
        assert!(other.def_files.is_empty());
    }

    #[test]
    fn overlap_ignores_root_files_and_sees_any_shared_path() {
        let mk = |files: &[&str]| {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            let mut m = ScannedMod::failed(Path::new("x.scs"), ScsFormat::Zip, "");
            m.file_hashes = listing_facts(&files, None, &[], None).file_hashes;
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
        assert_eq!(truck.effective_categories(None), ["truck"]);
        assert!(truck.installed_at > 0.0);

        let physics = &mods[1];
        assert_eq!(physics.format, ScsFormat::HashfsV1);
        assert_eq!(physics.effective_categories(None), ["physics"]);
        assert_eq!(physics.effective_categories(Some("sound")), ["sound"]);

        let map = &mods[2];
        assert_eq!(map.format, ScsFormat::Directory);
        assert!(map.is_map);
        assert_eq!(map.effective_categories(None), ["map"]);

        let broken = &mods[3];
        assert!(broken.manifest.is_none());
        assert!(broken.error.is_some());
        assert_eq!(broken.display_name(), "d_broken");
        assert_eq!(broken.effective_categories(None), ["other"]);
    }
}
