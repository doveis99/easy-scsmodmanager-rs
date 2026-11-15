//! Whether the world map of a load order fits together.
//!
//! The map screen draws roads and cities over a background picture. Where
//! that picture lies in the world is not part of the picture: it is two
//! values in `def/map_data.sii`, the point at its centre and the size it
//! stands for. Map mods that reach beyond the game's own picture bring a
//! larger one together with new values - in different packages, and often
//! several maps each bring their own `map_data.sii`. Only the topmost one
//! in the load order counts. When that is not the one made for the
//! picture on screen, everything is drawn in the wrong place.
//!
//! What the picture was made for is not written anywhere, so it is taken
//! from the mod that belongs to it: the one holding both, or a
//! `map_data.sii` in a package that builds on the package with the
//! picture (most of the files it needs are there). A hint, like the other
//! reports; where nothing tells, nothing is claimed.
//!
//! The second check is about reach: map sectors that lie outside the area
//! the picture covers have nothing underneath them on the map screen.

use std::collections::BTreeSet;
use std::path::Path;

use rayon::prelude::*;
use serde::Serialize;

use crate::game::Game;
use crate::scanner::ScannedMod;
use crate::scs::cityhash::city_hash64;
use crate::scs::hashfs::HashFsReader;
use crate::scs::ModSource;
use crate::sii::crypto::sii_bytes_to_text;

/// Where the game looks for the coordinates of the map picture.
pub const DATA_FILE: &str = "def/map_data.sii";

/// The map picture: one overview and four tiles, as texture objects and,
/// where a mod ships them loose, the images behind those.
pub const BACKGROUND_FILES: &[&str] = &[
    "material/ui/map/map.tobj",
    "material/ui/map/map0.tobj",
    "material/ui/map/map1.tobj",
    "material/ui/map/map2.tobj",
    "material/ui/map/map3.tobj",
    "material/ui/map/map.dds",
    "material/ui/map/map0.dds",
    "material/ui/map/map1.dds",
    "material/ui/map/map2.dds",
    "material/ui/map/map3.dds",
];

/// Edge of a map sector, in world units.
const SECTOR_SIZE: f64 = 4000.0;

/// Sector numbers tried when looking for the sectors of an archive that
/// does not list its files: +-256 sectors are a million units each way,
/// several times what the widest maps reach.
const SECTOR_RANGE: i16 = 256;

/// A `map_data.sii` in another package speaks for the picture when that
/// package needs at least this many files of the one with the picture ...
const MIN_SHARED: usize = 5;
/// ... and they are this share (percent) of all it needs from elsewhere.
/// Packages of one map were seen at 66% and 88%, unrelated maps under 3%.
const MIN_SHARED_PERCENT: usize = 20;

/// Sectors outside the picture are reported from this many on ...
const MIN_OUTSIDE: usize = 4;
/// ... when they are this share (percent) of the mod's sectors. A map
/// standing a sector or two over the edge is how its author left it.
const MIN_OUTSIDE_PERCENT: usize = 10;

/// How many real metres one world unit stands for: the scale the maps are
/// built in, which turns the size of the picture (given in real
/// kilometres) into world units.
pub fn world_scale(game: Game) -> f64 {
    match game {
        Game::Ets2 => 19.0,
        Game::Ats => 20.0,
    }
}

/// Where the map picture lies: the world position of its centre and the
/// real size it stands for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MapData {
    /// World units, x (east) and z (south).
    pub center: (f64, f64),
    /// Real kilometres, width and height.
    pub size_km: (f64, f64),
}

/// A rectangle in world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_z: f64,
    pub max_z: f64,
}

