//! Fixed ETS2/ATS load-order groups, the row layout shown in the active
//! list, and the automatic sort that puts every mod back into its group.
//!
//! All lists here are in **display order**: top = highest priority = loaded
//! last = the highest `active_mods[]` index in profile.sii.

use serde::Serialize;

pub const MAP_BASE: &str = "map_base";

#[derive(Debug, Clone, Copy, Serialize)]
pub struct LoadOrderGroup {
    pub id: &'static str,
    /// Effective-category tokens that belong to this group.
    pub tokens: &'static [&'static str],
}

/// Top to bottom. Tokens with no group of their own fall into `ui_other`.
pub const GROUPS: &[LoadOrderGroup] = &[
    LoadOrderGroup { id: MAP_BASE, tokens: &[MAP_BASE] },
    LoadOrderGroup { id: "graphics_weather", tokens: &["graphics", "weather_setup"] },
    LoadOrderGroup { id: "sound", tokens: &["sound"] },
    LoadOrderGroup { id: "physics", tokens: &["physics"] },
    LoadOrderGroup {
        id: "ui_other",
        tokens: &["ui", "other", "models", "movers", "walkers", "prefabs"],
    },
    LoadOrderGroup { id: "tuning_interior", tokens: &["tuning_parts", "interior"] },
    LoadOrderGroup { id: "ai_traffic", tokens: &["ai_traffic"] },
    LoadOrderGroup { id: "cargo", tokens: &["cargo_pack"] },
    LoadOrderGroup { id: "paint_jobs", tokens: &["paint_job"] },
    LoadOrderGroup { id: "trailers", tokens: &["trailer"] },
    LoadOrderGroup { id: "trucks", tokens: &["truck"] },
    LoadOrderGroup { id: "maps", tokens: &["map"] },
];

const UI_OTHER_INDEX: usize = 4;

/// Group index for a category token; unmapped tokens go to `ui_other`,
/// never to `map_base`.
pub fn group_index_for_token(token: &str) -> usize {
    GROUPS
        .iter()
        .position(|g| g.tokens.contains(&token))
        .unwrap_or(UI_OTHER_INDEX)
}

pub fn group_index_for_id(group_id: &str) -> Option<usize> {
    GROUPS.iter().position(|g| g.id == group_id)
}

/// First token of a group - the effective token of a mod pinned to it.
pub fn group_repr_token(group_id: &str) -> Option<&'static str> {
    group_index_for_id(group_id).map(|i| GROUPS[i].tokens[0])
}

/// Known "top of load order" mods that belong in the map_base block. They
/// are technically `map` or `ui` but must load last. Matched as
/// case-insensitive substrings so version suffixes still match.
pub const DEFAULT_MAP_BASE_NAMES: &[&str] = &[
    "BXPfix007",
    "ETS2 Global Background Map",
    "FullScreen Maps",
    "Loading Screen",
    "Small HUD Mirrors",
    "Tutorial Hints",
];

