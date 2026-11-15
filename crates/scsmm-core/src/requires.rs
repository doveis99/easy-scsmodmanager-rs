//! What an active mod needs that the load order does not have.
//!
//! A mod cannot declare that it needs another mod, but its files say so:
//! a definition that names a model, a material that names a texture. The
//! scanner keeps every such path that is neither the mod's own nor the
//! game's ([`ScannedMod::wants`]). Where no active mod brings such a file,
//! something is missing from the load order:
//!
//! * another installed mod has it and is not active - a package of the
//!   same map left out, the base of an add-on;
//! * nothing that is installed has it - a package that was never
//!   downloaded.
//!
//! This is a hint, like the conflict report. Every larger mod names a few
//! files that exist nowhere (leftovers of its making), so a handful of
//! missing files says nothing; the thresholds below ask for enough of them.
//! And what a mod needs without naming a path is not seen at all: a map's
//! sectors refer to definitions by name, so a missing definition package
//! only shows through the other packages that name its files.
//!
//! [`ScannedMod::wants`]: crate::scanner::ScannedMod::wants

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::scanner::ScannedMod;
use crate::scs::cityhash::city_hash64;

/// An installed mod is offered as the missing one when it brings at least
/// this many of the files a mod needs ...
const MIN_PROVIDED: usize = 5;
/// ... and at least this share of them, in percent. A big mod names a few
/// files that some unrelated mod happens to carry under the same path.
const MIN_PROVIDED_PERCENT: usize = 1;

/// Files that nothing installed brings are reported from this many on ...
const MIN_ABSENT: usize = 20;
/// ... when they are this share (percent) of all the paths the mod names;
/// complete map packs were seen with up to 15% dead references ...
const MIN_ABSENT_PERCENT: usize = 30;
/// ... or when this many of them sit in one top-level folder that neither
/// the game nor any installed mod has - a whole package is not there. A
/// map without its model package misses hundreds of files there; a
/// complete one was seen with 76 leftovers in such a folder.
const MIN_ABSENT_IN_FOLDER: usize = 100;

/// Examples of missing files kept for showing.
const MAX_EXAMPLES: usize = 8;

/// The game's own top-level folders. A missing file inside one of them can
/// be a leftover; a folder of any other name was brought by a mod.
const GAME_DIRS: &[&str] = &[
    "automat", "def", "dlc", "effect", "font", "locale", "map", "material", "model", "model2", "prefab", "prefab2",
    "road_template", "sound", "system", "ui", "unit", "vehicle", "video",
];

/// An installed mod that is not active and brings files an active one needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    /// Index into the scanned mods.
    pub index: usize,
    /// How many of the needed files it brings.
    pub files: usize,
}

/// Needed files that no installed mod brings.
#[derive(Debug, Clone, PartialEq)]
pub struct Absent {
    pub files: usize,
    /// Top-level folders (most files first) that nothing installed has.
    pub folders: Vec<String>,
    /// A few of the missing paths.
    pub examples: Vec<String>,
}

/// What one active mod misses in the load order.
#[derive(Debug, Clone, PartialEq)]
pub struct Missing {
    /// Installed mods to activate, the one bringing most first.
    pub inactive: Vec<Provider>,
    pub absent: Option<Absent>,
    /// How many different paths the mod names in all.
    pub named: usize,
}

/// The needed files of one mod, grouped by who brings them.
#[derive(Default)]
struct Needs {
    /// (the mods that have the file, how many such files). An empty list
    /// of mods is not kept here but in `absent`.
    by_providers: Vec<(Vec<usize>, usize)>,
    absent: usize,
    /// (folder, files) for the absent files in a folder nothing has.
    absent_folders: Vec<(String, usize)>,
    absent_examples: Vec<String>,
    wanted: usize,
    named: usize,
}

/// The needed files of one mod while the index is built: how many there
/// are per list of mods that have them, and the ones nobody has.
type Sorted<'a> = (HashMap<&'a [usize], usize>, Vec<&'a String>);

/// The top-level folder of a path.
fn top_of(path: &str) -> &str {
    path.split_once('/').map_or("", |(top, _)| top)
}

/// Who brings what the installed mods need. Built once per scan; asking
/// it about a load order is cheap.
#[derive(Default)]
pub struct Requirements {
    needs: Vec<Needs>,
}

