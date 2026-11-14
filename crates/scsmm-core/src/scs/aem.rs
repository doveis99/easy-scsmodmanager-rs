//! Reads the `AEM!` .scs container layout.
//!
//! A sequential format generic parsers miss (no `SCS#`, no ZIP header).
//! Repeated to EOF: `[AEM!][zero padding][u32 name_len LE][name][content]`.
//! `AEM!` is a separator, so an entry's content runs to the next valid
//! header. Images are stored verbatim; .sii files are raw-deflate.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use memchr::memmem;
use memmap2::Mmap;

use super::{inflate_raw, ModSource};
use crate::{Error, Result};

const AEM_MAGIC: &[u8] = b"AEM!";
const MAX_NAME_LEN: usize = 512;

pub struct AemSource {
    map: Mmap,
    index: HashMap<String, (usize, usize)>,
    order: Vec<String>,
}

impl AemSource {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: read-only mapping that only lives for the scan of one mod.
        let map = unsafe { Mmap::map(&file)? };
        let (index, order) = build_index(&map);
        Ok(Self { map, index, order })
    }
}

fn build_index(raw: &[u8]) -> (HashMap<String, (usize, usize)>, Vec<String>) {
    // (marker position, name, content start)
    let headers: Vec<(usize, String, usize)> = memmem::find_iter(raw, AEM_MAGIC)
        .filter_map(|i| parse_header(raw, i).map(|(name, start)| (i, name, start)))
        .collect();
    let mut index = HashMap::new();
    let mut order = Vec::new();
    for (idx, (_, name, start)) in headers.iter().enumerate() {
        let end = headers.get(idx + 1).map_or(raw.len(), |h| h.0);
        if !index.contains_key(name) {
            order.push(name.clone());
            index.insert(name.clone(), (*start, end.max(*start)));
        }
    }
    (index, order)
}

fn parse_header(raw: &[u8], mark: usize) -> Option<(String, usize)> {
    let mut j = mark + 4;
    while j < raw.len() && raw[j] == 0 {
        j += 1; // skip the reserved zero pad
    }
    let len_bytes = raw.get(j..j + 4)?;
    let name_len = u32::from_le_bytes(len_bytes.try_into().ok()?) as usize;
    if !(1..=MAX_NAME_LEN).contains(&name_len) {
        return None;
    }
    let name = raw.get(j + 4..j + 4 + name_len)?;
    if !name.iter().all(|b| (0x20..0x7f).contains(b)) {
        return None; // printable ASCII only
    }
    Some((String::from_utf8_lossy(name).into_owned(), j + 4 + name_len))
}

impl ModSource for AemSource {
    fn has(&mut self, path: &str) -> bool {
        self.index.contains_key(path)
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let (start, end) = *self
            .index
            .get(path)
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        let content = &self.map[start..end];
        match inflate_raw(content) {
            Ok(out) if !out.is_empty() => Ok(out),
            _ => Ok(content.to_vec()),
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

    fn entry(name: &str, content: &[u8]) -> Vec<u8> {
        let mut out = AEM_MAGIC.to_vec();
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(content);
        out
    }

    #[test]
    fn reads_deflated_and_stored_entries() {
        let manifest = b"SiiNunit { mod_package: .p { display_name: \"A\" } }";
        let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(manifest).unwrap();
        let deflated = z.finish().unwrap();

        let mut blob = entry("manifest.sii", &deflated);
        blob.extend(entry("icon.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.scs");
        std::fs::write(&path, blob).unwrap();

        let mut a = AemSource::open(&path).unwrap();
        assert_eq!(a.list_files(), vec!["manifest.sii", "icon.jpg"]);
        assert_eq!(a.read_bytes("manifest.sii").unwrap(), manifest);
        assert_eq!(
            a.read_bytes("icon.jpg").unwrap(),
            vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]
        );
        assert!(a.read_bytes("missing").is_err());
    }
}