impl MapData {
    /// Read the two values out of the text of a `map_data.sii` (with its
    /// includes already put in).
    pub fn parse(text: &str) -> Option<MapData> {
        let mut center = None;
        let mut size = None;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            let line = line.split("//").next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("ui_map_center") {
                center = pair(rest).or(center);
            } else if let Some(rest) = line.strip_prefix("ui_map_size") {
                size = pair(rest).or(size);
            }
        }
        let (center, size_km) = (center?, size?);
        (size_km.0 > 0.0 && size_km.1 > 0.0).then_some(MapData { center, size_km })
    }

    /// The area of the world the picture covers.
    pub fn bounds(&self, scale: f64) -> Bounds {
        let half_x = self.size_km.0 * 1000.0 / scale / 2.0;
        let half_z = self.size_km.1 * 1000.0 / scale / 2.0;
        Bounds {
            min_x: self.center.0 - half_x,
            max_x: self.center.0 + half_x,
            min_z: self.center.1 - half_z,
            max_z: self.center.1 + half_z,
        }
    }

    /// The same placement, give or take what rounding in a file changes.
    pub fn matches(&self, other: &MapData) -> bool {
        let near = |a: f64, b: f64| (a - b).abs() < 1.0;
        near(self.center.0, other.center.0)
            && near(self.center.1, other.center.1)
            && near(self.size_km.0, other.size_km.0)
            && near(self.size_km.1, other.size_km.1)
    }
}

/// `: (a, b)` as two numbers.
fn pair(rest: &str) -> Option<(f64, f64)> {
    let inner = rest.split_once('(')?.1.split_once(')')?.0;
    let (a, b) = inner.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// The sector a `map/<name>/sec+XXXX+YYYY.base` path stands for, with the
/// name of its map. `path` in lower case, without a leading slash.
pub fn sector_of(path: &str) -> Option<(&str, i16, i16)> {
    let (map, file) = path.strip_prefix("map/")?.split_once('/')?;
    let numbers = file.strip_prefix("sec")?.strip_suffix(".base")?;
    if numbers.len() != 10 || !numbers.is_char_boundary(5) {
        return None;
    }
    Some((map, numbers[..5].parse().ok()?, numbers[5..].parse().ok()?))
}

/// What one mod has to do with the world map.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapFacts {
    /// It brings a `def/map_data.sii`.
    pub has_data: bool,
    /// The placement that file gives; `None` when it could not be read.
    pub data: Option<MapData>,
    /// It brings the map picture.
    pub background: bool,
    /// Its map sectors (x, z), sorted, whatever map they belong to.
    pub sectors: Vec<(i16, i16)>,
    /// The maps (folders under `map/`) those sectors are in.
    pub maps: Vec<String>,
}

impl MapFacts {
    pub fn is_empty(&self) -> bool {
        !self.has_data && !self.background && self.sectors.is_empty()
    }
}

/// The placement in the `map_data.sii` stored as `path` in `source`.
/// Follows `@include`s, which is how some maps split the file up.
pub fn read_map_data(source: &mut dyn ModSource, path: &str) -> Option<MapData> {
    let mut text = String::new();
    append_with_includes(source, path, 0, &mut text);
    MapData::parse(&text)
}

