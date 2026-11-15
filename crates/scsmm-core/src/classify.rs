//! Works out what a mod is from what it carries, as a second opinion on
//! the category its manifest declares.
//!
//! Every file is given a [`Label`] - from its path, and where the content
//! was read, from the units it defines (a file of `accessory_paint_job_data`
//! units is a paint job wherever it sits, which also covers the files a
//! locked archive does not name). The labels add up to an [`Evidence`];
//! [`judge`] compares that with the manifest:
//!
//! * no usable claim in the manifest: the content decides;
//! * the claim fits the content: nothing to say;
//! * the claim has nothing at all behind it and the content is clear: the
//!   content wins;
//! * otherwise a clear majority only becomes a suggestion.
//!
//! Models, textures and other payload never vote - a definition says what
//! a mod is, the payload only comes with it.

use std::collections::HashMap;

use serde::Serialize;

use crate::categories::{canonical_categories, OTHER};
use crate::load_order::group_index_for_token;
use crate::sii::crypto::unwrap_sii;

/// What one file of a mod is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    /// Map sectors and descriptors below `map/`.
    Map,
    /// World definitions a mod adds: cities, companies, signs, models.
    World,
    /// Any other definition, and world definitions that only replace the
    /// game's own - a tweak, not new content.
    Misc,
    Paint,
    /// Engines, transmissions.
    Powertrain,
    Chassis,
    /// Accessories, wheels, tires.
    Accessory,
    Interior,
    /// Dashboard computer layouts below `ui/`.
    Dashboard,
    /// Dealer and company entries: the mod puts a truck on sale.
    TruckStruct,
    /// Cabins, truck data and other truck definitions.
    TruckOther,
    /// Models and textures below `vehicle/truck*/`.
    TruckAssets,
    Trailer,
    TrailerAssets,
    Cargo,
    Ai,
    Sound,
    Ui,
    Physics,
    Weather,
    Graphics,
    /// Payload that says nothing by itself.
    Assets,
}

const LABEL_COUNT: usize = 22;
const ALL_LABELS: [Label; LABEL_COUNT] = [
    Label::Map,
    Label::World,
    Label::Misc,
    Label::Paint,
    Label::Powertrain,
    Label::Chassis,
    Label::Accessory,
    Label::Interior,
    Label::Dashboard,
    Label::TruckStruct,
    Label::TruckOther,
    Label::TruckAssets,
    Label::Trailer,
    Label::TrailerAssets,
    Label::Cargo,
    Label::Ai,
    Label::Sound,
    Label::Ui,
    Label::Physics,
    Label::Weather,
    Label::Graphics,
    Label::Assets,
];

impl Label {
    /// The category of a mod made of such files, where that is clear.
    pub fn category(self) -> Option<&'static str> {
        Some(match self {
            Label::Map => "map",
            Label::Misc => OTHER,
            Label::Paint => "paint_job",
            Label::Powertrain | Label::Accessory => "tuning_parts",
            Label::Interior | Label::Dashboard => "interior",
            Label::TruckStruct => "truck",
            Label::Trailer => "trailer",
            Label::Cargo => "cargo_pack",
            Label::Ai => "ai_traffic",
            Label::Sound => "sound",
            Label::Ui => "ui",
            Label::Physics => "physics",
            Label::Weather => "weather_setup",
            Label::Graphics => "graphics",
            Label::World
            | Label::Chassis
            | Label::TruckOther
            | Label::TruckAssets
            | Label::TrailerAssets
            | Label::Assets => return None,
        })
    }

    /// Categories a mod may reasonably declare when it carries such files:
    /// an engine definition is at home in a sound pack, a tuning pack or a
    /// truck alike.
    fn at_home_in(self) -> &'static [&'static str] {
        match self {
            Label::Map => &["map"],
            Label::World => &[
                "map", "models", "prefabs", "movers", "walkers", "other", "ui", "graphics", "weather_setup",
                "ai_traffic", "cargo_pack",
            ],
            Label::Misc => &[
                "other", "ui", "models", "movers", "walkers", "prefabs", "graphics", "weather_setup", "ai_traffic",
                "physics", "sound",
            ],
            Label::Paint => &["paint_job", "truck", "trailer"],
            Label::Powertrain => &["tuning_parts", "sound", "truck", "physics"],
            Label::Chassis => &["physics", "truck", "tuning_parts", "sound", "trailer"],
            Label::Accessory => &["tuning_parts", "interior", "truck", "trailer", "sound"],
            Label::Interior => &["interior", "tuning_parts", "truck", "ui"],
            Label::Dashboard => &["interior", "ui"],
            Label::TruckStruct => &["truck"],
            Label::TruckOther => &["truck", "tuning_parts", "interior", "sound", "physics", "paint_job"],
            Label::TruckAssets => &["truck", "tuning_parts", "interior", "paint_job"],
            Label::Trailer => &["trailer", "cargo_pack", "map"],
            Label::TrailerAssets => &["trailer", "cargo_pack", "paint_job", "map", "other"],
            Label::Cargo => &["cargo_pack", "trailer", "map"],
            Label::Ai => &["ai_traffic", "map"],
            Label::Sound => &["sound"],
            Label::Ui => &["ui", "interior"],
            Label::Physics => &["physics"],
            Label::Weather => &["weather_setup", "graphics", "map"],
            Label::Graphics => &["graphics", "weather_setup", "map"],
            Label::Assets => {
                &["graphics", "weather_setup", "models", "movers", "walkers", "prefabs", "map", "other", "ui"]
            }
        }
    }

    /// True when a mod declaring `category` may carry such files: the
    /// category is one of the label's homes, or sorts into the same
    /// load-order group as the label's own category.
    pub fn fits(self, category: &str) -> bool {
        self.at_home_in().contains(&category)
            || self.category().is_some_and(|own| group_index_for_token(own) == group_index_for_token(category))
    }

    /// A definition, as opposed to the payload that comes with one.
    fn is_definition(self) -> bool {
        !matches!(self, Label::Assets | Label::TruckAssets | Label::TrailerAssets)
    }
}

