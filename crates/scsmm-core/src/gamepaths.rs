//! Every file path the game's own archives list, as a lookup by path hash.
//!
//! HashFS stores files under the CityHash64 of their path. A "locked" mod
//! archive has its directory listings stripped, so its files have hashes
//! but no names. Most mod files replace a file of the game, though, and the
//! game's archives do list their paths - hashing those gives the names
//! back for every file a locked archive overrides.
//!
//! Listing the game takes a second or two (hundreds of thousands of
//! paths), so the result is kept in a cache file and only rebuilt when the
//! game's archives change. The file is laid out for cheap lookups: a sorted
//! hash array to search, and the names in one blob that is only read where
//! a hash matched.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::scs::cityhash::city_hash64;
use crate::scs::hashfs::HashFsReader;
use crate::{format_err, Result};

const MAGIC: &[u8; 8] = b"SCSPATH1";

pub struct GamePaths {
    /// Sorted hashes of the unsalted paths.
    hashes: Vec<u64>,
    /// `offsets[i]..offsets[i + 1]` is the name of `hashes[i]` in the blob.
    offsets: Vec<u32>,
    file: File,
    blob_start: u64,
}

/// Name, size and modification time of every archive: when any of it
/// changes, the game was updated or a DLC was added.
fn signature(archives: &[PathBuf]) -> String {
    let mut out = String::new();
    for a in archives {
        let (len, mtime) = fs::metadata(a)
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                (m.len(), mtime)
            })
            .unwrap_or((0, 0));
        let name = a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push_str(&format!("{name}|{len}|{mtime}\n"));
    }
    out
}

fn game_archives(game_dir: &Path) -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> = fs::read_dir(game_dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    archives.retain(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("scs")));
    archives.sort();
    archives
}

