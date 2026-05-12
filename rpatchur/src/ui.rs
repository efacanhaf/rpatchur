use std::fs;
use std::path::PathBuf;

use crate::patcher::{get_patcher_name, PatcherCommand, PatcherConfiguration};
use crate::process::start_executable;
use serde::Deserialize;
use serde_json::Value;
use tinyfiledialogs as tfd;
use web_view::{Content, Handle, WebView};

/// 'Opaque" struct that can be used to update the UI.
#[derive(Clone)]
pub struct UiController {
    web_view_handle: Handle<WebViewUserData>,
}
impl UiController {
    pub fn new(web_view: &WebView<'_, WebViewUserData>) -> UiController {
        UiController {
            web_view_handle: web_view.handle(),
        }
    }

    /// Allows another thread to indicate the current status of the patching process.
    ///
    /// This updates the UI with useful information.
    pub fn dispatch_patching_status(&self, status: PatchingStatus) {
        if let Err(e) = self.web_view_handle.dispatch(move |webview| {
            let result = match status {
                PatchingStatus::Ready => webview.eval("patchingStatusReady()"),
                PatchingStatus::ReadyAfterUpdate => {
                    webview.eval("patchingStatusReadyAfterUpdate()")
                }
                PatchingStatus::Error(msg) => {
                    webview.eval(&format!("patchingStatusError(\"{}\")", msg))
                }
                PatchingStatus::DownloadInProgress(nb_downloaded, nb_total, bytes_per_sec) => {
                    webview.eval(&format!(
                        "patchingStatusDownloading({}, {}, {})",
                        nb_downloaded, nb_total, bytes_per_sec
                    ))
                }
                PatchingStatus::InstallationInProgress(nb_installed, nb_total) => webview.eval(
                    &format!("patchingStatusInstalling({}, {})", nb_installed, nb_total),
                ),
                PatchingStatus::ManualPatchApplied(name) => {
                    webview.eval(&format!("patchingStatusPatchApplied(\"{}\")", name))
                }
                PatchingStatus::PackDownload(id, fi, fc, downloaded, total, bps) => {
                    webview.eval(&format!(
                        "packDownloadProgress(\"{}\", {}, {}, {}, {}, {})",
                        id, fi, fc, downloaded, total, bps
                    ))
                }
                PatchingStatus::PackVerifying(id, file_name) => webview.eval(&format!(
                    "packDownloadVerifying(\"{}\", \"{}\")",
                    id, file_name
                )),
                PatchingStatus::PackComplete(id) => {
                    webview.eval(&format!("packDownloadComplete(\"{}\")", id))
                }
                PatchingStatus::PackCancelled(id) => {
                    webview.eval(&format!("packDownloadCancelled(\"{}\")", id))
                }
                PatchingStatus::PackFailed(id, err) => webview.eval(&format!(
                    "packDownloadFailed(\"{}\", \"{}\")",
                    id,
                    err.replace('"', "\\\"")
                )),
            };
            if let Err(e) = result {
                log::warn!("Failed to dispatch patching status: {}.", e);
            }
            Ok(())
        }) {
            log::warn!("Failed to dispatch patching status: {}.", e);
        }
    }

    pub fn set_patch_in_progress(&self, value: bool) {
        if let Err(e) = self.web_view_handle.dispatch(move |webview| {
            webview.user_data_mut().patching_in_progress = value;
            Ok(())
        }) {
            log::warn!("Failed to dispatch patching status: {}.", e);
        }
    }
}

/// Used to indicate the current status of the patching process.
pub enum PatchingStatus {
    Ready,
    /// Like `Ready`, but signals that DimensionsRO.yml itself was just patched,
    /// so the launcher must relaunch to re-read the new integrity hashes.
    ReadyAfterUpdate,
    Error(String),                         // Error message
    DownloadInProgress(usize, usize, u64), // Downloaded files, Total number, Bytes per second
    InstallationInProgress(usize, usize),  // Installed patches, Total number
    ManualPatchApplied(String),            // Patch file name
    /// Pack id, current file index, total files, downloaded bytes, total bytes, bytes/sec
    PackDownload(String, usize, usize, u64, u64, u64),
    PackVerifying(String, String), // pack id, file name
    PackComplete(String),          // pack id
    PackCancelled(String),         // pack id
    PackFailed(String, String),    // pack id, error message
}

