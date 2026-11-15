//! Updating the app from its GitHub releases.
//!
//! A release carries one portable zip (`scripts/portable.mjs` builds it)
//! with the program file at its root. An update is:
//!
//! 1. read `releases/latest` of the repository - nothing is sent but the
//!    request itself,
//! 2. download the zip and compare it with the SHA-256 GitHub lists for
//!    the asset,
//! 3. take the program file out of it and put it where the running one
//!    is. A running program cannot be overwritten, but it can be renamed:
//!    the old file is moved aside (`<name>.old`) and removed at the next
//!    start.
//!
//! Only the program file is replaced; the `data` folder beside it is not
//! touched. The HTTP half is behind the `net` cargo feature.

use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{format_err, Result};

macro_rules! repository {
    () => {
        "doveis99/easy-scsmodmanager-rs"
    };
}

pub const LATEST_ENDPOINT: &str = concat!("https://api.github.com/repos/", repository!(), "/releases/latest");
/// Where the releases can be read and downloaded by hand.
pub const RELEASES_PAGE: &str = concat!("https://github.com/", repository!(), "/releases");
/// Downloads are only taken from the repository's own releases.
const DOWNLOAD_PREFIX: &str = concat!("https://github.com/", repository!(), "/releases/download/");

/// The program file inside the portable zip.
pub const PROGRAM_NAME: &str = "easy-scsmodmanager.exe";
/// The zip is a few megabytes; anything far beyond that is not one.
pub const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_PROGRAM_BYTES: u64 = 512 * 1024 * 1024;

/// `1.2.3` or `v1.2.3` as numbers. Anything else - a pre-release suffix,
/// an empty part - is not a version the app updates to.
pub fn parse_version(text: &str) -> Option<Vec<u64>> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let parts: Vec<u64> = text
        .split('.')
        .map(|part| {
            let plain = !part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit());
            plain.then(|| part.parse().ok()).flatten()
        })
        .collect::<Option<_>>()?;
    (parts.len() <= 4).then_some(parts)
}

/// Whether `candidate` is a later version than `current`. A version that
/// cannot be read is never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (parse_version(candidate), parse_version(current)) else {
        return false;
    };
    // 1.2 and 1.2.0 are the same version
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

/// The portable zip of a release.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Package {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex, as GitHub lists it for the asset.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Release {
    /// Without the leading `v`.
    pub version: String,
    pub tag: String,
    /// `None` when the release has no zip this app can install by itself;
    /// the release page is then all there is to offer.
    pub package: Option<Package>,
}

impl Release {
    /// The release's page. The tag is a plain version (see
    /// [`parse_version`]), so the URL holds nothing but digits and dots.
    pub fn page_url(&self) -> String {
        format!("{RELEASES_PAGE}/tag/{}", self.tag)
    }
}

/// The name `scripts/portable.mjs` gives the zip of a version.
pub fn package_name(version: &str) -> String {
    format!("Easy-SCS-Mod-Manager-{version}-portable.zip")
}

