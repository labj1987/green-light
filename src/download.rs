use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_SECS: u64 = 3;

/// Download a file from `url` into `dest_dir`.
/// `progress_cb(downloaded_bytes, total_bytes_option)`
/// `cancel` — set to true to abort mid-download.
pub async fn download_run_file<F>(
    url: &str,
    dest_dir: &Path,
    filename: &str,
    mut progress_cb: F,
    cancel: Arc<AtomicBool>,
) -> Result<PathBuf>
where
    F: FnMut(u64, Option<u64>) + Send + 'static,
{
    let dest = dest_dir.join(filename);

    // Ensure the destination directory exists — e.g. ~/Downloads may not
    // exist yet, or HOME may point somewhere unexpected (root, containers).
    tokio::fs::create_dir_all(dest_dir)
        .await
        .with_context(|| format!("Could not create download directory: {}", dest_dir.display()))?;

    // connect_timeout + read_timeout instead of a total request timeout:
    // a total timeout would kill a slow-but-healthy multi-hundred-MB
    // download; read_timeout only fires when the stream actually stalls.
    let client = reqwest::Client::builder()
        .user_agent(concat!("green-light/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?;

    let mut last_err = anyhow::anyhow!("unknown error");

    for attempt in 0..MAX_RETRIES {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }

        if attempt > 0 {
            let wait = RETRY_BASE_SECS * (2u64.pow(attempt - 1));
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => { last_err = e.into(); continue; }
        };

        let status = resp.status();
        if status.is_client_error() {
            // 4xx (404, 403, ...) won't fix itself on retry — fail immediately.
            bail!("Server refused the download: HTTP {}", status);
        }
        if !status.is_success() {
            last_err = anyhow::anyhow!("HTTP {}", status);
            continue;
        }

        let total = resp.content_length();
        let mut downloaded: u64 = 0;
        let mut stream = resp.bytes_stream();

        let mut file = match tokio::fs::File::create(&dest).await {
            Ok(f) => f,
            Err(e) => { last_err = e.into(); continue; }
        };

        let mut failed = false;
        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = tokio::fs::remove_file(&dest).await;
                bail!("Download cancelled");
            }
            match chunk {
                Ok(bytes) => {
                    if let Err(e) = file.write_all(&bytes).await {
                        last_err = e.into(); failed = true; break;
                    }
                    downloaded += bytes.len() as u64;
                    progress_cb(downloaded, total);
                }
                Err(e) => { last_err = e.into(); failed = true; break; }
            }
        }

        if !failed {
            file.flush().await?;
            // A server that closes the stream early ends the loop cleanly
            // with a truncated file; catch it here rather than relying on a
            // checksum happening to exist.
            match total {
                Some(t) if downloaded != t => {
                    last_err = anyhow::anyhow!(
                        "Download truncated: received {} of {} bytes", downloaded, t);
                }
                _ => return Ok(dest),
            }
        }

        let _ = tokio::fs::remove_file(&dest).await;
    }

    Err(last_err).context(format!("Download failed after {} attempts", MAX_RETRIES))
}

/// Verify SHA256 of a file against an expected hex string.
/// Streams the file in 1 MiB chunks so a ~300 MB .run file is never
/// held in memory all at once.
pub async fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("Could not open file for SHA256 verification")?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = file
            .read(&mut buf)
            .await
            .context("Read error during SHA256 verification")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let result = hex::encode(hasher.finalize());

    if result.to_lowercase() != expected.to_lowercase() {
        bail!(
            "SHA256 mismatch:\n  expected: {}\n  got:      {}",
            expected, result
        );
    }
    Ok(())
}
