//! Mirrors the game's mod compatibility check - it never judges on its own.
//!
//! The game only marks a mod red because of its `compatible_versions[]`.
//! No key means "treated as compatible", even if it factually is not: a
//! player who deliberately runs an older mod must not see it falsely
//! flagged. Patterns use the `<major>.<minor>.*` form (`1.59.*`); the game
//! version comes from game.log.txt as `1.59.1.3`.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatStatus {
    /// Key present and matches.
    Compatible,
    /// Key present, none match - what the game marks red.
    Incompatible,
    /// Key empty or absent - not red, treated as usable.
    Unspecified,
    /// The game version could not be determined.
    UnknownGameVersion,
}

/// Segment-wise match. `*` is a wildcard for the rest; other segments must
/// equal the game's segment at the same position. A pattern shorter than
/// the game version (`1.59` vs `1.59.1.3`) matches as a prefix.
fn matches(game_segments: &[&str], pattern: &str) -> bool {
    for (i, seg) in pattern.split('.').enumerate() {
        if seg == "*" {
            return true;
        }
        if game_segments.get(i) != Some(&seg) {
            return false;
        }
    }
    true
}

/// The four-state compatibility verdict.
pub fn compat_status<S: AsRef<str>>(game_version: Option<&str>, compatible_versions: &[S]) -> CompatStatus {
    let patterns: Vec<&str> = compatible_versions.iter().map(|p| p.as_ref()).filter(|p| !p.is_empty()).collect();
    if patterns.is_empty() {
        return CompatStatus::Unspecified;
    }
    let Some(version) = game_version.filter(|v| !v.is_empty()) else {
        return CompatStatus::UnknownGameVersion;
    };
    let segments: Vec<&str> = version.split('.').collect();
    if patterns.iter().any(|p| matches(&segments, p)) {
        CompatStatus::Compatible
    } else {
        CompatStatus::Incompatible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_states() {
        let v = Some("1.59.1.3");
        assert_eq!(compat_status(v, &["1.59.*"]), CompatStatus::Compatible);
        assert_eq!(compat_status(v, &["1.57.*", "1.58.*"]), CompatStatus::Incompatible);
        assert_eq!(compat_status(v, &["1.58.*", "1.59.*"]), CompatStatus::Compatible);
        assert_eq!(compat_status::<&str>(v, &[]), CompatStatus::Unspecified);
        assert_eq!(compat_status(v, &[""]), CompatStatus::Unspecified);
        assert_eq!(compat_status(None, &["1.59.*"]), CompatStatus::UnknownGameVersion);
        assert_eq!(compat_status(Some(""), &["1.59.*"]), CompatStatus::UnknownGameVersion);
    }

    #[test]
    fn segment_matching() {
        let v = Some("1.59.1.3");
        assert_eq!(compat_status(v, &["1.59"]), CompatStatus::Compatible); // prefix
        assert_eq!(compat_status(v, &["1.59.1.3"]), CompatStatus::Compatible);
        assert_eq!(compat_status(v, &["1.59.1.3.0"]), CompatStatus::Incompatible);
        assert_eq!(compat_status(v, &["*"]), CompatStatus::Compatible);
        // segments compare whole, not as string prefixes
        assert_eq!(compat_status(v, &["1.5.*"]), CompatStatus::Incompatible);
        assert_eq!(compat_status(Some("1.6.0"), &["1.60.*"]), CompatStatus::Incompatible);
    }
}
