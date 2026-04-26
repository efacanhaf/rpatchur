//! Optional content pack downloads.
//!
//! Drives downloads of large GRF files (HD textures, mood, skybox, kRO data) that the user
//! opts into via the launcher UI. Each pack lists `OptionalPackFile` entries with a URL,
//! expected size, and SHA-256 digest. Files are streamed to a `.part` file alongside the
//! game install, verified, then atomically renamed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::cancellation::InterruptibleFnError;
use super::config::{OptionalPack, OptionalPackFile};

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
        let n = tokio::io::AsyncReadExt::read(&mut f, &mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Download a single file with streaming + sha256, honoring a cancellation flag.
async fn download_one<F>(
    file: &OptionalPackFile,
    dest: &Path,
    cancelled: Arc<AtomicBool>,
    mut on_progress: F,
) -> std::result::Result<(), InterruptibleFnError>
where
    F: FnMut(u64, u64, u64),
{
    let part_path = dest.with_extension("part");
    let _ = fs::remove_file(&part_path).await; // best-effort

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| InterruptibleFnError::Err(format!("client build: {}", e)))?;
    let resp = client
        .get(&file.url)
        .send()
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("GET {}: {}", file.url, e)))?
        .error_for_status()
        .map_err(|e| InterruptibleFnError::Err(format!("HTTP error: {}", e)))?;

    let total = resp.content_length().unwrap_or(file.size);
    let mut stream = resp.bytes_stream();
    let mut out = fs::File::create(&part_path)
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("create part: {}", e)))?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let started = Instant::now();
    let mut last_emit = Instant::now();

    while let Some(chunk) = stream.next().await {
        if cancelled.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&part_path).await;
            return Err(InterruptibleFnError::Interrupted);
        }
        let chunk = chunk.map_err(|e| InterruptibleFnError::Err(format!("recv: {}", e)))?;
        hasher.update(&chunk);
        out.write_all(&chunk)
            .await
            .map_err(|e| InterruptibleFnError::Err(format!("write: {}", e)))?;
        downloaded += chunk.len() as u64;

        if last_emit.elapsed().as_millis() > 250 {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let bps = (downloaded as f64 / secs) as u64;
            on_progress(downloaded, total, bps);
            last_emit = Instant::now();
        }
    }
    out.flush()
        .await
        .map_err(|e| InterruptibleFnError::Err(format!("flush: {}", e)))?;
    drop(out);

    let got = format!("{:x}", hasher.finalize());
    if !got.eq_ignore_ascii_case(&file.sha256) {
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
    cancelled: Arc<AtomicBool>,
    mut progress_cb: F,
) -> Result<()>
where
    F: FnMut(PackProgress) + Send + 'static,
{
    let total_bytes: u64 = pack.files.iter().map(|f| f.size).sum();
    progress_cb(PackProgress::Started {
        id: pack.id.clone(),
        total_bytes,
    });

    let file_count = pack.files.len();
    for (i, file) in pack.files.iter().enumerate() {
        let dest = install_path(&file.name)?;
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

        let pack_id = pack.id.clone();
        let file_name = file.name.clone();
        let mut local_cb = {
            let pack_id = pack_id.clone();
            let file_name = file_name.clone();
            move |downloaded: u64, total: u64, bps: u64| {
                progress_cb(PackProgress::File {
                    id: pack_id.clone(),
                    file_index: i,
                    file_count,
                    file_name: file_name.clone(),
                    downloaded,
                    total,
                    bytes_per_sec: bps,
                });
            }
        };

        match download_one(file, &dest, cancelled.clone(), &mut local_cb).await {
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
