//! ZIP-based `.scs` / `.zip` readers.
//!
//! [`ZipSource`] is the normal path (central directory via the `zip`
//! crate). [`RawZipSource`] is the fallback for archives that carry a set
//! encryption flag and an odd method label while the payload is ordinary
//! raw-deflate: it scans the local file headers itself and inflates each
//! entry directly.

use std::collections::hash_map::{Entry, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use memchr::memmem;
use memmap2::Mmap;

use super::{inflate_raw, ModSource};
use crate::{format_err, Error, Result};

pub struct ZipSource {
    archive: zip::ZipArchive<File>,
}

impl ZipSource {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self { archive: zip::ZipArchive::new(File::open(path)?)? })
    }
}

impl ModSource for ZipSource {
    fn has(&mut self, path: &str) -> bool {
        self.archive.index_for_name(path).is_some()
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let mut entry = self.archive.by_name(path)?;
        let mut out = Vec::with_capacity(entry.size().min(1 << 24) as usize);
        entry.read_to_end(&mut out)?;
        Ok(out)
    }

    fn list_files(&mut self) -> Vec<String> {
        self.archive.file_names().map(str::to_string).collect()
    }
}

/// True when the file ends in a usable ZIP central directory - the
/// equivalent of Python's `zipfile.is_zipfile` for header-less `.zip` mods.
pub fn is_zipfile(path: &Path) -> bool {
    File::open(path).ok().and_then(|f| zip::ZipArchive::new(f).ok()).is_some()
}

const ZIP_LOCAL: &[u8] = b"PK\x03\x04";
const LOCAL_HEADER_FIXED: usize = 30;
const STORED: u16 = 0;

#[derive(Clone, Copy)]
struct RawEntry {
    data_offset: usize,
    comp_size: usize,
    uncomp_size: usize,
    method: u16,
}

pub struct RawZipSource {
    map: Mmap,
    index: HashMap<String, RawEntry>,
    order: Vec<String>,
}

impl RawZipSource {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the mapping is read-only and only lives for the scan of
        // one mod; a concurrent truncation would at worst fail that scan.
        let map = unsafe { Mmap::map(&file)? };
        let (index, order) = Self::scan_local_headers(&map);
        Ok(Self { map, index, order })
    }

    fn scan_local_headers(raw: &[u8]) -> (HashMap<String, RawEntry>, Vec<String>) {
        let mut index = HashMap::new();
        let mut order = Vec::new();
        for i in memmem::find_iter(raw, ZIP_LOCAL) {
            let Some(h) = raw.get(i..i + LOCAL_HEADER_FIXED) else {
                break;
            };
            let method = u16::from_le_bytes([h[8], h[9]]);
            let comp = u32::from_le_bytes(h[18..22].try_into().expect("4 bytes")) as usize;
            let uncomp = u32::from_le_bytes(h[22..26].try_into().expect("4 bytes")) as usize;
            let name_len = u16::from_le_bytes([h[26], h[27]]) as usize;
            let extra_len = u16::from_le_bytes([h[28], h[29]]) as usize;
            let name_start = i + LOCAL_HEADER_FIXED;
            let Some(name) = raw.get(name_start..name_start + name_len) else {
                continue;
            };
            let key = String::from_utf8_lossy(name).into_owned();
            // first header wins; later duplicates are usually data descriptors
            if let Entry::Vacant(slot) = index.entry(key) {
                order.push(slot.key().clone());
                slot.insert(RawEntry {
                    data_offset: name_start + name_len + extra_len,
                    comp_size: comp,
                    uncomp_size: uncomp,
                    method,
                });
            }
        }
        (index, order)
    }
}

impl ModSource for RawZipSource {
    fn has(&mut self, path: &str) -> bool {
        self.index.contains_key(path)
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let e = *self.index.get(path).ok_or_else(|| Error::NotFound(path.to_string()))?;
        if e.comp_size == 0 {
            // streamed entry (data descriptor) - cannot be sliced reliably
            return Err(format_err(format!("no usable compressed size for {path}")));
        }
        let blob = self
            .map
            .get(e.data_offset..e.data_offset + e.comp_size)
            .ok_or_else(|| format_err(format!("entry runs past the end of the file: {path}")))?;
        let stored = |blob: &[u8]| {
            if e.uncomp_size != 0 {
                blob[..e.uncomp_size.min(blob.len())].to_vec()
            } else {
                blob.to_vec()
            }
        };
        if e.method == STORED {
            return Ok(stored(blob));
        }
        match inflate_raw(blob) {
            Ok(out) => Ok(out),
            // genuinely encrypted, or actually stored but mislabelled
            Err(_) if e.comp_size == e.uncomp_size => Ok(stored(blob)),
            Err(err) => Err(err.into()),
        }
    }

    fn list_files(&mut self) -> Vec<String> {
        self.order.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn zip_source_reads_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.scs");
        make_zip(&path, &[("manifest.sii", b"hello"), ("def/a.sii", b"x")]);
        assert!(is_zipfile(&path));
        let mut z = ZipSource::open(&path).unwrap();
        assert!(z.has("manifest.sii"));
        assert!(!z.has("nope"));
        assert_eq!(z.read_bytes("manifest.sii").unwrap(), b"hello");
        assert_eq!(z.list_files().len(), 2);
    }

    #[test]
    fn raw_source_survives_a_fake_encryption_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.scs");
        let body = b"SiiNunit { mod_package: .p { display_name: \"Locked\" } }".repeat(4);
        make_zip(&path, &[("manifest.sii", &body)]);

        // set the "encrypted" general-purpose bit in every header, the way
        // "locked" mods do; the payload itself stays plain raw-deflate
        let mut raw = std::fs::read(&path).unwrap();
        let locals: Vec<usize> = memmem::find_iter(&raw, ZIP_LOCAL).collect();
        for i in locals {
            raw[i + 6] |= 1;
        }
        let centrals: Vec<usize> = memmem::find_iter(&raw, b"PK\x01\x02").collect();
        for i in centrals {
            raw[i + 8] |= 1;
        }
        std::fs::write(&path, raw).unwrap();

        let mut normal = ZipSource::open(&path).unwrap();
        assert!(normal.read_bytes("manifest.sii").is_err());

        let mut r = RawZipSource::open(&path).unwrap();
        assert!(r.has("manifest.sii"));
        assert_eq!(r.read_bytes("manifest.sii").unwrap(), body);
        assert_eq!(r.list_files(), vec!["manifest.sii"]);
    }
}
