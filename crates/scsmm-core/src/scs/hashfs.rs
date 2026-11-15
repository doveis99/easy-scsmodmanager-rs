//! Reader for HashFS (`SCS#`) archives, v1 and v2.
//!
//! Entries are addressed by the CityHash64 of their path; names only exist
//! inside directory-listing entries, so listing the archive means walking
//! those from the root.
//!
//! v2 packed texture entries (.tobj/.dds, GDeflate) are not standalone
//! files: `has` reports them as absent and `read_bytes` refuses them.
//! `read_image_dds` rebuilds a real .dds from one instead.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::cityhash::hash_path;
use super::tobj_dds::reconstruct_dds;
use super::{inflate_zlib, ModSource};
use crate::{format_err, Error, Result};

const MAGIC: u32 = 0x2353_4353; // "SCS#"
const CITY_METHOD: &[u8] = b"CITY";

/// Standard SCS top-level directories, used to seed the walk for archives
/// that omit the root listing.
const KNOWN_TOP_DIRS: &[&str] = &[
    "automat", "def", "dlc", "effect", "font", "locale", "map", "material", "model", "model2", "prefab", "prefab2",
    "sound", "system", "ui", "unit", "vehicle",
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
    /// 12-byte texture metadata of an image entry.
    tobj_meta: Option<[u8; 12]>,
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
        file.read_exact(&mut head).map_err(|_| format_err("not a HashFS archive"))?;
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
        Ok(Self { file, version, salt, entries })
    }

    fn parse_v1(file: &mut File) -> Result<HashMap<u64, Entry>> {
        let header = read_at(file, 0, 24)?;
        let num_entries = u32_at(&header, 12) as usize;
        let start_offset = u64_at(&header, 16);
        let table = read_at(file, start_offset, num_entries * 32)?;
        let mut entries = HashMap::with_capacity(num_entries);
        for row in table.as_chunks::<32>().0 {
            let flags = u32_at(row, 16);
            entries.entry(u64_at(row, 0)).or_insert(Entry {
                offset: u64_at(row, 8),
                size: u32_at(row, 24),
                compressed_size: u32_at(row, 28),
                is_directory: flags & 0x1 != 0,
                is_compressed: flags & 0x2 != 0,
                is_image: false,
                tobj_meta: None,
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
        for row in entry_table.as_chunks::<16>().0 {
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

    /// `(all entries, directory listings, packed textures)` in the archive.
    /// Entries are stored by hash only, so this is the one count that does
    /// not depend on the directory listings being present.
    pub fn entry_counts(&self) -> (usize, usize, usize) {
        let dirs = self.entries.values().filter(|e| e.is_directory).count();
        let images = self.entries.values().filter(|e| e.is_image).count();
        (self.entries.len(), dirs, images)
    }

    /// The archive's path salt; 0 means paths are hashed as they are.
    pub fn salt(&self) -> u16 {
        self.salt
    }

    /// The hash under which this archive stores `path`.
    pub fn hash_of(&self, path: &str) -> u64 {
        hash_path(path, self.salt)
    }

    /// Hash and stored size of every file entry (directory listings left
    /// out), in hash order. Unlike [`iter_files`] this needs no listing, so
    /// it also covers the files a "locked" archive does not name.
    ///
    /// [`iter_files`]: HashFsReader::iter_files
    pub fn file_entries(&self) -> Vec<(u64, u32)> {
        let mut out: Vec<(u64, u32)> =
            self.entries.iter().filter(|(_, e)| !e.is_directory).map(|(hash, e)| (*hash, e.size)).collect();
        out.sort_unstable();
        out
    }

    /// True when the entry stored under `hash` is a v2 packed texture.
    pub fn is_image_hash(&self, hash: u64) -> bool {
        self.entries.get(&hash).is_some_and(|e| e.is_image)
    }

    /// Content of the file stored under `hash` - for entries whose path is
    /// not known. A packed texture comes back rebuilt as a .dds.
    pub fn read_by_hash(&mut self, hash: u64) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .get(&hash)
            .copied()
            .filter(|e| !e.is_directory)
            .ok_or_else(|| Error::NotFound(format!("{hash:016x}")))?;
        if !entry.is_image {
            return self.content(entry);
        }
        let meta = entry.tobj_meta.ok_or_else(|| format_err(format!("texture metadata missing: {hash:016x}")))?;
        let stored = if entry.is_compressed { entry.compressed_size } else { entry.size };
        let raw = read_at(&mut self.file, entry.offset, stored as usize)?;
        reconstruct_dds(&meta, &raw, entry.is_compressed)
    }

    /// True for a v2 packed texture, keyed by its `.tobj` path.
    pub fn is_image_entry(&self, path: &str) -> bool {
        self.entry(path).is_some_and(|e| e.is_image)
    }

    /// Rebuild a real .dds from a packed v2 texture entry.
    pub fn read_image_dds(&mut self, path: &str) -> Result<Vec<u8>> {
        let entry = self
            .entry(path)
            .filter(|e| e.is_image)
            .ok_or_else(|| format_err(format!("not a packed texture: {path}")))?;
        let meta = entry.tobj_meta.ok_or_else(|| format_err(format!("texture metadata missing: {path}")))?;
        let stored = if entry.is_compressed { entry.compressed_size } else { entry.size };
        let raw = read_at(&mut self.file, entry.offset, stored as usize)?;
        reconstruct_dds(&meta, &raw, entry.is_compressed)
    }

    /// `(subdirectories, files)` named in a directory listing entry.
    pub fn list_dir(&mut self, path: &str) -> Result<(Vec<String>, Vec<String>)> {
        let entry = self.entry(path).filter(|e| e.is_directory).ok_or_else(|| Error::NotFound(path.to_string()))?;
        let blob = self.content(entry)?;
        Ok(if self.version == 1 { parse_listing_v1(&blob) } else { parse_listing_v2(&blob) })
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
        self.entry(path).is_some_and(|e| !e.is_directory && !e.is_image)
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let entry = self.entry(path).filter(|e| !e.is_directory).ok_or_else(|| Error::NotFound(path.to_string()))?;
        if entry.is_image {
            return Err(format_err(format!("packed texture entry not supported: {path}")));
        }
        self.content(entry)
    }

    fn list_files(&mut self) -> Vec<String> {
        self.iter_files()
    }

    fn read_hashed(&mut self, hash: u64, max_bytes: usize) -> Option<Vec<u8>> {
        let entry = self
            .entries
            .get(&hash)
            .copied()
            .filter(|e| !e.is_directory && !e.is_image && e.size as usize <= max_bytes)?;
        self.content(entry).ok()
    }

    fn path_hashes(&mut self) -> Option<Vec<u64>> {
        // a salted archive hashes "<salt><path>": comparable with nothing
        // outside itself
        (self.salt == 0).then(|| self.entries.iter().filter(|(_, e)| !e.is_directory).map(|(hash, _)| *hash).collect())
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
                tobj_meta: None,
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
                tobj_meta: meta.get(body..body + 12).and_then(|m| m.try_into().ok()),
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
        build_v1_impl(salt, files, true)
    }

    /// The same archive without any directory listing - the way "locked"
    /// mods ship: every file is there, addressable by hash only.
    pub fn build_locked_v1(salt: u16, files: &[(&str, &[u8])]) -> Vec<u8> {
        let files: Vec<(&str, &[u8], bool)> = files.iter().map(|(p, c)| (*p, *c, false)).collect();
        build_v1_impl(salt, &files, false)
    }

    fn build_v1_impl(salt: u16, files: &[(&str, &[u8], bool)], listings: bool) -> Vec<u8> {
        use std::collections::BTreeMap;
        let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        dirs.entry(String::new()).or_default();
        for (path, _, _) in files {
            let parts: Vec<&str> = path.split('/').collect();
            let mut parent = String::new();
            for (i, part) in parts.iter().enumerate() {
                let is_file = i == parts.len() - 1;
                let line = if is_file { part.to_string() } else { format!("*{part}") };
                let listing = dirs.entry(parent.clone()).or_default();
                if !listing.contains(&line) {
                    listing.push(line);
                }
                if !is_file {
                    parent = if parent.is_empty() { part.to_string() } else { format!("{parent}/{part}") };
                    dirs.entry(parent.clone()).or_default();
                }
            }
        }

        // (hash, flags, stored bytes, real size)
        let mut blobs: Vec<(u64, u32, Vec<u8>, u32)> = Vec::new();
        for (path, content, compress) in files {
            let stored = if *compress { zlib(content) } else { content.to_vec() };
            let flags = if *compress { 0x2 } else { 0 };
            blobs.push((hash_path(path, salt), flags, stored, content.len() as u32));
        }
        for (dir, lines) in dirs.iter().filter(|_| listings) {
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

    pub enum V2Content<'a> {
        Plain {
            data: &'a [u8],
            compress: bool,
        },
        /// A packed texture: its 12-byte metadata and GDeflate payload.
        Image {
            meta: [u8; 12],
            payload: &'a [u8],
            raw_size: u32,
        },
    }

    /// Minimal HashFS v2 archive. Directory listings are derived.
    pub fn build_v2(salt: u16, files: &[(&str, V2Content)]) -> Vec<u8> {
        use std::collections::BTreeMap;
        // directory -> names in it (subdirectories carry the '/' marker)
        let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        dirs.entry(String::new()).or_default();
        for (path, _) in files {
            let parts: Vec<&str> = path.split('/').collect();
            let mut parent = String::new();
            for (i, part) in parts.iter().enumerate() {
                let is_file = i == parts.len() - 1;
                let name = if is_file { part.to_string() } else { format!("/{part}") };
                let listing = dirs.entry(parent.clone()).or_default();
                if !listing.contains(&name) {
                    listing.push(name);
                }
                if !is_file {
                    parent = if parent.is_empty() { part.to_string() } else { format!("{parent}/{part}") };
                    dirs.entry(parent.clone()).or_default();
                }
            }
        }

        // (hash, chunk type, tobj meta, stored bytes, real size, compressed)
        type Item = (u64, u8, Option<[u8; 12]>, Vec<u8>, u32, bool);
        let mut items: Vec<Item> = Vec::new();
        for (path, content) in files {
            let hash = hash_path(path, salt);
            match content {
                V2Content::Plain { data, compress } => {
                    let stored = if *compress { zlib(data) } else { data.to_vec() };
                    items.push((hash, super::CHUNK_PLAIN, None, stored, data.len() as u32, *compress));
                }
                V2Content::Image { meta, payload, raw_size } => {
                    items.push((hash, super::CHUNK_IMAGE, Some(*meta), payload.to_vec(), *raw_size, true));
                }
            }
        }
        for (dir, names) in &dirs {
            let mut blob = (names.len() as u32).to_le_bytes().to_vec();
            blob.extend(names.iter().map(|n| n.len() as u8));
            for n in names {
                blob.extend_from_slice(n.as_bytes());
            }
            let size = blob.len() as u32;
            items.push((hash_path(dir, salt), super::CHUNK_DIRECTORY, None, blob, size, false));
        }

        let mut out = vec![0u8; 49];
        let mut entry_table = Vec::new();
        let mut meta_table: Vec<u8> = Vec::new();
        for (hash, chunk, tobj, stored, size, compressed) in &items {
            while !out.len().is_multiple_of(super::V2_BLOCK_SIZE as usize) {
                out.push(0);
            }
            let offset_block = (out.len() as u64 / super::V2_BLOCK_SIZE) as u32;
            out.extend_from_slice(stored);

            let meta_index = (meta_table.len() / super::V2_META_BLOCK) as u32;
            meta_table.extend_from_slice(&[0, 0, 0, *chunk]); // one chunk header
            if let Some(tobj) = tobj {
                meta_table.extend_from_slice(tobj);
            }
            let csize = stored.len() as u32 | if *compressed { 0x1000_0000 } else { 0 };
            meta_table.extend_from_slice(&csize.to_le_bytes());
            meta_table.extend_from_slice(&size.to_le_bytes());
            meta_table.extend_from_slice(&0u32.to_le_bytes());
            meta_table.extend_from_slice(&offset_block.to_le_bytes());

            entry_table.extend_from_slice(&hash.to_le_bytes());
            entry_table.extend_from_slice(&meta_index.to_le_bytes());
            entry_table.extend_from_slice(&1u16.to_le_bytes()); // chunk count
            entry_table.extend_from_slice(&0u16.to_le_bytes());
        }
        let (entry_z, meta_z) = (zlib(&entry_table), zlib(&meta_table));
        let entry_start = out.len() as u64;
        out.extend_from_slice(&entry_z);
        let meta_start = out.len() as u64;
        out.extend_from_slice(&meta_z);

        out[0..4].copy_from_slice(&super::MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&2u16.to_le_bytes());
        out[6..8].copy_from_slice(&salt.to_le_bytes());
        out[8..12].copy_from_slice(b"CITY");
        out[12..16].copy_from_slice(&(items.len() as u32).to_le_bytes());
        out[16..20].copy_from_slice(&(entry_z.len() as u32).to_le_bytes());
        out[24..28].copy_from_slice(&(meta_z.len() as u32).to_le_bytes());
        out[28..36].copy_from_slice(&entry_start.to_le_bytes());
        out[36..44].copy_from_slice(&meta_start.to_le_bytes());
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
        assert_eq!(r.iter_files(), vec!["/def/vehicle/physics.sii", "/manifest.sii", "/map/europe.mbd"]);
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

    #[test]
    fn reads_a_v2_archive_with_a_packed_texture() {
        use testutil::V2Content;
        let meta: [u8; 12] = *include_bytes!("../../fixtures/scs_textures/tex0.tobj12");
        let payload = include_bytes!("../../fixtures/scs_textures/tex0.gdeflate");
        let expected = include_bytes!("../../fixtures/scs_textures/tex0.dds");
        let manifest = b"SiiNunit{mod_package:.p{display_name:\"V2\"}}";

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.scs");
        std::fs::write(
            &path,
            testutil::build_v2(
                5,
                &[
                    ("manifest.sii", V2Content::Plain { data: manifest, compress: true }),
                    ("def/a.sii", V2Content::Plain { data: b"plain", compress: false }),
                    ("material/ui/tex0.tobj", V2Content::Image { meta, payload, raw_size: 25104 }),
                ],
            ),
        )
        .unwrap();

        let mut r = HashFsReader::open(&path).unwrap();
        assert_eq!(r.read_bytes("manifest.sii").unwrap(), manifest);
        assert_eq!(r.read_bytes("def/a.sii").unwrap(), b"plain");
        assert_eq!(r.iter_files(), vec!["/def/a.sii", "/manifest.sii", "/material/ui/tex0.tobj"]);
        // the texture is not a plain file...
        assert!(r.is_image_entry("material/ui/tex0.tobj"));
        assert!(!r.is_image_entry("manifest.sii"));
        assert!(!r.has("material/ui/tex0.tobj"));
        assert!(r.read_bytes("material/ui/tex0.tobj").is_err());
        // ...but rebuilds into the reference .dds
        assert_eq!(r.read_image_dds("material/ui/tex0.tobj").unwrap(), expected);
        assert!(r.read_image_dds("manifest.sii").is_err());
    }
}
