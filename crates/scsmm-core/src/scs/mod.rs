//! Readers for the mod-container layouts SCS games accept.
//!
//! A mod payload is a `.scs` archive (ZIP, HashFS v1/v2 or the rare AEM!
//! layout), a `.zip` with the same internal tree, or an unpacked directory.
//! Everything is routed through [`ModSource`] so the scanner stays
//! format-agnostic.

pub mod aem;
pub mod cityhash;
pub mod dir;
pub mod gdeflate;
pub mod hashfs;
pub mod tobj_dds;
pub mod zip;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Serialize;

use crate::Result;

/// Minimal file-system-like interface the scanner needs.
pub trait ModSource {
    fn has(&mut self, path: &str) -> bool;
    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>>;
    /// Every file path in the container (directory entries may be included
    /// with a trailing `/`, as ZIP archives list them).
    fn list_files(&mut self) -> Vec<String>;

    /// CityHash64 of the path of every file in the container, for sources
    /// that address files by such a hash (HashFS). Unlike [`list_files`]
    /// this is complete even when the archive has no directory listings to
    /// name its files. `None` when the source has no comparable hashes.
    ///
    /// [`list_files`]: ModSource::list_files
    fn path_hashes(&mut self) -> Option<Vec<u64>> {
        None
    }

    fn read_text(&mut self, path: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.read_bytes(path)?).into_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScsFormat {
    Zip,
    HashfsV1,
    HashfsV2,
    Aem,
    Directory,
    Unknown,
}

const ZIP_LOCAL_FILE_HEADER: &[u8] = b"PK\x03\x04";
const HASHFS_MAGIC: &[u8] = b"SCS#";
const AEM_MAGIC: &[u8] = b"AEM!";

/// Sniff the container format from the first bytes of a file.
pub fn detect_format(path: &Path) -> Result<ScsFormat> {
    let mut head = [0u8; 6];
    let mut f = File::open(path)?;
    let mut filled = 0;
    while filled < head.len() {
        match f.read(&mut head[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let head = &head[..filled];
    if head.starts_with(ZIP_LOCAL_FILE_HEADER) {
        return Ok(ScsFormat::Zip);
    }
    if head.starts_with(HASHFS_MAGIC) && head.len() >= 6 {
        match u16::from_le_bytes([head[4], head[5]]) {
            1 => return Ok(ScsFormat::HashfsV1),
            2 => return Ok(ScsFormat::HashfsV2),
            _ => {}
        }
    }
    if head.starts_with(AEM_MAGIC) {
        return Ok(ScsFormat::Aem);
    }
    Ok(ScsFormat::Unknown)
}

pub(crate) fn inflate_raw(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

pub(crate) fn inflate_zlib(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}