fn u32_from(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

impl GamePaths {
    /// The paths of the game installed in `game_dir`, from `cache_file`
    /// when it is still current, otherwise listed afresh and cached.
    pub fn load_or_build(game_dir: &Path, cache_file: &Path) -> Result<GamePaths> {
        let archives = game_archives(game_dir);
        if archives.is_empty() {
            return Err(format_err("no game archives found"));
        }
        let sig = signature(&archives);
        if let Ok(cached) = Self::open(cache_file, Some(&sig)) {
            return Ok(cached);
        }
        let paths: Vec<String> = archives
            .par_iter()
            .flat_map_iter(|a| HashFsReader::open(a).map(|mut r| r.iter_files()).unwrap_or_default())
            .collect();
        Self::build(paths.iter().map(String::as_str), &sig, cache_file)
    }

    /// Write a cache file for `paths` and open it.
    pub fn build<'a>(paths: impl Iterator<Item = &'a str>, sig: &str, cache_file: &Path) -> Result<GamePaths> {
        let mut rows: Vec<(u64, &str)> = paths
            .map(|p| p.trim_start_matches('/'))
            .filter(|p| !p.is_empty())
            .map(|p| (city_hash64(p.as_bytes()), p))
            .collect();
        rows.sort_unstable();
        rows.dedup_by_key(|(hash, _)| *hash);

        if let Some(dir) = cache_file.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = cache_file.with_extension("tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            w.write_all(MAGIC)?;
            w.write_all(&(sig.len() as u32).to_le_bytes())?;
            w.write_all(sig.as_bytes())?;
            w.write_all(&(rows.len() as u32).to_le_bytes())?;
            for (hash, _) in &rows {
                w.write_all(&hash.to_le_bytes())?;
            }
            let mut offset = 0u32;
            for (_, path) in &rows {
                w.write_all(&offset.to_le_bytes())?;
                offset = offset
                    .checked_add(path.len() as u32)
                    .ok_or_else(|| format_err("path list too large"))?;
            }
            w.write_all(&offset.to_le_bytes())?;
            for (_, path) in &rows {
                w.write_all(path.as_bytes())?;
            }
            w.flush()?;
        }
        fs::rename(&tmp, cache_file)?;
        Self::open(cache_file, Some(sig))
    }

    /// Open a cache file; with `expected_sig`, only if it was built from
    /// exactly those archives.
    fn open(cache_file: &Path, expected_sig: Option<&str>) -> Result<GamePaths> {
        let stale = || format_err("path cache is missing or out of date");
        let file = File::open(cache_file)?;
        let file_len = file.metadata()?.len();
        let mut r = BufReader::new(file);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(stale());
        }
        let sig_len = u32_from(&mut r)? as u64;
        if sig_len > file_len {
            return Err(stale());
        }
        let mut sig = vec![0u8; sig_len as usize];
        r.read_exact(&mut sig)?;
        if expected_sig.is_some_and(|want| want.as_bytes() != sig.as_slice()) {
            return Err(stale());
        }
        let count = u32_from(&mut r)? as u64;
        let blob_start = 8 + 4 + sig_len + 4 + count * 8 + (count + 1) * 4;
        if blob_start > file_len {
            return Err(stale());
        }
        let mut raw = vec![0u8; (count * 8) as usize];
        r.read_exact(&mut raw)?;
        let hashes: Vec<u64> = raw
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
            .collect();
        let mut raw = vec![0u8; ((count + 1) * 4) as usize];
        r.read_exact(&mut raw)?;
        let offsets: Vec<u32> = raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect();
        // a truncated or damaged file must not be trusted for lookups
        let sorted = hashes.windows(2).all(|w| w[0] < w[1]) && offsets.windows(2).all(|w| w[0] <= w[1]);
        let blob_len = offsets.last().copied().unwrap_or(0) as u64;
        if !sorted || blob_start + blob_len != file_len {
            return Err(stale());
        }
        Ok(GamePaths {
            hashes,
            offsets,
            file: r.into_inner(),
            blob_start,
        })
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// The path (no leading slash) whose unsalted hash is `hash`.
    pub fn name_of(&mut self, hash: u64) -> Option<String> {
        let i = self.hashes.binary_search(&hash).ok()?;
        let (start, end) = (self.offsets[i] as u64, self.offsets[i + 1] as u64);
        self.file.seek(SeekFrom::Start(self.blob_start + start)).ok()?;
        let mut buf = vec![0u8; (end - start) as usize];
        self.file.read_exact(&mut buf).ok()?;
        String::from_utf8(buf).ok()
    }

    /// Every path, in hash order - for archives that salt their hashes,
    /// where each path has to be hashed again with the salt.
    pub fn for_each_name(&mut self, mut f: impl FnMut(&str)) -> Result<()> {
        self.file.seek(SeekFrom::Start(self.blob_start))?;
        let mut r = BufReader::new(&mut self.file);
        let mut buf = Vec::new();
        for w in self.offsets.windows(2) {
            buf.resize((w[1] - w[0]) as usize, 0);
            r.read_exact(&mut buf)?;
            if let Ok(name) = std::str::from_utf8(&buf) {
                f(name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scs::hashfs::testutil::build_v1;

    #[test]
    fn names_come_back_by_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("nested").join("paths.bin");
        let paths = ["/def/vehicle/physics.sii", "def/a.sii", "def/a.sii", "", "map/europe.mbd"];
        let mut gp = GamePaths::build(paths.iter().copied(), "sig", &cache).unwrap();
        assert_eq!(gp.len(), 3);
        assert_eq!(gp.name_of(city_hash64(b"def/vehicle/physics.sii")).as_deref(), Some("def/vehicle/physics.sii"));
        assert_eq!(gp.name_of(city_hash64(b"map/europe.mbd")).as_deref(), Some("map/europe.mbd"));
        assert_eq!(gp.name_of(city_hash64(b"def/b.sii")), None);
        let mut all = Vec::new();
        gp.for_each_name(|n| all.push(n.to_string())).unwrap();
        all.sort();
        assert_eq!(all, ["def/a.sii", "def/vehicle/physics.sii", "map/europe.mbd"]);
        // lookups still work after a full pass moved the file position
        assert_eq!(gp.name_of(city_hash64(b"def/a.sii")).as_deref(), Some("def/a.sii"));
    }

    #[test]
    fn the_cache_follows_the_game_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let game = tmp.path().join("game");
        fs::create_dir_all(&game).unwrap();
        let cache = tmp.path().join("paths.bin");
        assert!(GamePaths::load_or_build(&game, &cache).is_err(), "no archives, no dictionary");

        fs::write(game.join("def.scs"), build_v1(0, &[("def/a.sii", b"a", false)])).unwrap();
        fs::write(game.join("readme.txt"), b"not an archive").unwrap();
        let mut gp = GamePaths::load_or_build(&game, &cache).unwrap();
        assert_eq!(gp.len(), 1);
        assert!(gp.name_of(city_hash64(b"def/a.sii")).is_some());

        // unchanged archives: the cache file is used as it is
        let stamp = fs::metadata(&cache).unwrap().modified().unwrap();
        let again = GamePaths::load_or_build(&game, &cache).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(fs::metadata(&cache).unwrap().modified().unwrap(), stamp);

        // a new archive (an update, a DLC) rebuilds it
        fs::write(game.join("dlc_x.scs"), build_v1(0, &[("def/b.sii", b"b", false), ("map/x.mbd", b"m", false)])).unwrap();
        let mut gp = GamePaths::load_or_build(&game, &cache).unwrap();
        assert_eq!(gp.len(), 3);
        assert!(gp.name_of(city_hash64(b"map/x.mbd")).is_some());
    }

    #[test]
    fn a_damaged_cache_is_rebuilt_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let game = tmp.path().join("game");
        fs::create_dir_all(&game).unwrap();
        fs::write(game.join("def.scs"), build_v1(0, &[("def/a.sii", b"a", false)])).unwrap();
        let cache = tmp.path().join("paths.bin");
        GamePaths::load_or_build(&game, &cache).unwrap();

        let good = fs::read(&cache).unwrap();
        fs::write(&cache, &good[..good.len() - 3]).unwrap(); // truncated
        let mut gp = GamePaths::load_or_build(&game, &cache).unwrap();
        assert_eq!(gp.name_of(city_hash64(b"def/a.sii")).as_deref(), Some("def/a.sii"));
        fs::write(&cache, b"garbage").unwrap();
        assert_eq!(GamePaths::load_or_build(&game, &cache).unwrap().len(), 1);
    }
}
