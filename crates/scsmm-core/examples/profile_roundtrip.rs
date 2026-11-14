//! Checks the profile writer against a real profile without touching it.
//!
//!     cargo run -p scsmm-core --example profile_roundtrip -- <path to profile.sii>
//!
//! The profile directory is copied to a temp dir and every write happens on
//! that copy: the active list is reversed and saved, read back, then the
//! original order is saved again and compared with the original text.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use scsmm_core::profile::{read_profile, save_active_mods};
use scsmm_core::sii::crypto::{is_scsc, sii_bytes_to_text};

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn is_active_mods_line(line: &str) -> bool {
    line.trim_start().starts_with("active_mods")
}

fn other_lines(text: &str) -> Vec<&str> {
    text.split('\n').filter(|l| !is_active_mods_line(l)).collect()
}

fn check(label: &str, ok: bool, failures: &mut u32) {
    println!("  [{}] {label}", if ok { "ok" } else { "FAIL" });
    if !ok {
        *failures += 1;
    }
}

fn run(original_sii: &Path) -> Result<u32, Box<dyn std::error::Error>> {
    let original_dir = original_sii.parent().ok_or("profile path has no parent")?;
    let tmp = tempfile::tempdir()?;
    let copy_dir_path: PathBuf = tmp.path().join("profiles").join(
        original_dir.file_name().ok_or("profile dir has no name")?,
    );
    copy_dir(original_dir, &copy_dir_path)?;
    let sii = copy_dir_path.join("profile.sii");
    let backups = tmp.path().join("backups");

    let original_bytes = fs::read(&sii)?;
    let original_text = sii_bytes_to_text(&original_bytes, true)?;
    let original = read_profile(&sii)?;
    println!(
        "profile '{}': {} active mods, {} bytes on disk, {}",
        original.profile_name,
        original.active_mods.len(),
        original_bytes.len(),
        if is_scsc(&original_bytes) { "encrypted (ScsC)" } else { "plaintext" }
    );
    println!("working on a copy in {}", copy_dir_path.display());

    let mut failures = 0;

    // 1) save a reversed list
    let mut reversed = original.active_mods.clone();
    reversed.reverse();
    let backup = save_active_mods(&sii, &reversed, &backups)?;
    let after_bytes = fs::read(&sii)?;
    let after_text = sii_bytes_to_text(&after_bytes, true)?;
    let after = read_profile(&sii)?;
    println!("after saving the reversed list:");
    check("list reads back exactly as written", after.active_mods == reversed, &mut failures);
    check("profile name unchanged", after.profile_name == original.profile_name, &mut failures);
    check("written as plaintext", !is_scsc(&after_bytes), &mut failures);
    check(
        "every line outside active_mods is byte-identical",
        other_lines(&original_text) == other_lines(&after_text),
        &mut failures,
    );
    let entry_values = |text: &str| -> Vec<String> {
        let mut v: Vec<String> = text
            .split('\n')
            .filter(|l| is_active_mods_line(l) && l.contains('['))
            .filter_map(|l| l.split_once(':').map(|(_, value)| value.trim().to_string()))
            .collect();
        v.sort();
        v
    };
    check(
        "every entry string kept its exact original escaping",
        entry_values(&original_text) == entry_values(&after_text),
        &mut failures,
    );

    // 2) the backup holds the pre-edit profile
    let mut zip = zip::ZipArchive::new(fs::File::open(&backup.path)?)?;
    let mut backed_up = Vec::new();
    zip.by_name("profile.sii")?.read_to_end(&mut backed_up)?;
    check("backup holds the original profile.sii bytes", backed_up == original_bytes, &mut failures);
    let originals = fs::read_dir(original_dir)?.flatten().filter(|e| e.path().is_file()).count();
    check(
        &format!("backup holds all {originals} top-level files of the profile dir"),
        (0..zip.len()).filter(|&i| !zip.by_index(i).map(|f| f.name().contains('/')).unwrap_or(true)).count() == originals,
        &mut failures,
    );

    // 3) save the original order again
    save_active_mods(&sii, &original.active_mods, &backups)?;
    let restored_text = sii_bytes_to_text(&fs::read(&sii)?, true)?;
    println!("after saving the original order again:");
    check("list equals the original", read_profile(&sii)?.active_mods == original.active_mods, &mut failures);
    check(
        "text is byte-identical to the decrypted original",
        restored_text == original_text,
        &mut failures,
    );
    Ok(failures)
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: profile_roundtrip <path to profile.sii>");
        return ExitCode::FAILURE;
    };
    match run(Path::new(&path)) {
        Ok(0) => {
            println!("all checks passed; the real profile was not modified");
            ExitCode::SUCCESS
        }
        Ok(n) => {
            println!("{n} check(s) failed; the real profile was not modified");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
