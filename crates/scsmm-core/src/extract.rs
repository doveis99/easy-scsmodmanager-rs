//! Extract a whole .scs archive to a folder.
//!
//! For looking inside a mod or the game's own archives (base.scs, def.scs,
//! ...). HashFS v1/v2 and ZIP archives are supported; packed v2 textures
//! come out as real .dds files next to where their .tobj would be.
//!
//! A "locked" HashFS archive does not list its files. Their names are
//! recovered where they can be (see [`crate::recover`]); what stays
//! nameless is written under its hash into [`UNNAMED_DIR`], so every file
//! of the archive ends up on disk either way.
//!
//! One bad entry never aborts the run: it is counted, reported and the
//! extraction carries on.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::gamepaths::GamePaths;
use crate::recover::{recover_names, sniff_extension};
use crate::scs::hashfs::HashFsReader;
use crate::scs::{detect_format, ScsFormat};
use crate::{format_err, Result};

/// The failure list is for showing to a person; the count keeps going.
const MAX_REPORTED_FAILURES: usize = 100;

/// Folder (inside the destination) for files whose path could not be
/// recovered. They are named `<hash>.<extension guessed from content>`.
pub const UNNAMED_DIR: &str = "_unnamed";

#[derive(Default)]
pub struct ExtractOptions<'a> {
    /// Paths of the game's own files, for naming the files of a locked
    /// archive that replace one of them.
    pub game_paths: Option<&'a mut GamePaths>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractFailure {
    pub entry: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtractResult {
    pub total: usize,
    pub extracted: usize,
    pub failed: usize,
    pub cancelled: bool,
    /// Files the archive does not list (a "locked" archive) whose path
    /// was recovered all the same. Part of `total`.
    pub recovered: usize,
    /// Files whose path stayed unknown; they are written under their hash
    /// into [`UNNAMED_DIR`]. Part of `total`.
    pub unnamed: usize,
    /// At most the first [`MAX_REPORTED_FAILURES`] failures.
    pub failures: Vec<ExtractFailure>,
}

impl ExtractResult {
    fn record(&mut self, entry: &str, outcome: Result<()>) {
        match outcome {
            Ok(()) => self.extracted += 1,
            Err(e) => {
                self.failed += 1;
                if self.failures.len() < MAX_REPORTED_FAILURES {
                    self.failures.push(ExtractFailure { entry: entry.to_string(), error: e.to_string() });
                }
            }
        }
    }
}

/// Turn an archive entry name into a path under `dest_dir`.
///
/// Archive names are untrusted: anything that is not a plain relative path
/// (`..`, an absolute path, a drive or stream marker) is refused rather
/// than "cleaned up", so nothing can be written outside the destination.
fn target_path(dest_dir: &Path, relative: &str) -> Result<PathBuf> {
    let normalized = relative.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    let mut target = dest_dir.to_path_buf();
    let mut any = false;
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            return Err(format_err(format!("refusing unsafe path: {relative}")));
        }
        target.push(part);
        any = true;
    }
    if !any {
        return Err(format_err("empty entry name"));
    }
    Ok(target)
}

fn write_file(dest_dir: &Path, relative: &str, data: &[u8]) -> Result<()> {
    let target = target_path(dest_dir, relative)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, data)?;
    Ok(())
}

/// An image entry is keyed by its .tobj path; the rebuilt image is a .dds.
fn dds_name(path: &str) -> String {
    let p = path.trim_start_matches('/');
    match p.strip_suffix(".tobj") {
        Some(stem) => format!("{stem}.dds"),
        None => format!("{p}.dds"),
    }
}

