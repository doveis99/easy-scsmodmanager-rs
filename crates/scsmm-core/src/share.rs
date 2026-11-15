//! Sharing load orders between users, as files.
//!
//! Two formats, both compatible with the Python original:
//!
//! * **ModShare** (`*.modshare.json`) - the whole active list of a profile.
//! * **MapCombo** (`*.json`) - only the mods in the Maps group, in order.
//!
//! The match key is always the mod's internal name from profile.sii (the
//! `active_mods[]` token); display names travel along so a "missing mod"
//! message is readable. Everything here is pure: serialise, parse, compare
//! against what the receiver has installed, reorder.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::game::Game;
use crate::identity::{workshop_id_from_active_name, workshop_url};
use crate::profile::{ActiveMod, Profile};
use crate::{format_err, Result};

pub const MODSHARE_FORMAT: &str = "easy-scsmodmanager-modshare";
pub const MODSHARE_VERSION: u64 = 1;
pub const MODSHARE_SUFFIX: &str = ".modshare.json";
pub const MAPCOMBO_FORMAT: &str = "easy-scsmodmanager-mapcombo";
/// v2 added the per-entry package_version; v1 files still load.
pub const MAPCOMBO_VERSION: u64 = 2;

/// Compare two mod `package_version` strings numerically.
///
/// The version is third-party data and unreliable, so the comparison is
/// deliberately strict: every dot-segment must be a plain integer.
/// Anything else (`1.59.b`, `v2.4-final`, empty) is "not comparable" - a
/// mod is never guessed to be older or newer. Segments compare as numbers
/// (`2.10 > 2.2`) and a shorter version is zero-padded (`2.0 == 2.0.0`).
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    fn parse(v: &str) -> Option<Vec<u64>> {
        if v.is_empty() {
            return None;
        }
        v.split('.')
            .map(|seg| {
                (!seg.is_empty() && seg.bytes().all(|c| c.is_ascii_digit())).then(|| seg.parse::<u64>().ok()).flatten()
            })
            .collect()
    }
    let (mut pa, mut pb) = (parse(a)?, parse(b)?);
    let len = pa.len().max(pb.len());
    pa.resize(len, 0);
    pb.resize(len, 0);
    Some(pa.cmp(&pb))
}

// --------------------------------------------------------------------- //
// ModShare
// --------------------------------------------------------------------- //

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShareEntry {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    /// Version on the sender's machine; empty when unknown.
    #[serde(default)]
    pub package_version: String,
    /// The original app's load-order group pin. Carried for compatibility;
    /// this app writes it empty and ignores it on import.
    #[serde(default)]
    pub group: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShareList {
    pub game: Game,
    pub profile_name: String,
    /// In profile order: index 0 is the bottom of the load order.
    pub entries: Vec<ShareEntry>,
}

#[derive(Serialize)]
struct ShareFile<'a> {
    format: &'static str,
    version: u64,
    game: Game,
    profile_name: &'a str,
    mods: &'a [ShareEntry],
}

impl ShareList {
    /// Snapshot a profile's active list. `version_of` returns the locally
    /// installed version for an active entry, or `None` when unknown.
    pub fn from_profile(profile: &Profile, game: Game, version_of: impl Fn(&ActiveMod) -> Option<String>) -> Self {
        Self {
            game,
            profile_name: profile.profile_name.clone(),
            entries: profile
                .active_mods
                .iter()
                .map(|m| ShareEntry {
                    name: m.name.clone(),
                    display_name: m.display_name.clone(),
                    package_version: version_of(m).unwrap_or_default(),
                    group: String::new(),
                })
                .collect(),
        }
    }

    /// Pretty JSON suitable for a shareable file.
    pub fn serialize(&self) -> String {
        serde_json::to_string_pretty(&ShareFile {
            format: MODSHARE_FORMAT,
            version: MODSHARE_VERSION,
            game: self.game,
            profile_name: &self.profile_name,
            mods: &self.entries,
        })
        .expect("plain data serialises")
    }

