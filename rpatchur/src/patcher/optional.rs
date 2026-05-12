//! Optional content pack downloads.
//!
//! Drives downloads of large GRF files (HD textures, mood, skybox, kRO data) that the user
//! opts into via the launcher UI. Each pack lists `OptionalPackFile` entries with a URL,
//! expected size, and SHA-256 digest. Files are streamed to a `.part` file alongside the
//! game install, verified, then atomically renamed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::cancellation::InterruptibleFnError;
use super::config::{OptionalPack, OptionalPackFile};

/// Global cancel flag for optional-pack downloads. Set by `handle_cancel_pack_download`
/// in `ui.rs`, reset and read by `download_pack`/`download_one` here. A static avoids
/// the previous flume-listener race where a queued `DownloadPack` command could be
/// silently consumed by the cancellation listener.
pub static PACK_CANCEL: AtomicBool = AtomicBool::new(false);

pub fn request_pack_cancel() {
    PACK_CANCEL.store(true, Ordering::Relaxed);
}
pub fn reset_pack_cancel() {
    PACK_CANCEL.store(false, Ordering::Relaxed);
}

/// Status of an optional pack download — pushed to the JS UI.
#[derive(Clone)]
pub enum PackProgress {
    Started { id: String, total_bytes: u64 },
    File {
        id: String,
        file_index: usize,
        file_count: usize,
        file_name: String,
        downloaded: u64,
        total: u64,
        bytes_per_sec: u64,
    },
    Verifying { id: String, file_name: String },
    Complete { id: String },
    Cancelled { id: String },
    Failed { id: String, error: String },
}

/// Resolve where a pack file should land — alongside the launcher executable.
fn install_path(file_name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    let dir = exe.parent().context("exe has no parent dir")?;
    Ok(dir.join(file_name))
}

/// Returns true if `path` already matches `expected_size` and `expected_sha256`.
/// Aborts (returns false) if PACK_CANCEL is set during verification.
async fn already_installed(path: &Path, expected_size: u64, expected_sha256: &str) -> bool {
    let meta = match fs::metadata(path).await {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.len() != expected_size {
        return false;
    }
    match sha256_of_file(path).await {
        Ok(h) => h.eq_ignore_ascii_case(expected_sha256),
        Err(_) => false,
    }
}

async fn sha256_of_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        if PACK_CANCEL.load(Ordering::Relaxed) {
            anyhow::bail!("verification cancelled");
        }
        let n = tokio::io::AsyncReadExt::read(&mut f, &mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Download a single file with streaming + sha256, honoring the global PACK_CANCEL flag.
///
/// Supports resume: if a `.part` file already exists, its size is treated as
/// the start offset and a `Range: bytes=<N>-` header is sent. The server's
/// response (200 = full restart, 206 = partial) decides whether we append or
/// truncate. The final integrity check re-hashes the assembled `.part` once
/// the byte stream completes, since we cannot keep an incremental Sha256
/// state across launcher restarts.
async fn download_one<F>(
    file: &OptionalPackFile,
    dest: &Path,
    on_progress: &mut F,
) -> std::result::Result<(), InterruptibleFnError>
where
    F: FnMut(u64, u64, u64),
{
    let part_path = dest.with_extension("part");

    // Existing partial download? Range-request to resume.
    let resume_from: u64 = match fs::metadata(&part_path).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| InterruptibleFnError::Err(format!("client build: {}", e)))?;
    let mut req = client.get(&file.url);
    if resume_from > 0 && resume_from < file.size {
        req = req.header(reqwest::header::RANGE, format!("bytes={}-", resume_from));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("GET {}: {}", file.url, e)))?
        .error_for_status()
        .map_err(|e| InterruptibleFnError::Err(format!("HTTP error: {}", e)))?;

    // Did the server honor the range request?
    let status = resp.status().as_u16();
    let resuming = resume_from > 0 && resume_from < file.size && status == 206;
    if resume_from > 0 && !resuming {
        log::info!("server returned {} not 206; restarting download of {} from byte 0", status, file.name);
        let _ = fs::remove_file(&part_path).await;
    }

    let body_len = resp.content_length();
    let total = if resuming {
        body_len.map(|c| resume_from + c).unwrap_or(file.size)
    } else {
        body_len.unwrap_or(file.size)
    };

    let mut stream = resp.bytes_stream();
    let mut out = if resuming {
        fs::OpenOptions::new()
            .append(true)
            .open(&part_path)
            .await
            .map_err(|e| InterruptibleFnError::Err(format!("append part: {}", e)))?
    } else {
        fs::File::create(&part_path)
            .await
            .map_err(|e| InterruptibleFnError::Err(format!("create part: {}", e)))?
    };

    let mut downloaded: u64 = if resuming { resume_from } else { 0 };
    let started = Instant::now();
    let started_bytes = downloaded;
    let mut last_emit = Instant::now();

    // Emit an initial progress event so the UI immediately reflects the
    // resume offset (otherwise the bar starts at 0 even when 80% is on disk).
    on_progress(downloaded, total, 0);

    while let Some(chunk) = stream.next().await {
        if PACK_CANCEL.load(Ordering::Relaxed) {
            // Don't delete .part on cancel — leave it for the next resume.
            return Err(InterruptibleFnError::Interrupted);
        }
        let chunk = chunk.map_err(|e| InterruptibleFnError::Err(format!("recv: {}", e)))?;
        out.write_all(&chunk)
            .await
            .map_err(|e| InterruptibleFnError::Err(format!("write: {}", e)))?;
        downloaded += chunk.len() as u64;

        if last_emit.elapsed().as_millis() > 250 {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let bps = (((downloaded - started_bytes) as f64) / secs) as u64;
            on_progress(downloaded, total, bps);
            last_emit = Instant::now();
        }
    }
    out.flush()
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("flush: {}", e)))?;
    drop(out);

    // Hash the assembled .part. The caller already emits a "Verifying"
    // status before download_one; this call is just the final integrity
    // check before we promote .part to its final name.
    let got = sha256_of_file(&part_path)
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("sha256 verify: {}", e)))?;
    if !got.eq_ignore_ascii_case(&file.sha256) {
        // Hash mismatch is a hard failure — delete .part so the next
        // attempt starts clean rather than appending more bad bytes.
        let _ = fs::remove_file(&part_path).await;
        return Err(InterruptibleFnError::Err(format!(
            "checksum mismatch for {}: expected {}, got {}",
            file.name, file.sha256, got
        )));
    }
    fs::rename(&part_path, dest)
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("rename {} -> {}: {}", part_path.display(), dest.display(), e)))?;
    Ok(())
}

