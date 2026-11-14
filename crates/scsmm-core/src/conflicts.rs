//! Detect active mods that overwrite the same `def/` file.
//!
//! Two active mods conflict when their archives both carry the same
//! `def/...` path: whichever sits higher in the load order wins, the
//! other's version is shadowed. This is a HINT, never an error - for maps
//! an overlap is often intentional and the load order resolves it.
//!
//! Pure set logic over the def file lists captured at scan time (already
//! normalised for the 1.48 `base/` / `dlc_*/` package layers); no archive
//! I/O here. An inverted index (def path -> owners) keeps the cost
//! proportional to the number of shared files, not the square of the mod
//! count.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

/// A def path owned by more than this many active mods is a generic
/// override (climate, economy, traffic rules ...) that nearly every map
/// touches. Reporting those as conflicts would drown the real hints, so
/// they go to the neutral "frequently shared" tier instead.
pub const GENERIC_OWNER_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Wins some of its shared files, loses others.
    Partial,
    /// Loses every shared file - its overrides are effectively dead.
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LostFile {
    pub file: String,
    /// Name of the mod that wins this file (the top owner).
    pub winner: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModOverride {
    pub severity: Severity,
    /// The files this mod loses, sorted by path.
    pub lost: Vec<LostFile>,
    /// How many shared files this mod has in total (lost + won).
    pub shared_total: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FrequentFile {
    pub file: String,
    /// Total number of active mods carrying this file.
    pub owners: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ConflictReport {
    /// Mods that lose at least one shared file, keyed by name.
    pub overrides: BTreeMap<String, ModOverride>,
    /// Usually-intended overlaps shared by more than the generic limit of
    /// mods - neutral info, never a conflict. Sorted by path.
    pub frequent: BTreeMap<String, Vec<FrequentFile>>,
}

/// Analyse the active mods, given top-first (index 0 = highest in the load
/// order = wins). Each entry is `(name, def file paths)`; names must be
/// unique. A mod that wins all of its shared files is absent from
/// `overrides`.
pub fn analyze<S: AsRef<str>>(active_top_first: &[(&str, &[S])]) -> ConflictReport {
    // def path -> indices of its owners, in load order (top first)
    let mut owners: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, (_, defs)) in active_top_first.iter().enumerate() {
        let mut seen: HashSet<&str> = HashSet::new();
        for path in defs.iter().map(|p| p.as_ref()) {
            // a directory entry is never a real overlap
            if path.ends_with('/') || !seen.insert(path) {
                continue;
            }
            owners.entry(path).or_default().push(index);
        }
    }

    // per mod: (lost files with their winner, number of shared files)
    let mut lost: HashMap<usize, Vec<LostFile>> = HashMap::new();
    let mut shared_count: HashMap<usize, usize> = HashMap::new();
    let mut frequent: BTreeMap<String, Vec<FrequentFile>> = BTreeMap::new();
    for (path, indices) in &owners {
        if indices.len() < 2 {
            continue;
        }
        if indices.len() > GENERIC_OWNER_LIMIT {
            for &i in indices {
                frequent
                    .entry(active_top_first[i].0.to_string())
                    .or_default()
                    .push(FrequentFile {
                        file: path.to_string(),
                        owners: indices.len(),
                    });
            }
            continue;
        }
        // indices were pushed in load order, so the first one is the top owner
        let winner = indices[0];
        for &i in indices {
            *shared_count.entry(i).or_default() += 1;
            if i != winner {
                lost.entry(i).or_default().push(LostFile {
                    file: path.to_string(),
                    winner: active_top_first[winner].0.to_string(),
                });
            }
        }
    }

    let mut report = ConflictReport {
        frequent,
        ..Default::default()
    };
    for files in report.frequent.values_mut() {
        files.sort_by(|a, b| a.file.cmp(&b.file));
    }
    for (index, mut files) in lost {
        files.sort_by(|a, b| a.file.cmp(&b.file));
        let shared_total = shared_count[&index];
        report.overrides.insert(
            active_top_first[index].0.to_string(),
            ModOverride {
                severity: if files.len() == shared_total {
                    Severity::Full
                } else {
                    Severity::Partial
                },
                lost: files,
                shared_total,
            },
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mods: &[(&str, &[&str])]) -> ConflictReport {
        analyze(mods)
    }

    #[test]
    fn no_overlap_no_report() {
        let r = run(&[("a", &["def/a.sii"]), ("b", &["def/b.sii"])]);
        assert!(r.overrides.is_empty() && r.frequent.is_empty());
    }

    #[test]
    fn top_mod_wins_lower_mod_is_fully_overridden() {
        let r = run(&[("top", &["def/x.sii", "def/own.sii"]), ("low", &["def/x.sii"])]);
        assert!(!r.overrides.contains_key("top"));
        let low = &r.overrides["low"];
        assert_eq!(low.severity, Severity::Full);
        assert_eq!(low.shared_total, 1);
        assert_eq!(
            low.lost,
            vec![LostFile { file: "def/x.sii".into(), winner: "top".into() }]
        );
    }

    #[test]
    fn partial_when_a_mod_wins_some_and_loses_some() {
        let r = run(&[
            ("top", &["def/a.sii"]),
            ("mid", &["def/a.sii", "def/b.sii"]),
            ("low", &["def/b.sii", "def/a.sii"]),
        ]);
        let mid = &r.overrides["mid"];
        assert_eq!(mid.severity, Severity::Partial);
        assert_eq!(mid.lost.len(), 1);
        assert_eq!(mid.lost[0].winner, "top");
        let low = &r.overrides["low"];
        assert_eq!(low.severity, Severity::Full);
        // sorted by path, each with the top owner of that path
        assert_eq!(
            low.lost.iter().map(|l| (l.file.as_str(), l.winner.as_str())).collect::<Vec<_>>(),
            vec![("def/a.sii", "top"), ("def/b.sii", "mid")]
        );
    }

    #[test]
    fn reordering_flips_the_winner() {
        let r = run(&[("low", &["def/x.sii"]), ("top", &["def/x.sii"])]);
        assert!(r.overrides.contains_key("top"));
        assert!(!r.overrides.contains_key("low"));
    }

    #[test]
    fn directory_entries_and_duplicates_are_ignored() {
        let r = run(&[
            ("a", &["def/", "def/x.sii", "def/x.sii"]),
            ("b", &["def/", "def/y.sii"]),
        ]);
        assert!(r.overrides.is_empty());
    }

    #[test]
    fn widely_shared_files_are_frequent_not_conflicts() {
        let names: Vec<String> = (0..GENERIC_OWNER_LIMIT + 1).map(|i| format!("m{i}")).collect();
        let defs: &[&str] = &["def/climate.sii"];
        let mods: Vec<(&str, &[&str])> = names.iter().map(|n| (n.as_str(), defs)).collect();
        let r = analyze(&mods);
        assert!(r.overrides.is_empty());
        assert_eq!(r.frequent.len(), GENERIC_OWNER_LIMIT + 1);
        assert_eq!(
            r.frequent["m3"],
            vec![FrequentFile { file: "def/climate.sii".into(), owners: GENERIC_OWNER_LIMIT + 1 }]
        );

        // exactly at the limit it is still a conflict
        let mods: Vec<(&str, &[&str])> =
            names[..GENERIC_OWNER_LIMIT].iter().map(|n| (n.as_str(), defs)).collect();
        let r = analyze(&mods);
        assert!(r.frequent.is_empty());
        assert_eq!(r.overrides.len(), GENERIC_OWNER_LIMIT - 1);
    }
}
