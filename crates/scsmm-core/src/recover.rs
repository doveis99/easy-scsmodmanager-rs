//! Put names back on the files of a "locked" HashFS archive.
//!
//! Such an archive has its directory listings stripped: every file is
//! there, stored under the hash of its path, but nothing says what the
//! paths are. A hash cannot be turned back into a path - but a guessed
//! path can be hashed and compared, and a wrong guess simply matches
//! nothing. Good guesses come from three places:
//!
//! * the game's own archives ([`GamePaths`]): most mod files replace a file
//!   of the game, at the same path;
//! * the archive's own contents: definition files, materials and models
//!   name the files they use (`"/vehicle/truck/x/model.pmd"`,
//!   `@include "common.sui"`);
//! * naming conventions: an accessory unit called `name.truck.type` lives
//!   in `def/vehicle/truck/<truck>/<type>/<name>.sii`.
//!
//! Whatever stays nameless is still readable by its hash.

use std::collections::{HashMap, HashSet};

use crate::gamepaths::GamePaths;
use crate::scs::hashfs::HashFsReader;
use crate::sii::crypto::{decode_3nk, is_3nk};

/// Contents larger than this are not searched for paths: definitions,
/// materials and model descriptors are small, and reading every large
/// model or sound of a big archive would take longer than it is worth.
const MAX_SCANNED_BYTES: u32 = 1024 * 1024;
/// New names can make further relative references resolvable; this bounds
/// how often that is followed up.
const MAX_ROUNDS: usize = 8;

#[derive(Debug, Default)]
pub struct Recovery {
    /// Stored hash -> path (no leading slash) of each file that the
    /// listings do not reach but a guess matched.
    pub names: HashMap<u64, String>,
    pub by_dictionary: usize,
    pub by_reference: usize,
    /// Hashes of the files that remain nameless, in hash order.
    pub unnamed: Vec<u64>,
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_./\\-+~@$()[]!".contains(&b)
}

/// Path-looking strings in `data`: runs of path characters that contain a
/// separator or a dot and at least one letter. Works on text and on
/// binary files alike (a .pmd or .tobj holds its paths as plain bytes).
fn path_tokens(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for run in data.split(|b| !is_path_byte(*b)) {
        if run.len() < 3 || run.len() > 260 {
            continue;
        }
        if !run.iter().any(|b| *b == b'/' || *b == b'\\' || *b == b'.') || !run.iter().any(u8::is_ascii_alphabetic) {
            continue;
        }
        // all bytes are ASCII by construction
        let token = String::from_utf8_lossy(run).replace('\\', "/");
        // a sentence's full stop is not part of the path; leading dots are
        // (`../shared/x.sui`)
        let token = token.trim_end_matches('.').to_string();
        if token.len() >= 3 {
            out.push(token);
        }
    }
    out
}

/// `path` plus the files that conventionally sit next to it: a model
/// descriptor has geometry and collision files, a texture object has its
/// image, a sound bank its GUID list.
fn with_siblings(path: &str, out: &mut Vec<String>) {
    const SIBLINGS: &[(&str, &[&str])] = &[
        (".pmd", &[".pmg", ".pmc", ".pma", ".ppd"]),
        (".pmg", &[".pmd", ".pmc", ".pma"]),
        (".tobj", &[".dds", ".mat", ".png"]),
        (".mat", &[".tobj", ".dds"]),
        (".dds", &[".tobj", ".mat"]),
        (".bank", &[".bank.guids"]),
        (".sii", &[".sui"]),
    ];
    for (ext, others) in SIBLINGS {
        if let Some(stem) = path.strip_suffix(ext) {
            out.extend(others.iter().map(|other| format!("{stem}{other}")));
        }
    }
    out.push(path.to_string());
}