/// Download an entire pack (sequential file downloads). `progress_cb` receives detailed
/// progress events; `cancelled` is checked between (and during) chunk reads.
pub async fn download_pack<F>(
    pack: OptionalPack,
    mut progress_cb: F,
) -> Result<()>
where
    F: FnMut(PackProgress) + Send + 'static,
{
    reset_pack_cancel();
    let total_bytes: u64 = pack.files.iter().map(|f| f.size).sum();
    progress_cb(PackProgress::Started {
        id: pack.id.clone(),
        total_bytes,
    });

    let file_count = pack.files.len();
    for (i, file) in pack.files.iter().enumerate() {
        if PACK_CANCEL.load(Ordering::Relaxed) {
            progress_cb(PackProgress::Cancelled { id: pack.id });
            return Err(anyhow!("pack download cancelled"));
        }
        let dest = install_path(&file.name)?;
        // Emit a Verifying event before the (potentially long) sha256 read,
        // so the UI shows progress while we re-validate already-present files.
        progress_cb(PackProgress::Verifying {
            id: pack.id.clone(),
            file_name: file.name.clone(),
        });
        if already_installed(&dest, file.size, &file.sha256).await {
            log::info!("pack {} file {} already valid, skipping", pack.id, file.name);
            progress_cb(PackProgress::File {
                id: pack.id.clone(),
                file_index: i,
                file_count,
                file_name: file.name.clone(),
                downloaded: file.size,
                total: file.size,
                bytes_per_sec: 0,
            });
            continue;
        }
        // already_installed returns false if cancel was hit during the sha256
        // read; propagate cancel before proceeding to download.
        if PACK_CANCEL.load(Ordering::Relaxed) {
            progress_cb(PackProgress::Cancelled { id: pack.id });
            return Err(anyhow!("pack download cancelled"));
        }

        let pack_id = pack.id.clone();
        let file_name = file.name.clone();
        // Wrap progress_cb in a local closure that adds file context.
        // Borrow progress_cb mutably here so it isn't moved into the closure.
        let progress_cb_ref = &mut progress_cb;
        let pack_id_for_cb = pack_id.clone();
        let file_name_for_cb = file_name.clone();
        let mut local_cb = move |downloaded: u64, total: u64, bps: u64| {
            (progress_cb_ref)(PackProgress::File {
                id: pack_id_for_cb.clone(),
                file_index: i,
                file_count,
                file_name: file_name_for_cb.clone(),
                downloaded,
                total,
                bytes_per_sec: bps,
            });
        };

        let one_result = download_one(file, &dest, &mut local_cb).await;
        // local_cb (and the &mut borrow of progress_cb) drops here.
        drop(local_cb);
        match one_result {
            Ok(()) => {
                progress_cb(PackProgress::Verifying {
                    id: pack_id.clone(),
                    file_name: file_name.clone(),
                });
            }
            Err(InterruptibleFnError::Interrupted) => {
                progress_cb(PackProgress::Cancelled { id: pack_id });
                return Err(anyhow!("pack download cancelled"));
            }
            Err(InterruptibleFnError::Err(msg)) => {
                progress_cb(PackProgress::Failed {
                    id: pack_id,
                    error: msg.clone(),
                });
                return Err(anyhow!(msg));
            }
        }
    }

    progress_cb(PackProgress::Complete { id: pack.id.clone() });
    Ok(())
}
