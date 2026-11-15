//! Read-only command-line front end for scsmm-core. Useful for checking
//! what the scanner and the load-order logic see without opening the GUI.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::Instant;

use scsmm_core::classify::Confidence;
use scsmm_core::game::{self, Game, GameInstall};
use scsmm_core::gamepaths::GamePaths;
use scsmm_core::identity::ActiveModMatcher;
use scsmm_core::load_order::{self, Row, DEFAULT_MAP_BASE_NAMES};
use scsmm_core::profile;
use scsmm_core::scanner::{self, ScannedMod};
use scsmm_core::steam;
use scsmm_core::workshop_api::WorkshopCache;

const USAGE: &str = "\
scsmm - inspect ETS2/ATS mods and load order (read-only)

USAGE:
    scsmm installs                         detected game data folders
    scsmm scan     [--game ets2|ats]       every installed mod and its category
    scsmm classify [--game ets2|ats] [--all]
                                           what the content of each mod says it
                                           is, and why; without --all only the
                                           mods where that differs from the
                                           manifest
    scsmm profiles [--game ets2|ats]       profiles and their active mod count
    scsmm order    [--game ets2|ats] [--profile <name>] [--sorted]
                                           load order of a profile, grouped;
                                           --sorted previews the automatic sort
    scsmm extract <archive.scs> <folder>   unpack an archive into an empty folder
                                           (packed textures come out as .dds)
";

struct Args {
    game: Game,
    profile: Option<String>,
    sorted: bool,
    all: bool,
}

fn parse_args(rest: &[String]) -> Result<Args, String> {
    let mut args = Args { game: Game::Ets2, profile: None, sorted: false, all: false };
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game" => {
                let v = it.next().ok_or("--game needs a value")?;
                args.game = Game::parse(v).ok_or_else(|| format!("unknown game '{v}'"))?;
            }
            "--profile" => args.profile = Some(it.next().ok_or("--profile needs a value")?.clone()),
            "--sorted" => args.sorted = true,
            "--all" => args.all = true,
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(args)
}

fn install_for(game: Game, libs: &[std::path::PathBuf]) -> Result<GameInstall, String> {
    game::detect_game_installs(game, libs)
        .into_iter()
        .next()
        .ok_or_else(|| format!("no {} install detected", game.short_name()))
}

/// Where the app keeps what it can fetch or build again - the same
/// folder the GUI uses, so both share the game's path index.
fn cache_dir() -> std::path::PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) if cfg!(windows) => std::path::PathBuf::from(local).join("easy-scsmodmanager-rs"),
        _ => profile::system_data_dir().join("cache"),
    }
}

fn scan(install: &GameInstall, libs: &[std::path::PathBuf]) -> Vec<ScannedMod> {
    let version = game::read_game_version(&install.documents_dir);
    let install_dir = game::find_game_install_dir(install.game, libs);
    let owned = game::owned_dlc_tokens(install_dir.as_deref());
    // the game's own paths tell a replaced file from a new one; built on
    // the first run (a second or two), then read from the cache
    let cache = cache_dir().join("gamepaths").join(format!("{}.bin", install.game.short_name().to_lowercase()));
    let mut game_paths = install_dir.as_deref().and_then(|dir| GamePaths::load_or_build(dir, &cache).ok());
    scanner::scan_game_install(install, version.as_deref(), owned.as_ref(), game_paths.as_mut())
}

fn cmd_installs() -> Result<(), String> {
    let installs = game::detect_all_installs();
    if installs.is_empty() {
        println!("no installs detected");
    }
    for i in installs {
        println!("{} ({:?})", i.game.short_name(), i.kind);
        println!("  documents: {}", i.documents_dir.display());
        match &i.workshop_dir {
            Some(w) => println!("  workshop:  {}", w.display()),
            None => println!("  workshop:  (none)"),
        }
        match game::read_game_version(&i.documents_dir) {
            Some(v) => println!("  version:   {v}"),
            None => println!("  version:   (unknown)"),
        }
    }
    Ok(())
}