impl Requirements {
    pub fn build(mods: &[ScannedMod]) -> Self {
        // hash of every needed path, then one pass over what the mods bring
        let wanted: HashSet<u64> =
            mods.iter().flat_map(|m| m.wants.iter()).map(|path| city_hash64(path.as_bytes())).collect();
        let mut providers: HashMap<u64, Vec<usize>> = HashMap::new();
        if !wanted.is_empty() {
            for (index, m) in mods.iter().enumerate() {
                for hash in m.provides.iter().filter(|hash| wanted.contains(hash)) {
                    providers.entry(*hash).or_default().push(index);
                }
            }
        }
        // per mod: its needed files by who has them, and the ones nobody has
        let mut known_dirs: HashSet<&str> =
            GAME_DIRS.iter().copied().chain(mods.iter().flat_map(|m| m.top_dirs.iter().map(String::as_str))).collect();
        let mut sorted: Vec<Sorted> = Vec::with_capacity(mods.len());
        for (index, m) in mods.iter().enumerate() {
            let mut groups: HashMap<&[usize], usize> = HashMap::new();
            let mut absent: Vec<&String> = Vec::new();
            for path in m.wants.iter() {
                match providers.get(&city_hash64(path.as_bytes())) {
                    // a mod's own files are not in its `wants`, but a
                    // locked archive can name a file it holds unlisted
                    Some(list) if list.iter().any(|&p| p != index) => {
                        *groups.entry(list.as_slice()).or_default() += 1;
                        // a locked archive does not list its folders;
                        // a file found in one shows the folder is there
                        known_dirs.insert(top_of(path));
                    }
                    Some(_) => {}
                    None => absent.push(path),
                }
            }
            sorted.push((groups, absent));
        }

        let needs = mods
            .iter()
            .zip(sorted)
            .enumerate()
            .map(|(index, (m, (groups, absent)))| {
                let mut folders: BTreeMap<&str, usize> = BTreeMap::new();
                for path in &absent {
                    let top = top_of(path);
                    if !known_dirs.contains(top) {
                        *folders.entry(top).or_default() += 1;
                    }
                }
                let mut absent_folders: Vec<(String, usize)> =
                    folders.iter().map(|(folder, files)| (folder.to_string(), *files)).collect();
                absent_folders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                // the telling examples first: files of a folder nothing has
                let telling = |path: &&&String| folders.get(top_of(path)).is_some_and(|n| *n >= MIN_ABSENT_IN_FOLDER);
                let absent_examples = absent
                    .iter()
                    .filter(telling)
                    .chain(absent.iter().filter(|path| !telling(path)))
                    .take(MAX_EXAMPLES)
                    .map(|path| path.to_string())
                    .collect();
                let mut by_providers: Vec<(Vec<usize>, usize)> = groups
                    .into_iter()
                    .map(|(list, files)| (list.iter().copied().filter(|&p| p != index).collect(), files))
                    .collect();
                by_providers.sort();
                Needs {
                    by_providers,
                    absent: absent.len(),
                    absent_folders,
                    absent_examples,
                    wanted: m.wants.len(),
                    named: m.named_paths,
                }
            })
            .collect();
        Self { needs }
    }

    /// Whether this was built for a scan of `count` mods.
    pub fn covers(&self, count: usize) -> bool {
        self.needs.len() == count
    }