    /// Parse and validate a ModShare file.
    pub fn parse(text: &str) -> Result<Self> {
        let data: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format_err(format!("not JSON: {e}")))?;
        if data["format"].as_str() != Some(MODSHARE_FORMAT) {
            return Err(format_err("not a ModShare file"));
        }
        let version = data["version"].as_u64().filter(|v| *v >= 1).ok_or_else(|| format_err("bad version field"))?;
        if version > MODSHARE_VERSION {
            return Err(format_err(format!(
                "this file was written by a newer version (format {version}, supported {MODSHARE_VERSION})"
            )));
        }
        let game = data["game"].as_str().and_then(Game::parse).ok_or_else(|| format_err("unknown game"))?;
        let mods = data["mods"].as_array().ok_or_else(|| format_err("mods is not a list"))?;
        let entries = mods
            .iter()
            .map(|raw| {
                serde_json::from_value::<ShareEntry>(raw.clone())
                    .ok()
                    .filter(|e| !e.name.is_empty())
                    .ok_or_else(|| format_err("bad mod entry"))
            })
            .collect::<Result<Vec<_>>>()?;
        let profile_name = match &data["profile_name"] {
            serde_json::Value::Null => String::new(),
            serde_json::Value::String(s) => s.clone(),
            _ => return Err(format_err("profile_name must be a string")),
        };
        Ok(Self { game, profile_name, entries })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareStatus {
    /// Installed here.
    Found,
    /// Not installed; a workshop item the receiver can subscribe to.
    MissingWorkshop,
    /// Not installed; a local file the sender has to pass along.
    MissingLocal,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShareCheck {
    pub name: String,
    pub display_name: String,
    pub status: ShareStatus,
    /// Subscribe page, for a missing workshop item.
    pub workshop_url: Option<String>,
    pub share_version: String,
    /// Installed version; `None` when not installed or unknown.
    pub local_version: Option<String>,
    /// Installed, but strictly older than the version the list was built
    /// with. A hint only - it never blocks an import.
    pub outdated: bool,
}

/// Check every entry against the receiver's mods, in share order.
///
/// `installed` returns `None` when the mod is not installed and
/// `Some(version)` when it is (an empty version means "unknown"). The
/// caller builds it from the same matcher the load order uses, so all
/// matching strategies apply.
pub fn check_share(share: &ShareList, installed: impl Fn(&ShareEntry) -> Option<String>) -> Vec<ShareCheck> {
    share
        .entries
        .iter()
        .map(|entry| {
            let local = installed(entry);
            let workshop_url = workshop_id_from_active_name(&entry.name).map(|id| workshop_url(&id));
            let status = match (&local, &workshop_url) {
                (Some(_), _) => ShareStatus::Found,
                (None, Some(_)) => ShareStatus::MissingWorkshop,
                (None, None) => ShareStatus::MissingLocal,
            };
            let local_version = local.filter(|v| !v.is_empty());
            let outdated = local_version.as_deref().and_then(|l| compare_versions(l, &entry.package_version))
                == Some(Ordering::Less);
            ShareCheck {
                name: entry.name.clone(),
                display_name: entry.display_name.clone(),
                workshop_url: workshop_url.filter(|_| status == ShareStatus::MissingWorkshop),
                status,
                share_version: entry.package_version.clone(),
                local_version,
                outdated,
            }
        })
        .collect()
}

// --------------------------------------------------------------------- //
// MapCombo
// --------------------------------------------------------------------- //

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MapComboEntry {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    /// The version the combo was built with (v2); empty in v1 files.
    #[serde(default)]
    pub package_version: String,
}

#[derive(Serialize)]
struct MapComboFile<'a> {
    format: &'static str,
    version: u64,
    maps: &'a [MapComboEntry],
}

/// Render a maps block (top to bottom) as a shareable file.
pub fn serialize_map_combo(entries: &[MapComboEntry]) -> String {
    serde_json::to_string_pretty(&MapComboFile { format: MAPCOMBO_FORMAT, version: MAPCOMBO_VERSION, maps: entries })
        .expect("plain data serialises")
}

/// Parse a MapCombo file (v1 or v2).
pub fn parse_map_combo(text: &str) -> Result<Vec<MapComboEntry>> {
    let data: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format_err(format!("not JSON: {e}")))?;
    if data["format"].as_str() != Some(MAPCOMBO_FORMAT) {
        return Err(format_err("not a MapCombo file"));
    }
    data["maps"]
        .as_array()
        .ok_or_else(|| format_err("missing maps list"))?
        .iter()
        .map(|raw| {
            serde_json::from_value::<MapComboEntry>(raw.clone())
                .ok()
                .filter(|e| !e.name.is_empty())
                .ok_or_else(|| format_err("malformed map entry"))
        })
        .collect()
}

