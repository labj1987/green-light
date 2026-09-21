use anyhow::{Context, Result};
use regex::Regex;
use scraper::{Html, Selector};

/// NVIDIA's Linux driver release branch, classified from the version's major
/// number. Ranges researched against NVIDIA's driver download page and
/// endoflife.date/nvidia on 2026-08-12 — re-check before trusting this long
/// after that date, since NVIDIA rolls these forward roughly every 3 months
/// (New Feature Branch) to a year+ (Production/LTS Branch):
///   - New Feature Branch: R610 (610.43.02+) — early-adopter track, quarterly
///   - Production Branch: R595 (595.45.04+) — the recommended stable track
///   - Long Term Support Branch: R580 (580.65.06+) — 3 years of security
///     support, positioned alongside (not superseded by) the newer PB/NFB
///   - Legacy: the frozen major lines NVIDIA still updates for GPUs the
///     current unified driver no longer supports (R470 for Kepler, R390 for
///     Fermi). Every other superseded major (535, 550, 560, 570, etc.) was
///     itself once a Production or New Feature Branch but isn't a named
///     "Legacy" line today, so it's deliberately left unclassified below
///     rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverBranch {
    NewFeature,
    Production,
    LongTermSupport,
    Legacy,
}

impl DriverBranch {
    pub fn label(&self) -> &'static str {
        match self {
            DriverBranch::NewFeature => "New Feature",
            DriverBranch::Production => "Production",
            DriverBranch::LongTermSupport => "LTS",
            DriverBranch::Legacy => "Legacy",
        }
    }

    /// Adwaita style class giving each branch's badge a distinct tint,
    /// consistent with the install/version-status badges elsewhere in ui.rs.
    pub fn css_class(&self) -> &'static str {
        match self {
            DriverBranch::NewFeature => "accent",
            DriverBranch::Production => "success",
            DriverBranch::LongTermSupport => "warning",
            DriverBranch::Legacy => "dim-label",
        }
    }
}

fn classify_branch(version: &str) -> Option<DriverBranch> {
    let major: u32 = version.split('.').next()?.parse().ok()?;
    match major {
        610 => Some(DriverBranch::NewFeature),
        595 => Some(DriverBranch::Production),
        580 => Some(DriverBranch::LongTermSupport),
        470 | 390 => Some(DriverBranch::Legacy),
        _ => None,
    }
}

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

pub async fn fetch_versions() -> Result<Vec<DriverVersion>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("greenlight/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()?;

    let html = client
        .get(NVIDIA_BASE)
        .send()
        .await
        .context("Failed to reach NVIDIA download server")?
        .text()
        .await?;

    let document = Html::parse_document(&html);
    let selector = Selector::parse("a[href]").unwrap();
    let ver_re = Regex::new(&format!(r"^({})/?$", VERSION_PATTERN)).unwrap();

    let mut versions: Vec<DriverVersion> = document
        .select(&selector)
        .filter_map(|el| {
            let href = el.value().attr("href")?;
            let caps = ver_re.captures(href)?;
            let version = caps[1].to_string();
            let filename = format!("NVIDIA-Linux-x86_64-{}.run", version);
            let url = format!("{}{}/{}", NVIDIA_BASE, version, filename);
            let branch = classify_branch(&version);
            Some(DriverVersion { version, filename, url, branch })
        })
        .collect();

    versions.sort_by(|a, b| {
        let av: Vec<u32> = a.version.split('.').filter_map(|s| s.parse().ok()).collect();
        let bv: Vec<u32> = b.version.split('.').filter_map(|s| s.parse().ok()).collect();
        bv.cmp(&av)
    });

    Ok(versions)
}

/// Fetch the SHA256 checksum for a given version's .run file.
pub async fn fetch_checksum(version: &DriverVersion) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("greenlight/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(15))
        .build()?;

    let ver_str = &version.version;

    // Try .manifest first
    let manifest_url = format!("{}{}/{}", NVIDIA_BASE, ver_str, ".manifest");
    if let Ok(r) = client.get(&manifest_url).send().await {
        if r.status().is_success() {
            let text = r.text().await?;
            for line in text.lines() {
                if line.contains(&version.filename) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if let Some(hash) = parts.first() {
                        if hash.len() == 64 {
                            return Ok(Some(hash.to_string()));
                        }
                    }
                }
            }
        }
    }

    // Fallback: <filename>.sha256sum
    let sha_url = format!("{}{}/{}.sha256sum", NVIDIA_BASE, ver_str, version.filename);
    if let Ok(r) = client.get(&sha_url).send().await {
        if r.status().is_success() {
            let text = r.text().await?;
            let hash = text.split_whitespace().next().unwrap_or("").to_string();
            if hash.len() == 64 {
                return Ok(Some(hash));
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_branches() {
        assert_eq!(classify_branch("610.57.04"), Some(DriverBranch::NewFeature));
        assert_eq!(classify_branch("595.91.07"), Some(DriverBranch::Production));
        assert_eq!(classify_branch("580.178.04"), Some(DriverBranch::LongTermSupport));
        assert_eq!(classify_branch("470.256.02"), Some(DriverBranch::Legacy));
        assert_eq!(classify_branch("390.157"), Some(DriverBranch::Legacy));
    }

    #[test]
    fn leaves_unmatched_versions_unlabeled() {
        assert_eq!(classify_branch("570.211.01"), None);
        assert_eq!(classify_branch("535.309.01"), None);
        assert_eq!(classify_branch("96.43.23"), None);
    }

    #[tokio::test]
    #[ignore]
    async fn live_fetch_shows_branch_labels() {
        let versions = fetch_versions().await.expect("live fetch failed");
        assert!(!versions.is_empty());
        let mut seen = std::collections::HashSet::new();
        for v in &versions {
            if let Some(b) = v.branch {
                seen.insert(b.label());
            }
        }
        println!("branches seen live: {seen:?}");
        assert!(seen.contains("New Feature") || seen.contains("Production"));
    }
}
