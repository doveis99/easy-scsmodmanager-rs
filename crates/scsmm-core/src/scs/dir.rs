//! An unpacked mod directory (the folder that holds `manifest.sii`).

use std::fs;
use std::path::{Path, PathBuf};

use super::ModSource;
use crate::{Error, Result};

pub struct DirSource {
    root: PathBuf,
}

impl DirSource {
    pub fn open(directory: &Path) -> Result<Self> {
        if !directory.is_dir() {
            return Err(Error::NotFound(format!("not a directory: {}", directory.display())));
        }
        Ok(Self { root: directory.to_path_buf() })
    }

    /// Resolve an archive-style relative path, refusing anything that would
    /// climb out of the mod directory (a manifest names its own icon file).
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let rel = Path::new(path.trim_start_matches('/'));
        if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return None;
        }
        Some(self.root.join(rel))
    }

    fn walk(&self, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                self.walk(&path, out);
            } else if kind.is_file() {
                if let Ok(rel) = path.strip_prefix(&self.root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
}

impl ModSource for DirSource {
    fn has(&mut self, path: &str) -> bool {
        self.resolve(path).is_some_and(|p| p.is_file())
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let target = self.resolve(path).filter(|p| p.is_file()).ok_or_else(|| Error::NotFound(path.to_string()))?;
        Ok(fs::read(target)?)
    }

    fn list_files(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&self.root, &mut out);
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_reads_and_stays_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("mod");
        fs::create_dir_all(root.join("def/vehicle")).unwrap();
        fs::write(root.join("manifest.sii"), b"m").unwrap();
        fs::write(root.join("def/vehicle/a.sii"), b"a").unwrap();
        fs::write(tmp.path().join("secret.txt"), b"s").unwrap();

        let mut d = DirSource::open(&root).unwrap();
        assert_eq!(d.list_files(), vec!["def/vehicle/a.sii", "manifest.sii"]);
        assert_eq!(d.read_bytes("manifest.sii").unwrap(), b"m");
        assert!(!d.has("../secret.txt"));
        assert!(d.read_bytes("../secret.txt").is_err());
    }
}