pub fn is_map_base<S: AsRef<str>>(mod_name: &str, display_name: &str, names: &[S]) -> bool {
    let haystack = format!("{mod_name}\n{display_name}").to_lowercase();
    names
        .iter()
        .map(|f| f.as_ref())
        .any(|frag| !frag.is_empty() && haystack.contains(&frag.to_lowercase()))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Row {
    /// Group header; present for every group, even an empty one.
    Spacer { group_id: &'static str },
    Mod {
        /// Index into the list passed to [`build_rows`].
        index: usize,
        /// The mod sits below a group that should come after it.
        misplaced: bool,
        expected_group_id: &'static str,
    },
}

/// Turn the active mods' category tokens (display order) into rows with
/// group headers and misplacement flags. Mod order is never changed.
pub fn build_rows<S: AsRef<str>>(tokens: &[S]) -> Vec<Row> {
    let mut rows = vec![Row::Spacer { group_id: GROUPS[0].id }];
    let mut current = 0;
    for (index, token) in tokens.iter().enumerate() {
        let g = group_index_for_token(token.as_ref());
        if g >= current {
            for group in &GROUPS[current + 1..=g] {
                rows.push(Row::Spacer { group_id: group.id });
            }
            current = g;
        }
        rows.push(Row::Mod {
            index,
            misplaced: g < current,
            expected_group_id: GROUPS[g].id,
        });
    }
    for group in &GROUPS[current + 1..] {
        rows.push(Row::Spacer { group_id: group.id });
    }
    rows
}

/// The permutation that sorts mods into their groups: a stable sort by
/// group index, so the relative order inside each group is kept exactly as
/// the user had it. Returns indices into `tokens`.
pub fn auto_sort_order<S: AsRef<str>>(tokens: &[S]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..tokens.len()).collect();
    order.sort_by_key(|&i| group_index_for_token(tokens[i].as_ref()));
    order
}

/// Sort mods into their groups without ever inverting a pair that must
/// keep its order.
///
/// `keep_above` lists pairs `(upper, lower)` of indices into `tokens` -
/// mods that overwrite each other's files, so whichever is higher wins.
/// The current order of such a pair is the user's (working) decision: a
/// patch sits above the mod it patches no matter what category either one
/// declares. Sorting by group alone would flip those, so this is a
/// topological sort instead: of all mods whose required predecessors are
/// already placed, the one with the earliest group (then the earliest
/// current position) goes next.
///
/// Without constraints the result equals [`auto_sort_order`]. With them, a
/// mod held back by a predecessor simply follows it and stays flagged as
/// outside its group - which is correct, and what the caller reports.
/// Pairs that are not `upper < lower` are ignored, so the constraint graph
/// is acyclic by construction.
pub fn auto_sort_preserving<S: AsRef<str>>(tokens: &[S], keep_above: &[(usize, usize)]) -> Vec<usize> {
    let n = tokens.len();
    let groups: Vec<usize> = tokens.iter().map(|t| group_index_for_token(t.as_ref())).collect();
    let mut blockers = vec![0usize; n];
    let mut below: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(upper, lower) in keep_above {
        if upper < lower && lower < n {
            blockers[lower] += 1;
            below[upper].push(lower);
        }
    }
    let mut placed = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for _ in 0..n {
        // n is a load order (tens of mods): a linear scan beats a heap here
        let Some(next) = (0..n)
            .filter(|&i| !placed[i] && blockers[i] == 0)
            .min_by_key(|&i| (groups[i], i))
        else {
            break; // unreachable: every edge goes from a lower to a higher index
        };
        placed[next] = true;
        order.push(next);
        for &lower in &below[next] {
            blockers[lower] -= 1;
        }
    }
    order
}

/// Number of mods [`build_rows`] would flag as misplaced.
pub fn misplaced_count<S: AsRef<str>>(tokens: &[S]) -> usize {
    build_rows(tokens)
        .iter()
        .filter(|r| matches!(r, Row::Mod { misplaced: true, .. }))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_lookup() {
        assert_eq!(GROUPS[UI_OTHER_INDEX].id, "ui_other");
        assert_eq!(group_index_for_token("map_base"), 0);
        assert_eq!(group_index_for_token("map"), GROUPS.len() - 1);
        assert_eq!(group_index_for_token("nonsense"), UI_OTHER_INDEX);
        assert_eq!(group_repr_token("tuning_interior"), Some("tuning_parts"));
        assert_eq!(group_repr_token("nope"), None);
    }

    #[test]
    fn map_base_matching() {
        assert!(is_map_base("bxpfix007_v2", "", DEFAULT_MAP_BASE_NAMES));
        assert!(is_map_base("x", "Nice Loading Screen Pack", DEFAULT_MAP_BASE_NAMES));
        assert!(!is_map_base("promods", "ProMods Map", DEFAULT_MAP_BASE_NAMES));
        assert!(!is_map_base("anything", "x", &[""]));
    }

    #[test]
    fn rows_have_every_spacer_and_flag_misplaced() {
        // sound, then a truck, then a sound mod sitting too low
        let rows = build_rows(&["sound", "truck", "sound"]);
        let spacers = rows.iter().filter(|r| matches!(r, Row::Spacer { .. })).count();
        assert_eq!(spacers, GROUPS.len());
        let mods: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Mod { index, misplaced, expected_group_id } => {
                    Some((*index, *misplaced, *expected_group_id))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            mods,
            vec![(0, false, "sound"), (1, false, "trucks"), (2, true, "sound")]
        );
        assert_eq!(misplaced_count(&["sound", "truck", "sound"]), 1);
    }

    #[test]
    fn empty_list_is_all_spacers() {
        assert_eq!(build_rows::<&str>(&[]).len(), GROUPS.len());
    }

    #[test]
    fn preserving_sort_equals_plain_sort_without_constraints() {
        let tokens = ["truck", "sound", "map", "truck", "map_base", "sound"];
        assert_eq!(auto_sort_preserving(&tokens, &[]), auto_sort_order(&tokens));
        assert!(auto_sort_preserving::<&str>(&[], &[]).is_empty());
    }

    #[test]
    fn preserving_sort_keeps_a_patch_above_what_it_patches() {
        // a "truck" patch sits above the "other" mod it patches; by group
        // alone the patched mod (ui_other) would jump above the patch
        let tokens = ["truck", "other", "sound", "map"];
        assert_eq!(auto_sort_order(&tokens), vec![2, 1, 0, 3]);
        let order = auto_sort_preserving(&tokens, &[(0, 1)]);
        assert_eq!(order, vec![2, 0, 1, 3]);
        let pos = |i: usize| order.iter().position(|&x| x == i).unwrap();
        assert!(pos(0) < pos(1));
        // the held mod is still reported as outside its group
        let sorted: Vec<&str> = order.iter().map(|&i| tokens[i]).collect();
        assert_eq!(misplaced_count(&sorted), 1);
    }

    #[test]
    fn preserving_sort_respects_every_constraint_and_is_a_permutation() {
        let tokens = ["map", "truck", "sound", "other", "physics", "truck", "graphics", "sound", "map"];
        let keep: Vec<(usize, usize)> = vec![(0, 2), (1, 3), (3, 4), (5, 7), (2, 7), (0, 8)];
        let order = auto_sort_preserving(&tokens, &keep);
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..tokens.len()).collect::<Vec<_>>());
        let pos = |i: usize| order.iter().position(|&x| x == i).unwrap();
        for (upper, lower) in keep {
            assert!(pos(upper) < pos(lower), "{upper} must stay above {lower}: {order:?}");
        }
        // sorting again changes nothing
        let sorted: Vec<&str> = order.iter().map(|&i| tokens[i]).collect();
        let again_keep: Vec<(usize, usize)> = [(0, 2), (1, 3), (3, 4), (5, 7), (2, 7), (0, 8)]
            .iter()
            .map(|&(u, l)| (pos(u), pos(l)))
            .collect();
        assert_eq!(auto_sort_preserving(&sorted, &again_keep), (0..tokens.len()).collect::<Vec<_>>());
    }

    #[test]
    fn preserving_sort_ignores_backward_and_out_of_range_pairs() {
        let tokens = ["truck", "sound"];
        assert_eq!(auto_sort_preserving(&tokens, &[(1, 0), (0, 9), (1, 1)]), vec![1, 0]);
    }

    #[test]
    fn auto_sort_is_stable_and_clears_misplacement() {
        let tokens = ["truck", "sound", "map", "truck", "map_base", "sound"];
        let order = auto_sort_order(&tokens);
        assert_eq!(order, vec![4, 1, 5, 0, 3, 2]);
        let sorted: Vec<&str> = order.iter().map(|&i| tokens[i]).collect();
        assert_eq!(misplaced_count(&sorted), 0);
    }
}