fn append_with_includes(source: &mut dyn ModSource, path: &str, depth: usize, out: &mut String) {
    let Ok(data) = source.read_bytes(path) else {
        return;
    };
    let Ok(text) = sii_bytes_to_text(&data, false) else {
        return;
    };
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    for line in text.lines() {
        let included = line.trim_start().strip_prefix("@include").and_then(|rest| rest.split('"').nth(1));
        match included {
            Some(name) if depth < 3 => {
                let target = match name.strip_prefix('/') {
                    Some(absolute) => absolute.to_string(),
                    None if dir.is_empty() => name.to_string(),
                    None => format!("{dir}/{name}"),
                };
                append_with_includes(source, &target, depth + 1, out);
            }
            Some(_) => {}
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
}

/// The placement the game itself uses, from the `def.scs` in its folder.
pub fn game_map_data(game_dir: &Path) -> Option<MapData> {
    let mut reader = HashFsReader::open(&game_dir.join("def.scs")).ok()?;
    read_map_data(&mut reader, DATA_FILE)
}

/// Find the map sectors of archives that do not list their files: their
/// names follow a pattern, so every name is tried against the nameless
/// hashes. `mods` are changed in place.
pub fn find_hidden_sectors(mods: &mut [ScannedMod]) {
    let nameless: std::collections::HashMap<u64, Vec<usize>> = {
        let mut map: std::collections::HashMap<u64, Vec<usize>> = std::collections::HashMap::new();
        for (index, m) in mods.iter().enumerate() {
            for hash in m.hidden.iter() {
                map.entry(*hash).or_default().push(index);
            }
        }
        map
    };
    if nameless.is_empty() {
        return;
    }
    // the maps of both games, and whatever maps the mods themselves name
    let maps: BTreeSet<String> = ["europe", "usa"]
        .into_iter()
        .map(str::to_string)
        .chain(mods.iter().flat_map(|m| m.world_map.maps.iter().cloned()))
        .collect();
    let maps: Vec<String> = maps.into_iter().collect();
    let found: Vec<(usize, usize, i16, i16)> = (-SECTOR_RANGE..=SECTOR_RANGE)
        .into_par_iter()
        .flat_map_iter(|x| {
            let mut found = Vec::new();
            let mut name = String::with_capacity(48);
            for (map_index, map) in maps.iter().enumerate() {
                for z in -SECTOR_RANGE..=SECTOR_RANGE {
                    name.clear();
                    sector_file(map, x, z, &mut name);
                    if let Some(owners) = nameless.get(&city_hash64(name.as_bytes())) {
                        found.extend(owners.iter().map(|&owner| (owner, map_index, x, z)));
                    }
                }
            }
            found
        })
        .collect();
    for (owner, map_index, x, z) in found {
        let facts = &mut mods[owner].world_map;
        facts.sectors.push((x, z));
        if !facts.maps.contains(&maps[map_index]) {
            facts.maps.push(maps[map_index].clone());
        }
    }
    for m in mods.iter_mut().filter(|m| !m.hidden.is_empty()) {
        m.world_map.sectors.sort_unstable();
        m.world_map.sectors.dedup();
        m.world_map.maps.sort();
    }
}

/// `map/<map>/sec+XXXX+YYYY.base`, appended to `out`.
fn sector_file(map: &str, x: i16, z: i16, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(out, "map/{map}/sec{x:+05}{z:+05}.base");
}

/// What the picture on screen was made for, and who says so.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnFor {
    pub data: MapData,
    /// The installed mod (index into the scan) whose `map_data.sii` tells;
    /// `None` for the game's own picture and values.
    pub told_by: Option<usize>,
}

/// An active mod with sectors the picture does not reach.
#[derive(Debug, Clone, PartialEq)]
pub struct Outside {
    /// Position in the active list.
    pub position: usize,
    pub sectors: usize,
    pub total: usize,
}

/// The world map of one load order. Positions count in the active list,
/// top first.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The mod whose `map_data.sii` the game uses: the topmost one that
    /// has it. `None`: no active mod has one, the game's own applies.
    pub data_from: Option<usize>,
    /// The placement in effect; `None` when the file could not be read.
    pub data: Option<MapData>,
    /// Active mods below that bring the file too, to no effect.
    pub data_overridden: Vec<usize>,
    /// The mod whose map picture is shown; `None` for the game's own.
    pub background_from: Option<usize>,
    pub background_overridden: Vec<usize>,
    /// What that picture was made for, where something tells.
    pub drawn_for: Option<DrawnFor>,
    /// The placement in effect is not the one the picture was made for.
    pub mismatch: bool,
    /// Active mods whose `map_data.sii` has the placement the picture was
    /// made for: one of them belongs on top of the others.
    pub matching: Vec<usize>,
    /// The area the picture covers under the placement in effect.
    pub bounds: Option<Bounds>,
    pub outside: Vec<Outside>,
}

impl Report {
    /// Something here needs the user's attention.
    pub fn has_problem(&self) -> bool {
        self.mismatch || !self.outside.is_empty()
    }
}

/// How much of what `needy` needs from elsewhere `holder` has, as
/// (files, of all it needs).
fn needs_from(needy: &ScannedMod, holder: &ScannedMod) -> (usize, usize) {
    let files = needy
        .wants
        .iter()
        .filter(|path| holder.provides.binary_search(&city_hash64(path.as_bytes())).is_ok())
        .count();
    (files, needy.wants.len())
}

