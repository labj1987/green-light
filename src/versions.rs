use anyhow::{bail, Context, Result};
use regex::Regex;
use scraper::{Html, Selector};

use crate::branches::DriverBranch;

#[derive(Debug, Clone)]
pub struct DriverVersion {
    pub version: String,
    pub filename: String,
    pub url: String,
    pub branch: Option<DriverBranch>,
}

/// Regex fragment for an NVIDIA driver version (e.g. `595.84` or `595.84.01`).
const VERSION_PATTERN: &str = r"\d+\.\d+(?:\.\d+)?";

/// Extract the driver version from an official installer filename such as
/// `NVIDIA-Linux-x86_64-595.84.run`. Returns `None` for anything that
/// doesn't match exactly (e.g. `...-595.84-vulkan.run`), so callers can show
/// "Unknown" rather than guess.
pub fn version_from_filename(filename: &str) -> Option<String> {
    let re = Regex::new(&format!(r"^NVIDIA-Linux-x86_64-({})\.run$", VERSION_PATTERN)).ok()?;
    re.captures(filename).map(|c| c[1].to_string())
}

const NVIDIA_BASE: &str = "https://download.nvidia.com/XFree86/Linux-x86_64/";

/// The driver versions, without branch labels: those come from a separate
/// and slower source (`branches.rs`), which the UI applies when it answers.
pub async fn fetch_versions() -> Result<Vec<DriverVersion>> {
    fetch_versions_from(NVIDIA_BASE).await
}

async fn fetch_versions_from(base: &str) -> Result<Vec<DriverVersion>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("green-light/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client
        .get(base)
        .send()
        .await
        .context("Failed to reach NVIDIA download server")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("NVIDIA's download server answered HTTP {}", status);
    }
    let html = resp.text().await.context("Failed to read NVIDIA's version list")?;
    let versions = parse_version_listing(&html, base);
    if versions.is_empty() {
        bail!("NVIDIA's download page listed no driver versions");
    }
    Ok(versions)
}

/// The driver versions linked from the download directory listing, newest first.
fn parse_version_listing(html: &str, base: &str) -> Vec<DriverVersion> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a[href]").unwrap();
    let ver_re = Regex::new(&format!(r"^({})/?$", VERSION_PATTERN)).unwrap();

    let mut versions: Vec<DriverVersion> = document
        .select(&selector)
        .filter_map(|el| {
            let href = el.value().attr("href")?;
            let caps = ver_re.captures(href)?;
            let version = caps[1].to_string();
            let filename = format!("NVIDIA-Linux-x86_64-{}.run", version);
            let url = format!("{}{}/{}", base, version, filename);
            Some(DriverVersion { version, filename, url, branch: None })
        })
        .collect();

    versions.sort_by(|a, b| {
        let av: Vec<u32> = a.version.split('.').filter_map(|s| s.parse().ok()).collect();
        let bv: Vec<u32> = b.version.split('.').filter_map(|s| s.parse().ok()).collect();
        bv.cmp(&av)
    });
    versions
}

/// Whether `s` is a SHA256 in hex.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The checksum in a `sha256sum` line (`<hash>  <filename>`), lowercase, when
/// it is a well-formed SHA256 for `filename`.
fn parse_sha256sum(text: &str, filename: &str) -> Option<String> {
    let mut fields = text.split_whitespace();
    let hash = fields.next()?;
    let name = fields.next().map(|n| n.trim_start_matches('*'));
    if !is_sha256_hex(hash) || name.is_some_and(|n| n != filename) {
        return None;
    }
    Some(hash.to_ascii_lowercase())
}

