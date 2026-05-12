// dro_update.exe — DimensionsRO launcher self-update helper.
//
// Spawned by the running launcher via `handle_setup` (ShellExecuteEx "runas").
// Behaviour:
//   * If `DimensionsRO.exe.new` exists next to us, treat this as an update:
//     wait for the running launcher to release the file lock on
//     `DimensionsRO.exe`, rename `.new` over the live binary, then spawn the
//     fresh launcher.
//   * Otherwise, behave as a Setup.exe passthrough so the launcher's gear
//     button keeps opening the real Setup utility while `setup.path` points
//     at us (during the THOR-update window).
//
// This binary intentionally has no external dependencies — keeps it small
// and avoids dragging a second std runtime into the GRF tree.

#![windows_subsystem = "windows"]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const LAUNCHER_NAME: &str = "DimensionsRO.exe";
const STAGED_NAME: &str = "DimensionsRO.exe.new";
const SETUP_NAME: &str = "Setup.exe";

/// How long we'll wait for the launcher process to release its image lock
/// before giving up. ShellExecuteEx returns before our parent calls
/// webview.exit(), so a few seconds is normally enough.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn main() {
    // Resolve the install directory. ShellExecuteEx sets CWD to the
    // launcher's current_dir, but be defensive and also try our own
    // executable's directory.
    let dir = install_dir();
    let staged = dir.join(STAGED_NAME);
    let live = dir.join(LAUNCHER_NAME);

    if staged.exists() {
        run_update(&dir, &staged, &live);
    } else {
        run_passthrough(&dir);
    }
}

fn install_dir() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Apply the staged update. Best-effort: if the swap or relaunch fails we
/// leave `.new` in place so a subsequent run can retry.
fn run_update(dir: &Path, staged: &Path, live: &Path) {
    if !wait_for_unlock(live) {
        // Couldn't get exclusive access — bail without touching anything.
        // Next invocation (or a manual click on the gear button) will retry.
        return;
    }

    // Windows fs::rename refuses to overwrite an existing target, so we
    // remove the live binary first. NotFound is fine — means a prior run
    // already got this far.
    match fs::remove_file(live) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("remove_file {} failed: {}", live.display(), e);
            return;
        }
    }

    if let Err(e) = fs::rename(staged, live) {
        eprintln!("rename {} -> {} failed: {}", staged.display(), live.display(), e);
        return;
    }

    // Relaunch the new binary. We don't pass any arguments — the new
    // launcher will read DimensionsRO.yml fresh and figure out its own state.
    let _ = Command::new(live).current_dir(dir).spawn();
}

/// Behave like the gear button used to: just open Setup.exe.
fn run_passthrough(dir: &Path) {
    let setup = dir.join(SETUP_NAME);
    if setup.exists() {
        let _ = Command::new(&setup).current_dir(dir).spawn();
    }
    // If Setup.exe is missing too, there's nothing useful to do. Silent exit.
}

/// Poll until we can open `path` for write, indicating the previous holder
/// (the running launcher) has released its image lock. Returns true on
/// success, false on timeout.
fn wait_for_unlock(path: &Path) -> bool {
    let deadline = Instant::now() + LOCK_WAIT_TIMEOUT;
    loop {
        if try_acquire_write(path) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(LOCK_POLL_INTERVAL);
    }
}

/// Attempt to open `path` with write access. We immediately drop the handle —
/// the goal is just to verify that no other process holds an exclusive
/// reference (i.e. the OS no longer treats the file as a running image).
fn try_acquire_write(path: &Path) -> bool {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .is_ok()
}