/// The placement the picture of `installed[holder]` was made for.
fn made_for(installed: &[ScannedMod], holder: usize) -> Option<DrawnFor> {
    let picture = &installed[holder];
    if let Some(data) = picture.world_map.data {
        return Some(DrawnFor { data, told_by: Some(holder) });
    }
    // the map_data.sii of the package that goes with this one: a package
    // of the same release that needs files of it, else the one that
    // builds on it most
    installed
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != holder)
        .filter_map(|(index, m)| {
            let data = m.world_map.data?;
            let (files, of) = needs_from(m, picture);
            if files < MIN_SHARED {
                return None;
            }
            // (`of` is not zero once `files` is not)
            let same = same_release(m, picture);
            (same || files * 100 >= of * MIN_SHARED_PERCENT).then(|| (same, files * 1000 / of, index, data))
        })
        .max_by_key(|(same, share, index, _)| (*same, *share, std::cmp::Reverse(*index)))
        .map(|(_, _, index, data)| DrawnFor { data, told_by: Some(index) })
}

/// Two packages of one release: the same author and the same version in
/// their manifests. A map split into packages numbers them alike, and its
/// definitions need far more files of the model package than of the one
/// with the picture - too few to tell by share alone.
fn same_release(a: &ScannedMod, b: &ScannedMod) -> bool {
    let (Some(a), Some(b)) = (&a.manifest, &b.manifest) else {
        return false;
    };
    let same = |x: &str, y: &str| !x.trim().is_empty() && x.trim().eq_ignore_ascii_case(y.trim());
    same(&a.author, &b.author) && same(&a.package_version, &b.package_version)
}