pub struct WebViewUserData {
    patcher_config: PatcherConfiguration,
    patching_thread_tx: flume::Sender<PatcherCommand>,
    patching_in_progress: bool,
}
impl WebViewUserData {
    pub fn new(
        patcher_config: PatcherConfiguration,
        patching_thread_tx: flume::Sender<PatcherCommand>,
    ) -> WebViewUserData {
        WebViewUserData {
            patcher_config,
            patching_thread_tx,
            patching_in_progress: false,
        }
    }
}
impl Drop for WebViewUserData {
    fn drop(&mut self) {
        // Ask the patching thread to stop whenever WebViewUserData is dropped.
        // The Quit command is consumed at the next iteration of the patcher
        // loop, which doesn't help if a long-running download is mid-stream;
        // also raise the global PACK_CANCEL flag so download_one bails on
        // its next chunk read. Without this the launcher process keeps
        // hammering the URL after the window closes.
        let _res = self.patching_thread_tx.try_send(PatcherCommand::Quit);
        crate::patcher::optional::request_pack_cancel();
    }
}

/// Creates a `WebView` object with the appropriate settings for our needs.
pub fn build_webview<'a>(
    title: &'a str,
    user_data: WebViewUserData,
) -> web_view::WVResult<WebView<'a, WebViewUserData>> {
    web_view::builder()
        .title(title)
        .content(Content::Url(user_data.patcher_config.web.index_url.clone()))
        .size(
            user_data.patcher_config.window.width,
            user_data.patcher_config.window.height,
        )
        .resizable(user_data.patcher_config.window.resizable)
        .user_data(user_data)
        .invoke_handler(|webview, arg| {
            match arg {
                "play" => handle_play(webview),
                "setup" => handle_setup(webview),
                "exit" => handle_exit(webview),
                "relaunch" => handle_relaunch(webview),
                "start_update" => handle_start_update(webview),
                "cancel_update" => handle_cancel_update(webview),
                "reset_cache" => handle_reset_cache(webview),
                "manual_patch" => handle_manual_patch(webview),
                "list_optional_packs" => handle_list_optional_packs(webview),
                "cancel_pack_download" => handle_cancel_pack_download(webview),
                "get_active_mode" => handle_get_active_mode(webview),
                "cleanup_hd" => handle_cleanup_hd(webview),
                "verify_play" => handle_verify_play(webview),
                request => handle_json_request(webview, request),
            }
            Ok(())
        })
        .build()
}

/// Opens the configured game client with the configured arguments.
/// Performs a sha256 integrity check on Ragexe.exe and DATA.ini against the
/// active mode's expected hashes (configured under `mode_artifacts` in YAML).
/// On mismatch, aborts the launch and reports the failure to JS via
/// `playPreflightFailed(msg)`. If the hashes match the OTHER mode (i.e. the
/// swap state got inverted), performs an automatic swap and proceeds.
///
/// This function can create elevated processes on Windows with UAC activated.
fn handle_play(webview: &mut WebView<WebViewUserData>) {
    if let Err(e) = preflight_integrity(webview) {
        log::warn!("preflight integrity failed: {}", e);
        let escaped = e.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = webview.eval(&format!("playPreflightFailed(\"{}\")", escaped));
        return;
    }
    let client_arguments = webview.user_data().patcher_config.play.arguments.clone();
    start_game_client(webview, &client_arguments);
}

