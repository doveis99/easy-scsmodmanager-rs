//! Reader for HashFS (`SCS#`) archives, v1 and v2.
//!
//! Entries are addressed by the CityHash64 of their path; names only exist
//! inside directory-listing entries, so listing the archive means walking
//! those from the root.
//!
//! v2 packed texture entries (.tobj/.dds, GDeflate) are recognised but not
//! decoded yet: `has` reports them as absent and `read_bytes` refuses them,
//! exactly like the Python reader's plain-file interface.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::cityhash::hash_path;
use super::{inflate_zlib, ModSource};
use crate::{format_err, Error, Result};

const MAGIC: u32 = 0x2353_4353; // "SCS#"
const CITY_METHOD: &[u8] = b"CITY";

/// Standard SCS top-level directories, used to seed the walk for archives
/// that omit the root listing.
const KNOWN_TOP_DIRS: &[&str] = &[
    "automat", "def", "dlc", "effect", "font", "locale", "map", "material", "model", "model2",
    "prefab", "prefab2", "sound", "system", "ui", "unit", "vehicle",
];
const KNOWN_ROOT_FILES: &[&str] = &[
    "manifest.sii",
    "version.sii",
    "version.txt",
    "description.txt",
    "mod_description.txt",
    "icon.jpg",
    "mod_workshop_icon.jpg",
];

const V2_BLOCK_SIZE: u64 = 16;
const V2_META_BLOCK: usize = 4;
const CHUNK_IMAGE: u8 = 1;
const CHUNK_PLAIN: u8 = 128;
const CHUNK_DIRECTORY: u8 = 129;

#[derive(Debug, Clone, Copy)]
struct Entry {
    offset: u64,
    size: u32,
    compressed_size: u32,
    is_directory: bool,
    is_compressed: bool,
    is_image: bool,
}

pub struct HashFsReader {
    file: File,
    version: u16,
    salt: u16,
    entries: HashMap<u64, Entry>,
}

fn u16_at(b: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([b[pos], b[pos + 1]])
}
fn u32_at(b: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(b[pos..pos + 4].try_into().expect("4 bytes"))
}
fn u64_at(b: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(b[pos..pos + 8].try_into().expect("8 bytes"))
}