/// Look at the world map of a load order. `active` is the list top
/// first, as indices into `installed` (`None` for an entry that is not
/// installed); `game` the game's own placement where it could be read.
/// `None` when no active mod has anything to do with the map.
pub fn analyze(installed: &[ScannedMod], active: &[Option<usize>], game: Option<MapData>, scale: f64) -> Option<Report> {
    let facts = |position: usize| active[position].map(|index| &installed[index].world_map);
    let positions = |test: fn(&MapFacts) -> bool| -> Vec<usize> {
        (0..active.len()).filter(|&p| facts(p).is_some_and(test)).collect()
    };
    if (0..active.len()).all(|p| facts(p).is_none_or(MapFacts::is_empty)) {
        return None;
    }

    let mut with_data = positions(|f| f.has_data);
    let mut with_background = positions(|f| f.background);
    let data_from = (!with_data.is_empty()).then(|| with_data.remove(0));
    let background_from = (!with_background.is_empty()).then(|| with_background.remove(0));

    let data = match data_from {
        Some(position) => facts(position).and_then(|f| f.data),
        None => game,
    };
    let drawn_for = match background_from {
        Some(position) => active[position].and_then(|index| made_for(installed, index)),
        None => game.map(|data| DrawnFor { data, told_by: None }),
    };
    let mismatch = matches!((&data, &drawn_for), (Some(have), Some(want)) if !have.matches(&want.data));
    let matching = match &drawn_for {
        Some(want) => (0..active.len())
            .filter(|&p| facts(p).and_then(|f| f.data).is_some_and(|d| d.matches(&want.data)))
            .collect(),
        None => Vec::new(),
    };

    let bounds = data.map(|d| d.bounds(scale));
    let outside = match bounds {
        Some(b) => (0..active.len())
            .filter_map(|position| {
                let sectors = &facts(position)?.sectors;
                let out = sectors
                    .iter()
                    .filter(|(x, z)| {
                        let (x, z) = (f64::from(*x) * SECTOR_SIZE, f64::from(*z) * SECTOR_SIZE);
                        x + SECTOR_SIZE <= b.min_x || x >= b.max_x || z + SECTOR_SIZE <= b.min_z || z >= b.max_z
                    })
                    .count();
                (out >= MIN_OUTSIDE && out * 100 >= sectors.len() * MIN_OUTSIDE_PERCENT).then_some(Outside {
                    position,
                    sectors: out,
                    total: sectors.len(),
                })
            })
            .collect(),
        None => Vec::new(),
    };

    Some(Report {
        data_from,
        data,
        data_overridden: with_data,
        background_from,
        background_overridden: with_background,
        drawn_for,
        mismatch,
        matching,
        bounds,
        outside,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const GAME: MapData = MapData { center: (-13500.0, -4950.0), size_km: (5308.2, 5200.0) };
    const WIDE: MapData = MapData { center: (66700.0, -24700.0), size_km: (8127.0, 7928.0) };

    fn plain(name: &str) -> ScannedMod {
        crate::scanner::scan_one(Path::new(name), None)
    }

    fn with_data(name: &str, data: MapData) -> ScannedMod {
        let mut m = plain(name);
        m.world_map.has_data = true;
        m.world_map.data = Some(data);
        m
    }

    fn with_picture(name: &str, files: &[&str]) -> ScannedMod {
        let mut m = plain(name);
        m.world_map.background = true;
        let mut provides: Vec<u64> = files.iter().map(|f| city_hash64(f.as_bytes())).collect();
        provides.sort_unstable();
        m.provides = Arc::from(provides);
        m
    }

    fn needing(mut m: ScannedMod, wants: &[&str]) -> ScannedMod {
        let mut wants: Vec<String> = wants.iter().map(|w| w.to_string()).collect();
        wants.sort();
        m.wants = Arc::from(wants);
        m
    }

    fn with_sectors(name: &str, sectors: &[(i16, i16)]) -> ScannedMod {
        let mut m = plain(name);
        m.world_map.sectors = sectors.to_vec();
        m
    }

    fn all(count: usize) -> Vec<Option<usize>> {
        (0..count).map(Some).collect()
    }

    const ASSETS: &[&str] = &["a/1", "a/2", "a/3", "a/4", "a/5", "a/6"];

    #[test]
    fn the_two_values_are_read_whatever_stands_around_them() {
        let text = "map_data : .map.data {\n\tzoom_uplift[7]: 45000.0\n\tui_map_center:\t(66700, -24700)\t// in game units\n \
                    ui_map_size:   (8127, 7928.5) # km\n\tui_map_camera_min: (-100000.0, -170000.0)\n}";
        assert_eq!(MapData::parse(text), Some(MapData { center: (66700.0, -24700.0), size_km: (8127.0, 7928.5) }));
        // a commented-out line is not a value; one value alone is not enough
        assert_eq!(MapData::parse("# ui_map_center: (1, 2)\nui_map_size: (3, 4)"), None);
        assert_eq!(MapData::parse("ui_map_center: (1, 2)\nui_map_size: (0, 4)"), None);
        assert_eq!(MapData::parse("ui_map_center: (1; 2)\nui_map_size: (3, 4)"), None);
    }

    #[test]
    fn the_picture_covers_its_size_at_the_scale_of_the_world() {
        let b = GAME.bounds(19.0);
        assert!((b.min_x - -153189.5).abs() < 1.0 && (b.max_x - 126189.5).abs() < 1.0);
        assert!((b.min_z - -141792.1).abs() < 1.0 && (b.max_z - 131892.1).abs() < 1.0);
        assert!(GAME.matches(&MapData { center: (-13500.4, -4950.0), size_km: (5308.2, 5200.3) }));
        assert!(!GAME.matches(&WIDE));
    }

    #[test]
    fn sector_names_are_told_from_other_map_files() {
        assert_eq!(sector_of("map/europe/sec+0010-0002.base"), Some(("europe", 10, -2)));
        assert_eq!(sector_of("map/russia/sec-0034+0021.base"), Some(("russia", -34, 21)));
        for other in ["map/europe/sec+0010-0002.aux", "map/europe.mbd", "map/europe/sec+10-2.base", "def/sec+0010-0002.base"] {
            assert_eq!(sector_of(other), None, "{other}");
        }
    }

    #[test]
    fn includes_are_followed_when_reading_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let def = tmp.path().join("def");
        std::fs::create_dir_all(def.join("map_data")).unwrap();
        std::fs::write(
            def.join("map_data.sii"),
            "SiiNunit\n{\nmap_data : .map.data {\n@include \"/def/map_data/ui_map.sui\"\n@include \"map_data/size.sui\"\n}\n}",
        )
        .unwrap();
        std::fs::write(def.join("map_data/ui_map.sui"), "\tui_map_center:\t(260450, 43850)\t// editor\n").unwrap();
        std::fs::write(def.join("map_data/size.sui"), "\tui_map_size: (17495.28, 17138.67)\n").unwrap();
        let mut source = crate::scs::dir::DirSource::open(tmp.path()).unwrap();
        assert_eq!(
            read_map_data(&mut source, DATA_FILE),
            Some(MapData { center: (260450.0, 43850.0), size_km: (17495.28, 17138.67) })
        );
        assert_eq!(read_map_data(&mut source, "def/none.sii"), None);
    }

    #[test]
    fn mods_that_leave_the_map_alone_give_no_report() {
        let mods = [plain("truck"), plain("sound")];
        assert_eq!(analyze(&mods, &all(2), Some(GAME), 19.0), None);
        assert_eq!(analyze(&mods, &[None], Some(GAME), 19.0), None);
    }

    #[test]
    fn the_topmost_map_data_counts_and_must_fit_the_picture() {
        // a map in three packages, and another map that only widens the
        // camera and keeps the game's placement
        let mods = [
            with_data("other-def", GAME),
            needing(with_data("pack-def", WIDE), ASSETS),
            with_picture("pack-assets", ASSETS),
        ];
        // the other map on top: its values with the pack's picture
        let report = analyze(&mods, &all(3), Some(GAME), 19.0).unwrap();
        assert_eq!((report.data_from, report.data), (Some(0), Some(GAME)));
        assert_eq!(report.data_overridden, [1]);
        assert_eq!(report.background_from, Some(2));
        assert_eq!(report.drawn_for, Some(DrawnFor { data: WIDE, told_by: Some(1) }));
        assert!(report.mismatch && report.has_problem());
        assert_eq!(report.matching, [1]);

        // the pack's own definitions on top: all is well
        let report = analyze(&mods, &[Some(1), Some(0), Some(2)], Some(GAME), 19.0).unwrap();
        assert_eq!((report.data_from, report.data), (Some(0), Some(WIDE)));
        assert!(!report.mismatch && !report.has_problem());
        assert_eq!(report.matching, [0]);
    }

    #[test]
    fn the_games_own_picture_wants_the_games_own_values() {
        // nothing replaces the picture, but a map brings values for one
        // that is not there
        let mods = [with_data("far-map", WIDE)];
        let report = analyze(&mods, &all(1), Some(GAME), 19.0).unwrap();
        assert_eq!(report.background_from, None);
        assert_eq!(report.drawn_for, Some(DrawnFor { data: GAME, told_by: None }));
        assert!(report.mismatch && report.matching.is_empty());

        // a picture without anything saying where it lies, under the
        // game's values: the picture alone was activated
        let mods = [needing(with_data("pack-def", WIDE), ASSETS), with_picture("pack-assets", ASSETS)];
        let report = analyze(&mods, &[Some(1)], Some(GAME), 19.0).unwrap();
        assert_eq!((report.data_from, report.data), (None, Some(GAME)));
        assert_eq!(report.drawn_for, Some(DrawnFor { data: WIDE, told_by: Some(0) }));
        assert!(report.mismatch);
    }

    #[test]
    fn a_mod_with_picture_and_values_speaks_for_itself() {
        let mut both = with_picture("background", &["x/1"]);
        both.world_map.has_data = true;
        both.world_map.data = Some(WIDE);
        let mods = [both, with_data("other-def", GAME)];
        let report = analyze(&mods, &all(2), Some(GAME), 19.0).unwrap();
        assert_eq!(report.drawn_for, Some(DrawnFor { data: WIDE, told_by: Some(0) }));
        assert!(!report.mismatch);
        // the other way round the values of the other mod win
        let report = analyze(&mods, &[Some(1), Some(0)], Some(GAME), 19.0).unwrap();
        assert!(report.mismatch);
        assert_eq!(report.matching, [1]);
    }

    #[test]
    fn nothing_is_claimed_where_nothing_tells_what_the_picture_is_for() {
        // a map that needs a few files of the picture's package among many
        // others is not its definition package
        let mut wants: Vec<String> = (0..400).map(|i| format!("m/{i}")).collect();
        wants.extend(ASSETS.iter().map(|a| a.to_string()));
        let wants: Vec<&str> = wants.iter().map(String::as_str).collect();
        let mods = [needing(with_data("unrelated", GAME), &wants), with_picture("pack-assets", ASSETS)];
        let report = analyze(&mods, &all(2), Some(GAME), 19.0).unwrap();
        assert_eq!(report.drawn_for, None);
        assert!(!report.mismatch && report.matching.is_empty());

        // the same few files tell once both are packages of one release
        let released = |mut m: ScannedMod| {
            m.manifest = Some(crate::manifest::ModManifest {
                author: "Map Team".into(),
                package_version: "2.84".into(),
                ..Default::default()
            });
            m
        };
        let mods = [
            released(needing(with_data("pack-def", WIDE), &wants)),
            released(with_picture("pack-assets", ASSETS)),
            needing(with_data("unrelated", GAME), &wants),
        ];
        let report = analyze(&mods, &[Some(2), Some(0), Some(1)], Some(GAME), 19.0).unwrap();
        assert_eq!(report.drawn_for, Some(DrawnFor { data: WIDE, told_by: Some(0) }));
        assert!(report.mismatch);
        // ... and not when the version differs or nothing of it is needed
        let mut other_version = released(needing(with_data("old-def", WIDE), &wants));
        other_version.manifest.as_mut().unwrap().package_version = "2.70".into();
        let mods = [other_version, released(with_picture("pack-assets", ASSETS)), released(with_data("lone-def", WIDE))];
        assert_eq!(analyze(&mods, &all(3), Some(GAME), 19.0).unwrap().drawn_for, None);

        // values that could not be read are not compared either
        let mut unread = plain("odd-def");
        unread.world_map.has_data = true;
        let report = analyze(&[unread], &all(1), Some(GAME), 19.0).unwrap();
        assert_eq!((report.data_from, report.data, report.mismatch), (Some(0), None, false));
        assert_eq!(report.bounds, None);
    }

    #[test]
    fn sectors_beyond_the_picture_are_counted_per_map() {
        // the game's picture ends near x = 126190; sector 32 starts at 128000
        let inside: Vec<(i16, i16)> = (0..30).map(|x| (x, 0)).collect();
        let mut reaching = inside.clone();
        reaching.extend((32..40).map(|x| (x, 0)));
        let mods = [with_sectors("near", &inside), with_sectors("far", &reaching), with_sectors("edge", &[(31, 0), (32, 0)])];
        let report = analyze(&mods, &all(3), Some(GAME), 19.0).unwrap();
        assert_eq!(report.outside, [Outside { position: 1, sectors: 8, total: 38 }]);
        assert!(report.has_problem() && !report.mismatch);
        // under wider values the same sectors are covered
        let wide = [with_data("def", WIDE), with_sectors("far", &reaching)];
        let report = analyze(&wide, &all(2), Some(GAME), 19.0).unwrap();
        assert!(report.outside.is_empty());
    }

    #[test]
    fn hidden_sectors_are_found_by_trying_their_names() {
        let mut locked = plain("locked");
        let mut hidden: Vec<u64> = ["map/europe/sec+0035-0004.base", "map/europe/sec-0004+0019.base", "map/europe/sec+0035-0004.aux"]
            .iter()
            .map(|p| city_hash64(p.as_bytes()))
            .collect();
        hidden.push(city_hash64(b"map/russia/sec+0012-0020.base"));
        hidden.sort_unstable();
        locked.hidden = Arc::from(hidden);
        let mut listed = plain("listed");
        listed.world_map.maps = vec!["russia".to_string()];
        let mut mods = [locked, listed];
        find_hidden_sectors(&mut mods);
        assert_eq!(mods[0].world_map.sectors, [(-4, 19), (12, -20), (35, -4)]);
        assert_eq!(mods[0].world_map.maps, ["europe", "russia"]);
        assert!(mods[1].world_map.sectors.is_empty());
    }
}