/// Label of a file by its path. `path` is normalised: no leading slash,
/// lower case, package layers stripped. `replaces_game_file` is whether
/// the game has a file at this path (`None` when that is not known).
pub fn path_label(path: &str, replaces_game_file: Option<bool>) -> Label {
    let parts: Vec<&str> = path.split('/').collect();
    let file = parts.last().copied().unwrap_or("");
    let ext = file.rsplit_once('.').map_or("", |(_, ext)| ext);
    if matches!(ext, "bank" | "guids" | "soundref" | "ogg" | "wav") {
        return Label::Sound;
    }
    let second = parts.get(1).copied().unwrap_or("");
    match parts[0] {
        "map" => Label::Map,
        "sound" => Label::Sound,
        "ui" if second == "dashboard" || file.starts_with("dashboard") => Label::Dashboard,
        "ui" => Label::Ui,
        "def" => def_label(&parts[1..], file, replaces_game_file),
        "vehicle" if second.starts_with("truck") => Label::TruckAssets,
        "vehicle" if second.starts_with("trailer") => Label::TrailerAssets,
        _ => Label::Assets,
    }
}

/// `parts` is the path below `def/`.
fn def_label(parts: &[&str], file: &str, replaces_game_file: Option<bool>) -> Label {
    // new world content, or only the game's own definitions rewritten
    let world = if replaces_game_file == Some(true) { Label::Misc } else { Label::World };
    let first = parts.first().copied().unwrap_or("");
    match first {
        "vehicle" => vehicle_def_label(&parts[1..]),
        "cargo" => Label::Cargo,
        "climate" => Label::Weather,
        "world" if file.starts_with("traffic") || file.starts_with("semaphore") => Label::Ai,
        "world" if file.starts_with("flare") => Label::Graphics,
        "world" => world,
        _ if first.starts_with("traffic") => Label::Ai,
        _ if ["city", "country", "company", "sign", "ferry"].iter().any(|p| first.starts_with(p)) => world,
        _ => Label::Misc,
    }
}

/// `parts` is the path below `def/vehicle/`.
fn vehicle_def_label(parts: &[&str]) -> Label {
    let first = parts.first().copied().unwrap_or("");
    match first {
        _ if first.starts_with("physics") => Label::Physics,
        // def/vehicle/truck/<model>/<part>/<file>
        "truck" | "car" if parts.len() >= 4 => match parts[2] {
            "paint_job" => Label::Paint,
            "engine" | "transmission" | "retarder" | "differential" => Label::Powertrain,
            "chassis" => Label::Chassis,
            "interior" => Label::Interior,
            "sound" => Label::Sound,
            "cabin" | "data" => Label::TruckOther,
            // every other folder is an accessory slot
            _ => Label::Accessory,
        },
        "truck" | "car" => Label::TruckOther,
        _ if first.starts_with("truck_dealer") || first.starts_with("truck_company") => Label::TruckStruct,
        _ if first.starts_with("trailer") && parts.iter().any(|p| p.ends_with("paint_job")) => Label::Paint,
        _ if first.starts_with("trailer") => Label::Trailer,
        "ai" => Label::Ai,
        _ if first.starts_with("traffic") => Label::Ai,
        "transmission" | "engine" => Label::Powertrain,
        "addon_hookups" => Label::Accessory,
        _ if is_wheel_folder(first) => Label::Accessory,
        _ => Label::Misc,
    }
}

/// `f_tire`, `r_wheel`, `f_disc`, ...
fn is_wheel_folder(name: &str) -> bool {
    name.strip_prefix("f_")
        .or_else(|| name.strip_prefix("r_"))
        .is_some_and(|kind| ["tire", "wheel", "disc", "hub", "nuts", "cover"].iter().any(|k| kind.starts_with(k)))
}

