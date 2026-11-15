//! Which active mods belong together: a mod and the patches and add-ons
//! made for it.
//!
//! A mod cannot declare that it needs another mod (`manifest.sii` only has
//! `dlc_dependencies`), so this is read from the files. A mod that
//! overwrites a file which exists only because another mod brings it (the
//! game has no such file) was made for that mod: a chassis patch for a
//! truck, a road connection for a map. Two mods that merely replace the
//! same file of the game are a conflict to order, not a relation - neither
//! needs the other. The scanner keeps the fingerprint this takes
//! ([`ScannedMod::shares_own_files`]); the load order says which of the
//! two is the base: the lower one, whose file is overwritten.
//!
//! This is a hint, like the conflict report. A patch that only adds new
//! files to its base is not seen, and a shared library placed above the
//! mods that use it looks like their patch.
//!
//! Pure index logic over the active list in display order (top first).
//!
//! [`ScannedMod::shares_own_files`]: crate::scanner::ScannedMod::shares_own_files

use serde::Serialize;

/// The relations of one mod, as indices into the active list, top first.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModRelations {
    /// Mods above this one that overwrite files only it brings: its
    /// patches and add-ons.
    pub patches: Vec<usize>,
    /// Mods below this one whose own files it overwrites: what it was
    /// made for.
    pub bases: Vec<usize>,
    /// Every mod that builds on this one, directly or through another
    /// patch - what may stop working when this mod is taken out.
    pub dependents: Vec<usize>,
}

impl ModRelations {
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty() && self.bases.is_empty()
    }
}

/// Relations among `count` active mods. `related(upper, lower)` is asked
/// once per pair with `upper < lower` and tells whether the two share a
/// file that only one of them brings.
pub fn analyze(count: usize, related: impl Fn(usize, usize) -> bool) -> Vec<ModRelations> {
    let mut out = vec![ModRelations::default(); count];
    for upper in 0..count {
        for lower in upper + 1..count {
            if related(upper, lower) {
                out[upper].bases.push(lower);
                out[lower].patches.push(upper);
            }
        }
    }
    // a patch always sits above its base, so going down the list every
    // patch already knows its own dependents
    for index in 0..count {
        let mut reached = vec![false; index];
        for &patch in &out[index].patches {
            reached[patch] = true;
            for &further in &out[patch].dependents {
                reached[further] = true;
            }
        }
        out[index].dependents = (0..index).filter(|&i| reached[i]).collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(count: usize, pairs: &[(usize, usize)]) -> Vec<ModRelations> {
        analyze(count, |upper, lower| pairs.contains(&(upper, lower)))
    }

    #[test]
    fn unrelated_mods_have_no_relations() {
        assert!(run(3, &[]).iter().all(ModRelations::is_empty));
        assert!(run(0, &[]).is_empty());
    }

    #[test]
    fn the_upper_mod_is_the_patch_of_the_lower_one() {
        let r = run(3, &[(0, 2)]);
        assert_eq!(r[0], ModRelations { patches: vec![], bases: vec![2], dependents: vec![] });
        assert!(r[1].is_empty() && r[1].dependents.is_empty());
        assert_eq!(r[2], ModRelations { patches: vec![0], bases: vec![], dependents: vec![0] });
    }

    #[test]
    fn dependents_follow_a_chain_of_patches() {
        // 3 is a truck, 2 fixes it, 0 patches the fix; 1 is something else
        let r = run(4, &[(2, 3), (0, 2)]);
        assert_eq!(r[3].patches, [2]);
        assert_eq!(r[3].dependents, [0, 2]);
        assert_eq!(r[2].dependents, [0]);
        assert_eq!((r[2].patches.as_slice(), r[2].bases.as_slice()), (&[0][..], &[3][..]));
        assert!(r[0].dependents.is_empty() && r[1].is_empty());
    }

    #[test]
    fn a_mod_reached_twice_is_listed_once_in_list_order() {
        // 0 patches both 1 and 2, and 1 patches 3, 2 patches 3
        let r = run(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        assert_eq!(r[3].dependents, [0, 1, 2]);
        assert_eq!(r[0].bases, [1, 2]);
        // taking out a patch never involves its base
        assert!(r[0].dependents.is_empty());
        assert_eq!(r[1].dependents, [0]);
    }
}