fn cmd_scan(args: &Args) -> Result<(), String> {
    let libs = steam::discover_steam_libraries();
    let install = install_for(args.game, &libs)?;
    let started = Instant::now();
    let mods = scan(&install, &libs);
    let elapsed = started.elapsed();

    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut failed = 0;
    for m in &mods {
        let cats = m.effective_categories(&[]);
        *by_category.entry(cats[0].clone()).or_default() += 1;
        let source = if m.workshop_id().is_some() { "ws" } else { "local" };
        let note = match &m.error {
            Some(e) => {
                failed += 1;
                format!("  !! {e}")
            }
            None => String::new(),
        };
        println!(
            "{:<13} {:<5} {:<9} {}{}",
            cats.join(","),
            source,
            format!("{:?}", m.format).to_lowercase(),
            m.display_name(),
            note
        );
    }
    println!("\n{} mods in {:.2?} ({failed} unreadable)", mods.len(), elapsed);
    for (cat, n) in by_category {
        println!("  {cat:<14} {n}");
    }
    Ok(())
}

fn cmd_classify(args: &Args) -> Result<(), String> {
    let libs = steam::discover_steam_libraries();
    let install = install_for(args.game, &libs)?;
    let started = Instant::now();
    let mods = scan(&install, &libs);
    let elapsed = started.elapsed();
    // workshop mods rarely name themselves in the manifest
    let titles = WorkshopCache::open(&cache_dir().join("workshop"));

    let (mut applied, mut suggested) = (0, 0);
    for m in &mods {
        let verdict = m.verdict();
        let suggestion = m.suggested_category(&[]);
        let auto = m.effective_categories(&[]);
        let overruled =
            verdict.as_ref().is_some_and(|v| v.applied && auto.first().map(String::as_str) == Some(v.category));
        applied += usize::from(overruled);
        suggested += usize::from(suggestion.is_some());
        if !args.all && !overruled && suggestion.is_none() {
            continue;
        }
        let title =
            m.workshop_id().and_then(|id| titles.title(&id).map(str::to_string)).unwrap_or_else(|| m.display_name());
        let manifest = m.manifest.as_ref().map(|x| x.categories.join(",")).unwrap_or_default();
        println!("{title}");
        print!("    manifest: {:<22} auto: {}", if manifest.is_empty() { "-" } else { &manifest }, auto.join(","));
        match &verdict {
            Some(v) => {
                let sure = if v.confidence == Confidence::High { "high" } else { "medium" };
                let what = if overruled {
                    "from content".to_string()
                } else if suggestion.is_some() {
                    format!("suggests {}", v.category)
                } else {
                    // a map descriptor or the physics file decided instead
                    format!("content says {}", v.category)
                };
                println!("    <- {what} ({sure}, {:.0}% of definitions)", v.share * 100.0);
            }
            None => println!(),
        }
        let e = &m.evidence;
        let signals: Vec<String> = e
            .signals
            .iter()
            .take(6)
            .map(|s| format!("{}{} x{}", if s.from_content { "unit " } else { "" }, s.key, s.count))
            .collect();
        if !signals.is_empty() {
            println!("    {}", signals.join(" | "));
        }
        if e.hidden > 0 {
            println!(
                "    locked: {} unnamed files, {} of {} looked into could be read",
                e.hidden, e.hidden_read, e.hidden_tried
            );
        }
    }
    println!("\n{} mods in {:.2?}: {applied} recategorised from content, {suggested} suggestions", mods.len(), elapsed);
    Ok(())
}

fn cmd_profiles(args: &Args) -> Result<(), String> {
    let libs = steam::discover_steam_libraries();
    let install = install_for(args.game, &libs)?;
    for sii in profile::discover_profiles(&install) {
        match profile::read_profile(&sii) {
            Ok(p) => println!("{:<24} {:>4} active mods   {}", p.profile_name, p.active_mods.len(), sii.display()),
            Err(e) => println!("(unreadable: {e})   {}", sii.display()),
        }
    }
    Ok(())
}