/// The pattern a path counts under in the explanation: the truck model
/// is left out, deeper folders are cut off.
fn path_key(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let dirs = &parts[..parts.len() - 1];
    match dirs {
        ["def", "vehicle", kind @ ("truck" | "car"), _, part, ..] => format!("def/vehicle/{kind}/*/{part}"),
        ["def", "vehicle", sub, ..] => format!("def/vehicle/{sub}"),
        ["def", sub, ..] => format!("def/{sub}"),
        ["def"] => "def".to_string(),
        [first, second, ..] => format!("{first}/{second}"),
        [first] => (*first).to_string(),
        [] => String::new(),
    }
}

/// The unit classes that settle what a file is.
const CLASSES: &[(&str, Label)] = &[
    ("accessory_paint_job_data", Label::Paint),
    ("simple_paint_job_data", Label::Paint),
    ("accessory_engine_data", Label::Powertrain),
    ("accessory_transmission_data", Label::Powertrain),
    ("accessory_retarder_data", Label::Powertrain),
    ("accessory_chassis_data", Label::Chassis),
    ("accessory_addon_data", Label::Accessory),
    ("accessory_addon_tank_data", Label::Accessory),
    ("accessory_addon_patch_data", Label::Accessory),
    ("accessory_wheel_data", Label::Accessory),
    ("accessory_rim_data", Label::Accessory),
    ("accessory_hookup_data", Label::Accessory),
    ("accessory_head_lights_data", Label::Accessory),
    ("accessory_horn_addon_data", Label::Accessory),
    ("accessory_interior_data", Label::Interior),
    ("accessory_addon_int_ui_data", Label::Interior),
    ("accessory_hookup_int_ui_data", Label::Interior),
    ("accessory_cabin_data", Label::TruckOther),
    ("accessory_truck_data", Label::TruckOther),
    ("accessory_sound_data", Label::Sound),
    ("sound_item_data", Label::Sound),
    ("sound_item_reverb", Label::Sound),
    ("sound_three_eq_data", Label::Sound),
    ("voice_navigation_config", Label::Sound),
    ("trailer_def", Label::Trailer),
    ("trailer_definition", Label::Trailer),
    ("trailer_configuration", Label::Trailer),
    ("trailer_body_set", Label::Trailer),
    ("accessory_trailer_body_data", Label::Trailer),
    ("accessory_trailer_data", Label::Trailer),
    ("cargo_data", Label::Cargo),
    ("traffic_vehicle", Label::Ai),
    ("traffic_trailer", Label::Ai),
    ("traffic_spawn_condition", Label::Ai),
    ("tr_semaphore_profile", Label::Ai),
    ("traffic_data", Label::Ai),
    ("physics_data", Label::Physics),
    ("sun_profile", Label::Weather),
    ("flare_vehicle", Label::Graphics),
    ("flare_blink", Label::Graphics),
    ("flare_traffic_light", Label::Graphics),
    ("particle_gen", Label::Graphics),
    ("vehicle_particle", Label::Graphics),
];

const BSII_MAGIC: &[u8] = b"BSII";

/// The first telling unit class of a binary SII file.
///
/// The binary layout declares every class before its first unit, in a
/// structure block that carries the class name as plain text:
///
/// ```text
/// u32  0            block type: a structure definition
/// u8   1            valid
/// u32  id
/// u32  length, then the class name
/// ...  the fields: u32 value type (never 0), u32 length, field name
/// ```
///
/// Reading the units themselves takes a full decoder; for the class
/// names it is enough to find those blocks. The header in front of the
/// name tells a class from a field that happens to share its name.
fn binary_class(data: &[u8]) -> Option<(Label, &'static str)> {
    // block type + valid + id + length
    const HEADER: usize = 4 + 1 + 4 + 4;
    CLASSES
        .iter()
        .filter_map(|(name, label)| {
            let declared = memchr::memmem::find_iter(data, name.as_bytes()).find(|at| {
                let Some(start) = at.checked_sub(HEADER) else {
                    return false;
                };
                let header = &data[start..*at];
                header[..5] == [0, 0, 0, 0, 1] && header[9..] == (name.len() as u32).to_le_bytes()
            })?;
            Some((declared, *label, *name))
        })
        .min_by_key(|(at, _, _)| *at)
        .map(|(_, label, name)| (label, name))
}

const SOUND_BANK: &str = "sound bank";
const SOUND_REFERENCE: &str = "sound reference";

