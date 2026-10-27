//! Driver branch labels (Production, New Feature, ...) taken from NVIDIA's
//! own data, so they follow NVIDIA as branches roll over instead of going
//! stale. NVIDIA's Unix driver page fills in its "Latest Production Branch",
//! "Latest New Feature Branch" and "Latest Legacy GPU" lines by asking its
//! driver lookup service; the page's HTML carries the service's address and
//! the exact queries. They are read from the page and the same questions are
//! asked here:
//!
//! - Production: the branch of the latest Production Branch driver.
//! - New Feature: the branch of the latest New Feature Branch driver.
//! - Legacy: the branch of the latest legacy-GPU driver.
//! - Older Production: an older production branch that NVIDIA has released
//!   drivers for since the current Production Branch began, such as a
//!   long-term branch. NVIDIA's data doesn't mark long-term support, so the
//!   label doesn't claim it.
//!
//! Every other branch stays unlabeled, and with no answer from NVIDIA nothing
//! is labeled rather than guessed.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::HashMap;

pub const UNIX_DRIVERS_PAGE: &str = "https://www.nvidia.com/en-us/drivers/unix/";

/// The page's language (en-US), which its queries are built with.
const COUNTRY_ID: &str = "1033";

/// How many production drivers to ask for (the service returns at most 30).
/// It lists them newest branch first, each branch newest driver first, so
/// this covers all of the current Production Branch and the latest drivers
/// of the branches before it.
const PRODUCTION_HISTORY: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverBranch {
    NewFeature,
    Production,
    OlderProduction,
    Legacy,
}

impl DriverBranch {
    pub fn label(&self) -> &'static str {
        match self {
            DriverBranch::NewFeature => "New Feature",
            DriverBranch::Production => "Production",
            DriverBranch::OlderProduction => "Older Production",
            DriverBranch::Legacy => "Legacy",
        }
    }

    /// Adwaita style class giving each branch's badge a distinct tint,
    /// consistent with the install/version-status badges elsewhere in ui.rs.
    pub fn css_class(&self) -> &'static str {
        match self {
            DriverBranch::NewFeature => "accent",
            DriverBranch::Production => "success",
            DriverBranch::OlderProduction => "warning",
            DriverBranch::Legacy => "dim-label",
        }
    }
}

/// One driver as the lookup service describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Major version ("Release": "595").
    pub branch: u32,
    /// (year, month, day) of "ReleaseDateTime": "Tue Sep 22, 2026".
    pub date: Option<(u32, u32, u32)>,
}

/// The branch numbers behind each label.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Branches {
    pub production: Option<u32>,
    pub new_feature: Option<u32>,
    pub legacy: Option<u32>,
    pub older_production: Vec<u32>,
}

impl Branches {
    pub fn classify(&self, version: &str) -> Option<DriverBranch> {
        let major: u32 = version.split('.').next()?.parse().ok()?;
        if self.new_feature == Some(major) {
            Some(DriverBranch::NewFeature)
        } else if self.production == Some(major) {
            Some(DriverBranch::Production)
        } else if self.legacy == Some(major) {
            Some(DriverBranch::Legacy)
        } else if self.older_production.contains(&major) {
            Some(DriverBranch::OlderProduction)
        } else {
            None
        }
    }
}

/// The lookup service URL (up to its query parameters) and the page's Linux
/// x86_64 queries by type ("PB", "NFB", "BETA", "Legacy").
#[derive(Debug, PartialEq, Eq)]
pub struct PageQueries {
    pub lookup_url: String,
    pub queries: HashMap<String, String>,
}