/// Hash Ragexe.exe + DATA.ini, compare to the active mode's expected hashes.
/// If active artifacts actually match the OTHER mode, perform a corrective
/// swap. Returns Err with a user-facing message on unrecoverable mismatch.
fn preflight_integrity(webview: &mut WebView<WebViewUserData>) -> Result<(), String> {
    let artifacts = webview.user_data().patcher_config.mode_artifacts.clone();
    let mode = detect_active_mode(Some(&artifacts)).to_string();
    if mode == "unknown" {
        return Err("estado dos arquivos inconsistente — reinstale o cliente".into());
    }
    let active_expected = match mode.as_str() {
        "vanilla" => artifacts.vanilla.as_ref(),
        "hd" => artifacts.hd.as_ref(),
        _ => None,
    };
    let other_expected = match mode.as_str() {
        "vanilla" => artifacts.hd.as_ref(),
        "hd" => artifacts.vanilla.as_ref(),
        _ => None,
    };
    let active_expected = match active_expected {
        Some(a) => a,
        None => return Ok(()),
    };

    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {}", e))?;
    let r_hash = sha256_file(&cwd.join("Ragexe.exe"))
        .map_err(|e| format!("hash Ragexe.exe: {}", e))?;
    let d_hash = sha256_file(&cwd.join("DATA.ini"))
        .map_err(|e| format!("hash DATA.ini: {}", e))?;

    if r_hash == active_expected.ragexe_sha256 && d_hash == active_expected.dataini_sha256 {
        return Ok(());
    }
    if let Some(other) = other_expected {
        if r_hash == other.ragexe_sha256 && d_hash == other.dataini_sha256 {
            // Active files match the OTHER mode — swap and try again.
            log::info!("preflight: active files belong to other mode, swapping");
            apply_mode_swap(&mode, Some(&artifacts))?;
            return Ok(());
        }
    }
    Err(format!(
        "Ragexe.exe ou DATA.ini divergem do esperado para o modo {} — reinstale o cliente",
        mode
    ))
}

