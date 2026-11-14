//! Reads ETS2 / ATS user profiles and writes the active mod list back.
//!
//! A profile lives in `Documents/<game>/profiles/<hex>/` (local only),
//! `Documents/<game>/steam_profiles/<hex>/` or the Steam Cloud user-data
//! root. The directory name is the display name encoded as UTF-8 hex.
//! `profile.sii` may be plaintext or ScsC-encrypted.
//!
//! The active list is an indexed array in the `user_profile` unit:
//!
//! ```text
//! active_mods: 2
//! active_mods[0]: "name|Display Name"
//! active_mods[1]: "other|Other Name"
//! ```
//!
//! Index 0 is the bottom of the in-game load order, the highest index the
//! top.

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::game::GameInstall;
use crate::sii::crypto::sii_bytes_to_text;
use crate::sii::parse_sii;
use crate::steam::find_steam_installs;
use crate::{format_err, Error, Result};

const PROFILE_UNIT_CLASS: &str = "user_profile";
const PROFILE_DIRECTORY_NAMES: &[&str] = &["profiles", "steam_profiles"];
const PROFILE_FILE_NAME: &str = "profile.sii";
const APP_DIR_NAME: &str = "easy-scsmodmanager-rs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveMod {
    pub name: String,
    pub display_name: String,
}