/// Guesses that need no context: the token as an absolute path, and for
/// a dotted unit name the definition file it conventionally lives in.
fn absolute_guesses(token: &str, out: &mut Vec<String>) {
    // a sound reference carries its event after '#'
    let token = token.split('#').next().unwrap_or("");
    let path = token.trim_start_matches('/');
    if path.is_empty() {
        return;
    }
    with_siblings(path, out);
    if path.contains('/') {
        return;
    }
    // `dc13_370.scania.s_2016.engine` ->
    // def/vehicle/truck/scania.s_2016/engine/dc13_370.sii
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() >= 3 && parts.iter().all(|p| !p.is_empty()) {
        let (name, kind) = (parts[0], parts[parts.len() - 1]);
        let vehicle = parts[1..parts.len() - 1].join(".");
        for class in ["truck", "trailer", "trailer_owned"] {
            with_siblings(&format!("def/vehicle/{class}/{vehicle}/{kind}/{name}.sii"), out);
        }
    }
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Resolve `token` against the directory of the file it was found in,
/// following `..` segments.
fn relative_guess(dir: &str, token: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in token.split('#').next().unwrap_or("").split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Name as many of the files as possible that the listings (`listed`) do
/// not reach. `should_cancel` is polled between files.
pub fn recover_names(
    reader: &mut HashFsReader,
    listed: &[String],
    dictionary: Option<&mut GamePaths>,
    should_cancel: &dyn Fn() -> bool,
) -> Recovery {
    let entries = reader.file_entries();
    // hash -> name of every file whose path is known so far
    let mut known: HashMap<u64, String> =
        listed.iter().map(|p| p.trim_start_matches('/')).map(|p| (reader.hash_of(p), p.to_string())).collect();
    let mut unnamed: HashSet<u64> =
        entries.iter().map(|(hash, _)| *hash).filter(|hash| !known.contains_key(hash)).collect();
    let mut recovery = Recovery::default();
    if unnamed.is_empty() {
        return recovery;
    }

    // 1. paths of the game's own files
    if let Some(dict) = dictionary {
        let mut hits: Vec<(u64, String)> = Vec::new();
        if reader.salt() == 0 {
            for hash in &unnamed {
                if let Some(name) = dict.name_of(*hash) {
                    hits.push((*hash, name));
                }
            }
        } else {
            // salted hashes match nothing as they are: hash every path anew
            let _ = dict.for_each_name(|name| {
                let hash = reader.hash_of(name);
                if unnamed.contains(&hash) {
                    hits.push((hash, name.to_string()));
                }
            });
        }
        for (hash, name) in hits {
            if unnamed.remove(&hash) {
                recovery.by_dictionary += 1;
                recovery.names.insert(hash, name.clone());
                known.insert(hash, name);
            }
        }
    }

    // 2. paths named inside the archive's own files
    let mut relative: HashMap<u64, Vec<String>> = HashMap::new();
    let mut guesses: Vec<String> = Vec::new();
    for (hash, size) in &entries {
        if unnamed.is_empty() || should_cancel() {
            break;
        }
        if *size > MAX_SCANNED_BYTES || reader.is_image_hash(*hash) {
            continue;
        }
        let Ok(data) = reader.read_by_hash(*hash) else {
            continue;
        };
        // a scrambled definition names its files just the same
        let data = if is_3nk(&data) { decode_3nk(&data).unwrap_or(data) } else { data };
        let mut local: Vec<String> = Vec::new();
        for token in path_tokens(&data) {
            guesses.clear();
            absolute_guesses(&token, &mut guesses);
            for guess in guesses.drain(..) {
                let guessed = reader.hash_of(&guess);
                if unnamed.remove(&guessed) {
                    recovery.by_reference += 1;
                    recovery.names.insert(guessed, guess.clone());
                    known.insert(guessed, guess);
                }
            }
            // "x.sui" or "../common/x.sui": only meaningful next to the
            // file that says it, whose own path may not be known yet
            if !token.starts_with('/') && token.contains('.') {
                local.push(token);
            }
        }
        if !local.is_empty() {
            local.sort_unstable();
            local.dedup();
            relative.insert(*hash, local);
        }
    }
    for _ in 0..MAX_ROUNDS {
        if unnamed.is_empty() || should_cancel() {
            break;
        }
        let mut found: Vec<(u64, String)> = Vec::new();
        for (source, tokens) in &relative {
            let Some(source_path) = known.get(source) else {
                continue;
            };
            let dir = parent_dir(source_path);
            for token in tokens {
                let Some(base) = relative_guess(dir, token) else {
                    continue;
                };
                guesses.clear();
                with_siblings(&base, &mut guesses);
                for guess in guesses.drain(..) {
                    let guessed = reader.hash_of(&guess);
                    if unnamed.contains(&guessed) {
                        found.push((guessed, guess));
                    }
                }
            }
        }
        if found.is_empty() {
            break;
        }
        for (hash, name) in found {
            if unnamed.remove(&hash) {
                recovery.by_reference += 1;
                recovery.names.insert(hash, name.clone());
                known.insert(hash, name);
            }
        }
    }

    let mut left: Vec<u64> = unnamed.into_iter().collect();
    left.sort_unstable();
    recovery.unnamed = left;
    recovery
}

/// A file extension for content whose name is not known, from the first
/// bytes - so a nameless file can at least be opened with the right tool.
pub fn sniff_extension(data: &[u8]) -> &'static str {
    let text_start = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    let starts = |magic: &[u8]| data.starts_with(magic);
    if text_start.starts_with(b"SiiNunit") || starts(b"BSII") || starts(b"ScsC") || is_3nk(data) {
        "sii"
    } else if starts(b"DDS ") {
        "dds"
    } else if starts(b"\x89PNG") {
        "png"
    } else if starts(b"\xff\xd8\xff") {
        "jpg"
    } else if starts(b"OggS") {
        "ogg"
    } else if starts(b"RIFF") {
        "bank"
    } else if starts(&[0x01, 0x0a, 0xb1, 0x70]) {
        "tobj"
    } else if text_start.starts_with(b"material") || text_start.starts_with(b"effect") {
        "mat"
    } else if !data.is_empty() && text_start.iter().take(512).all(|b| b.is_ascii_graphic() || b.is_ascii_whitespace()) {
        "txt"
    } else {
        "bin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scs::hashfs::testutil::build_locked_v1;
    use std::fs;

    fn never() -> bool {
        false
    }

    const CHASSIS: &[u8] = b"SiiNunit\n{\naccessory_chassis_data : 4x2.fancy.truck.chassis\n{\n\
        detail_model: \"/vehicle/truck/fancy/chassis.pmd\"\n\
        sounds[]: \"/sound/truck/fancy.bank#engine/start\"\n\
        @include \"common.sui\"\n@include \"../shared/tires.sui\"\n}\n}\n";

    fn locked(salt: u16) -> Vec<u8> {
        build_locked_v1(
            salt,
            &[
                ("manifest.sii", b"SiiNunit\n{\n}\n"),
                // replaces a file of the game
                ("def/vehicle/physics.sii", b"SiiNunit\n{\n}\n"),
                // named by nothing but its own unit name
                ("def/vehicle/truck/fancy.truck/chassis/4x2.sii", CHASSIS),
                // named inside the chassis file, absolutely ...
                ("vehicle/truck/fancy/chassis.pmd", b"\x01\x02/vehicle/truck/fancy/paint.tobj\x00"),
                ("vehicle/truck/fancy/chassis.pmg", b"geometry"),
                ("vehicle/truck/fancy/paint.tobj", b"\x01\x0a\xb1\x70"),
                ("vehicle/truck/fancy/paint.dds", b"DDS pixels"),
                ("sound/truck/fancy.bank", b"RIFFdata"),
                ("sound/truck/fancy.bank.guids", b"guids"),
                // ... and relatively
                ("def/vehicle/truck/fancy.truck/chassis/common.sui", b"@include \"deeper.sui\""),
                ("def/vehicle/truck/fancy.truck/chassis/deeper.sui", b"x: 1"),
                ("def/vehicle/truck/fancy.truck/shared/tires.sui", b"y: 2"),
                // referenced nowhere
                ("def/secret/thing.sii", b"SiiNunit\n{\n}\n"),
            ],
        )
    }

    fn recover(salt: u16) -> (Recovery, Vec<String>) {
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("locked.scs");
        fs::write(&scs, locked(salt)).unwrap();
        let mut dict = GamePaths::build(
            ["def/vehicle/physics.sii", "def/unrelated.sii"].into_iter(),
            "sig",
            &tmp.path().join("paths.bin"),
        )
        .unwrap();
        let mut reader = HashFsReader::open(&scs).unwrap();
        let listed = reader.iter_files();
        let r = recover_names(&mut reader, &listed, Some(&mut dict), &never);
        let mut names: Vec<String> = r.names.values().cloned().collect();
        names.sort();
        (r, names)
    }

    #[test]
    fn names_come_from_the_game_references_and_conventions() {
        let (r, names) = recover(0);
        assert_eq!(
            names,
            [
                "def/vehicle/physics.sii",
                "def/vehicle/truck/fancy.truck/chassis/4x2.sii",
                "def/vehicle/truck/fancy.truck/chassis/common.sui",
                "def/vehicle/truck/fancy.truck/chassis/deeper.sui",
                "def/vehicle/truck/fancy.truck/shared/tires.sui",
                "sound/truck/fancy.bank",
                "sound/truck/fancy.bank.guids",
                "vehicle/truck/fancy/chassis.pmd",
                "vehicle/truck/fancy/chassis.pmg",
                "vehicle/truck/fancy/paint.dds",
                "vehicle/truck/fancy/paint.tobj",
            ]
        );
        assert_eq!((r.by_dictionary, r.by_reference), (1, 10));
        // the one file nothing points at stays nameless, and is counted
        assert_eq!(r.unnamed.len(), 1);
    }

    #[test]
    fn a_salted_archive_is_matched_through_its_salt() {
        let (r, names) = recover(7);
        assert_eq!(r.by_dictionary, 1);
        assert_eq!(names.len(), 11);
        assert_eq!(r.unnamed.len(), 1);
    }

    #[test]
    fn nothing_is_invented_for_a_listed_archive() {
        use crate::scs::hashfs::testutil::build_v1;
        let tmp = tempfile::tempdir().unwrap();
        let scs = tmp.path().join("open.scs");
        fs::write(&scs, build_v1(0, &[("manifest.sii", b"m", false), ("def/a.sii", CHASSIS, false)])).unwrap();
        let mut reader = HashFsReader::open(&scs).unwrap();
        let listed = reader.iter_files();
        let r = recover_names(&mut reader, &listed, None, &never);
        assert!(r.names.is_empty() && r.unnamed.is_empty());
    }

    #[test]
    fn tokens_and_guesses() {
        let tokens = path_tokens(b"model: \"/a/b.pmd\"  x 1.0 3.5 ..\\up\\c.sui |nope| name.truck.id.engine");
        assert_eq!(tokens, ["/a/b.pmd", "../up/c.sui", "name.truck.id.engine"]);
        assert_eq!(relative_guess("def/x/y", "../z/a.sui").as_deref(), Some("def/x/z/a.sui"));
        assert_eq!(relative_guess("def", "../../a.sui"), None);
        assert_eq!(relative_guess("", "a.sui").as_deref(), Some("a.sui"));
        let mut out = Vec::new();
        absolute_guesses("name.truck.id.engine", &mut out);
        assert!(out.contains(&"def/vehicle/truck/truck.id/engine/name.sii".to_string()));
        assert!(out.contains(&"def/vehicle/trailer_owned/truck.id/engine/name.sui".to_string()));
    }

    #[test]
    fn extensions_from_content() {
        assert_eq!(sniff_extension(b"SiiNunit\n{"), "sii");
        assert_eq!(sniff_extension(b"\xef\xbb\xbfSiiNunit"), "sii");
        assert_eq!(sniff_extension(b"3nK\x01\x00\x10\x3b"), "sii");
        assert_eq!(sniff_extension(b"DDS \x7c"), "dds");
        assert_eq!(sniff_extension(b"material : \"eut2.dif\""), "mat");
        assert_eq!(sniff_extension(b"plain words\n"), "txt");
        assert_eq!(sniff_extension(b"\x00\x01\x02"), "bin");
        assert_eq!(sniff_extension(b""), "bin");
    }
}