/// Perform the rename swap to make `target_mode` active. Internal helper.
fn apply_mode_swap(target_mode: &str, cfg: Option<&crate::patcher::ModeArtifacts>) -> Result<(), String> {
    let current = detect_active_mode(cfg);
    if current == target_mode {
        return Ok(());
    }
    if current == "unknown" {
        return Err("estado dos arquivos inconsistente".into());
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {}", e))?;
    let ragexe_active = cwd.join("Ragexe.exe");
    let data_active = cwd.join("DATA.ini");
    let deact_ragexe = cwd.join(format!("Ragexe-{}.dat", current));
    let deact_data = cwd.join(format!("DATA-{}.ini.bak", current));
    let act_ragexe = cwd.join(format!("Ragexe-{}.dat", target_mode));
    let act_data = cwd.join(format!("DATA-{}.ini.bak", target_mode));
    if !act_ragexe.exists() {
        return Err(format!("faltando {}", act_ragexe.file_name().unwrap().to_string_lossy()));
    }
    if !act_data.exists() {
        return Err(format!("faltando {}", act_data.file_name().unwrap().to_string_lossy()));
    }
    if ragexe_active.exists() {
        fs::rename(&ragexe_active, &deact_ragexe).map_err(|e| format!("rename Ragexe.exe: {}", e))?;
    }
    if data_active.exists() {
        if let Err(e) = fs::rename(&data_active, &deact_data) {
            let _ = fs::rename(&deact_ragexe, &ragexe_active);
            return Err(format!("rename DATA.ini: {}", e));
        }
    }
    fs::rename(&act_ragexe, &ragexe_active).map_err(|e| format!("rename act ragexe: {}", e))?;
    fs::rename(&act_data, &data_active).map_err(|e| format!("rename act data: {}", e))?;
    Ok(())
}

/// Opens the configured 'Setup' software with the configured arguments.
///
/// This function can create elevated processes on Windows with UAC activated.
fn handle_setup(webview: &mut WebView<WebViewUserData>) {
    let setup_exe: &String = &webview.user_data().patcher_config.setup.path;
    let setup_arguments = &webview.user_data().patcher_config.setup.arguments;
    let exit_on_success = webview
        .user_data()
        .patcher_config
        .setup
        .exit_on_success
        .unwrap_or(false);
    match start_executable(setup_exe, setup_arguments) {
        Ok(success) => {
            if success {
                log::trace!("Setup software started");
                if exit_on_success {
                    webview.exit();
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to start setup software: {}", e);
        }
    }
}

/// Exits the patcher cleanly.
fn handle_exit(webview: &mut WebView<WebViewUserData>) {
    webview.exit();
}

/// Spawns a fresh copy of the launcher and exits the current one. Used after a
/// patch updates DimensionsRO.yml, so the new instance reads the fresh
/// integrity hashes from disk.
fn handle_relaunch(webview: &mut WebView<WebViewUserData>) {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(&exe).spawn() {
                log::warn!("Failed to spawn relaunch process: {}", e);
                return;
            }
            log::info!("Relaunched {:?}, exiting current instance", exe);
            webview.exit();
        }
        Err(e) => log::warn!("Failed to resolve current_exe for relaunch: {}", e),
    }
}

/// Starts the patching task/thread.
fn handle_start_update(webview: &mut WebView<WebViewUserData>) {
    // Patching is already in progress, abort.
    if webview.user_data().patching_in_progress {
        let res = webview.eval("notificationInProgress()");
        if let Err(e) = res {
            log::warn!("Failed to dispatch notification: {}.", e);
        }
        return;
    }

    let send_res = webview
        .user_data_mut()
        .patching_thread_tx
        .send(PatcherCommand::StartUpdate);
    if send_res.is_ok() {
        log::trace!("Sent StartUpdate command to patching thread");
    }
}

/// Cancels the patching task/thread.
fn handle_cancel_update(webview: &mut WebView<WebViewUserData>) {
    if webview
        .user_data_mut()
        .patching_thread_tx
        .send(PatcherCommand::CancelUpdate)
        .is_ok()
    {
        log::trace!("Sent CancelUpdate command to patching thread");
    }
}

/// Resets the patcher cache (which is used to keep track of already applied
/// patches).
fn handle_reset_cache(_webview: &mut WebView<WebViewUserData>) {
    if let Ok(patcher_name) = get_patcher_name() {
        let cache_file_path = PathBuf::from(patcher_name).with_extension("dat");
        if let Err(e) = fs::remove_file(cache_file_path) {
            log::warn!("Failed to remove the cache file: {}", e);
        }
    }
}

/// Asks the user to provide a patch file to apply
fn handle_manual_patch(webview: &mut WebView<WebViewUserData>) {
    // Patching is already in progress, abort.
    if webview.user_data().patching_in_progress {
        let res = webview.eval("notificationInProgress()");
        if let Err(e) = res {
            log::warn!("Failed to dispatch notification: {}.", e);
        }
        return;
    }

    let opt_path = tfd::open_file_dialog(
        "Select a file",
        "",
        Some((&["*.thor"], "Patch Files (*.thor)")),
    );
    if let Some(path) = opt_path {
        log::info!("Requesting manual patch '{}'", path);
        if webview
            .user_data_mut()
            .patching_thread_tx
            .send(PatcherCommand::ApplyPatch(PathBuf::from(path)))
            .is_ok()
        {
            log::trace!("Sent ApplyPatch command to patching thread");
        }
    }
}

/// Parses JSON requests (for invoking functions with parameters) and dispatches
/// them to the invoked function.
fn handle_json_request(webview: &mut WebView<WebViewUserData>, request: &str) {
    let result: serde_json::Result<Value> = serde_json::from_str(request);
    match result {
        Err(e) => {
            log::error!("Invalid JSON request: {}", e);
        }
        Ok(json_req) => {
            let function_name = json_req["function"].as_str();
            if let Some(function_name) = function_name {
                let function_params = json_req["parameters"].clone();
                match function_name {
                    "login" => handle_login(webview, function_params),
                    "open_url" => handle_open_url(function_params),
                    "download_pack" => handle_download_pack(webview, function_params),
                    "apply_mode" => handle_apply_mode(webview, function_params),
                    _ => {
                        log::error!("Unknown function '{}'", function_name);
                    }
                }
            }
        }
    }
}

/// Parameters expected for the login function
#[derive(Deserialize)]
struct LoginParameters {
    login: String,
    password: String,
}

/// Launches the game client with the given credentials
fn handle_login(webview: &mut WebView<WebViewUserData>, parameters: Value) {
    let result: serde_json::Result<LoginParameters> = serde_json::from_value(parameters);
    match result {
        Err(e) => log::error!("Invalid arguments given for 'login': {}", e),
        Ok(login_params) => {
            // Push credentials to the list of arguments first
            let mut play_arguments: Vec<String> = vec![
                format!("-t:{}", login_params.password),
                login_params.login,
                "server".to_string(),
            ];
            play_arguments.extend(
                webview
                    .user_data()
                    .patcher_config
                    .play
                    .arguments
                    .iter()
                    .cloned(),
            );
            start_game_client(webview, &play_arguments);
        }
    }
}

/// Parameters expected for the open_url function
#[derive(Deserialize)]
struct OpenUrlParameters {
    url: String,
}

/// Opens an URL with the native URL Handler
fn handle_open_url(parameters: Value) {
    let result: serde_json::Result<OpenUrlParameters> = serde_json::from_value(parameters);
    match result {
        Err(e) => log::error!("Invalid arguments given for 'open_url': {}", e),
        Ok(params) => match open::that(params.url) {
            Ok(exit_status) => {
                if !exit_status.success() {
                    if let Some(code) = exit_status.code() {
                        log::error!("Command returned non-zero exit status {}!", code);
                    }
                }
            }
            Err(why) => {
                log::error!("Error open_url function: '{}'", why);
            }
        },
    }
}

/// Handles `list_optional_packs` — returns the configured optional packs as JSON to the UI.
fn handle_list_optional_packs(webview: &mut WebView<WebViewUserData>) {
    let packs = webview.user_data().patcher_config.optional_packs.clone();
    let json = match serde_json::to_string(&packs) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Failed to serialize optional packs: {}", e);
            return;
        }
    };
    let escaped = json.replace('\\', "\\\\").replace('\'', "\\'");
    if let Err(e) = webview.eval(&format!("optionalPacksList('{}')", escaped)) {
        log::warn!("Failed to dispatch optional packs list: {}", e);
    }
}