impl ActiveMod {
    /// Entries are stored as `<mod_name>|<display name>`. Display names may
    /// contain pipes themselves, so only the first one splits.
    pub fn parse(raw: &str) -> Self {
        match raw.split_once('|') {
            Some((name, display)) => Self {
                name: name.to_string(),
                display_name: display.to_string(),
            },
            None => Self {
                name: raw.to_string(),
                display_name: String::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    /// Decoded directory name (falls back to the raw hex).
    pub dir_name: String,
    pub profile_name: String,
    /// In file order: index 0 = bottom of the load order.
    pub active_mods: Vec<ActiveMod>,
}

/// SCS encodes the profile display name as hex(utf-8).
pub fn decode_profile_dir_name(hex_name: &str) -> String {
    let decoded = (|| {
        if hex_name.len() % 2 != 0 || !hex_name.is_ascii() {
            return None;
        }
        let bytes = (0..hex_name.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_name[i..i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        String::from_utf8(bytes).ok()
    })();
    decoded.unwrap_or_else(|| hex_name.to_string())
}

pub fn read_profile(profile_sii_path: &Path) -> Result<Profile> {
    let data = fs::read(profile_sii_path)?;
    let text = sii_bytes_to_text(&data, false)?;
    let units = parse_sii(&text)?;
    let unit = units
        .iter()
        .find(|u| u.class == PROFILE_UNIT_CLASS)
        .ok_or_else(|| format_err(format!("No '{PROFILE_UNIT_CLASS}' unit found in profile")))?;
    let dir_name = decode_profile_dir_name(
        &profile_sii_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    Ok(Profile {
        profile_name: unit
            .text("profile_name")
            .map_or_else(|| dir_name.clone(), str::to_string),
        // `active_mods` is just the count scalar when there are no entries
        active_mods: unit
            .list("active_mods", false)
            .iter()
            .map(|raw| ActiveMod::parse(raw))
            .collect(),
        dir_name,
    })
}

fn subdirs_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// Steam Cloud profile parents:
/// `<steam>/userdata/<user-id>/<app-id>/remote/{profiles,steam_profiles}/`.
fn steam_userdata_profile_dirs(app_id: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for steam in find_steam_installs() {
        for user_dir in subdirs_sorted(&steam.join("userdata")) {
            let is_user_id = user_dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit()));
            if !is_user_id {
                continue;
            }
            let remote = user_dir.join(app_id.to_string()).join("remote");
            out.extend(
                PROFILE_DIRECTORY_NAMES
                    .iter()
                    .map(|sub| remote.join(sub))
                    .filter(|p| p.is_dir()),
            );
        }
    }
    out
}

/// Every `profile.sii` for the install across all known locations. A
/// profile mirrored under several roots is returned once, keyed by its hex
/// directory name (the first root wins).
pub fn discover_profiles(install: &GameInstall) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = PROFILE_DIRECTORY_NAMES
        .iter()
        .map(|sub| install.documents_dir.join(sub))
        .collect();
    parents.extend(steam_userdata_profile_dirs(install.game.app_id()));

    let mut seen: Vec<String> = Vec::new();
    let mut results = Vec::new();
    for parent in parents {
        for profile_dir in subdirs_sorted(&parent) {
            let sii = profile_dir.join(PROFILE_FILE_NAME);
            let Some(name) = profile_dir.file_name().map(|n| n.to_string_lossy().into_owned())
            else {
                continue;
            };
            if sii.is_file() && !seen.contains(&name) {
                seen.push(name);
                results.push(sii);
            }
        }
    }
    results
}

// --------------------------------------------------------------------- //
// writing
// --------------------------------------------------------------------- //

/// After `active_mods`, optional whitespace: what is left of the line.
fn after_key(line: &str) -> Option<&str> {
    Some(line.trim_start().strip_prefix("active_mods")?.trim_start())
}

/// `active_mods[<n>]:` entry line -> the text after the colon.
fn entry_rest(line: &str) -> Option<&str> {
    let rest = after_key(line)?.strip_prefix('[')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    rest[digits..]
        .strip_prefix(']')?
        .trim_start()
        .strip_prefix(':')
}

/// `active_mods: <n>` count line -> its indentation.
fn count_indent(line: &str) -> Option<&str> {
    let value = after_key(line)?.strip_prefix(':')?.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(&line[..line.len() - line.trim_start().len()])
}

/// The raw text between the quotes of an entry value, escapes left intact.
fn quoted_inner(rest: &str) -> Option<&str> {
    let body = rest.trim_start().strip_prefix('"')?;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(&body[..i]),
            _ => {}
        }
    }
    None
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Return `text` with its active_mods block swapped for `mods`.
///
/// Existing entries are re-emitted byte-exact (the game's own escaping of
/// their display names is never reformatted); only new entries are
/// serialised here. Every other line is left untouched.
pub fn replace_active_mods(text: &str, mods: &[ActiveMod]) -> Result<String> {
    // active name -> the exact stored string
    let mut originals: Vec<(&str, &str)> = Vec::new();
    for line in text.split('\n') {
        if let Some(inner) = entry_rest(line).and_then(quoted_inner) {
            let name = inner.split('|').next().unwrap_or(inner);
            if !originals.iter().any(|(n, _)| *n == name) {
                originals.push((name, inner));
            }
        }
    }
    let serialize = |m: &ActiveMod| -> String {
        if let Some((_, inner)) = originals.iter().find(|(n, _)| *n == m.name) {
            return (*inner).to_string();
        }
        if m.display_name.is_empty() {
            escape(&m.name)
        } else {
            format!("{}|{}", escape(&m.name), escape(&m.display_name))
        }
    };

    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in text.split('\n') {
        if entry_rest(line).is_some() {
            continue; // drop old indexed entries wherever they sit
        }
        if let Some(indent) = count_indent(line) {
            let eol = if line.ends_with('\r') { "\r" } else { "" };
            out.push(format!("{indent}active_mods: {}{eol}", mods.len()));
            for (i, m) in mods.iter().enumerate() {
                out.push(format!("{indent}active_mods[{i}]: \"{}\"{eol}", serialize(m)));
            }
            replaced = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        return Err(format_err("no active_mods line found in profile text"));
    }
    Ok(out.join("\n"))
}

/// Rewrite the active list in `profile.sii` (atomic).
///
/// Always writes plaintext SiiNunit, even when the input was ScsC: the
/// game validates the HMAC of an encrypted profile and falls back to
/// profile.bak.sii when it fails, which it would for one written here.
/// Plaintext profiles are read without that check.
pub fn write_active_mods(profile_sii_path: &Path, mods: &[ActiveMod]) -> Result<()> {
    let raw = fs::read(profile_sii_path)?;
    // strict decode: never risk a lossy round-trip on the user's profile
    let text = sii_bytes_to_text(&raw, true)?;
    let new_text = replace_active_mods(&text, mods)?;

    // validate before touching disk: the result must parse and must hold
    // exactly the list that was asked for
    let units = parse_sii(&new_text)?;
    let written = units
        .iter()
        .find(|u| u.class == PROFILE_UNIT_CLASS)
        .map(|u| u.list("active_mods", false))
        .ok_or_else(|| format_err("rewritten profile lost its user_profile unit"))?;
    let names_match = written.len() == mods.len()
        && written
            .iter()
            .zip(mods)
            .all(|(raw, m)| ActiveMod::parse(raw).name == m.name);
    if !names_match {
        return Err(format_err("rewritten profile does not match the requested list"));
    }
    atomic_write(profile_sii_path, new_text.as_bytes())
}

fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| format_err("profile path has no parent directory"))?;
    let tmp = dir.join(format!(".{}.{}.tmp", PROFILE_FILE_NAME, std::process::id()));
    let result = (|| -> Result<()> {
        let mut fh = File::create(&tmp)?;
        fh.write_all(data)?;
        fh.sync_all()?;
        drop(fh);
        fs::rename(&tmp, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// --------------------------------------------------------------------- //
// backups
// --------------------------------------------------------------------- //

static DATA_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Keep all app data in `dir` instead of the per-user location - the
/// portable GUI points this at a folder next to its executable. Takes
/// effect once, before anything asks for the data dir; returns whether it
/// was applied.
pub fn set_data_dir(dir: PathBuf) -> bool {
    DATA_DIR_OVERRIDE.set(dir).is_ok()
}

/// Whether [`set_data_dir`] is in effect.
pub fn data_dir_is_overridden() -> bool {
    DATA_DIR_OVERRIDE.get().is_some()
}

/// Where this app keeps its data: the override from [`set_data_dir`] when
/// there is one, else [`system_data_dir`].
pub fn default_data_dir() -> PathBuf {
    DATA_DIR_OVERRIDE.get().cloned().unwrap_or_else(system_data_dir)
}

/// Per-user data directory of this app: `%APPDATA%` on Windows, otherwise
/// `$XDG_DATA_HOME` or `~/.local/share`.
pub fn system_data_dir() -> PathBuf {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| {
            env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
        })
    };
    base.unwrap_or_else(env::temp_dir).join(APP_DIR_NAME)
}

pub fn default_backup_root() -> PathBuf {
    default_data_dir().join("profile_backups")
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub profile_dir_name: String,
    /// `YYYY-MM-DD_HH-MM-SS`, local time.
    pub timestamp: String,
    pub size_bytes: u64,
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_files(&path, out),
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// Zip the whole profile directory of `sii_path` into a fresh backup under
/// `root/<hex-dir-name>/<timestamp>.zip` - outside the game's profile
/// tree, so the game never sees it.
pub fn create_backup(sii_path: &Path, root: &Path) -> Result<BackupEntry> {
    let profile_dir = sii_path
        .parent()
        .filter(|p| p.is_dir())
        .ok_or_else(|| Error::NotFound(sii_path.display().to_string()))?;
    let dir_name = profile_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target_dir = root.join(&dir_name);
    fs::create_dir_all(&target_dir)?;

    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    // a second backup within the same second must not replace the first
    let target = (0..)
        .map(|n| match n {
            0 => target_dir.join(format!("{timestamp}.zip")),
            n => target_dir.join(format!("{timestamp}_{n}.zip")),
        })
        .find(|p| !p.exists())
        .expect("unbounded range");
    let tmp = target.with_extension("zip.tmp");

    let mut files = Vec::new();
    collect_files(profile_dir, &mut files);
    files.sort();

    let result = (|| -> Result<()> {
        let mut zw = zip::ZipWriter::new(File::create(&tmp)?);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for file in &files {
            let rel = file
                .strip_prefix(profile_dir)
                .map_err(|_| format_err("file escaped the profile directory"))?
                .to_string_lossy()
                .replace('\\', "/");
            zw.start_file(rel, opts)?;
            zw.write_all(&fs::read(file)?)?;
        }
        zw.finish()?.sync_all()?;
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(BackupEntry {
        size_bytes: fs::metadata(&target).map(|m| m.len()).unwrap_or(0),
        path: target,
        profile_dir_name: dir_name,
        timestamp,
    })
}

fn backup_dir_for(sii_path: &Path, root: &Path) -> Result<PathBuf> {
    let name = sii_path
        .parent()
        .and_then(|p| p.file_name())
        .ok_or_else(|| format_err("profile path has no directory name"))?;
    Ok(root.join(name))
}

/// Backups of this profile, newest first.
pub fn list_backups(sii_path: &Path, root: &Path) -> Vec<BackupEntry> {
    let Ok(dir) = backup_dir_for(sii_path, root) else {
        return Vec::new();
    };
    let profile_dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut entries: Vec<BackupEntry> = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")))
                .map(|path| BackupEntry {
                    // file names are `YYYY-MM-DD_HH-MM-SS[_n].zip`
                    timestamp: path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    size_bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    profile_dir_name: profile_dir_name.clone(),
                    path,
                })
                .collect()
        })
        .unwrap_or_default();
    // the timestamp format sorts chronologically as text
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    entries
}

/// Resolve `backup_path` to one of this profile's own backups. Anything
/// else - another profile's backup, a path outside the backup root - is
/// refused, so a restore or delete can never be pointed at a stray file.
fn own_backup(sii_path: &Path, root: &Path, backup_path: &Path) -> Result<BackupEntry> {
    list_backups(sii_path, root)
        .into_iter()
        .find(|b| b.path == backup_path)
        .ok_or_else(|| Error::NotFound(format!("not a backup of this profile: {}", backup_path.display())))
}

/// Extract a backup over the profile directory of `sii_path`.
///
/// The current state is backed up first, so a restore can itself be
/// undone. Existing files are overwritten; files that are not in the
/// backup are left alone. Returns the safety backup that was taken.
pub fn restore_backup(sii_path: &Path, root: &Path, backup_path: &Path) -> Result<BackupEntry> {
    let backup = own_backup(sii_path, root, backup_path)?;
    let profile_dir = sii_path
        .parent()
        .ok_or_else(|| format_err("profile path has no parent directory"))?;

    // read the whole archive before touching the profile: a corrupt backup
    // must fail here, not halfway through overwriting files
    let mut archive = zip::ZipArchive::new(File::open(&backup.path)?)?;
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        // `enclosed_name` refuses absolute paths and `..` components
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| format_err(format!("unsafe path in backup: {}", entry.name())))?;
        let mut data = Vec::with_capacity(entry.size().min(1 << 24) as usize);
        std::io::Read::read_to_end(&mut entry, &mut data)?;
        files.push((rel, data));
    }
    if !files.iter().any(|(rel, _)| rel == Path::new(PROFILE_FILE_NAME)) {
        return Err(format_err("backup does not contain a profile.sii"));
    }

    let safety = create_backup(sii_path, root)?;
    for (rel, data) in files {
        let target = profile_dir.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, data)?;
    }
    Ok(safety)
}

pub fn delete_backup(sii_path: &Path, root: &Path, backup_path: &Path) -> Result<()> {
    let backup = own_backup(sii_path, root, backup_path)?;
    fs::remove_file(backup.path)?;
    Ok(())
}

/// Keep the newest `keep` backups of this profile and delete the rest.
/// `keep == 0` keeps everything. Returns how many were removed.
pub fn prune_backups(sii_path: &Path, root: &Path, keep: u32) -> usize {
    if keep == 0 {
        return 0;
    }
    list_backups(sii_path, root)
        .into_iter()
        .skip(keep as usize)
        .filter(|b| fs::remove_file(&b.path).is_ok())
        .count()
}

/// Back up the profile, then write the new active list. The backup holds
/// the pre-edit profile, so one is always recoverable.
pub fn save_active_mods(
    profile_sii_path: &Path,
    mods: &[ActiveMod],
    backup_root: &Path,
) -> Result<BackupEntry> {
    let entry = create_backup(profile_sii_path, backup_root)?;
    write_active_mods(profile_sii_path, mods)?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "SiiNunit\n{\nuser_profile : _nameless.1 {\n face: 3\n active_mods: 3\n active_mods[0]: \"a|Alpha \\\"quoted\\\"\"\n active_mods[1]: \"b|Beta | pipe\"\n active_mods[2]: \"c\"\n profile_name: \"Jo\"\n}\n}\n";

    fn am(name: &str, display: &str) -> ActiveMod {
        ActiveMod {
            name: name.into(),
            display_name: display.into(),
        }
    }

    #[test]
    fn active_mod_parsing() {
        assert_eq!(ActiveMod::parse("b|Beta | pipe"), am("b", "Beta | pipe"));
        assert_eq!(ActiveMod::parse("c"), am("c", ""));
    }

    #[test]
    fn dir_name_decoding() {
        assert_eq!(decode_profile_dir_name("4A6F"), "Jo");
        assert_eq!(decode_profile_dir_name("ED959C"), "한");
        assert_eq!(decode_profile_dir_name("xyz"), "xyz");
        assert_eq!(decode_profile_dir_name("FF"), "FF"); // not UTF-8
    }

    #[test]
    fn line_classifiers() {
        assert!(entry_rest(" active_mods[12]: \"x\"").is_some());
        assert!(entry_rest(" active_mods: 3").is_none());
        assert!(entry_rest(" active_mods_extra[0]: 1").is_none());
        assert_eq!(count_indent("  active_mods: 3\r"), Some("  "));
        assert_eq!(count_indent(" active_mods[0]: \"x\""), None);
        assert_eq!(count_indent(" active_mods: x"), None);
    }

    #[test]
    fn replace_reorders_byte_exact_and_adds_new() {
        let mods = [am("c", ""), am("new", "New \"Mod\""), am("a", "ignored display")];
        let out = replace_active_mods(PROFILE, &mods).unwrap();
        assert!(out.contains(" active_mods: 3\n active_mods[0]: \"c\"\n"));
        assert!(out.contains(" active_mods[1]: \"new|New \\\"Mod\\\"\"\n"));
        // existing entry keeps its exact original escaping and display name
        assert!(out.contains(" active_mods[2]: \"a|Alpha \\\"quoted\\\"\"\n"));
        assert!(!out.contains("Beta"));
        assert!(out.contains(" face: 3\n") && out.contains(" profile_name: \"Jo\"\n"));
        // and it still parses to exactly that list
        let units = parse_sii(&out).unwrap();
        assert_eq!(units[0].list("active_mods", false).len(), 3);
    }

    #[test]
    fn replace_handles_empty_and_crlf() {
        let out = replace_active_mods(PROFILE, &[]).unwrap();
        assert!(out.contains(" active_mods: 0\n profile_name"));
        let crlf = PROFILE.replace('\n', "\r\n");
        let out = replace_active_mods(&crlf, &[am("z", "Z")]).unwrap();
        assert!(out.contains(" active_mods: 1\r\n active_mods[0]: \"z|Z\"\r\n profile_name"));
        assert!(replace_active_mods("SiiNunit { user_profile : x { } }", &[]).is_err());
    }

    #[test]
    fn read_write_round_trip_with_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let pdir = tmp.path().join("profiles").join("4A6F");
        fs::create_dir_all(&pdir).unwrap();
        let sii = pdir.join("profile.sii");
        fs::write(&sii, PROFILE).unwrap();
        fs::write(pdir.join("controls.sii"), b"controls").unwrap();

        let p = read_profile(&sii).unwrap();
        assert_eq!(p.dir_name, "Jo");
        assert_eq!(p.profile_name, "Jo");
        assert_eq!(p.active_mods.len(), 3);
        assert_eq!(p.active_mods[0], am("a", "Alpha \"quoted\""));

        let mut reordered = p.active_mods.clone();
        reordered.reverse();
        let backups = tmp.path().join("backups");
        let entry = save_active_mods(&sii, &reordered, &backups).unwrap();
        assert!(entry.path.is_file() && entry.size_bytes > 0);
        assert_eq!(entry.profile_dir_name, "4A6F");

        let again = read_profile(&sii).unwrap();
        assert_eq!(again.active_mods, reordered);
        assert_eq!(fs::read(pdir.join("controls.sii")).unwrap(), b"controls");

        // the backup holds the pre-edit profile
        let mut z = zip::ZipArchive::new(File::open(&entry.path).unwrap()).unwrap();
        let mut old = String::new();
        std::io::Read::read_to_string(&mut z.by_name("profile.sii").unwrap(), &mut old).unwrap();
        assert_eq!(old, PROFILE);
        // no temp files left behind
        assert_eq!(fs::read_dir(&pdir).unwrap().count(), 2);
    }

