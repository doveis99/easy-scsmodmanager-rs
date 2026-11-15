//! Commands of the self-update: look for a newer release, download it,
//! swap the program file and start again. The work itself is in
//! `scsmm_core::update`; this layer keeps the release that was found and
//! reports progress to the UI.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use scsmm_core::update::{self, Release};

use super::{coded, err, lock, open_url, AppState, BusyGuard, CmdResult};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Files of the portable folder that say which version it is. They follow
/// the program file when they are there.
const COMPANIONS: [&str; 2] = ["README.txt", "LICENSE.txt"];
const MAX_COMPANION_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
pub struct UpdateState {
    /// The release the last check found. Installing takes its download
    /// from here, never from the UI.
    latest: Mutex<Option<Release>>,
    busy: AtomicBool,
    cancel: AtomicBool,
    /// The program file was replaced; only a restart is left to do.
    installed: AtomicBool,
}

#[derive(Serialize)]
pub struct UpdateInfo {
    current: &'static str,
    latest: String,
    newer: bool,
    /// The app can replace itself with this release. Otherwise the release
    /// page is all it can offer.
    installable: bool,
    /// Bytes to download.
    size: u64,
}

/// The portable zip holds the Windows program. A development build only
/// reports what is out: replacing it would put a release build into the
/// build folder.
fn installable(release: &Release) -> bool {
    cfg!(windows) && !cfg!(debug_assertions) && release.package.is_some()
}

/// Remove the program file a finished update moved aside. Right after the
/// restart the previous process may still hold it, so try for a while.
pub fn finish_previous_update() {
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    if update::remove_leftovers(&program) {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            if update::remove_leftovers(&program) {
                break;
            }
        }
    });
}

/// Ask GitHub for the latest release. The check at start (`manual` unset)
/// does nothing when it is switched off in the settings.
#[tauri::command(async)]
pub fn check_update(manual: bool, state: State<'_, AppState>) -> CmdResult<Option<UpdateInfo>> {
    if !manual && !lock(&state)?.settings().update_check {
        return Ok(None);
    }
    let release = update::fetch_latest().map_err(|e| coded("update_check_failed", &[&e.to_string()]))?;
    let info = UpdateInfo {
        current: CURRENT_VERSION,
        newer: update::is_newer(&release.version, CURRENT_VERSION),
        installable: installable(&release),
        size: release.package.as_ref().map_or(0, |p| p.size),
        latest: release.version.clone(),
    };
    *state.update.latest.lock().map_err(err)? = Some(release);
    Ok(Some(info))
}

#[derive(Serialize, Clone)]
pub struct DownloadProgress {
    done: u64,
    total: u64,
}

/// Write the zip's copy over a companion file that is in the folder.
/// Nothing depends on these, so a failure is not one of the update.
fn refresh_companions(folder: &Path, package: &[u8]) {
    for name in COMPANIONS {
        let target = folder.join(name);
        if !target.is_file() {
            continue;
        }
        if let Ok(Some(bytes)) = update::package_file(package, name, MAX_COMPANION_BYTES) {
            let _ = fs::write(target, bytes);
        }
    }
}

/// Download the release the last check found, check it and put its
/// program file in place of the running one. The running app goes on as
/// it is until `restart_app`. Returns the installed version.
#[tauri::command(async)]
pub fn install_update(on_progress: Channel<DownloadProgress>, state: State<'_, AppState>) -> CmdResult<String> {
    let updates = &state.update;
    let release = updates
        .latest
        .lock()
        .map_err(err)?
        .clone()
        .filter(|r| update::is_newer(&r.version, CURRENT_VERSION))
        .ok_or_else(|| coded("update_none", &[]))?;
    let package = release
        .package
        .as_ref()
        .filter(|_| installable(&release))
        .ok_or_else(|| coded("update_no_package", &[]))?;

    if updates.busy.swap(true, Ordering::SeqCst) {
        return Err(coded("update_busy", &[]));
    }
    let _busy = BusyGuard(&updates.busy);
    if updates.installed.load(Ordering::SeqCst) {
        return Ok(release.version);
    }
    updates.cancel.store(false, Ordering::SeqCst);
    let cancelled = || updates.cancel.load(Ordering::SeqCst);

    // the screen needs a steady trickle, not one message per chunk
    let total = package.size;
    let mut last_sent = Instant::now() - Duration::from_secs(1);
    let bytes = update::download_package(
        package,
        &mut |done| {
            if done == total || last_sent.elapsed() >= Duration::from_millis(40) {
                last_sent = Instant::now();
                let _ = on_progress.send(DownloadProgress { done, total });
            }
        },
        &cancelled,
    )
    .map_err(|e| coded("update_download", &[&e.to_string()]))?
    .ok_or_else(|| coded("update_cancelled", &[]))?;

    let program = update::program_from_package(&bytes).map_err(|e| coded("update_bad_package", &[&e.to_string()]))?;
    // the last moment to stop: after the swap the new version is in place
    if cancelled() {
        return Err(coded("update_cancelled", &[]));
    }
    let running = std::env::current_exe().map_err(err)?;
    update::replace_program(&running, &program).map_err(|e| coded("update_write", &[&e.to_string()]))?;
    updates.installed.store(true, Ordering::SeqCst);
    if let Some(folder) = running.parent() {
        refresh_companions(folder, &bytes);
    }
    Ok(release.version)
}

/// Ask the running download to stop.
#[tauri::command]
pub fn cancel_update(state: State<'_, AppState>) {
    state.update.cancel.store(true, Ordering::SeqCst);
}

/// Start the app again, as the version that was just installed.
#[tauri::command]
pub fn restart_app(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    if !state.update.installed.load(Ordering::SeqCst) {
        return Err(coded("update_none", &[]));
    }
    app.request_restart();
    Ok(())
}

/// Open the page of the release the last check found - its notes and its
/// download - in the default browser; the list of releases without one.
#[tauri::command]
pub fn open_release_page(state: State<'_, AppState>) -> CmdResult<()> {
    let url = match state.update.latest.lock().map_err(err)?.as_ref() {
        Some(release) => release.page_url(),
        None => update::RELEASES_PAGE.to_string(),
    };
    open_url(&url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn companions_follow_the_program_only_where_they_are() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [("README.txt", "new readme"), ("LICENSE.txt", "new licence"), ("other.txt", "x")] {
            writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        let package = writer.finish().unwrap().into_inner();

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("README.txt"), "old readme").unwrap();
        refresh_companions(tmp.path(), &package);
        assert_eq!(fs::read_to_string(tmp.path().join("README.txt")).unwrap(), "new readme");
        // a folder that never had the licence does not get one, and
        // nothing else leaves the zip
        assert!(!tmp.path().join("LICENSE.txt").exists());
        assert!(!tmp.path().join("other.txt").exists());

        // a broken zip changes nothing
        refresh_companions(tmp.path(), b"not a zip");
        assert_eq!(fs::read_to_string(tmp.path().join("README.txt")).unwrap(), "new readme");
    }
}
