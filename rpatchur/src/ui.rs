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
        // Ask the patching thread to stop whenever WebViewUserData is dropped
        let _res = self.patching_thread_tx.try_send(PatcherCommand::Quit);
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
                "start_update" => handle_start_update(webview),
                "cancel_update" => handle_cancel_update(webview),
                "reset_cache" => handle_reset_cache(webview),
                "manual_patch" => handle_manual_patch(webview),
                "list_optional_packs" => handle_list_optional_packs(webview),
                "cancel_pack_download" => handle_cancel_pack_download(webview),
                request => handle_json_request(webview, request),
            }
            Ok(())
        })
        .build()
}

/// Opens the configured game client with the configured arguments.
///
/// This function can create elevated processes on Windows with UAC activated.
fn handle_play(webview: &mut WebView<WebViewUserData>) {
    let client_arguments = webview.user_data().patcher_config.play.arguments.clone();
    start_game_client(webview, &client_arguments);
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

/// Handles `download_pack` — sends a `DownloadPack` command to the patching thread.
fn handle_download_pack(webview: &mut WebView<WebViewUserData>, parameters: Value) {
    let parsed: serde_json::Result<DownloadPackParameters> = serde_json::from_value(parameters);
    match parsed {
        Err(e) => log::error!("Invalid arguments given for 'download_pack': {}", e),
        Ok(p) => {
            // Refuse if any patching activity is already running.
            if webview.user_data().patching_in_progress {
                let _ = webview.eval("notificationInProgress()");
                return;
            }
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