    /// What mod `index` misses when exactly the mods `is_active` says are
    /// in the load order. `None` when nothing worth telling is missing.
    pub fn missing(&self, index: usize, is_active: impl Fn(usize) -> bool) -> Option<Missing> {
        let needs = self.needs.get(index)?;

        // files no active mod brings, by the inactive mods that could
        let mut open: Vec<(&[usize], usize)> = needs
            .by_providers
            .iter()
            .filter(|(list, _)| !list.iter().any(|&p| is_active(p)))
            .map(|(list, files)| (list.as_slice(), *files))
            .collect();
        // the same file is often in several mods (one map carrying a copy
        // of another's models): take the mod that settles most, then look
        // at what is still open
        let enough = |files: usize| files >= MIN_PROVIDED && files * 100 >= needs.wanted * MIN_PROVIDED_PERCENT;
        let mut inactive = Vec::new();
        loop {
            let mut totals: BTreeMap<usize, usize> = BTreeMap::new();
            for (list, files) in &open {
                for &p in *list {
                    *totals.entry(p).or_default() += files;
                }
            }
            // most files first; the earlier mod on a tie
            let Some((&best, &files)) = totals.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))) else {
                break;
            };
            if !enough(files) {
                break;
            }
            inactive.push(Provider { index: best, files });
            open.retain(|(list, _)| !list.contains(&best));
        }

        let in_folders: usize = needs
            .absent_folders
            .iter()
            .filter(|(_, files)| *files >= MIN_ABSENT_IN_FOLDER)
            .map(|(_, files)| files)
            .sum();
        let telling =
            needs.absent >= MIN_ABSENT && (needs.absent * 100 >= needs.named * MIN_ABSENT_PERCENT || in_folders > 0);
        let absent = telling.then(|| Absent {
            files: needs.absent,
            folders: needs
                .absent_folders
                .iter()
                .filter(|(_, files)| *files >= MIN_ABSENT_IN_FOLDER)
                .map(|(folder, _)| folder.clone())
                .collect(),
            examples: needs.absent_examples.clone(),
        });

        if inactive.is_empty() && absent.is_none() {
            return None;
        }
        Some(Missing { inactive, absent, named: needs.named })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    /// A scanned mod that has `files` and names `wants` besides them.
    fn scanned(name: &str, files: &[&str], wants: &[String]) -> ScannedMod {
        let mut m = crate::scanner::scan_one(Path::new(name), None);
        let mut provides: Vec<u64> = files.iter().map(|f| city_hash64(f.as_bytes())).collect();
        provides.sort_unstable();
        m.provides = Arc::from(provides);
        let mut dirs: Vec<String> =
            files.iter().filter_map(|f| f.split_once('/')).map(|(d, _)| d.to_string()).collect();
        dirs.sort();
        dirs.dedup();
        m.top_dirs = dirs;
        let mut sorted = wants.to_vec();
        sorted.sort();
        m.named_paths = files.len() + sorted.len();
        m.wants = Arc::from(sorted);
        m
    }

    fn paths(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| format!("{prefix}/file_{i:03}.pmd")).collect()
    }

    fn refs(list: &[String]) -> Vec<&str> {
        list.iter().map(String::as_str).collect()
    }

    #[test]
    fn nothing_is_missing_while_the_provider_is_active() {
        let models = paths("model/pack", 40);
        let mods = [scanned("def", &["def/a.sii"], &models), scanned("models", &refs(&models), &[])];
        let req = Requirements::build(&mods);
        assert!(req.covers(2));
        assert_eq!(req.missing(0, |_| true), None);
        assert_eq!(req.missing(1, |_| true), None);
    }

    #[test]
    fn an_installed_mod_left_out_of_the_load_order_is_named() {
        let models = paths("model/pack", 40);
        let mods = [scanned("def", &["def/a.sii"], &models), scanned("models", &refs(&models), &[])];
        let req = Requirements::build(&mods);
        let missing = req.missing(0, |i| i == 0).unwrap();
        assert_eq!(missing.inactive, [Provider { index: 1, files: 40 }]);
        assert_eq!(missing.absent, None);
    }

    #[test]
    fn a_copy_of_the_files_in_another_active_mod_is_enough() {
        // two maps carry the same models; either one settles the need
        let models = paths("model/pack", 40);
        let mods = [
            scanned("def", &["def/a.sii"], &models),
            scanned("models", &refs(&models), &[]),
            scanned("copy", &refs(&models), &[]),
        ];
        let req = Requirements::build(&mods);
        assert_eq!(req.missing(0, |i| i != 1), None);
        // with neither active only one of the two is asked for
        let missing = req.missing(0, |i| i == 0).unwrap();
        assert_eq!(missing.inactive, [Provider { index: 1, files: 40 }]);
    }

    #[test]
    fn several_missing_packages_are_listed_by_what_they_bring() {
        let models = paths("model/pack", 30);
        let assets = paths("material/pack", 60);
        let both: Vec<String> = models.iter().chain(&assets).cloned().collect();
        let mods = [
            scanned("def", &["def/a.sii"], &both),
            scanned("models", &refs(&models), &[]),
            scanned("assets", &refs(&assets), &[]),
        ];
        let req = Requirements::build(&mods);
        let missing = req.missing(0, |i| i == 0).unwrap();
        assert_eq!(missing.inactive, [Provider { index: 2, files: 60 }, Provider { index: 1, files: 30 }]);
        // one of them active: only the other is asked for
        let missing = req.missing(0, |i| i != 1).unwrap();
        assert_eq!(missing.inactive, [Provider { index: 1, files: 30 }]);
    }

    #[test]
    fn a_few_files_an_unrelated_mod_happens_to_have_do_not_count() {
        // 3 of 500 needed files sit in some inactive mod: under both limits
        let wanted = paths("model/pack", 500);
        let chance: Vec<&str> = wanted[..3].iter().map(String::as_str).collect();
        let rest: Vec<&str> = wanted[3..].iter().map(String::as_str).collect();
        let mods =
            [scanned("def", &["def/a.sii"], &wanted), scanned("other", &chance, &[]), scanned("models", &rest, &[])];
        let req = Requirements::build(&mods);
        assert_eq!(req.missing(0, |i| i != 1), None);
        // enough files, but far below one percent of what the mod needs
        let many = paths("model/pack", 2000);
        let some: Vec<&str> = many[..8].iter().map(String::as_str).collect();
        let most: Vec<&str> = many[8..].iter().map(String::as_str).collect();
        let mods = [scanned("def", &["def/a.sii"], &many), scanned("other", &some, &[]), scanned("models", &most, &[])];
        let req = Requirements::build(&mods);
        assert_eq!(req.missing(0, |i| i != 1), None);
    }

    #[test]
    fn files_nothing_installed_has_are_reported_when_they_are_many() {
        // most of what the mod names exists nowhere
        let gone = paths("model/pack", 60);
        let mods = [scanned("def", &["def/a.sii"], &gone)];
        let req = Requirements::build(&mods);
        let missing = req.missing(0, |_| true).unwrap();
        let absent = missing.absent.unwrap();
        assert_eq!(absent.files, 60);
        // "model" is a folder of the game: no folder stands out
        assert!(absent.folders.is_empty());
        assert_eq!(absent.examples.len(), MAX_EXAMPLES);
        assert!(missing.inactive.is_empty());
    }

    #[test]
    fn dead_references_of_a_complete_mod_are_not_reported() {
        // 60 files that exist nowhere among 1000 named paths, scattered
        // over folders of the game
        let own: Vec<String> = paths("model/own", 940);
        let dead = paths("model/old", 60);
        let mods = [scanned("big", &refs(&own), &dead)];
        let req = Requirements::build(&mods);
        assert_eq!(req.missing(0, |_| true), None);
    }

    #[test]
    fn a_folder_nothing_has_tells_even_at_a_small_share() {
        // 150 files of a folder no mod and not the game has, and 10 stray
        // ones, among 3000
        let own: Vec<String> = paths("model/own", 2840);
        let mut wanted = paths("m0d3l/other", 150);
        wanted.extend(paths("model/old", 10));
        let mods = [scanned("map", &refs(&own), &wanted)];
        let req = Requirements::build(&mods);
        let absent = req.missing(0, |_| true).unwrap().absent.unwrap();
        assert_eq!(absent.files, 160);
        assert_eq!(absent.folders, ["m0d3l"]);
        assert!(absent.examples.iter().all(|e| e.starts_with("m0d3l/")));

        // the folder exists in an installed mod: those files are leftovers
        let with_folder = [scanned("map", &refs(&own), &wanted), scanned("pack", &["m0d3l/other/else.pmd"], &[])];
        let req = Requirements::build(&with_folder);
        assert_eq!(req.missing(0, |_| true), None);

        // a locked archive lists no folders, but another mod finds a file
        // of that folder in it: the folder is there all the same
        let mut locked = scanned("locked", &["m0d3l/new/shop.pmd"], &[]);
        locked.top_dirs.clear();
        let with_locked = [
            scanned("map", &refs(&own), &wanted),
            locked,
            scanned("def", &["def/a.sii"], &["m0d3l/new/shop.pmd".to_string()]),
        ];
        let req = Requirements::build(&with_locked);
        assert_eq!(req.missing(0, |_| true), None);

        // fewer than a package's worth in such a folder says nothing
        let few = [scanned("map", &refs(&own), &paths("sign/demo", 76))];
        assert_eq!(Requirements::build(&few).missing(0, |_| true), None);
    }

    #[test]
    fn a_stale_index_answers_nothing() {
        let req = Requirements::build(&[]);
        assert!(req.covers(0) && !req.covers(1));
        assert_eq!(req.missing(3, |_| true), None);
    }
}