/// Reorder the maps block of a load order to match a combo.
///
/// `order` is the whole list (top to bottom) and `maps_block` the names in
/// it that sit in the Maps group. Maps named in the combo come first, in
/// combo order; maps the combo does not mention keep their relative order
/// and follow, so a local map is never dropped. Every non-map stays where
/// it was: the reordered run goes in at the position of the first map.
pub fn apply_map_combo(order: &[String], maps_block: &HashSet<String>, combo: &[MapComboEntry]) -> Vec<String> {
    let mut ordered: Vec<&String> =
        combo.iter().filter_map(|e| order.iter().find(|n| **n == e.name && maps_block.contains(*n))).collect();
    ordered.dedup();
    let placed: HashSet<&String> = ordered.iter().copied().collect();
    ordered.extend(order.iter().filter(|n| maps_block.contains(*n) && !placed.contains(n)));

    let mut result: Vec<String> = Vec::with_capacity(order.len());
    let mut inserted = false;
    for name in order {
        if maps_block.contains(name) {
            if !inserted {
                result.extend(ordered.iter().map(|n| (*n).clone()));
                inserted = true;
            }
        } else {
            result.push(name.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, version: &str) -> ShareEntry {
        ShareEntry {
            name: name.into(),
            display_name: format!("{name} display"),
            package_version: version.into(),
            group: String::new(),
        }
    }

    #[test]
    fn version_comparison_is_strict_and_numeric() {
        assert_eq!(compare_versions("2.10", "2.2"), Some(Ordering::Greater));
        assert_eq!(compare_versions("2.0", "2.0.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.4.1", "1.6.5.3"), Some(Ordering::Less));
        assert_eq!(compare_versions("1.59.b", "1.59"), None);
        assert_eq!(compare_versions("v2.4", "2.4"), None);
        assert_eq!(compare_versions("", "1"), None);
        assert_eq!(compare_versions("1..2", "1.2"), None);
    }

    #[test]
    fn modshare_round_trip() {
        let share = ShareList {
            game: Game::Ats,
            profile_name: "한글 프로필".into(),
            entries: vec![entry("a", "1.0"), entry("mod_workshop_package.000000003A4B7C12", "")],
        };
        let text = share.serialize();
        assert!(text.contains("\"format\": \"easy-scsmodmanager-modshare\""));
        assert!(text.contains("\"game\": \"ats\""));
        assert!(text.contains("한글 프로필")); // not escaped
        assert_eq!(ShareList::parse(&text).unwrap(), share);
    }

    #[test]
    fn modshare_reads_a_file_from_the_python_app() {
        let text = r#"{
  "format": "easy-scsmodmanager-modshare",
  "version": 1,
  "game": "ets2",
  "profile_name": "Jo",
  "mods": [
    {"name": "promods-def", "display_name": "ProMods Def", "package_version": "2.70", "group": "maps"},
    {"name": "bare"}
  ]
}"#;
        let share = ShareList::parse(text).unwrap();
        assert_eq!(share.game, Game::Ets2);
        assert_eq!(share.entries.len(), 2);
        assert_eq!(share.entries[0].group, "maps");
        assert_eq!(share.entries[1].display_name, "");
    }

    #[test]
    fn modshare_rejects_bad_input() {
        let ok = r#"{"format":"easy-scsmodmanager-modshare","version":1,"game":"ets2","mods":[]}"#;
        assert!(ShareList::parse(ok).is_ok());
        for bad in [
            "not json",
            "[]",
            r#"{"format":"something-else","version":1,"game":"ets2","mods":[]}"#,
            r#"{"format":"easy-scsmodmanager-modshare","version":0,"game":"ets2","mods":[]}"#,
            r#"{"format":"easy-scsmodmanager-modshare","version":1,"game":"gta","mods":[]}"#,
            r#"{"format":"easy-scsmodmanager-modshare","version":1,"game":"ets2","mods":{}}"#,
            r#"{"format":"easy-scsmodmanager-modshare","version":1,"game":"ets2","mods":[{"name":""}]}"#,
            r#"{"format":"easy-scsmodmanager-modshare","version":1,"game":"ets2","mods":[{"display_name":"x"}]}"#,
            r#"{"format":"easy-scsmodmanager-modshare","version":1,"game":"ets2","profile_name":5,"mods":[]}"#,
        ] {
            assert!(ShareList::parse(bad).is_err(), "should reject: {bad}");
        }
        let newer = r#"{"format":"easy-scsmodmanager-modshare","version":2,"game":"ets2","mods":[]}"#;
        assert!(ShareList::parse(newer).unwrap_err().to_string().contains("newer version"));
    }

    #[test]
    fn check_buckets_and_outdated_hint() {
        let share = ShareList {
            game: Game::Ets2,
            profile_name: String::new(),
            entries: vec![
                entry("installed_same", "1.2"),
                entry("installed_old", "2.0"),
                entry("installed_unknown_version", "2.0"),
                entry("mod_workshop_package.000000003A4B7C12", "1.0"),
                entry("missing_local", "1.0"),
            ],
        };
        let checks = check_share(&share, |e| match e.name.as_str() {
            "installed_same" => Some("1.2".into()),
            "installed_old" => Some("1.9".into()),
            "installed_unknown_version" => Some(String::new()),
            _ => None,
        });
        let summary: Vec<(ShareStatus, bool)> = checks.iter().map(|c| (c.status, c.outdated)).collect();
        assert_eq!(
            summary,
            vec![
                (ShareStatus::Found, false),
                (ShareStatus::Found, true),
                (ShareStatus::Found, false),
                (ShareStatus::MissingWorkshop, false),
                (ShareStatus::MissingLocal, false),
            ]
        );
        assert_eq!(checks[2].local_version, None);
        assert!(checks[3].workshop_url.as_deref().unwrap().ends_with("?id=978025490"));
        assert_eq!(checks[0].workshop_url, None);
    }

    #[test]
    fn mapcombo_round_trip_and_v1() {
        let combo = vec![
            MapComboEntry {
                name: "promods-def".into(),
                display_name: "ProMods Def".into(),
                package_version: "2.70".into(),
            },
            MapComboEntry { name: "rusmap".into(), display_name: "RusMap".into(), package_version: String::new() },
        ];
        let text = serialize_map_combo(&combo);
        assert!(text.contains("\"version\": 2"));
        assert_eq!(parse_map_combo(&text).unwrap(), combo);

        let v1 = r#"{"format":"easy-scsmodmanager-mapcombo","version":1,"maps":[{"name":"a","display_name":"A"}]}"#;
        assert_eq!(parse_map_combo(v1).unwrap()[0].package_version, "");
        assert!(parse_map_combo(r#"{"format":"easy-scsmodmanager-modshare","maps":[]}"#).is_err());
        assert!(parse_map_combo(r#"{"format":"easy-scsmodmanager-mapcombo","maps":[{"display_name":"x"}]}"#).is_err());
    }

    #[test]
    fn map_combo_reorders_only_the_maps_block() {
        let order: Vec<String> =
            ["sound", "truck", "map_c", "map_a", "map_b", "map_local"].iter().map(|s| s.to_string()).collect();
        let block: HashSet<String> = order.iter().filter(|n| n.starts_with("map_")).cloned().collect();
        let combo: Vec<MapComboEntry> = ["map_a", "map_b", "map_not_installed", "map_c"]
            .iter()
            .map(|n| MapComboEntry { name: n.to_string(), ..Default::default() })
            .collect();
        assert_eq!(apply_map_combo(&order, &block, &combo), ["sound", "truck", "map_a", "map_b", "map_c", "map_local"]);
        // a combo naming a non-map never moves it
        let combo2 = vec![MapComboEntry { name: "truck".into(), ..Default::default() }];
        assert_eq!(apply_map_combo(&order, &block, &combo2), order);
        // nothing to do without maps
        assert_eq!(apply_map_combo(&order[..2], &HashSet::new(), &combo), order[..2]);
    }
}