/// Parameters expected for the `download_pack` JSON function.
#[derive(Deserialize)]
struct DownloadPackParameters {
    id: String,
}

fn handle_download_pack(webview: &mut WebView<WebViewUserData>, parameters: Value) {
    let parsed: serde_json::Result<DownloadPackParameters> = serde_json::from_value(parameters);
    match parsed {
        Err(e) => log::error!("Invalid arguments given for 'download_pack': {}", e),
        Ok(p) => {
            if !webview
                .user_data()
                .patcher_config
                .optional_packs
                .iter()
                .any(|op| op.id == p.id)
            {
                log::warn!("Unknown optional pack id requested: {}", p.id);
                return;
            }
            if webview
                .user_data_mut()
                .patching_thread_tx
                .send(PatcherCommand::DownloadPack(p.id))
                .is_ok()
            {
                log::trace!("Sent DownloadPack command to patching thread");
            }
        }
    }
}

/// Cancel an in-progress optional pack download.
fn handle_cancel_pack_download(_webview: &mut WebView<WebViewUserData>) {
    crate::patcher::optional::request_pack_cancel();
    log::trace!("Pack cancel flag set");
}

/// Parameters for the `apply_mode` JSON request.
#[derive(Deserialize)]
struct ApplyModeParameters {
    mode: String,
}

/// Reports the currently-active mode to JS via `activeModeResult(mode)`.
fn handle_get_active_mode(webview: &mut WebView<WebViewUserData>) {
    let artifacts = webview.user_data().patcher_config.mode_artifacts.clone();
    let mode = detect_active_mode(Some(&artifacts));
    let _ = webview.eval(&format!("activeModeResult(\"{}\")", mode));
}