/// Fetch the SHA256 NVIDIA publishes for a version's .run file, as
/// `<filename>.sha256sum` next to it. `Ok(None)` only when NVIDIA has no such
/// file (HTTP 404); a network error, any other HTTP status or an unreadable
/// checksum file is an error, never "no checksum".
pub async fn fetch_checksum(version: &DriverVersion) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("green-light/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(15))
        .build()?;

    let sha_url = format!("{}.sha256sum", version.url);
    let resp = client
        .get(&sha_url)
        .send()
        .await
        .context("Could not reach NVIDIA's download server for the checksum")?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("NVIDIA's download server answered HTTP {} for the checksum", status);
    }
    let text = resp.text().await.context("Could not read the checksum file")?;
    match parse_sha256sum(&text, &version.filename) {
        Some(hash) => Ok(Some(hash)),
        None => bail!("NVIDIA's checksum file for {} is not a valid SHA256 line", version.filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_from_official_filename() {
        assert_eq!(
            version_from_filename("NVIDIA-Linux-x86_64-595.84.run").as_deref(),
            Some("595.84")
        );
        assert_eq!(
            version_from_filename("NVIDIA-Linux-x86_64-580.178.04.run").as_deref(),
            Some("580.178.04")
        );
    }

    #[test]
    fn rejects_non_matching_filenames() {
        assert_eq!(version_from_filename("NVIDIA-Linux-x86_64-595.84-vulkan.run"), None);
        assert_eq!(version_from_filename("driver.run"), None);
    }

    #[test]
    fn sha256sum_parsing() {
        let h = "849ef0ef8e842b9806b2cde9f11c1303d54f1a9a769467e4e5d961b2fe1182a7";
        let f = "NVIDIA-Linux-x86_64-580.95.05.run";
        assert_eq!(parse_sha256sum(&format!("{h}  {f}\n"), f).as_deref(), Some(h));
        assert_eq!(parse_sha256sum(&format!("{}  {f}", h.to_uppercase()), f).as_deref(), Some(h));
        assert_eq!(parse_sha256sum(&format!("{h} *{f}"), f).as_deref(), Some(h));
        assert_eq!(parse_sha256sum(h, f).as_deref(), Some(h));
        assert_eq!(parse_sha256sum(&format!("{h}  other.run"), f), None);
        assert_eq!(parse_sha256sum("<html>Not found</html>", f), None);
        assert_eq!(parse_sha256sum(&format!("{}  {f}", &h[..63]), f), None);
        assert_eq!(parse_sha256sum(&format!("{}g  {f}", &h[..63]), f), None);
        assert_eq!(parse_sha256sum("", f), None);
        // Multi-byte text of the right byte length must not panic or pass.
        assert!(!is_sha256_hex(&"é".repeat(32)));
    }

    fn version_at(base: &str, v: &str) -> DriverVersion {
        let filename = format!("NVIDIA-Linux-x86_64-{v}.run");
        DriverVersion {
            version: v.to_string(),
            url: format!("{base}{v}/{filename}"),
            filename,
            branch: None,
        }
    }

    #[tokio::test]
    async fn checksum_found_missing_and_failing() {
        use crate::test_http::{dead_url, route, serve};
        let h = "849ef0ef8e842b9806b2cde9f11c1303d54f1a9a769467e4e5d961b2fe1182a7";
        let ok_body = format!("{h}  NVIDIA-Linux-x86_64-1.0.run\n");
        let base = serve(vec![
            route("/1.0/NVIDIA-Linux-x86_64-1.0.run.sha256sum", 200, ok_body.as_bytes()),
            route("/3.0/NVIDIA-Linux-x86_64-3.0.run.sha256sum", 500, b"oops"),
            route("/4.0/NVIDIA-Linux-x86_64-4.0.run.sha256sum", 200, b"<html>maintenance</html>"),
            route("/5.0/NVIDIA-Linux-x86_64-5.0.run.sha256sum", 403, b"denied"),
        ])
        .await;
        assert_eq!(fetch_checksum(&version_at(&base, "1.0")).await.unwrap().as_deref(), Some(h));
        // Only a 404 means "NVIDIA publishes none".
        assert_eq!(fetch_checksum(&version_at(&base, "2.0")).await.unwrap(), None);
        for v in ["3.0", "4.0", "5.0"] {
            assert!(fetch_checksum(&version_at(&base, v)).await.is_err(), "{v}");
        }
        let dead = dead_url().await;
        assert!(fetch_checksum(&version_at(&dead, "1.0")).await.is_err());
    }

    #[tokio::test]
    async fn version_list_errors() {
        use crate::test_http::{dead_url, route, serve};
        let listing = br#"<a href="../">..</a><a href="580.95.05/">580.95.05/</a>
            <a href="595.84/">595.84/</a><a href="latest.txt">latest.txt</a>"#;
        let base = serve(vec![route("/", 200, listing)]).await;
        let v = fetch_versions_from(&base).await.unwrap();
        assert_eq!(v.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["595.84", "580.95.05"]);
        assert_eq!(v[0].url, format!("{base}595.84/NVIDIA-Linux-x86_64-595.84.run"));

        let down = serve(vec![route("/", 503, b"<html>Service Unavailable</html>")]).await;
        let e = fetch_versions_from(&down).await.unwrap_err();
        assert!(e.to_string().contains("503"), "{e}");
        let empty = serve(vec![route("/", 200, b"<html>nothing here</html>")]).await;
        assert!(fetch_versions_from(&empty).await.is_err());
        assert!(fetch_versions_from(&dead_url().await).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn live_fetch_shows_branch_labels() {
        let mut versions = fetch_versions().await.expect("live fetch failed");
        assert!(!versions.is_empty());
        let branches = crate::branches::fetch_branches(crate::branches::UNIX_DRIVERS_PAGE)
            .await
            .expect("live branch fetch failed");
        for v in &mut versions {
            v.branch = branches.classify(&v.version);
        }
        let mut seen = std::collections::HashSet::new();
        for v in &versions {
            if let Some(b) = v.branch {
                seen.insert(b.label());
            }
        }
        println!("branches seen live: {seen:?}");
        assert!(seen.contains("New Feature") && seen.contains("Production"));
        let newest = &versions[0];
        let cs = fetch_checksum(newest).await.expect("live checksum fetch failed");
        println!("{} sha256: {cs:?}", newest.version);
        assert!(cs.is_some_and(|c| is_sha256_hex(&c)));
    }
}
