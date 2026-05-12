#![windows_subsystem = "windows"]

mod patcher;
mod process;
mod ui;

use log::LevelFilter;
use std::env;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use simple_logger::SimpleLogger;
use structopt::StructOpt;
use tinyfiledialogs as tfd;
use tokio::runtime;

use patcher::{
    patcher_thread_routine, retrieve_patcher_configuration, PatcherCommand, PatcherConfiguration,
};
use ui::{UiController, WebViewUserData};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
const PKG_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

#[derive(Debug, StructOpt)]
#[structopt(name = PKG_NAME, version = PKG_VERSION, author = PKG_AUTHORS, about = PKG_DESCRIPTION)]
struct Opt {
    /// Sets a custom working directory
    #[structopt(short, long, parse(from_os_str))]
    working_directory: Option<PathBuf>,
}

fn main() -> Result<()> {
    SimpleLogger::new()
        .with_level(LevelFilter::Off)
        .with_module_level(PKG_NAME, LevelFilter::Info)
        .init()
        .with_context(|| "Failed to initalize the logger")?;

    // Parse CLI arguments
    let cli_args = Opt::from_args();
    if let Some(working_directory) = cli_args.working_directory {
        env::set_current_dir(working_directory)
            .with_context(|| "Specified working directory is invalid or inaccessible")?;
    };

    // Clean up the leftover `<exe>.old` that the previous launcher instance
    // moved aside when applying a self-update. We can only do this now —
    // before that prior process exits, Windows still holds the image lock
    // on it. Best-effort: a failure here just means the stale file lingers
    // until the next launch.
    clean_stale_self_update_artifacts();

    let config = match retrieve_patcher_configuration(None) {
        Err(e) => {
            let err_msg = "Failed to retrieve the patcher's configuration";
            tfd::message_box_ok(
                "Error",
                format!("Error: {}: {:#}.", err_msg, e).as_str(),
                tfd::MessageBoxIcon::Error,
            );
            return Err(e);
        }
        Ok(v) => v,
    };

    // Create a channel to allow the webview's thread to communicate with the patching thread
    let (tx, rx) = flume::bounded(32);
    let window_title = config.window.title.clone();
    let webview = ui::build_webview(
        window_title.as_str(),
        WebViewUserData::new(config.clone(), tx),
    )
    .with_context(|| "Failed to build a web view")?;

    // Strip WS_MAXIMIZEBOX so the maximize button disappears (resizable=false
    // greys it but doesn't hide it on Windows).
    #[cfg(windows)]
    {
        let title_clone = window_title.clone();
        std::thread::spawn(move || disable_maximize_button(&title_clone));
    }

    // Spawn a patching thread
    let patching_thread = new_patching_thread(rx, UiController::new(&webview), config);
    webview
        .run()
        .with_context(|| "Failed to run the web view")?;
    // Join the patching thread
    patching_thread
        .join()
        .map_err(|_| anyhow!("Failed to join patching thread"))?
        .with_context(|| "Patching thread ran into an error")?;

    Ok(())
}

/// Delete the `<exe>.old` companion that's left behind after a self-update.
/// `apply_patch_to_disk::stage_self_for_replace` renames the running launcher
/// out of the way before extracting the new binary; once we reach this point
/// in the *new* process, the previous instance has already exited and the
/// `.old` file is just dead weight.
fn clean_stale_self_update_artifacts() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut old_path = exe.clone();
    let ext = match old_path.extension() {
        Some(e) => format!("{}.old", e.to_string_lossy()),
        None => "old".to_string(),
    };
    old_path.set_extension(ext);
    let _ = std::fs::remove_file(&old_path);
}

/// Polls for the launcher window by title and removes WS_MAXIMIZEBOX so the
/// maximize button is hidden (and the window is no longer fullscreen-able).
#[cfg(windows)]
fn disable_maximize_button(title: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::thread::sleep;
    use std::time::Duration;
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        FindWindowW, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_MAXIMIZEBOX,
    };

    let title_w: Vec<u16> = OsStr::new(title)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Poll up to ~5 seconds; the window is created on the UI thread which
    // hasn't started its event loop yet at this point.
    let mut hwnd: HWND = std::ptr::null_mut();
    for _ in 0..50 {
        unsafe {
            hwnd = FindWindowW(std::ptr::null(), title_w.as_ptr());
            if !hwnd.is_null() {
                break;
            }
        }
        sleep(Duration::from_millis(100));
    }
    if hwnd.is_null() {
        return;
    }

    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        // LONG_PTR is i32 on i686 (the target we ship) and i64 on x64. We
        // only build for i686 in CI, so settle on i32 explicitly to keep
        // bitand operand types in sync.
        #[cfg(target_pointer_width = "32")]
        let new_style: i32 = style & !(WS_MAXIMIZEBOX as i32);
        #[cfg(target_pointer_width = "64")]
        let new_style: isize = style & !(WS_MAXIMIZEBOX as isize);
        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

/// Spawns a new thread that runs a single threaded tokio runtime to execute the patcher routine
fn new_patching_thread(
    rx: flume::Receiver<PatcherCommand>,
    ui_ctrl: UiController,
    config: PatcherConfiguration,
) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || {
        // Build a tokio runtime that runs a scheduler on the current thread and a reactor
        let tokio_rt = runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .with_context(|| "Failed to build a tokio runtime")?;
        // Block on the patching task from our synchronous function
        tokio_rt.block_on(patcher_thread_routine(ui_ctrl, config, rx));

        Ok(())
    })
}
