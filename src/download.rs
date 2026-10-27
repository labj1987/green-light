use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_RETRIES: u32 = 3;
const RETRY_BASE: Duration = Duration::from_secs(3);

/// Mode of a finished download. Explicit rather than 0666 minus the umask:
/// the install script refuses a .run file anyone but its owner can change,
/// and Ubuntu's default umask (002) would leave it group-writable.
const DOWNLOAD_MODE: u32 = 0o644;

/// Download a file from `url` into `dest_dir`.
/// `progress_cb(downloaded_bytes, total_bytes_option)`
/// `cancel` — set to true to abort mid-download.
///
/// The bytes go to `<filename>.part` and are renamed into place only once
/// complete, so the final name never holds a partial file; any failure
/// deletes the `.part`.
pub async fn download_run_file<F>(url: &str, dest_dir: &Path, filename: &str, progress_cb: F, cancel: Arc<AtomicBool>) -> Result<PathBuf>
where
    F: FnMut(u64, Option<u64>) + Send + 'static,
{
    download_with_retry_base(url, dest_dir, filename, progress_cb, cancel, RETRY_BASE).await
}

async fn download_with_retry_base<F>(
    url: &str,
    dest_dir: &Path,
    filename: &str,
    mut progress_cb: F,
    cancel: Arc<AtomicBool>,
    retry_base: Duration,
) -> Result<PathBuf>
where
    F: FnMut(u64, Option<u64>) + Send + 'static,
{
    let dest = dest_dir.join(filename);
    let part = dest_dir.join(format!("{filename}.part"));

    // Ensure the destination directory exists — e.g. ~/Downloads may not
    // exist yet, or HOME may point somewhere unexpected (root, containers).
    tokio::fs::create_dir_all(dest_dir).await.with_context(|| format!("Could not create download directory: {}", dest_dir.display()))?;

    // connect_timeout + read_timeout instead of a total request timeout:
    // a total timeout would kill a slow-but-healthy multi-hundred-MB
    // download; read_timeout only fires when the stream actually stalls.
    let client = reqwest::Client::builder()
        .user_agent(concat!("green-light/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()?;

    let mut last_err = anyhow::anyhow!("unknown error");

    for attempt in 0..MAX_RETRIES {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }

        if attempt > 0 {
            tokio::time::sleep(retry_base * 2u32.pow(attempt - 1)).await;
        }

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = e.into();
                continue;
            }
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
        match write_part(&part, resp, total, &mut progress_cb, &cancel).await {
            Ok(()) => {}
            Err(Attempt::Cancelled) => {
                let _ = tokio::fs::remove_file(&part).await;
                bail!("Download cancelled");
            }
            Err(Attempt::Failed(e)) => {
                let _ = tokio::fs::remove_file(&part).await;
                last_err = e;
                continue;
            }
        }
        if let Err(e) = tokio::fs::rename(&part, &dest).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e).with_context(|| format!("Could not move the download to {}", dest.display()));
        }
        return Ok(dest);
    }

    Err(last_err).context(format!("Download failed after {} attempts", MAX_RETRIES))
}

enum Attempt {
    Cancelled,
    Failed(anyhow::Error),
}

/// Stream the response body into `part` (created fresh, mode 0644) and make
/// sure every byte reached the disk.
async fn write_part<F>(part: &Path, resp: reqwest::Response, total: Option<u64>, progress_cb: &mut F, cancel: &AtomicBool) -> std::result::Result<(), Attempt>
where
    F: FnMut(u64, Option<u64>),
{
    let fail = |e: anyhow::Error| Attempt::Failed(e);
    let mut file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(DOWNLOAD_MODE).open(part).await.map_err(|e| fail(e.into()))?;
    // A .part left over from an earlier run keeps its old mode on open.
    file.set_permissions(std::fs::Permissions::from_mode(DOWNLOAD_MODE)).await.map_err(|e| fail(e.into()))?;

    let mut downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            return Err(Attempt::Cancelled);
        }
        let bytes = chunk.map_err(|e| fail(e.into()))?;
        file.write_all(&bytes).await.map_err(|e| fail(e.into()))?;
        downloaded += bytes.len() as u64;
        progress_cb(downloaded, total);
    }
    // A server that closes the stream early ends the loop cleanly with a
    // truncated file; catch it here rather than relying on a checksum
    // happening to exist.
    if let Some(t) = total {
        if downloaded != t {
            return Err(fail(anyhow::anyhow!("Download truncated: received {} of {} bytes", downloaded, t)));
        }
    }
    file.flush().await.map_err(|e| fail(e.into()))?;
    file.sync_all().await.map_err(|e| fail(e.into()))?;
    Ok(())
}

/// Verify SHA256 of a file against an expected hex string.
/// Streams the file in 1 MiB chunks so a ~300 MB .run file is never
/// held in memory all at once.
pub async fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = tokio::fs::File::open(path).await.context("Could not open file for SHA256 verification")?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = file.read(&mut buf).await.context("Read error during SHA256 verification")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let result = hex::encode(hasher.finalize());

    if result.to_lowercase() != expected.to_lowercase() {
        bail!("SHA256 mismatch:\n  expected: {}\n  got:      {}", expected, result);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{route, serve, Route};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "green-light-dl-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn get(base: &str, dir: &Path, name: &str) -> Result<PathBuf> {
        download_with_retry_base(&format!("{base}{name}"), dir, name, |_, _| {}, Arc::new(AtomicBool::new(false)), Duration::from_millis(10)).await
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn download_is_0644_whole_and_replaces_old_file() {
        let body = vec![7u8; 100_000];
        let base = serve(vec![route("/d.run", 200, &body)]).await;
        let dir = scratch();
        // A leftover group-writable copy and .part from an earlier attempt.
        for name in ["d.run", "d.run.part"] {
            std::fs::write(dir.join(name), b"old").unwrap();
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o664)).unwrap();
        }
        let path = get(&base, &dir, "d.run").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
        assert_eq!(entries(&dir), ["d.run"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_downloads_leave_nothing_behind() {
        let truncated = Route { announce_len: Some(5000), ..route("/short.run", 200, b"only a few bytes") };
        let base = serve(vec![truncated, route("/busy.run", 503, b"busy")]).await;
        let dir = scratch();
        let e = get(&base, &dir, "short.run").await.unwrap_err();
        assert!(format!("{e:#}").contains("attempts"), "{e:#}");
        let e = get(&base, &dir, "missing.run").await.unwrap_err();
        assert!(e.to_string().contains("404"), "{e}");
        assert!(get(&base, &dir, "busy.run").await.is_err());
        assert!(entries(&dir).is_empty(), "{:?}", entries(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cancel_deletes_the_partial_file() {
        let base = serve(vec![route("/c.run", 200, &vec![1u8; 4_000_000])]).await;
        let dir = scratch();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let e = download_with_retry_base(
            &format!("{base}c.run"),
            &dir,
            "c.run",
            move |_, _| flag.store(true, Ordering::Relaxed),
            cancel,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("cancelled"), "{e}");
        assert!(entries(&dir).is_empty(), "{:?}", entries(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