/// Read a `releases/latest` response.
///
/// The zip is only accepted with everything needed to check it: a download
/// URL inside this repository's releases, a size and a SHA-256.
pub fn parse_latest(body: &str) -> Result<Release> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format_err(format!("release response: {e}")))?;
    let tag = payload["tag_name"].as_str().unwrap_or_default().trim();
    if parse_version(tag).is_none() {
        return Err(format_err("the latest release has no version tag"));
    }
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let wanted = package_name(version);
    let package = payload["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|asset| asset["name"].as_str() == Some(wanted.as_str()))
        .find_map(|asset| {
            let url = asset["browser_download_url"].as_str()?;
            let size = asset["size"].as_u64()?;
            let sha256 = asset["digest"].as_str()?.strip_prefix("sha256:")?.to_ascii_lowercase();
            let checkable = url.starts_with(DOWNLOAD_PREFIX)
                && (1..=MAX_PACKAGE_BYTES).contains(&size)
                && sha256.len() == 64
                && sha256.bytes().all(|b| b.is_ascii_hexdigit());
            checkable.then(|| Package {
                name: wanted.clone(),
                url: url.to_string(),
                size,
                sha256,
            })
        });
    Ok(Release {
        version: version.to_string(),
        tag: tag.to_string(),
        package,
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The downloaded zip must be exactly the file the release lists.
pub fn verify_package(bytes: &[u8], package: &Package) -> Result<()> {
    if bytes.len() as u64 != package.size || sha256_hex(bytes) != package.sha256 {
        return Err(format_err("the download does not match the release's SHA-256"));
    }
    Ok(())
}

/// One file from the root of the zip, or `None` when it has none of that
/// name. The file is read into memory, so its size is capped.
pub fn package_file(package: &[u8], name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(package))?;
    let mut entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    // the size in the zip's directory is only a claim; count what arrives
    (&mut entry).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(format_err(format!("{name} in the update is too large")));
    }
    Ok(Some(bytes))
}

/// The new program file out of the zip.
pub fn program_from_package(package: &[u8]) -> Result<Vec<u8>> {
    let program = package_file(package, PROGRAM_NAME, MAX_PROGRAM_BYTES)?
        .ok_or_else(|| format_err(format!("the update holds no {PROGRAM_NAME}")))?;
    // a Windows executable starts with "MZ"
    if !program.starts_with(b"MZ") {
        return Err(format_err(format!("{PROGRAM_NAME} in the update is not a program")));
    }
    Ok(program)
}

/// `<file name><suffix>` in the same folder.
fn beside(program: &Path, suffix: &str) -> PathBuf {
    let mut name = program.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    program.with_file_name(name)
}

fn remove_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Put `new_program` where `program` is. The file that was there is kept
/// as `<name>.old` - it may be the running program, which can be renamed
/// but not removed. When anything fails the old file is back in its place.
pub fn replace_program(program: &Path, new_program: &[u8]) -> Result<()> {
    let staged = beside(program, ".new");
    let old = beside(program, ".old");
    let swap = || -> io::Result<()> {
        fs::write(&staged, new_program)?;
        // the executable bit, where there is one. Not on Windows: a
        // read-only flag would come along and keep the file from being
        // removed at the next update.
        #[cfg(unix)]
        fs::set_permissions(&staged, fs::metadata(program)?.permissions())?;
        remove_if_there(&old)?;
        fs::rename(program, &old)?;
        if let Err(e) = fs::rename(&staged, program) {
            let _ = fs::rename(&old, program);
            return Err(e);
        }
        Ok(())
    };
    let done = swap();
    if done.is_err() {
        let _ = fs::remove_file(&staged);
    }
    Ok(done?)
}

/// Remove what an update left beside the program: the previous program
/// file and a download that was never put in place. Returns whether
/// nothing is left - the previous file stays locked until the process
/// that ran from it has ended.
pub fn remove_leftovers(program: &Path) -> bool {
    // both are tried, also when the first cannot be removed yet
    let old_gone = remove_if_there(&beside(program, ".old")).is_ok();
    let staged_gone = remove_if_there(&beside(program, ".new")).is_ok();
    old_gone && staged_gone
}

#[cfg(feature = "net")]
mod net {
    use super::*;
    use std::time::Duration;

    const USER_AGENT: &str = "easy-scsmodmanager-rs";

    fn request_err(what: &str, e: ureq::Error) -> crate::Error {
        match e {
            ureq::Error::Status(code, _) => format_err(format!("{what}: HTTP {code}")),
            ureq::Error::Transport(t) => format_err(format!("{what}: {t}")),
        }
    }

    /// Ask GitHub for the latest release.
    pub fn fetch_latest() -> Result<Release> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build();
        let body = agent
            .get(LATEST_ENDPOINT)
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| request_err("release lookup failed", e))?
            .into_string()?;
        parse_latest(&body)
    }

    /// Download the zip of a release and check it against the release's
    /// SHA-256. `progress` gets the bytes received so far; `Ok(None)` means
    /// `cancelled` said to stop.
    pub fn download_package(
        package: &Package,
        progress: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<Vec<u8>>> {
        if !package.url.starts_with(DOWNLOAD_PREFIX) {
            return Err(format_err("refusing a download from outside the releases"));
        }
        // no overall timeout: a slow line is fine as long as bytes arrive
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build();
        let mut reader = agent
            .get(&package.url)
            .call()
            .map_err(|e| request_err("download failed", e))?
            .into_reader()
            .take(package.size + 1);
        let mut bytes = Vec::with_capacity(package.size as usize);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if cancelled() {
                return Ok(None);
            }
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            progress(bytes.len() as u64);
        }
        verify_package(&bytes, package)?;
        Ok(Some(bytes))
    }
}