fn read_at(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    // a corrupt table can declare absurd sizes; never allocate past the file
    let file_len = file.metadata()?.len();
    if offset.saturating_add(len as u64) > file_len {
        return Err(format_err("HashFS entry points past the end of the file"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

impl HashFsReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut head = [0u8; 12];
        file.read_exact(&mut head)
            .map_err(|_| format_err("not a HashFS archive"))?;
        if u32_at(&head, 0) != MAGIC {
            return Err(format_err("not a HashFS archive"));
        }
        let version = u16_at(&head, 4);
        let salt = u16_at(&head, 6);
        if &head[8..12] != CITY_METHOD {
            return Err(format_err("unsupported hash method"));
        }
        let entries = match version {
            1 => Self::parse_v1(&mut file)?,
            2 => Self::parse_v2(&mut file)?,
            other => return Err(format_err(format!("Unsupported HashFS version {other}"))),
        };
        Ok(Self {
            file,
            version,
            salt,
            entries,
        })
    }

    fn parse_v1(file: &mut File) -> Result<HashMap<u64, Entry>> {
        let header = read_at(file, 0, 24)?;
        let num_entries = u32_at(&header, 12) as usize;
        let start_offset = u64_at(&header, 16);
        let table = read_at(file, start_offset, num_entries * 32)?;
        let mut entries = HashMap::with_capacity(num_entries);
        for row in table.chunks_exact(32) {
            let flags = u32_at(row, 16);
            entries.entry(u64_at(row, 0)).or_insert(Entry {
                offset: u64_at(row, 8),
                size: u32_at(row, 24),
                compressed_size: u32_at(row, 28),
                is_directory: flags & 0x1 != 0,
                is_compressed: flags & 0x2 != 0,
                is_image: false,
            });
        }
        Ok(entries)
    }

    fn parse_v2(file: &mut File) -> Result<HashMap<u64, Entry>> {
        let header = read_at(file, 0, 49)?;
        let entry_table_len = u32_at(&header, 16) as usize;
        let metadata_table_len = u32_at(&header, 24) as usize;
        let entry_table_start = u64_at(&header, 28);
        let metadata_table_start = u64_at(&header, 36);

        let entry_table = inflate_zlib(&read_at(file, entry_table_start, entry_table_len)?)?;
        let meta = inflate_zlib(&read_at(file, metadata_table_start, metadata_table_len)?)?;

        let mut entries = HashMap::with_capacity(entry_table.len() / 16);
        for row in entry_table.chunks_exact(16) {
            let meta_index = u32_at(row, 8) as usize;
            let meta_count = u16_at(row, 12) as usize;
            if let Some(entry) = decode_v2_metadata(&meta, meta_index, meta_count) {
                entries.entry(u64_at(row, 0)).or_insert(entry);
            }
        }
        Ok(entries)
    }

    fn entry(&self, path: &str) -> Option<Entry> {
        self.entries.get(&hash_path(path, self.salt)).copied()
    }

    fn content(&mut self, entry: Entry) -> Result<Vec<u8>> {
        if entry.is_compressed {
            let raw = read_at(&mut self.file, entry.offset, entry.compressed_size as usize)?;
            Ok(inflate_zlib(&raw)?)
        } else {
            read_at(&mut self.file, entry.offset, entry.size as usize)
        }
    }

    /// `(subdirectories, files)` named in a directory listing entry.
    pub fn list_dir(&mut self, path: &str) -> Result<(Vec<String>, Vec<String>)> {
        let entry = self
            .entry(path)
            .filter(|e| e.is_directory)
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        let blob = self.content(entry)?;
        Ok(if self.version == 1 {
            parse_listing_v1(&blob)
        } else {
            parse_listing_v2(&blob)
        })
    }

    /// Every file path in the archive (with a leading `/`), walked from the
    /// directory listings. Unreadable listings are skipped, not fatal.
    pub fn iter_files(&mut self) -> Vec<String> {
        let mut found: HashSet<String> = HashSet::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = vec!["/".to_string()];
        stack.extend(KNOWN_TOP_DIRS.iter().map(|n| format!("/{n}")));
        while let Some(next) = stack.pop() {
            let canon = format!("/{}", next.trim_matches('/'));
            if !seen.insert(canon.clone()) {
                continue;
            }
            let Ok((subdirs, files)) = self.list_dir(&canon) else {
                continue;
            };
            let base = if canon == "/" { "" } else { canon.as_str() };
            found.extend(files.iter().map(|name| format!("{base}/{name}")));
            stack.extend(subdirs.iter().map(|name| format!("{base}/{name}")));
        }
        for name in KNOWN_ROOT_FILES {
            if self.has(name) {
                found.insert(format!("/{name}"));
            }
        }
        let mut out: Vec<String> = found.into_iter().collect();
        out.sort();
        out
    }
}

impl ModSource for HashFsReader {
    fn has(&mut self, path: &str) -> bool {
        self.entry(path)
            .is_some_and(|e| !e.is_directory && !e.is_image)
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let entry = self
            .entry(path)
            .filter(|e| !e.is_directory)
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        if entry.is_image {
            return Err(format_err(format!(
                "packed texture entry not supported: {path}"
            )));
        }
        self.content(entry)
    }

    fn list_files(&mut self) -> Vec<String> {
        self.iter_files()
    }
}

fn parse_listing_v1(blob: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut subdirs = Vec::new();
    let mut files = Vec::new();
    let text = String::from_utf8_lossy(blob);
    for line in text.split(['\n', '\r']) {
        if line.is_empty() {
            continue;
        }
        match line.strip_prefix('*') {
            Some(dir) => subdirs.push(dir.to_string()),
            None => files.push(line.to_string()),
        }
    }
    (subdirs, files)
}

fn parse_listing_v2(blob: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut subdirs = Vec::new();
    let mut files = Vec::new();
    if blob.len() < 4 {
        return (subdirs, files);
    }
    let count = u32_at(blob, 0) as usize;
    let Some(lengths) = blob.get(4..4 + count) else {
        return (subdirs, files);
    };
    let mut pos = 4 + count;
    for &length in lengths {
        let Some(raw) = blob.get(pos..pos + length as usize) else {
            break;
        };
        pos += length as usize;
        let name = String::from_utf8_lossy(raw);
        match name.strip_prefix('/') {
            Some(dir) => subdirs.push(dir.to_string()),
            None => files.push(name.into_owned()),
        }
    }
    (subdirs, files)
}

fn decode_v2_metadata(meta: &[u8], meta_index: usize, meta_count: usize) -> Option<Entry> {
    let pos = meta_index * V2_META_BLOCK;
    // first chunk header: 3 index bytes + 1 type byte
    let chunk_type = *meta.get(pos + 3)?;
    let body = pos + meta_count * 4;
    match chunk_type {
        CHUNK_PLAIN | CHUNK_DIRECTORY => {
            let (offset, size, csize, compressed) = decode_main_metadata(meta, body)?;
            Some(Entry {
                offset,
                size,
                compressed_size: csize,
                is_directory: chunk_type == CHUNK_DIRECTORY,
                is_compressed: compressed,
                is_image: false,
            })
        }
        CHUNK_IMAGE => {
            // 12 bytes of packed tobj/dds metadata precede the main metadata
            let (offset, size, csize, compressed) = decode_main_metadata(meta, body + 12)?;
            Some(Entry {
                offset,
                size,
                compressed_size: csize,
                is_directory: false,
                is_compressed: compressed,
                is_image: true,
            })
        }
        _ => None, // unimplemented chunk type - skip
    }
}

fn decode_main_metadata(meta: &[u8], pos: usize) -> Option<(u64, u32, u32, bool)> {
    let m = meta.get(pos..pos + 16)?;
    let csize = u32_at(m, 0) & 0x0FFF_FFFF;
    let compressed = m[3] & 0x10 != 0; // flags nibble, bit 4
    let size = u32_at(m, 4) & 0x0FFF_FFFF;
    let offset_block = u32_at(m, 12) as u64;
    Some((offset_block * V2_BLOCK_SIZE, size, csize, compressed))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Builds tiny HashFS v1 archives so the reader can be tested without
    //! shipping game data.
    use super::super::cityhash::hash_path;
    use std::io::Write;

    pub fn zlib(data: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    }

    /// `files`: (path, content, compress). Directory listings are derived.
    pub fn build_v1(salt: u16, files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        use std::collections::BTreeMap;
        let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        dirs.entry(String::new()).or_default();
        for (path, _, _) in files {
            let parts: Vec<&str> = path.split('/').collect();
            let mut parent = String::new();
            for (i, part) in parts.iter().enumerate() {
                let is_file = i == parts.len() - 1;
                let line = if is_file {
                    part.to_string()
                } else {
                    format!("*{part}")
                };
                let listing = dirs.entry(parent.clone()).or_default();
                if !listing.contains(&line) {
                    listing.push(line);
                }
                if !is_file {
                    parent = if parent.is_empty() {
                        part.to_string()
                    } else {
                        format!("{parent}/{part}")
                    };
                    dirs.entry(parent.clone()).or_default();
                }
            }
        }

        // (hash, flags, stored bytes, real size)
        let mut blobs: Vec<(u64, u32, Vec<u8>, u32)> = Vec::new();
        for (path, content, compress) in files {
            let stored = if *compress {
                zlib(content)
            } else {
                content.to_vec()
            };
            let flags = if *compress { 0x2 } else { 0 };
            blobs.push((hash_path(path, salt), flags, stored, content.len() as u32));
        }
        for (dir, lines) in &dirs {
            let text = lines.join("\n").into_bytes();
            blobs.push((hash_path(dir, salt), 0x1, text.clone(), text.len() as u32));
        }

        let mut out = Vec::new();
        out.extend_from_slice(&super::MAGIC.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&salt.to_le_bytes());
        out.extend_from_slice(b"CITY");
        out.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
        let table_offset_pos = out.len();
        out.extend_from_slice(&0u64.to_le_bytes());

        let mut rows = Vec::new();
        for (hash, flags, stored, size) in &blobs {
            let offset = out.len() as u64;
            out.extend_from_slice(stored);
            rows.push((*hash, offset, *flags, *size, stored.len() as u32));
        }
        let table_offset = out.len() as u64;
        out[table_offset_pos..table_offset_pos + 8].copy_from_slice(&table_offset.to_le_bytes());
        for (hash, offset, flags, size, csize) in rows {
            out.extend_from_slice(&hash.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // crc, unused
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&csize.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_walks_a_v1_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mod.scs");
        let manifest = b"SiiNunit{mod_package:.p{display_name:\"X\"}}";
        let blob = testutil::build_v1(
            3,
            &[
                ("manifest.sii", manifest, true),
                ("def/vehicle/physics.sii", b"p", false),
                ("map/europe.mbd", b"m", false),
            ],
        );
        std::fs::write(&path, blob).unwrap();

        let mut r = HashFsReader::open(&path).unwrap();
        assert!(r.has("manifest.sii"));
        assert!(r.has("/manifest.sii"));
        assert!(!r.has("def")); // directories are not files
        assert!(!r.has("nope.txt"));
        assert_eq!(r.read_bytes("manifest.sii").unwrap(), manifest);
        assert_eq!(
            r.iter_files(),
            vec!["/def/vehicle/physics.sii", "/manifest.sii", "/map/europe.mbd"]
        );
    }

    #[test]
    fn rejects_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.scs");
        std::fs::write(&path, b"PK\x03\x04 definitely a zip").unwrap();
        assert!(HashFsReader::open(&path).is_err());
    }

    #[test]
    fn v2_main_metadata_bit_layout() {
        let mut m = [0u8; 16];
        m[0..4].copy_from_slice(&(0x1000_0000u32 | 300).to_le_bytes()); // compressed, csize 300
        m[4..8].copy_from_slice(&1000u32.to_le_bytes());
        m[12..16].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(decode_main_metadata(&m, 0), Some((80, 1000, 300, true)));
        assert_eq!(decode_main_metadata(&m, 4), None); // out of range, no panic
    }

    #[test]
    fn v2_listing() {
        let mut blob = 3u32.to_le_bytes().to_vec();
        blob.extend_from_slice(&[4, 12, 2]);
        blob.extend_from_slice(b"/defmanifest.siiab");
        let (dirs, files) = parse_listing_v2(&blob);
        assert_eq!(dirs, vec!["def"]);
        assert_eq!(files, vec!["manifest.sii", "ab"]);
    }
}