/// Switches to vanilla mode (if needed) and deletes every file declared in
/// the `hd` optional pack to free disk space. Reports to JS via
/// `cleanupHdResult(success, freedBytes, msg)`.
fn handle_cleanup_hd(webview: &mut WebView<WebViewUserData>) {
    let artifacts = webview.user_data().patcher_config.mode_artifacts.clone();
    // Make sure we're not running HD when wiping HD files.
    if detect_active_mode(Some(&artifacts)) == "hd" {
        if let Err(e) = apply_mode_swap("vanilla", Some(&artifacts)) {
            let escaped = e.replace('\\', "\\\\").replace('"', "\\\"");
            let _ = webview.eval(&format!("cleanupHdResult(false, 0, \"{}\")", escaped));
            return;
        }
    }
    let pack = webview
        .user_data()
        .patcher_config
        .optional_packs
        .iter()
        .find(|p| p.id == "hd")
        .cloned();
    let pack = match pack {
        Some(p) => p,
        None => {
            let _ = webview.eval(
                "cleanupHdResult(false, 0, \"pack hd nao configurado em optional_packs\")",
            );
            return;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            let _ = webview.eval(&format!(
                "cleanupHdResult(false, 0, \"cwd: {}\")",
                e
            ));
            return;
        }
    };
    let mut freed: u64 = 0;
    let mut errors: Vec<String> = Vec::new();
    for f in pack.files.iter() {
        let p = cwd.join(&f.name);
        if !p.exists() {
            continue;
        }
        let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if let Err(e) = std::fs::remove_file(&p) {
            errors.push(format!("{}: {}", f.name, e));
        } else {
            freed += sz;
        }
    }
    if errors.is_empty() {
        let _ = webview.eval(&format!("cleanupHdResult(true, {}, \"\")", freed));
    } else {
        let msg = errors.join("; ").replace('\\', "\\\\").replace('"', "\\\"");
        let _ = webview.eval(&format!("cleanupHdResult(false, {}, \"{}\")", freed, msg));
    }
}