fn cmd_order(args: &Args) -> Result<(), String> {
    let libs = steam::discover_steam_libraries();
    let install = install_for(args.game, &libs)?;
    let profiles: Vec<_> =
        profile::discover_profiles(&install).into_iter().filter_map(|sii| profile::read_profile(&sii).ok()).collect();
    let chosen = match &args.profile {
        Some(name) => profiles
            .iter()
            .find(|p| p.profile_name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("no profile named '{name}'"))?,
        // default: the profile with the most active mods
        None => profiles.iter().max_by_key(|p| p.active_mods.len()).ok_or("no profiles found")?,
    };

    let mods = scan(&install, &libs);
    let matcher = ActiveModMatcher::new(&mods);

    // display order: top = highest index
    let mut display: Vec<_> = chosen.active_mods.iter().rev().collect();
    let token_for = |a: &profile::ActiveMod| -> String {
        if load_order::is_map_base(&a.name, &a.display_name, DEFAULT_MAP_BASE_NAMES) {
            return load_order::MAP_BASE.to_string();
        }
        match matcher.lookup(a) {
            Some(i) => mods[i].effective_categories(&[])[0].clone(),
            None => "other".to_string(),
        }
    };
    let mut tokens: Vec<String> = display.iter().map(|a| token_for(a)).collect();
    let before = load_order::misplaced_count(&tokens);
    if args.sorted {
        let order = load_order::auto_sort_order(&tokens);
        display = order.iter().map(|&i| display[i]).collect();
        tokens = order.iter().map(|&i| tokens[i].clone()).collect();
    }

    println!(
        "profile '{}': {} active mods, {} misplaced{}",
        chosen.profile_name,
        display.len(),
        before,
        if args.sorted { " (showing automatic sort)" } else { "" }
    );
    for row in load_order::build_rows(&tokens) {
        match row {
            Row::Spacer { group_id } => println!("--- {group_id} ---"),
            Row::Mod { index, misplaced, expected_group_id } => {
                let a = display[index];
                let label = if a.display_name.is_empty() { &a.name } else { &a.display_name };
                let missing = if matcher.lookup(a).is_none() { "  [not installed]" } else { "" };
                let flag = if misplaced { format!("  <- belongs in {expected_group_id}") } else { String::new() };
                println!("    {label}{missing}{flag}");
            }
        }
    }
    Ok(())
}

fn cmd_extract(rest: &[String]) -> Result<(), String> {
    let [archive, dest] = rest else {
        return Err("usage: scsmm extract <archive.scs> <folder>".into());
    };
    let (archive, dest) = (std::path::Path::new(archive), std::path::Path::new(dest));
    // never mix an extraction into existing files
    if dest.exists() && std::fs::read_dir(dest).map_err(|e| e.to_string())?.next().is_some() {
        return Err(format!("{} is not empty", dest.display()));
    }
    let started = Instant::now();
    let mut last_percent = usize::MAX;
    let result = scsmm_core::extract::extract_scs(
        archive,
        dest,
        &mut |done, total| {
            let percent = done * 100 / total.max(1);
            if percent != last_percent && percent % 10 == 0 {
                last_percent = percent;
                eprintln!("  {percent}%  ({done}/{total})");
            }
        },
        &|| false,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "{} of {} files extracted in {:.2?}, {} failed",
        result.extracted,
        result.total,
        started.elapsed(),
        result.failed
    );
    if result.recovered + result.unnamed > 0 {
        println!(
            "locked archive: {} file names recovered, {} files without a name written to {}/",
            result.recovered,
            result.unnamed,
            scsmm_core::extract::UNNAMED_DIR
        );
    }
    for f in &result.failures {
        println!("  failed: {}  ({})", f.entry, f.error);
    }
    Ok(())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = argv.split_first() else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    let result = match cmd.as_str() {
        "installs" => cmd_installs(),
        "scan" => parse_args(rest).and_then(|a| cmd_scan(&a)),
        "classify" => parse_args(rest).and_then(|a| cmd_classify(&a)),
        "profiles" => parse_args(rest).and_then(|a| cmd_profiles(&a)),
        "order" => parse_args(rest).and_then(|a| cmd_order(&a)),
        "extract" => cmd_extract(rest),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