/// Label of a file by its content, with the unit class (or marker) that
/// gave it away. `None` when the content settles nothing: binary data, a
/// text without a telling unit.
pub fn content_label(data: &[u8]) -> Option<(Label, &'static str)> {
    if data.starts_with(b"RIFF") || data.starts_with(b"OggS") {
        return Some((Label::Sound, SOUND_BANK));
    }
    let plain = unwrap_sii(data).ok()?;
    if plain.starts_with(BSII_MAGIC) {
        return binary_class(&plain);
    }
    // a definition is text; anything with a NUL near the start is not
    if plain.iter().take(512).any(|b| *b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(&plain);
    for line in text.lines() {
        // `class : name {` - the name may also be on a line of its own
        let Some((class, _)) = line.split_once(':') else {
            continue;
        };
        let class = class.trim();
        if let Some((name, label)) = CLASSES.iter().find(|(name, _)| *name == class) {
            return Some((*label, name));
        }
    }
    // an include fragment has no unit of its own, but names its sounds; a
    // whole file of some other unit may mention a sound without being one
    if !text.contains("SiiNunit") && (text.contains("/sound/") || text.contains(".bank")) {
        return Some((Label::Sound, SOUND_REFERENCE));
    }
    None
}

/// One thing the analysis saw, for the explanation shown to the user.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signal {
    pub label: Label,
    /// A path pattern (`def/vehicle/truck/*/paint_job`) or a unit class.
    pub key: String,
    /// Files below the path, or files read that define the class.
    pub count: usize,
    /// Read from file content rather than taken from a path.
    pub from_content: bool,
}

/// How many signals an [`Evidence`] keeps for the explanation.
const MAX_SIGNALS: usize = 12;

/// What the files of one mod add up to.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Evidence {
    /// Estimated number of files per label, indexed like [`ALL_LABELS`].
    counts: [f32; LABEL_COUNT],
    /// The strongest signals, most frequent first.
    pub signals: Vec<Signal>,
    /// Files the archive holds without naming them.
    pub hidden: usize,
    /// How many of those were looked into, and how many said something.
    pub hidden_tried: usize,
    pub hidden_read: usize,
}

impl Evidence {
    pub fn count(&self, label: Label) -> f32 {
        self.counts[label as usize]
    }

    /// Number of definition files, payload left out.
    fn definitions(&self) -> f32 {
        ALL_LABELS.iter().filter(|l| l.is_definition()).map(|l| self.count(*l)).sum()
    }

    /// Whether the content is known well enough to contradict a manifest:
    /// a locked archive whose hidden files stayed unreadable is not.
    fn is_visible(&self) -> bool {
        self.hidden == 0 || (self.hidden_tried > 0 && self.hidden_read * 2 >= self.hidden_tried)
    }
}

#[derive(Default, Clone, Copy)]
struct Bucket {
    files: usize,
    sampled: usize,
    /// Content labels of the sampled files, indexed like [`ALL_LABELS`].
    relabelled: [usize; LABEL_COUNT],
}

/// Collects the files of one mod into an [`Evidence`].
///
/// Reading every file of a large mod would take too long, so content is
/// sampled: the caller reads a few files of each path label and the share
/// that turns out to be something else is applied to the whole label.
#[derive(Default)]
pub struct EvidenceBuilder {
    buckets: [Bucket; LABEL_COUNT],
    hidden: Bucket,
    path_keys: HashMap<String, (Label, usize)>,
    content_keys: HashMap<&'static str, (Label, usize)>,
}

impl EvidenceBuilder {
    /// Count a named file. Returns its label, under which a sample of its
    /// content is to be reported.
    pub fn add_path(&mut self, path: &str, replaces_game_file: Option<bool>) -> Label {
        let label = path_label(path, replaces_game_file);
        self.buckets[label as usize].files += 1;
        if label.is_definition() {
            self.path_keys.entry(path_key(path)).or_insert((label, 0)).1 += 1;
        }
        label
    }