/// Hash a file with sha256, returning hex string.
fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    // Heap-allocated 1 MiB buffer; an 8 MiB stack array crashes the UI thread
    // (which has a 1 MB default stack on Windows).
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verifies the binary + DATA.ini integrity for the active mode and reports
/// the result to JS via `verifyPlayResult(mode, status, msg)`.
///
/// status values:
///   "ok"             -> launch the game
///   "needs_swap"     -> standby files matched the active mode's expected hash;
///                       UI should call apply_mode to fix automatically
///   "needs_repair_hd"-> we're in HD but Ragexe/DATA mismatched and standby
///                       can't fix; UI should redownload via download_pack
///   "corrupt"        -> nothing matches; user must reinstall
fn handle_verify_play(webview: &mut WebView<WebViewUserData>) {
    let artifacts = webview.user_data().patcher_config.mode_artifacts.clone();
    let mode = detect_active_mode(Some(&artifacts)).to_string();
    if mode == "unknown" {
        let _ = webview.eval(&format!(
            "verifyPlayResult(\"unknown\", \"corrupt\", \"estado de arquivos inconsistente\")"
        ));
        return;
    }

    let expected_active = match mode.as_str() {
        "vanilla" => artifacts.vanilla.as_ref(),
        "hd" => artifacts.hd.as_ref(),
        _ => None,
    };
    let expected_other = match mode.as_str() {
        "vanilla" => artifacts.hd.as_ref(),
        "hd" => artifacts.vanilla.as_ref(),
        _ => None,
    };
    let expected_active = match expected_active {
        Some(a) => a,
        None => {
            // No expected hashes configured — skip verification, accept active state.
            let _ = webview.eval(&format!(
                "verifyPlayResult(\"{}\", \"ok\", \"\")",
                mode
            ));
            return;
        }
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let ragexe_active = cwd.join("Ragexe.exe");
    let data_active = cwd.join("DATA.ini");

    let r_hash = match sha256_file(&ragexe_active) {
        Ok(h) => h,
        Err(e) => {
            let _ = webview.eval(&format!(
                "verifyPlayResult(\"{}\", \"corrupt\", \"hash Ragexe.exe falhou: {}\")",
                mode, e
            ));
            return;
        }
    };
    let d_hash = match sha256_file(&data_active) {
        Ok(h) => h,
        Err(e) => {
            let _ = webview.eval(&format!(
                "verifyPlayResult(\"{}\", \"corrupt\", \"hash DATA.ini falhou: {}\")",
                mode, e
            ));
            return;
        }
    };

    if r_hash == expected_active.ragexe_sha256 && d_hash == expected_active.dataini_sha256 {
        let _ = webview.eval(&format!("verifyPlayResult(\"{}\", \"ok\", \"\")", mode));
        return;
    }

    // Active files don't match expected active hashes.
    // Check if they match the OTHER mode (i.e. the user has the swap inverted).
    if let Some(other) = expected_other {
        if r_hash == other.ragexe_sha256 && d_hash == other.dataini_sha256 {
            // Active files actually correspond to the OTHER mode → swap fixes it.
            let _ = webview.eval(&format!(
                "verifyPlayResult(\"{}\", \"needs_swap\", \"arquivos ativos sao do modo {}\")",
                mode,
                if mode == "vanilla" { "hd" } else { "vanilla" }
            ));
            return;
        }
    }

    // No match at all — corruption.
    let msg = if mode == "hd" {
        "Ragexe.exe ou DATA.ini divergem do esperado. Reinstale o cliente."
    } else {
        "Ragexe.exe ou DATA.ini divergem do esperado. Reinstale o cliente."
    };
    let _ = webview.eval(&format!(
        "verifyPlayResult(\"{}\", \"corrupt\", \"{}\")",
        mode, msg
    ));
}

/// Reply to JS with `applyModeResult(mode, success, error)`.
fn dispatch_apply_mode_result(webview: &mut WebView<WebViewUserData>, mode: &str, ok: bool, err: &str) {
    let escaped_err = err.replace('\\', "\\\\").replace('"', "\\\"");
    let _ = webview.eval(&format!(
        "applyModeResult(\"{}\", {}, \"{}\")",
        mode, ok, escaped_err
    ));
}

/// Detect which mode is currently active. When `cfg` is provided, hashes
/// Ragexe.exe and matches it against the configured mode_artifacts —
/// robust to redundant or missing standby `.dat` files. Falls back to
/// the legacy existence-based check when `cfg` is `None` or the hash
/// matches no configured mode.
///
/// Returns "hd", "vanilla", or "unknown".
fn detect_active_mode(cfg: Option<&crate::patcher::ModeArtifacts>) -> &'static str {
    let cwd = std::env::current_dir().unwrap_or_default();

    // Primary: hash-based detection.
    if let Some(artifacts) = cfg {
        if let Ok(h) = sha256_file(&cwd.join("Ragexe.exe")) {
            if let Some(v) = &artifacts.vanilla {
                if h == v.ragexe_sha256 {
                    return "vanilla";
                }
            }
            if let Some(hd) = &artifacts.hd {
                if h == hd.ragexe_sha256 {
                    return "hd";
                }
            }
        }
    }

    // Fallback: existence-based (legacy / no config).
    let v_dat = cwd.join("Ragexe-vanilla.dat");
    let h_dat = cwd.join("Ragexe-hd.dat");
    match (v_dat.exists(), h_dat.exists()) {
        (true, false) => "hd",
        (false, true) => "vanilla",
        _ => "unknown",
    }
}

