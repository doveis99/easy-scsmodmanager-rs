//! Extract a whole .scs archive to a folder.
//!
//! For looking inside a mod or the game's own archives (base.scs, def.scs,
//! ...). HashFS v1/v2 and ZIP archives are supported; packed v2 textures
//! come out as real .dds files next to where their .tobj would be.
//!
//! One bad entry never aborts the run: it is counted, reported and the
//! extraction carries on.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::scs::hashfs::HashFsReader;
use crate::scs::{detect_format, ModSource, ScsFormat};
use crate::{format_err, Result};

/// The failure list is for showing to a person; the count keeps going.
const MAX_REPORTED_FAILURES: usize = 100;

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
                    self.failures.push(ExtractFailure {
                        entry: entry.to_string(),
                        error: e.to_string(),
                    });
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
    match detect_format(scs_path)? {
        ScsFormat::HashfsV1 | ScsFormat::HashfsV2 => extract_hashfs(scs_path, dest_dir, on_progress, should_cancel),
        ScsFormat::Zip => extract_zip(scs_path, dest_dir, on_progress, should_cancel),
        other => Err(format_err(format!("cannot extract a {other:?} archive"))),
    }
}

fn extract_hashfs(
    scs_path: &Path,
    dest_dir: &Path,
    on_progress: &mut dyn FnMut(usize, usize),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractResult> {
    let mut reader = HashFsReader::open(scs_path)?;
    let paths = reader.iter_files();
    let mut result = ExtractResult {
        total: paths.len(),
        ..Default::default()
    };
    for (done, entry) in paths.iter().enumerate() {
        if should_cancel() {
            result.cancelled = true;
            break;
        }
        let outcome = if reader.is_image_entry(entry) {
            reader
                .read_image_dds(entry)
                .and_then(|dds| write_file(dest_dir, &dds_name(entry), &dds))
        } else {
            reader
                .read_bytes(entry)
                .and_then(|data| write_file(dest_dir, entry, &data))
        };
        result.record(entry, outcome);
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
    let names: Vec<String> = archive
        .file_names()
        .filter(|n| !n.ends_with('/'))
        .map(str::to_string)
        .collect();
    let mut result = ExtractResult {
        total: names.len(),
        ..Default::default()
    };
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
        fs::write(
            &scs,
            build_v1(0, &[("manifest.sii", b"manifest", true), ("def/vehicle/a.sii", b"a", false)]),
        )
        .unwrap();
        let dest = tmp.path().join("out");
        let mut seen = Vec::new();
        let r = extract_scs(&scs, &dest, &mut |d, t| seen.push((d, t)), &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed, r.cancelled), (2, 2, 0, false));
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
        make_zip(
            &scs,
            &[
                ("def/", b""),
                ("def/a.sii", b"a"),
                ("../escaped.txt", b"evil"),
                ("manifest.sii", b"m"),
            ],
        );
        let dest = tmp.path().join("nested").join("out");
        let r = extract_scs(&scs, &dest, &mut |_, _| {}, &never).unwrap();
        assert_eq!((r.total, r.extracted, r.failed), (3, 2, 1));
        assert_eq!(r.failures[0].entry, "../escaped.txt");
        assert_eq!(fs::read(dest.join("def").join("a.sii")).unwrap(), b"a");
        assert!(!tmp.path().join("nested").join("escaped.txt").exists());
        assert!(!tmp.path().join("escaped.txt").exists());
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
