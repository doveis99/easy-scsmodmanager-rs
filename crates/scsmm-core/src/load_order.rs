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
            for gi in current + 1..=g {
                rows.push(Row::Spacer { group_id: GROUPS[gi].id });
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
    fn auto_sort_is_stable_and_clears_misplacement() {
        let tokens = ["truck", "sound", "map", "truck", "map_base", "sound"];
        let order = auto_sort_order(&tokens);
        assert_eq!(order, vec![4, 1, 5, 0, 3, 2]);
        let sorted: Vec<&str> = order.iter().map(|&i| tokens[i]).collect();
        assert_eq!(misplaced_count(&sorted), 0);
    }
}