/// Swap Ragexe.exe + DATA.ini between vanilla/HD by renaming standby files.
fn handle_apply_mode(webview: &mut WebView<WebViewUserData>, parameters: Value) {
    let parsed: serde_json::Result<ApplyModeParameters> = serde_json::from_value(parameters);
    let target = match parsed {
        Err(e) => {
            log::error!("Invalid arguments for 'apply_mode': {}", e);
            return;
        }
        Ok(p) => p.mode,
    };
    if target != "vanilla" && target != "hd" {
        dispatch_apply_mode_result(webview, &target, false, "modo invalido");
        return;
    }

    let artifacts = webview.user_data().patcher_config.mode_artifacts.clone();
    let current = detect_active_mode(Some(&artifacts));
    if current == target {
        dispatch_apply_mode_result(webview, &target, true, "");
        return;
    }
    if current == "unknown" {
        dispatch_apply_mode_result(
            webview,
            &target,
            false,
            "estado dos arquivos inconsistente — reinstale o cliente",
        );
        return;
    }

    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            dispatch_apply_mode_result(webview, &target, false, &format!("cwd: {}", e));
            return;
        }
    };
    let ragexe_active = cwd.join("Ragexe.exe");
    let data_active = cwd.join("DATA.ini");
    let deact_ragexe = cwd.join(format!("Ragexe-{}.dat", current));
    let deact_data = cwd.join(format!("DATA-{}.ini.bak", current));
    let act_ragexe = cwd.join(format!("Ragexe-{}.dat", target));
    let act_data = cwd.join(format!("DATA-{}.ini.bak", target));

    if !act_ragexe.exists() {
        dispatch_apply_mode_result(
            webview,
            &target,
            false,
            &format!("faltando {}", act_ragexe.file_name().unwrap().to_string_lossy()),
        );
        return;
    }
    if !act_data.exists() {
        dispatch_apply_mode_result(
            webview,
            &target,
            false,
            &format!("faltando {}", act_data.file_name().unwrap().to_string_lossy()),
        );
        return;
    }

    // Validate sha256 of the standby artifacts BEFORE renaming them to active.
    // If hashes are configured in mode_artifacts and don't match, abort the
    // swap so we never expose a tampered Ragexe.exe / DATA.ini.
    let artifacts = webview.user_data().patcher_config.mode_artifacts.clone();
    let target_expected = match target.as_str() {
        "vanilla" => artifacts.vanilla.as_ref(),
        "hd" => artifacts.hd.as_ref(),
        _ => None,
    };
    if let Some(exp) = target_expected {
        match sha256_file(&act_ragexe) {
            Ok(h) if h != exp.ragexe_sha256 => {
                dispatch_apply_mode_result(webview, &target, false,
                    &format!("Ragexe-{}.dat sha256 invalido — reinstale o cliente", target));
                return;
            }
            Err(e) => {
                dispatch_apply_mode_result(webview, &target, false,
                    &format!("hash Ragexe-{}.dat: {}", target, e));
                return;
            }
            _ => {}
        }
        match sha256_file(&act_data) {
            Ok(h) if h != exp.dataini_sha256 => {
                dispatch_apply_mode_result(webview, &target, false,
                    &format!("DATA-{}.ini.bak sha256 invalido — reinstale o cliente", target));
                return;
            }
            Err(e) => {
                dispatch_apply_mode_result(webview, &target, false,
                    &format!("hash DATA-{}.ini.bak: {}", target, e));
                return;
            }
            _ => {}
        }
    }

    // Active → standby
    if ragexe_active.exists() {
        if let Err(e) = fs::rename(&ragexe_active, &deact_ragexe) {
            dispatch_apply_mode_result(webview, &target, false, &format!("rename Ragexe.exe: {}", e));
            return;
        }
    }
    if data_active.exists() {
        if let Err(e) = fs::rename(&data_active, &deact_data) {
            // Try to roll back the ragexe rename
            let _ = fs::rename(&deact_ragexe, &ragexe_active);
            dispatch_apply_mode_result(webview, &target, false, &format!("rename DATA.ini: {}", e));
            return;
        }
    }
    // Standby → active
    if let Err(e) = fs::rename(&act_ragexe, &ragexe_active) {
        dispatch_apply_mode_result(webview, &target, false, &format!("rename {}->Ragexe.exe: {}", act_ragexe.display(), e));
        return;
    }
    if let Err(e) = fs::rename(&act_data, &data_active) {
        dispatch_apply_mode_result(webview, &target, false, &format!("rename {}->DATA.ini: {}", act_data.display(), e));
        return;
    }

    log::info!("apply_mode: switched to {}", target);
    dispatch_apply_mode_result(webview, &target, true, "");
}

fn start_game_client(webview: &mut WebView<WebViewUserData>, client_arguments: &[String]) {
    let client_exe: &String = &webview.user_data().patcher_config.play.path;
    let exit_on_success = webview
        .user_data()
        .patcher_config
        .play
        .exit_on_success
        .unwrap_or(true);
    match start_executable(client_exe, client_arguments) {
        Ok(success) => {
            if success {
                log::trace!("Client started");
                if exit_on_success {
                    webview.exit();
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to start client: {}", e);
        }
    }
}
