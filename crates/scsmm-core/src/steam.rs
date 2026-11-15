//! Discovers Steam installs and library roots.
//!
//! Steam lists its library locations in `steamapps/libraryfolders.vdf`
//! inside the Steam install directory; there can be several on different
//! drives.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const LIBRARYFOLDERS_RELATIVE: &str = "steamapps/libraryfolders.vdf";

/// One registry string value, or `None` when missing / not on Windows.
/// `expand` runs `%VAR%` expansion for REG_EXPAND_SZ values.
#[cfg(windows)]
pub fn read_registry_string(hive: &str, subkey: &str, name: &str, expand: bool) -> Option<String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let root = match hive {
        "HKCU" => HKEY_CURRENT_USER,
        "HKLM" => HKEY_LOCAL_MACHINE,
        _ => return None,
    };
    let key = winreg::RegKey::predef(root).open_subkey(subkey).ok()?;
    let value: String = key.get_value(name).ok()?;
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    Some(if expand { expand_env_vars(text) } else { text.to_string() })
}

#[cfg(not(windows))]
pub fn read_registry_string(_hive: &str, _subkey: &str, _name: &str, _expand: bool) -> Option<String> {
    None
}

/// Expand `%NAME%` references; unknown names are left as written.
pub fn expand_env_vars(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match env::var(name) {
                    Ok(value) if !name.is_empty() => out.push_str(&value),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn home_dir() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default())
}

/// Platform-typical Steam install paths, not filtered for existence.
pub fn steam_install_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        // registry first - it knows where Steam actually went
        let reg = read_registry_string("HKCU", r"Software\Valve\Steam", "SteamPath", false)
            .or_else(|| read_registry_string("HKLM", r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath", false));
        out.extend(reg.map(PathBuf::from));
        for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
            out.extend(env::var_os(var).map(|root| PathBuf::from(root).join("Steam")));
        }
        out.extend(env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("scoop/apps/steam/current")));
    } else if cfg!(target_os = "macos") {
        out.push(home_dir().join("Library/Application Support/Steam"));
    } else {
        let home = home_dir();
        let xdg = env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
        out.push(home.join(".steam/steam"));
        out.push(xdg.join("Steam"));
        out.push(home.join(".var/app/com.valvesoftware.Steam/data/Steam"));
        out.push(home.join("snap/steam/common/.local/share/Steam"));
    }
    out
}

/// Steam install dirs that actually contain libraryfolders.vdf.
pub fn find_steam_installs() -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for c in steam_install_candidates() {
        if c.join(LIBRARYFOLDERS_RELATIVE).is_file() && !seen.iter().any(|s| same_path(s, &c)) {
            seen.push(c);
        }
    }
    seen
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
    norm(a) == norm(b)
}

/// Quoted strings of a VDF document, in order, with `\\` and `\"` unescaped.
fn vdf_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut s = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => s.extend(chars.next()),
                '"' => break,
                other => s.push(other),
            }
        }
        out.push(s);
    }
    out
}

/// Library root paths from a libraryfolders.vdf, in file order.
///
/// The file is a nested key/value list; every library block has exactly
/// one `"path" "<root>"` pair, so the value following each `path` key is
/// all that is needed.
pub fn library_paths_from_vdf(text: &str) -> Vec<PathBuf> {
    let strings = vdf_strings(text);
    strings
        .windows(2)
        .filter(|w| w[0].eq_ignore_ascii_case("path") && !w[1].is_empty())
        .map(|w| PathBuf::from(&w[1]))
        .collect()
}

/// All library roots across all detected Steam installs, deduplicated.
pub fn discover_steam_libraries() -> Vec<PathBuf> {
    let mut result: Vec<PathBuf> = Vec::new();
    for install in find_steam_installs() {
        let Ok(text) = fs::read_to_string(install.join(LIBRARYFOLDERS_RELATIVE)) else {
            continue;
        };
        for lib in library_paths_from_vdf(&text) {
            if !result.iter().any(|r| same_path(r, &lib)) {
                result.push(lib);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_library_paths() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"apps"
		{
			"227300"		"12345"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        assert_eq!(
            library_paths_from_vdf(vdf),
            vec![PathBuf::from(r"C:\Program Files (x86)\Steam"), PathBuf::from(r"D:\SteamLibrary")]
        );
    }

    #[test]
    fn expands_known_variables_only() {
        // SAFETY: test-local variable, no other thread reads it.
        unsafe { env::set_var("SCSMM_TEST_VAR", "X:\\home") };
        assert_eq!(expand_env_vars("%SCSMM_TEST_VAR%\\Documents"), "X:\\home\\Documents");
        assert_eq!(expand_env_vars("%NOPE_NOT_SET_1%\\a"), "%NOPE_NOT_SET_1%\\a");
        assert_eq!(expand_env_vars("100% sure"), "100% sure");
    }
}