/// Read the lookup service address and the Linux x86_64 queries from the
/// Unix driver page. `None` when the page no longer has them.
pub fn parse_unix_page(html: &str) -> Option<PageQueries> {
    let endpoint = Regex::new(r#"addProperty\(\s*'web'\s*,\s*'gfDriversServicesToolkitUrlEndpoint'\s*,\s*'([^']+)'"#).ok()?.captures(html)?[1].to_string();
    let path = Regex::new(r#"baseDrvrLkupSrvcURL\s*=\s*servicesToolkitUrl\s*\+\s*'([^']*)'"#).ok()?.captures(html)?[1].to_string();
    let entry =
        Regex::new(r#""urlParams":\s*"([^"]*)"\s*\+\s*countryID\s*\+\s*"([^"]*)",\s*"qryDesc":\s*"[^"]*",\s*"drvrType":\s*"(\w+)",\s*"osCode":\s*"linux64""#)
            .ok()?;
    let queries: HashMap<String, String> = entry.captures_iter(html).map(|c| (c[3].to_string(), format!("{}{COUNTRY_ID}{}", &c[1], &c[2]))).collect();
    if !queries.contains_key("PB") {
        return None;
    }
    Some(PageQueries { lookup_url: format!("{endpoint}{path}{COUNTRY_ID}"), queries })
}

fn parse_date(text: &str) -> Option<(u32, u32, u32)> {
    // "Tue Sep 22, 2026"
    let mut f = text.split_whitespace().skip(1);
    let month = match f.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day = f.next()?.trim_end_matches(',').parse().ok()?;
    let year = f.next()?.parse().ok()?;
    Some((year, month, day))
}

/// The drivers in a lookup service answer.
pub fn parse_lookup(json: &str) -> Result<Vec<Release>> {
    let v: serde_json::Value = serde_json::from_str(json).context("NVIDIA's driver lookup answered something other than JSON")?;
    // "Success" is the number of drivers found.
    let found: u32 = v["Success"].as_str().and_then(|n| n.trim().parse().ok()).context("NVIDIA's driver lookup answered in an unknown format")?;
    if found == 0 {
        return Ok(vec![]);
    }
    let Some(ids) = v["IDS"].as_array() else { return Ok(vec![]) };
    Ok(ids
        .iter()
        .filter_map(|e| {
            let info = &e["downloadInfo"];
            Some(Release { branch: info["Release"].as_str()?.trim().parse().ok()?, date: info["ReleaseDateTime"].as_str().and_then(parse_date) })
        })
        .collect())
}

/// The labels from the answers to the Production (a long history of it), New
/// Feature and Legacy queries.
pub fn derive_branches(production: &[Release], new_feature: &[Release], legacy: &[Release]) -> Branches {
    // The current Production Branch: that of the newest production driver.
    let newest = production.iter().enumerate().max_by(|(ia, a), (ib, b)| a.date.cmp(&b.date).then(ib.cmp(ia))).map(|(_, r)| r);
    let prod = newest.map(|r| r.branch);
    // Since when it has been out, as far as the history reaches.
    let start = production.iter().filter(|r| Some(r.branch) == prod).filter_map(|r| r.date).min();
    let mut older: Vec<u32> = match (prod, start) {
        (Some(p), Some(start)) => production.iter().filter(|r| r.branch < p && r.date.is_some_and(|d| d >= start)).map(|r| r.branch).collect(),
        _ => vec![],
    };
    older.sort_unstable();
    older.dedup();
    Branches {
        production: prod,
        new_feature: new_feature.first().map(|r| r.branch).filter(|b| Some(*b) != prod),
        legacy: legacy.first().map(|r| r.branch).filter(|b| Some(*b) != prod),
        older_production: older,
    }
}

/// Fetch the labels from NVIDIA: the Unix driver page for the queries, then
/// the lookup service.
pub async fn fetch_branches(page_url: &str) -> Result<Branches> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("green-light/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()?;
    let get = |url: String| {
        let client = client.clone();
        async move {
            let resp = client.get(&url).send().await.with_context(|| format!("Could not reach {url}"))?;
            if !resp.status().is_success() {
                bail!("{url} answered HTTP {}", resp.status());
            }
            resp.text().await.with_context(|| format!("Could not read {url}"))
        }
    };

    let page = get(page_url.to_string()).await?;
    let Some(q) = parse_unix_page(&page) else {
        bail!("NVIDIA's driver page no longer lists its branch queries");
    };
    let count = Regex::new(r"numberOfResults=\d+").unwrap();
    let pb = &q.queries["PB"];
    let pb = if count.is_match(pb) {
        count.replace(pb, format!("numberOfResults={PRODUCTION_HISTORY}")).into_owned()
    } else {
        format!("{pb}&numberOfResults={PRODUCTION_HISTORY}")
    };
    let lookup = |params: Option<&String>| {
        let url = params.map(|p| format!("{}&{}", q.lookup_url, p));
        let get = &get;
        async move {
            match url {
                Some(url) => parse_lookup(&get(url).await?),
                None => Ok(vec![]),
            }
        }
    };
    let (prod, nfb, legacy) = tokio::join!(lookup(Some(&pb)), lookup(q.queries.get("NFB")), lookup(q.queries.get("Legacy")));
    let prod = prod?;
    if prod.is_empty() {
        bail!("NVIDIA's driver lookup listed no production drivers");
    }
    Ok(derive_branches(&prod, &nfb?, &legacy?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(branch: u32, date: (u32, u32, u32)) -> Release {
        Release { branch, date: Some(date) }
    }

    /// NVIDIA's answers as of October 2026.
    fn today() -> Branches {
        let production = [
            r(595, (2026, 9, 22)),
            r(595, (2026, 8, 27)),
            r(580, (2026, 8, 3)),
            r(595, (2026, 3, 24)),
            r(580, (2026, 3, 10)),
            r(570, (2026, 1, 13)),
            r(580, (2025, 9, 30)),
        ];
        derive_branches(&production, &[r(615, (2026, 10, 7))], &[r(470, (2024, 6, 4))])
    }

    #[test]
    fn labels_follow_nvidia() {
        let b = today();
        assert_eq!(b.classify("615.78.08"), Some(DriverBranch::NewFeature));
        assert_eq!(b.classify("595.104.02"), Some(DriverBranch::Production));
        assert_eq!(b.classify("580.178.04"), Some(DriverBranch::OlderProduction));
        assert_eq!(b.classify("470.256.02"), Some(DriverBranch::Legacy));
        // Superseded: a previous New Feature Branch, an unmaintained branch.
        assert_eq!(b.classify("610.57.04"), None);
        assert_eq!(b.classify("570.211.01"), None);
        assert_eq!(b.classify("390.157"), None);
        assert_eq!(b.classify("not-a-version"), None);
    }

    #[test]
    fn labels_roll_over_with_nvidia() {
        // A later day: 615 has become the Production Branch, 620 is new.
        let production = [r(615, (2027, 1, 20)), r(595, (2027, 1, 12)), r(615, (2026, 12, 15)), r(595, (2026, 11, 2)), r(580, (2026, 11, 1))];
        let b = derive_branches(&production, &[r(620, (2027, 1, 22))], &[]);
        assert_eq!(b.classify("620.1"), Some(DriverBranch::NewFeature));
        assert_eq!(b.classify("615.5"), Some(DriverBranch::Production));
        assert_eq!(b.classify("595.1"), Some(DriverBranch::OlderProduction));
        // Nothing since 615 started: no longer updated.
        assert_eq!(b.classify("580.1"), None);
        assert_eq!(b.classify("470.1"), None);
    }

    #[test]
    fn no_data_labels_nothing() {
        let b = derive_branches(&[], &[], &[]);
        assert_eq!(b, Branches::default());
        assert_eq!(b.classify("595.84"), None);
    }

    const PAGE: &str = r##"
    nvidiaGDC.addProperty('web', 'gfDriversServicesToolkitUrlEndpoint', 'https://gfwsl.example/services_toolkit');
    baseDrvrLkupSrvcURL =  servicesToolkitUrl + '/services/x/AjaxDriverService.php?func=DriverManualLookup&languageCode=' + countryID;
    const drvrLkupInputs = { "IDS": [
      { "drvrLkupInfo": {
            "urlParams": "psid=133&pfid=1075&osID=12&languageCode="+countryID+"&beta=0&qnf=0&numberOfResults=1",
            "qryDesc": "Latest Production Branch Version",
            "drvrType": "PB",
            "osCode": "linux64",
            "idDrvrType": "#linux64 .PB" } },
      { "drvrLkupInfo": {
            "urlParams": "psid=133&pfid=1075&osID=12&languageCode="+countryID+"&qnf=1&numberOfResults=1",
            "qryDesc": "Latest New Feature Branch Version",
            "drvrType": "NFB",
            "osCode": "linux64",
            "idDrvrType": "#linux64 .NFB" } },
      { "drvrLkupInfo": {
            "urlParams": "psid=122&pfid=944&osID=22&languageCode="+countryID+"&release=470&numberOfResults=1",
            "qryDesc": "Latest Legacy GPU version (470.xx series)",
            "drvrType": "Legacy",
            "osCode": "freebsd",
            "idDrvrType": "#freebsd .Legacy" } }
    ] };"##;

    #[test]
    fn reads_the_queries_from_the_page() {
        let q = parse_unix_page(PAGE).unwrap();
        assert_eq!(q.lookup_url, "https://gfwsl.example/services_toolkit/services/x/AjaxDriverService.php?func=DriverManualLookup&languageCode=1033");
        assert_eq!(q.queries.len(), 2, "only the Linux x86_64 queries");
        assert_eq!(q.queries["PB"], "psid=133&pfid=1075&osID=12&languageCode=1033&beta=0&qnf=0&numberOfResults=1");
        assert!(q.queries["NFB"].contains("qnf=1"));
        assert_eq!(parse_unix_page("<html>redesigned</html>"), None);
    }

    #[test]
    fn reads_lookup_answers() {
        let json = r#"{"Success":"1","IDS":[
            {"downloadInfo":{"Release":"595","DisplayVersion":"595.104.02","ReleaseDateTime":"Tue Sep 22, 2026"}},
            {"downloadInfo":{"Release":"580","DisplayVersion":"","ReleaseDateTime":"odd"}},
            {"downloadInfo":{"Release":"","ReleaseDateTime":"Tue Sep 22, 2026"}}]}"#;
        assert_eq!(parse_lookup(json).unwrap(), vec![r(595, (2026, 9, 22)), Release { branch: 580, date: None }]);
        assert_eq!(parse_lookup(r#"{"Success":"0"}"#).unwrap(), vec![]);
        assert!(parse_lookup(r#"{"IDS":[]}"#).is_err());
        assert!(parse_lookup("<html>").is_err());
    }

    #[tokio::test]
    async fn fetches_from_nvidia_style_endpoints() {
        use crate::test_http::{route, serve, Route};
        let lookup = "/svc/AjaxDriverService.php";
        let answer = |rels: &[(&str, &str)]| {
            let ids: Vec<String> = rels.iter().map(|(rel, date)| format!(r#"{{"downloadInfo":{{"Release":"{rel}","ReleaseDateTime":"{date}"}}}}"#)).collect();
            format!(r#"{{"Success":"1","IDS":[{}]}}"#, ids.join(","))
        };
        let pb = answer(&[("595", "Tue Sep 22, 2026"), ("580", "Mon Aug 03, 2026"), ("595", "Tue Mar 24, 2026")]);
        let nfb = answer(&[("615", "Wed Oct 07, 2026")]);
        let legacy = answer(&[("470", "Tue Jun 04, 2024")]);
        let svc = serve(vec![
            Route { query: "qnf=0", ..route(lookup, 200, pb.as_bytes()) },
            Route { query: "qnf=1", ..route(lookup, 200, nfb.as_bytes()) },
            Route { query: "release=470", ..route(lookup, 200, legacy.as_bytes()) },
        ])
        .await;
        // The page names the lookup service; point it at the test server.
        let page = PAGE
            .replace("https://gfwsl.example/services_toolkit", svc.trim_end_matches('/'))
            .replace("/services/x/AjaxDriverService.php", "/svc/AjaxDriverService.php")
            .replace("osID=22", "osID=12")
            .replace("\"freebsd\"", "\"linux64\"");
        let page_srv = serve(vec![route("/unix/", 200, page.as_bytes())]).await;
        let b = fetch_branches(&format!("{page_srv}unix/")).await.unwrap();
        assert_eq!(b.production, Some(595));
        assert_eq!(b.new_feature, Some(615));
        assert_eq!(b.older_production, vec![580]);
        assert_eq!(b.legacy, Some(470));
        // NVIDIA unreachable or failing: an error, never a guessed label.
        let broken = page.replace("release=470", "release=999");
        let half = serve(vec![route("/unix/", 200, broken.as_bytes())]).await;
        assert!(fetch_branches(&format!("{half}unix/")).await.is_err());
        let down = serve(vec![route("/unix/", 503, b"")]).await;
        assert!(fetch_branches(&format!("{down}unix/")).await.is_err());
    }
}