#[cfg(feature = "net")]
pub use net::{download_package, fetch_latest};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SHA: &str = "ff78f150483a087a1e34f3f0811e58da007c04cc370a7b9c2c9f40a7ccfded9f";

    fn latest(tag: &str, assets: &str) -> String {
        format!(r#"{{"tag_name":"{tag}","name":"x","body":"notes","assets":[{assets}]}}"#)
    }

    fn asset(name: &str, url: &str, digest: &str) -> String {
        format!(r#"{{"name":"{name}","size":4626128,"digest":"{digest}","browser_download_url":"{url}"}}"#)
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(parse_version("v0.3.0"), Some(vec![0, 3, 0]));
        assert_eq!(parse_version(" 1.10 "), Some(vec![1, 10]));
        for bad in ["", "v", "1..2", "1.2-beta", "1.2.3.4.5", "latest", "1.x", "99999999999.0"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!(is_newer("0.4.0", "0.3.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(is_newer("0.3.0.1", "0.3"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.3", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        // what cannot be read is never an update
        assert!(!is_newer("0.4.0-rc1", "0.3.0"));
        assert!(!is_newer("0.4.0", "dev"));
    }

    #[test]
    fn the_latest_release_names_its_zip() {
        let url = "https://github.com/doveis99/easy-scsmodmanager-rs/releases/download/v0.3.0/Easy-SCS-Mod-Manager-0.3.0-portable.zip";
        let name = "Easy-SCS-Mod-Manager-0.3.0-portable.zip";
        let digest = format!("sha256:{}", SHA.to_uppercase());
        let body = latest(
            "v0.3.0",
            &[asset("notes.txt", url, &digest), asset(name, url, &digest)].join(","),
        );
        let release = parse_latest(&body).unwrap();
        assert_eq!(release.version, "0.3.0");
        assert_eq!(release.tag, "v0.3.0");
        assert_eq!(
            release.page_url(),
            "https://github.com/doveis99/easy-scsmodmanager-rs/releases/tag/v0.3.0"
        );
        let package = release.package.unwrap();
        assert_eq!((package.name.as_str(), package.url.as_str(), package.size), (name, url, 4_626_128));
        assert_eq!(package.sha256, SHA);
    }

    #[test]
    fn a_zip_that_cannot_be_checked_is_not_offered() {
        let name = "Easy-SCS-Mod-Manager-0.4.0-portable.zip";
        let good_url = "https://github.com/doveis99/easy-scsmodmanager-rs/releases/download/v0.4.0/x.zip";
        let digest = format!("sha256:{SHA}");
        let package = |assets: &str| parse_latest(&latest("v0.4.0", assets)).unwrap().package;

        assert!(package(&asset(name, good_url, &digest)).is_some());
        // from somewhere else, without a digest, with another kind of digest
        assert!(package(&asset(name, "https://example.com/x.zip", &digest)).is_none());
        assert!(package(&asset(name, "http://github.com/doveis99/easy-scsmodmanager-rs/releases/download/x.zip", &digest)).is_none());
        assert!(package(&asset(name, good_url, "")).is_none());
        assert!(package(&asset(name, good_url, "sha512:abcd")).is_none());
        assert!(package(&asset(name, good_url, "sha256:zz")).is_none());
        // the zip of another version, or no assets at all
        assert!(package(&asset("Easy-SCS-Mod-Manager-0.3.0-portable.zip", good_url, &digest)).is_none());
        assert!(package("").is_none());
        assert!(parse_latest(r#"{"tag_name":"v0.4.0"}"#).unwrap().package.is_none());

        // no release, or a tag that is not a version
        assert!(parse_latest("not json").is_err());
        assert!(parse_latest(r#"{"message":"Not Found"}"#).is_err());
        assert!(parse_latest(&latest("nightly", "")).is_err());
        assert!(parse_latest(&latest("v1.0/../..", "")).is_err());
    }

    #[test]
    fn the_download_must_match_the_release() {
        let bytes = b"portable zip";
        let package = Package {
            name: "x.zip".into(),
            url: String::new(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        };
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        verify_package(bytes, &package).unwrap();
        assert!(verify_package(b"portable zap", &package).is_err());
        assert!(verify_package(b"portable zip ", &package).is_err());
    }

    #[test]
    fn the_program_is_taken_from_the_root_of_the_zip() {
        let zip = zip_of(&[("README.txt", b"read me"), (PROGRAM_NAME, b"MZ\x90\x00program")]);
        assert_eq!(program_from_package(&zip).unwrap(), b"MZ\x90\x00program");
        assert_eq!(package_file(&zip, "README.txt", 100).unwrap().as_deref(), Some(&b"read me"[..]));
        assert_eq!(package_file(&zip, "LICENSE.txt", 100).unwrap(), None);
        assert!(package_file(&zip, "README.txt", 3).is_err());

        // no program, a program in a folder, something that is not one
        assert!(program_from_package(&zip_of(&[("README.txt", b"x")])).is_err());
        assert!(program_from_package(&zip_of(&[("sub/easy-scsmodmanager.exe", b"MZ")])).is_err());
        assert!(program_from_package(&zip_of(&[(PROGRAM_NAME, b"<html>")])).is_err());
        assert!(program_from_package(b"not a zip").is_err());
    }

    #[test]
    fn the_program_is_swapped_and_the_old_one_kept_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("app.exe");
        let old = tmp.path().join("app.exe.old");
        let staged = tmp.path().join("app.exe.new");
        fs::write(&program, b"version 1").unwrap();

        replace_program(&program, b"version 2").unwrap();
        assert_eq!(fs::read(&program).unwrap(), b"version 2");
        assert_eq!(fs::read(&old).unwrap(), b"version 1");
        assert!(!staged.exists());

        // a second update replaces the file kept from the first
        replace_program(&program, b"version 3").unwrap();
        assert_eq!(fs::read(&program).unwrap(), b"version 3");
        assert_eq!(fs::read(&old).unwrap(), b"version 2");

        // the next start clears up, also a download that was left behind
        fs::write(&staged, b"half").unwrap();
        assert!(remove_leftovers(&program));
        assert!(!old.exists() && !staged.exists());
        assert!(remove_leftovers(&program));
        assert_eq!(fs::read(&program).unwrap(), b"version 3");
    }

    #[test]
    fn a_failed_swap_leaves_the_program_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("app.exe");
        // nothing to replace: the staged file must not stay behind
        assert!(replace_program(&program, b"new").is_err());
        assert!(!program.exists());
        assert!(!tmp.path().join("app.exe.new").exists());

        // the place of the old file is taken by something that cannot be removed
        fs::write(&program, b"version 1").unwrap();
        fs::create_dir(tmp.path().join("app.exe.old")).unwrap();
        assert!(replace_program(&program, b"version 2").is_err());
        assert_eq!(fs::read(&program).unwrap(), b"version 1");
        assert!(!tmp.path().join("app.exe.new").exists());
    }
}