/// Extract every file in `scs_path` into `dest_dir`.
///
/// `on_progress(done, total)` is called after each entry; `should_cancel`
/// is polled before each one. Formats that cannot be walked (AEM, unknown)
/// are an error.
pub fn extract_scs(
    scs_path: &Path,
    dest_dir: &Path,
    on_progress: &mut dyn FnMut(usize, usize),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractResult> {
    extract_scs_with(scs_path, dest_dir, ExtractOptions::default(), on_progress, should_cancel)
}

/// [`extract_scs`] with the game's path list at hand for locked archives.
pub fn extract_scs_with(
    scs_path: &Path,
    dest_dir: &Path,
    options: ExtractOptions,
    on_progress: &mut dyn FnMut(usize, usize),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractResult> {
    match detect_format(scs_path)? {
        ScsFormat::HashfsV1 | ScsFormat::HashfsV2 => {
            extract_hashfs(scs_path, dest_dir, options, on_progress, should_cancel)
        }
        ScsFormat::Zip => extract_zip(scs_path, dest_dir, on_progress, should_cancel),
        other => Err(format_err(format!("cannot extract a {other:?} archive"))),
    }
}

fn extract_hashfs(
    scs_path: &Path,
    dest_dir: &Path,
    options: ExtractOptions,
    on_progress: &mut dyn FnMut(usize, usize),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractResult> {
    let mut reader = HashFsReader::open(scs_path)?;
    let listed = reader.iter_files();
    let recovery = recover_names(&mut reader, &listed, options.game_paths, should_cancel);

    // (hash, where it goes): listed files, then the ones named after all,
    // then the nameless ones
    let mut work: Vec<(u64, Option<String>)> =
        listed.iter().map(|path| (reader.hash_of(path), Some(path.clone()))).collect();
    let mut recovered: Vec<(u64, String)> = recovery.names.into_iter().collect();
    recovered.sort_by(|a, b| a.1.cmp(&b.1));
    let mut result =
        ExtractResult { recovered: recovered.len(), unnamed: recovery.unnamed.len(), ..Default::default() };
    work.extend(recovered.into_iter().map(|(hash, path)| (hash, Some(path))));
    work.extend(recovery.unnamed.into_iter().map(|hash| (hash, None)));
    result.total = work.len();

    for (done, (hash, path)) in work.iter().enumerate() {
        if should_cancel() {
            result.cancelled = true;
            break;
        }
        // a packed texture comes back from the reader as a finished .dds
        let is_image = reader.is_image_hash(*hash);
        let data = reader.read_by_hash(*hash);
        let (label, outcome) = match path {
            Some(path) => {
                let target = if is_image { dds_name(path) } else { path.clone() };
                (path.clone(), data.and_then(|data| write_file(dest_dir, &target, &data)))
            }
            None => {
                let label = format!("{UNNAMED_DIR}/{hash:016x}");
                let outcome = data.and_then(|data| {
                    let ext = if is_image { "dds" } else { sniff_extension(&data) };
                    write_file(dest_dir, &format!("{label}.{ext}"), &data)
                });
                (label, outcome)
            }
        };
        result.record(&label, outcome);
        on_progress(done + 1, result.total);
    }
    Ok(result)
}

fn extract_zip(
    scs_path: &Path,
    dest_dir: &Path,
    on_progress: &mut dyn FnMut(usize, usize),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractResult> {
    let mut archive = zip::ZipArchive::new(fs::File::open(scs_path)?)?;
    let names: Vec<String> = archive.file_names().filter(|n| !n.ends_with('/')).map(str::to_string).collect();
    let mut result = ExtractResult { total: names.len(), ..Default::default() };
    for (done, name) in names.iter().enumerate() {
        if should_cancel() {
            result.cancelled = true;
            break;
        }
        let outcome = (|| -> Result<()> {
            // check the destination first: a refused name reads nothing
            target_path(dest_dir, name)?;
            let mut entry = archive.by_name(name)?;
            let mut data = Vec::with_capacity(entry.size().min(1 << 24) as usize);
            entry.read_to_end(&mut data)?;
            write_file(dest_dir, name, &data)
        })();
        result.record(name, outcome);
        on_progress(done + 1, result.total);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scs::hashfs::testutil::{build_v1, build_v2, V2Content};
    use std::cell::Cell;
    use std::io::Write;

    fn never() -> bool {
        false
    }

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in files {
            if name.ends_with('/') {
                w.add_directory(name.trim_end_matches('/'), opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap();
    }

    #[test]
    fn unsafe_names_are_refused() {
        let dest = Path::new("dest");
        for bad in ["../x", "a/../../x", "/../x", "C:/x", "a/b:stream", "", "/", "a//b", "./a"] {
            assert!(target_path(dest, bad).is_err(), "should refuse {bad:?}");
        }
        assert_eq!(target_path(dest, "/def/a.sii").unwrap(), dest.join("def").join("a.sii"));
        assert_eq!(target_path(dest, "def\\a.sii").unwrap(), dest.join("def").join("a.sii"));
        assert_eq!(dds_name("/material/ui/x.tobj"), "material/ui/x.dds");
        assert_eq!(dds_name("odd"), "odd.dds");
    }

    #[test]
    fn extracts_a_hashfs_v1_archive_with_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("m.scs");
        fs::write(&scs, build_v1(0, &[("manifest.sii", b"manifest", true), ("def/vehicle/a.sii", b"a", false)]))
            .unwrap();
        let dest = tmp.path().join("out");
        let mut seen = Vec::new();
        let r = extract_scs(&scs, &dest, &mut |d, t| seen.push((d, t)), &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed, r.cancelled), (2, 2, 0, false));
        assert_eq!((r.recovered, r.unnamed), (0, 0));
        assert_eq!(seen, vec![(1, 2), (2, 2)]);
        assert_eq!(fs::read(dest.join("manifest.sii")).unwrap(), b"manifest");
        assert_eq!(fs::read(dest.join("def").join("vehicle").join("a.sii")).unwrap(), b"a");
    }

    #[test]
    fn packed_textures_come_out_as_dds() {
        let meta: [u8; 12] = *include_bytes!("../fixtures/scs_textures/tex1.tobj12");
        let payload = include_bytes!("../fixtures/scs_textures/tex1.gdeflate");
        let expected = include_bytes!("../fixtures/scs_textures/tex1.dds");
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("v2.scs");
        fs::write(
            &scs,
            build_v2(
                0,
                &[
                    ("manifest.sii", V2Content::Plain { data: b"m", compress: false }),
                    ("material/ui/tex1.tobj", V2Content::Image { meta, payload, raw_size: 16904 }),
                    // a texture whose stream is damaged must not stop the run
                    ("material/ui/broken.tobj", V2Content::Image { meta, payload: b"garbage", raw_size: 16904 }),
                ],
            ),
        )
        .unwrap();
        let dest = tmp.path().join("out");
        let r = extract_scs(&scs, &dest, &mut |_, _| {}, &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed), (3, 2, 1));
        assert_eq!(r.failures[0].entry, "/material/ui/broken.tobj");
        assert_eq!(fs::read(dest.join("material").join("ui").join("tex1.dds")).unwrap(), expected);
        assert!(!dest.join("material").join("ui").join("tex1.tobj").exists());
        assert!(!dest.join("material").join("ui").join("broken.dds").exists());
    }

    #[test]
    fn zip_extraction_never_leaves_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("z.scs");
        make_zip(&scs, &[("def/", b""), ("def/a.sii", b"a"), ("../escaped.txt", b"evil"), ("manifest.sii", b"m")]);
        let dest = tmp.path().join("nested").join("out");
        let r = extract_scs(&scs, &dest, &mut |_, _| {}, &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed), (3, 2, 1));
        assert_eq!(r.failures[0].entry, "../escaped.txt");
        assert_eq!(fs::read(dest.join("def").join("a.sii")).unwrap(), b"a");
        assert!(!tmp.path().join("nested").join("escaped.txt").exists());
        assert!(!tmp.path().join("escaped.txt").exists());
    }

    #[test]
    fn a_locked_archive_is_extracted_whole() {
        use crate::scs::cityhash::city_hash64;
        use crate::scs::hashfs::testutil::build_locked_v1;
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("locked.scs");
        fs::write(
            &scs,
            build_locked_v1(
                0,
                &[
                    ("manifest.sii", b"m"),
                    // replaces a file of the game
                    ("def/vehicle/physics.sii", b"SiiNunit\n{\n@include \"extra.sui\"\n}\n"),
                    // named by the file above
                    ("def/vehicle/extra.sui", b"x: 1"),
                    // named by nothing
                    ("def/secret.sii", b"SiiNunit\n{\n}\n"),
                    ("sound/secret.bank", b"\x00\x01\x02"),
                ],
            ),
        )
        .unwrap();

        // without the game's paths only what the archive itself gives away
        let dest = tmp.path().join("plain");
        let r = extract_scs(&scs, &dest, &mut |_, _| {}, &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed), (5, 5, 0));
        assert_eq!((r.recovered, r.unnamed), (0, 4));
        assert_eq!(fs::read(dest.join("manifest.sii")).unwrap(), b"m");
        assert_eq!(fs::read_dir(dest.join(UNNAMED_DIR)).unwrap().count(), 4);

        // with them: the override gets its name, and through it the include
        let mut paths = crate::gamepaths::GamePaths::build(
            ["def/vehicle/physics.sii"].into_iter(),
            "sig",
            &tmp.path().join("paths.bin"),
        )
        .unwrap();
        let dest = tmp.path().join("named");
        let options = ExtractOptions { game_paths: Some(&mut paths) };
        let mut seen = Vec::new();
        let r = extract_scs_with(&scs, &dest, options, &mut |d, t| seen.push((d, t)), &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed), (5, 5, 0));
        assert_eq!((r.recovered, r.unnamed), (2, 2));
        assert_eq!(seen.last(), Some(&(5, 5)));
        assert!(dest.join("def").join("vehicle").join("physics.sii").is_file());
        assert_eq!(fs::read(dest.join("def").join("vehicle").join("extra.sui")).unwrap(), b"x: 1");
        // the nameless rest: by hash, with an extension from the content
        let secret = format!("{:016x}.sii", city_hash64(b"def/secret.sii"));
        assert!(dest.join(UNNAMED_DIR).join(secret).is_file());
        let bank = format!("{:016x}.bin", city_hash64(b"sound/secret.bank"));
        assert_eq!(fs::read(dest.join(UNNAMED_DIR).join(bank)).unwrap(), b"\x00\x01\x02");
        assert!(!dest.join("def").join("secret.sii").exists());
    }

    #[test]
    fn cancelling_stops_between_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("z.scs");
        make_zip(&scs, &[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")]);
        let dest = tmp.path().join("out");
        let done = Cell::new(0);
        let r = extract_scs(&scs, &dest, &mut |d, _| done.set(d), &|| done.get() >= 1).unwrap();
        assert!(r.cancelled);
        assert_eq!((r.total, r.extracted), (3, 1));
    }

    #[test]
    fn unsupported_formats_are_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("x.scs");
        fs::write(&scs, b"AEM!\0\0\0\0").unwrap();
        assert!(extract_scs(&scs, tmp.path(), &mut |_, _| {}, &never).is_err());
        fs::write(&scs, b"garbage").unwrap();
        assert!(extract_scs(&scs, tmp.path(), &mut |_, _| {}, &never).is_err());
    }
}