    #[test]
    fn backups_list_restore_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let pdir = tmp.path().join("profiles").join("4A6F");
        fs::create_dir_all(pdir.join("save").join("1")).unwrap();
        let sii = pdir.join("profile.sii");
        fs::write(&sii, PROFILE).unwrap();
        fs::write(pdir.join("save").join("1").join("game.sii"), b"save one").unwrap();
        let root = tmp.path().join("backups");

        assert!(list_backups(&sii, &root).is_empty());
        let first = create_backup(&sii, &root).unwrap();

        // change the profile, add a file the backup does not know
        fs::write(&sii, "changed").unwrap();
        fs::write(pdir.join("save").join("1").join("game.sii"), b"changed save").unwrap();
        fs::write(pdir.join("new.txt"), b"keep me").unwrap();

        let safety = restore_backup(&sii, &root, &first.path).unwrap();
        assert_eq!(fs::read_to_string(&sii).unwrap(), PROFILE);
        assert_eq!(fs::read(pdir.join("save").join("1").join("game.sii")).unwrap(), b"save one");
        assert_eq!(fs::read(pdir.join("new.txt")).unwrap(), b"keep me");

        // the restore took a safety backup of the changed state, newest first
        let listed = list_backups(&sii, &root);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].path, safety.path);
        assert_eq!(listed[1].path, first.path);
        let mut z = zip::ZipArchive::new(File::open(&safety.path).unwrap()).unwrap();
        let mut held = String::new();
        std::io::Read::read_to_string(&mut z.by_name("profile.sii").unwrap(), &mut held).unwrap();
        assert_eq!(held, "changed");

        // pruning keeps the newest, 0 keeps all
        assert_eq!(prune_backups(&sii, &root, 0), 0);
        assert_eq!(prune_backups(&sii, &root, 1), 1);
        let left = list_backups(&sii, &root);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, safety.path);

        delete_backup(&sii, &root, &safety.path).unwrap();
        assert!(list_backups(&sii, &root).is_empty());
    }

    #[test]
    fn backups_of_other_profiles_and_stray_files_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("backups");
        let mut siis = Vec::new();
        for name in ["4A6F", "4B6F"] {
            let pdir = tmp.path().join("profiles").join(name);
            fs::create_dir_all(&pdir).unwrap();
            fs::write(pdir.join("profile.sii"), PROFILE).unwrap();
            siis.push(pdir.join("profile.sii"));
        }
        let other = create_backup(&siis[1], &root).unwrap();
        assert!(restore_backup(&siis[0], &root, &other.path).is_err());
        assert!(delete_backup(&siis[0], &root, &other.path).is_err());
        assert!(other.path.is_file());

        let stray = tmp.path().join("stray.zip");
        fs::write(&stray, b"x").unwrap();
        assert!(restore_backup(&siis[0], &root, &stray).is_err());
        assert!(delete_backup(&siis[0], &root, &stray).is_err());
        assert!(stray.is_file());
    }

    #[test]
    fn a_backup_without_a_profile_is_not_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let pdir = tmp.path().join("profiles").join("4A6F");
        fs::create_dir_all(&pdir).unwrap();
        let sii = pdir.join("profile.sii");
        fs::write(&sii, PROFILE).unwrap();
        let root = tmp.path().join("backups");
        let dir = root.join("4A6F");
        fs::create_dir_all(&dir).unwrap();
        let bogus = dir.join("2020-01-01_00-00-00.zip");
        let mut zw = zip::ZipWriter::new(File::create(&bogus).unwrap());
        zw.start_file("controls.sii", zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();

        assert!(restore_backup(&sii, &root, &bogus).is_err());
        assert_eq!(fs::read_to_string(&sii).unwrap(), PROFILE);
        // nothing was written and no safety backup was taken for a refused restore
        assert_eq!(list_backups(&sii, &root).len(), 1);
    }
}