    fn note(&mut self, found: Option<(Label, &'static str)>, relabelled: &mut [usize; LABEL_COUNT]) {
        if let Some((label, key)) = found {
            relabelled[label as usize] += 1;
            self.content_keys.entry(key).or_insert((label, 0)).1 += 1;
        }
    }

    /// Report the content of a file counted under `path_label`.
    pub fn add_sample(&mut self, path_label: Label, data: &[u8]) {
        let mut bucket = self.buckets[path_label as usize];
        bucket.sampled += 1;
        self.note(content_label(data), &mut bucket.relabelled);
        self.buckets[path_label as usize] = bucket;
    }

    /// The number of files the archive holds without naming them.
    pub fn set_hidden(&mut self, total: usize) {
        self.hidden.files = total;
    }

    /// Report one unnamed file that was looked into; `None` when it could
    /// not be read (too large, a packed texture).
    pub fn add_hidden_sample(&mut self, data: Option<&[u8]>) {
        let mut bucket = self.hidden;
        bucket.sampled += 1;
        self.note(data.and_then(content_label), &mut bucket.relabelled);
        self.hidden = bucket;
    }

    pub fn finish(self) -> Evidence {
        let mut counts = [0f32; LABEL_COUNT];
        for (index, bucket) in self.buckets.iter().enumerate() {
            counts[index] += bucket.files as f32;
            if bucket.sampled == 0 {
                continue;
            }
            // the sampled share that is really something else moves over
            let per_sample = bucket.files as f32 / bucket.sampled as f32;
            for (other, hits) in bucket.relabelled.iter().enumerate() {
                if other != index && *hits > 0 {
                    let moved = *hits as f32 * per_sample;
                    counts[index] -= moved;
                    counts[other] += moved;
                }
            }
        }
        if self.hidden.sampled > 0 {
            let per_sample = self.hidden.files as f32 / self.hidden.sampled as f32;
            for (label, hits) in self.hidden.relabelled.iter().enumerate() {
                counts[label] += *hits as f32 * per_sample;
            }
        }

        let mut signals: Vec<Signal> = self
            .path_keys
            .into_iter()
            .map(|(key, (label, count))| Signal { label, key, count, from_content: false })
            .chain(self.content_keys.into_iter().map(|(key, (label, count))| Signal {
                label,
                key: key.to_string(),
                count,
                from_content: true,
            }))
            .collect();
        signals.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        signals.truncate(MAX_SIGNALS);

        Evidence {
            counts,
            signals,
            hidden: self.hidden.files,
            hidden_tried: self.hidden.sampled,
            hidden_read: self.hidden.relabelled.iter().sum(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Medium,
    High,
}

/// What the content says a mod is, where that differs from its manifest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub category: &'static str,
    pub confidence: Confidence,
    /// Share of the mod's definition files behind the category.
    pub share: f32,
    /// Sure enough to replace the manifest's category; otherwise it is
    /// only a suggestion for the user.
    pub applied: bool,
    /// The categories to use when applied: the judged one first, then
    /// whatever the manifest declares that the content does back.
    pub categories: Vec<String>,
}

/// A manifest-less mod takes the content's word from this share on.
const UNCLAIMED_SHARE: f32 = 0.5;
/// Share needed to overrule a claim that has nothing behind it ...
const OVERRULE_SHARE: f32 = 0.6;
/// ... and the fewest definition files to do that on.
const OVERRULE_FILES: f32 = 3.0;
/// Share needed to suggest something else than a claim that has support.
const SUGGEST_SHARE: f32 = 0.7;
const HIGH_SHARE: f32 = 0.8;

/// Compare the evidence with the categories the manifest declares.
/// `None` when the content agrees with the manifest or says too little.
pub fn judge<S: AsRef<str>>(evidence: &Evidence, manifest_categories: &[S]) -> Option<Verdict> {
    let claims: Vec<String> = canonical_categories(manifest_categories).into_iter().filter(|c| c != OTHER).collect();
    let definitions = evidence.definitions();
    if definitions < 0.5 {
        return None;
    }
    let support =
        |category: &str| -> f32 { ALL_LABELS.iter().filter(|l| l.fits(category)).map(|l| evidence.count(*l)).sum() };
    let categories_for = |judged: &'static str| -> Vec<String> {
        let mut out = vec![judged.to_string()];
        out.extend(claims.iter().filter(|c| c.as_str() != judged && support(c) >= 0.5).cloned());
        out
    };

    // a mod that puts a truck on sale and brings its chassis and cabin is
    // a truck, however many accessories and paint jobs come along
    let on_sale = evidence.count(Label::TruckStruct) >= 0.5;
    let whole_truck = on_sale && evidence.count(Label::Chassis) >= 0.5 && evidence.count(Label::TruckOther) >= 0.5;
    let (label, share) = if whole_truck || (on_sale && claims.is_empty()) {
        (Label::TruckStruct, 1.0)
    } else {
        let top = ALL_LABELS
            .iter()
            .copied()
            .filter(|l| l.category().is_some())
            .max_by(|a, b| evidence.count(*a).total_cmp(&evidence.count(*b)))?;
        if evidence.count(top) < 0.5 {
            // only definitions that fit anything (world content, cabins)
            return None;
        }
        (top, evidence.count(top) / definitions)
    };
    let category = label.category()?;
    let confident = if label == Label::TruckStruct { whole_truck } else { share >= HIGH_SHARE };
    let confidence = if confident { Confidence::High } else { Confidence::Medium };

    let Some(claim) = claims.first() else {
        // nothing declared: the content is all there is to go by
        return (share >= UNCLAIMED_SHARE && category != OTHER).then(|| Verdict {
            category,
            confidence,
            share,
            applied: true,
            categories: categories_for(category),
        });
    };
    if label.fits(claim) {
        return None;
    }
    let backing = support(claim);
    let unsupported = backing < 0.5;
    let applied = unsupported && share >= OVERRULE_SHARE && definitions >= OVERRULE_FILES && evidence.is_visible();
    // a suggestion takes a clear majority that also outweighs whatever
    // backs the claim: one stray UI file in a pack of map assets does not
    let outweighs = label == Label::TruckStruct || evidence.count(label) > backing;
    if !applied && !(share >= SUGGEST_SHARE && outweighs) {
        return None;
    }
    Some(Verdict {
        category,
        // overruling a manifest is only done on a sure footing
        confidence: if applied { Confidence::High } else { Confidence::Medium },
        share,
        applied,
        categories: categories_for(category),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(paths: &[&str]) -> Evidence {
        let mut b = EvidenceBuilder::default();
        for p in paths {
            b.add_path(p, None);
        }
        b.finish()
    }

    fn many(pattern: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| pattern.replace('#', &i.to_string())).collect()
    }

    fn judged(paths: &[String], manifest: &[&str]) -> Option<(&'static str, bool)> {
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        judge(&evidence(&refs), manifest).map(|v| (v.category, v.applied))
    }

    #[test]
    fn labels_by_path() {
        let l = |p| path_label(p, None);
        assert_eq!(l("def/vehicle/truck/scania.s/paint_job/red.sii"), Label::Paint);
        assert_eq!(l("def/vehicle/truck/scania.s/engine/dc16.sii"), Label::Powertrain);
        assert_eq!(l("def/vehicle/truck/scania.s/chassis/6x4.sii"), Label::Chassis);
        assert_eq!(l("def/vehicle/truck/scania.s/accessory/f_grill/a.sii"), Label::Accessory);
        assert_eq!(l("def/vehicle/truck/scania.s/f_bumper/a.sii"), Label::Accessory);
        assert_eq!(l("def/vehicle/car/ford.f150/interior/a.sii"), Label::Interior);
        assert_eq!(l("def/vehicle/truck/scania.s/data.sii"), Label::TruckOther);
        assert_eq!(l("def/vehicle/truck_dealer/scania/s.sii"), Label::TruckStruct);
        assert_eq!(l("def/vehicle/truck_company_uk/s.sii"), Label::TruckStruct);
        assert_eq!(l("def/vehicle/trailer_owned/scs.box/chassis.sii"), Label::Trailer);
        assert_eq!(l("def/vehicle/trailer/x/company_paint_job/a.sii"), Label::Paint);
        assert_eq!(l("def/vehicle/r_tire/a.sii"), Label::Accessory);
        assert_eq!(l("def/vehicle/physics.sii"), Label::Physics);
        assert_eq!(l("def/vehicle/ai/car.sui"), Label::Ai);
        assert_eq!(l("def/world/traffic_lane.sii"), Label::Ai);
        assert_eq!(l("def/climate/default/nice.sii"), Label::Weather);
        assert_eq!(l("def/economy_data.sii"), Label::Misc);
        assert_eq!(l("def/vehicle/truck/x/interior/sound.soundref"), Label::Sound);
        assert_eq!(l("sound/truck/engine.bank"), Label::Sound);
        assert_eq!(l("ui/dashboard/volvo.sii"), Label::Dashboard);
        assert_eq!(l("ui/template/dashboard_text.x.sii"), Label::Dashboard);
        assert_eq!(l("ui/adviser.sii"), Label::Ui);
        assert_eq!(l("map/europe/sec+0001+0001.base"), Label::Map);
        assert_eq!(l("vehicle/truck/upgrade/mirror/a.pmd"), Label::TruckAssets);
        assert_eq!(l("material/ui/accessory/a.mat"), Label::Assets);
    }

    #[test]
    fn world_definitions_are_content_only_when_new() {
        assert_eq!(path_label("def/world/model.sii", None), Label::World);
        assert_eq!(path_label("def/world/model.sii", Some(false)), Label::World);
        assert_eq!(path_label("def/world/model.sii", Some(true)), Label::Misc);
        assert_eq!(path_label("def/city.promods.sii", Some(false)), Label::World);
    }

    #[test]
    fn labels_by_content() {
        let label = |text: &str| content_label(text.as_bytes()).map(|(l, _)| l);
        assert_eq!(label("SiiNunit\n{\naccessory_paint_job_data : a.b.paint_job\n{\n}\n}"), Some(Label::Paint));
        assert_eq!(label("accessory_addon_int_ui_data: gps.x.set_glass {\n name: \"x\"\n}"), Some(Label::Interior));
        assert_eq!(label("physics_data: .phdata\n{\n}"), Some(Label::Physics));
        assert_eq!(label("\tsounds[]: \"air|/sound/truck/air.bank#x\"\n"), Some(Label::Sound));
        assert_eq!(label("SiiNunit\n{\nmover_desc : m {\n sound: \"/sound/x.bank\"\n}\n}"), None);
        // a property that merely looks like a class is not one
        assert_eq!(label("SiiNunit\n{\ncompany_def : x {\n name: \"accessory_paint_job_data\"\n}\n}"), None);
        assert_eq!(content_label(b"RIFF\x00\x00\x00\x00FEV "), Some((Label::Sound, SOUND_BANK)));
        assert_eq!(content_label(&[0u8, 1, 2, 3]), None);

        // scrambled definitions are read like plain ones
        let plain = b"SiiNunit\n{\naccessory_chassis_data : c.d.chassis\n{\n}\n}";
        let scrambled = crate::sii::crypto::testutil::scramble_3nk(plain, 7);
        assert_eq!(content_label(&scrambled), Some((Label::Chassis, "accessory_chassis_data")));
    }

    /// A binary SII file with one structure block per class, each with a
    /// single field - the layout of the game's own binary files.
    fn binary_sii(classes: &[(&str, &str)]) -> Vec<u8> {
        let text = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        let mut out = b"BSII".to_vec();
        out.extend_from_slice(&3u32.to_le_bytes());
        for (id, (class, field)) in classes.iter().enumerate() {
            out.extend_from_slice(&0u32.to_le_bytes()); // a structure block
            out.push(1); // valid
            out.extend_from_slice(&(id as u32 + 1).to_le_bytes());
            text(&mut out, class);
            out.extend_from_slice(&0x39u32.to_le_bytes()); // field type
            text(&mut out, field);
            out.extend_from_slice(&0u32.to_le_bytes()); // end of fields
        }
        out
    }

    #[test]
    fn binary_definitions_are_read_by_their_class() {
        let paint = binary_sii(&[("company_def", "name"), ("accessory_paint_job_data", "price")]);
        assert_eq!(content_label(&paint), Some((Label::Paint, "accessory_paint_job_data")));
        // the class declared first decides, as the first unit of a text file
        let mixed = binary_sii(&[("accessory_chassis_data", "price"), ("accessory_paint_job_data", "price")]);
        assert_eq!(content_label(&mixed), Some((Label::Chassis, "accessory_chassis_data")));
        // a field named like a class is not a class
        let field = binary_sii(&[("economy", "traffic_data")]);
        assert_eq!(content_label(&field), None);
        // scrambled on top of binary
        let scrambled = crate::sii::crypto::testutil::scramble_3nk(&paint, 0x21);
        assert_eq!(content_label(&scrambled), Some((Label::Paint, "accessory_paint_job_data")));
        assert_eq!(content_label(b"BSII"), None);
    }

    #[test]
    fn samples_move_their_share_of_a_bucket() {
        let mut b = EvidenceBuilder::default();
        for i in 0..100 {
            b.add_path(&format!("def/vehicle/truck/x/accessory/gps/{i}.sii"), None);
        }
        // three of four sampled accessories are dashboard computers
        for _ in 0..3 {
            b.add_sample(Label::Accessory, b"accessory_addon_int_ui_data : a.b.c {}");
        }
        b.add_sample(Label::Accessory, b"accessory_addon_data : a.b.c {}");
        let e = b.finish();
        assert_eq!((e.count(Label::Interior), e.count(Label::Accessory)), (75.0, 25.0));
        assert_eq!(e.signals[0].key, "def/vehicle/truck/*/accessory");
        assert!(e.signals.iter().any(|s| s.from_content && s.key == "accessory_addon_int_ui_data" && s.count == 3));
    }

    #[test]
    fn hidden_files_count_by_what_the_samples_say() {
        let mut b = EvidenceBuilder::default();
        b.set_hidden(800);
        for _ in 0..90 {
            b.add_hidden_sample(Some(b"accessory_chassis_data : a.b.chassis {}"));
        }
        for _ in 0..10 {
            b.add_hidden_sample(None);
        }
        let e = b.finish();
        assert_eq!(e.count(Label::Chassis), 720.0);
        assert_eq!((e.hidden, e.hidden_tried, e.hidden_read), (800, 100, 90));
        assert!(e.is_visible());
    }

    #[test]
    fn an_empty_manifest_takes_the_contents_word() {
        let paint = many("def/vehicle/truck/scania.s/paint_job/#.sii", 10);
        assert_eq!(judged(&paint, &[]), Some(("paint_job", true)));
        assert_eq!(judged(&paint, &["other"]), Some(("paint_job", true)));
        assert_eq!(judged(&paint, &["no_such_category"]), Some(("paint_job", true)));
        // nothing but payload, or only "other" things: nothing to add
        assert_eq!(judged(&many("model/sign/#.pmd", 50), &[]), None);
        assert_eq!(judged(&many("def/economy_#.sii", 3), &[]), None);
        // no majority
        let mut mixed = many("def/vehicle/truck/x/paint_job/#.sii", 4);
        mixed.extend(many("def/vehicle/truck/x/engine/#.sii", 3));
        mixed.extend(many("def/cargo/#.sii", 3));
        assert_eq!(judged(&mixed, &[]), None);
    }

    #[test]
    fn a_fitting_claim_is_left_alone() {
        // engine definitions shipped with a sound pack
        assert_eq!(judged(&many("def/vehicle/truck/x/engine/#.sii", 40), &["sound"]), None);
        // interior definitions in a UI mod: same load-order neighbourhood
        assert_eq!(judged(&many("def/vehicle/truck/x/interior/#.sii", 40), &["ui"]), None);
        // weather declared as graphics: the same group
        assert_eq!(judged(&many("def/climate/x/#.sii", 14), &["graphics"]), None);
        // a map's definition package without a map descriptor
        let mut map_defs = many("def/sign/board/#.sii", 100);
        map_defs.extend(many("def/vehicle/trailer_defs/#.sii", 20));
        assert_eq!(judged(&map_defs, &["map"]), None);
    }

    #[test]
    fn an_unsupported_claim_is_overruled() {
        // dashboard layouts declared as a truck
        let mut dash = many("ui/dashboard/truck_#.sii", 3);
        dash.extend(many("material/ui/dashboard/#.dds", 18));
        assert_eq!(judged(&dash, &["truck"]), Some(("interior", true)));
        // too little to overrule on: a suggestion
        let camera = vec!["def/camera/units/interior_flb.sii".to_string()];
        assert_eq!(judged(&camera, &["paint_job"]), Some(("other", false)));
        // one stray definition among payload that backs the claim: nothing
        let mut assets = many("material/sign/#.mat", 400);
        assets.push("ui/white.sii".to_string());
        assert_eq!(judged(&assets, &["map"]), None);
    }

    #[test]
    fn a_supported_claim_only_gets_a_suggestion() {
        // accessories for many trucks, declared as a truck: at home there
        let mut parts = many("def/vehicle/truck/t#/accessory/mirror/a.sii", 40);
        parts.extend(many("vehicle/truck/upgrade/mirror/#.pmd", 100));
        parts.extend(many("def/vehicle/truck/t#/data.sii", 2));
        assert_eq!(judged(&parts, &["truck"]), None);
        // sound declared second: the content puts it first, as a suggestion
        let mut sound = many("def/vehicle/truck/t#/interior/a.soundref", 30);
        sound.extend(many("def/vehicle/truck/t#/data.sii", 2));
        let refs: Vec<&str> = sound.iter().map(String::as_str).collect();
        let v = judge(&evidence(&refs), &["truck", "sound"]).unwrap();
        assert_eq!((v.category, v.applied), ("sound", false));
        assert_eq!(v.categories, ["sound", "truck"]);
    }

    #[test]
    fn world_tweaks_are_not_a_map() {
        let mut b = EvidenceBuilder::default();
        for i in 0..120 {
            b.add_path(&format!("def/world/model_{i}.sii"), Some(true));
        }
        let v = judge(&b.finish(), &["map"]);
        assert_eq!(v.map(|v| (v.category, v.applied)), Some(("other", true)));
        // the same files, new to the game: a map's definitions
        let mut b = EvidenceBuilder::default();
        for i in 0..120 {
            b.add_path(&format!("def/world/model_{i}.sii"), Some(false));
        }
        assert_eq!(judge(&b.finish(), &["map"]), None);
    }

    #[test]
    fn a_truck_on_sale_is_a_truck() {
        let mut truck = many("def/vehicle/truck/new.one/accessory/x/#.sii", 300);
        truck.extend(many("def/vehicle/truck/new.one/paint_job/#.sii", 80));
        truck.extend(many("def/vehicle/truck/new.one/chassis/#.sii", 6));
        truck.extend(many("def/vehicle/truck/new.one/cabin/#.sii", 3));
        truck.extend(many("def/vehicle/truck_dealer/new/#.sii", 4));
        assert_eq!(judged(&truck, &[]), Some(("truck", true)));
        assert_eq!(judged(&truck, &["truck"]), None);
        assert_eq!(judged(&truck, &["paint_job"]), Some(("truck", false)));
        // dealer entries alone decide only where nothing is declared
        let mut patch = many("def/vehicle/truck/new.one/accessory/x/#.sii", 300);
        patch.extend(many("def/vehicle/truck_dealer/new/#.sii", 4));
        assert_eq!(judged(&patch, &[]), Some(("truck", true)));
        assert_eq!(judged(&patch, &["tuning_parts"]), None);
    }

    #[test]
    fn locked_archives_overrule_only_when_they_could_be_read() {
        let build = |readable: usize| {
            let mut b = EvidenceBuilder::default();
            b.set_hidden(200);
            for i in 0..100 {
                b.add_hidden_sample((i < readable).then_some(b"accessory_paint_job_data : a.b.c {}".as_slice()));
            }
            b.finish()
        };
        assert_eq!(judge(&build(90), &["sound"]).map(|v| v.applied), Some(true));
        assert_eq!(judge(&build(10), &["sound"]).map(|v| v.applied), Some(false));
    }
}
