//! "New version available" check against the project's GitHub releases.
//!
//! Notification only: the app never downloads or replaces itself. The MSI (Program Files, needs
//! admin rights) stays the single way to install, so Windows keeps an accurate record of the
//! installed version.

use anyhow::{Context, Result};
use serde::Deserialize;

const RELEASES_API: &str = "https://api.github.com/repos/TounSec/PSS-BattleStats/releases";

/// A published release newer than the running build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
}

/// Picks the highest release strictly newer than `current` from a `/releases` JSON payload.
/// Pre-releases are included on purpose (the app is in beta); tags that aren't valid semver
/// (e.g. a stray "nightly") are skipped. Pure/offline - no network access.
pub fn pick_newer(json: &str, current: &str) -> Result<Option<UpdateInfo>> {
    let current = semver::Version::parse(current).context("Invalid current version")?;
    let releases: Vec<Release> =
        serde_json::from_str(json).context("Failed to parse GitHub releases response")?;

    Ok(releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = semver::Version::parse(r.tag_name.trim_start_matches('v')).ok()?;
            (version > current).then_some((version, r.html_url))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(version, url)| UpdateInfo {
            version: version.to_string(),
            url,
        }))
}

/// Asks GitHub for the release list and compares it with the running version.
pub async fn check(http: &reqwest::Client) -> Result<Option<UpdateInfo>> {
    let body = http
        .get(RELEASES_API)
        // GitHub's API rejects requests without a User-Agent.
        .header(
            reqwest::header::USER_AGENT,
            concat!("pss-battlestats/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("Failed to call the GitHub releases API")?
        .error_for_status()
        .context("GitHub releases API returned an error")?
        .text()
        .await
        .context("Failed to read the GitHub releases response")?;

    pick_newer(&body, env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASES: &str = r#"[
        {"tag_name":"v0.2.0-beta.1","html_url":"https://example/0.2.0-beta.1","draft":false},
        {"tag_name":"v0.1.1","html_url":"https://example/0.1.1","draft":false},
        {"tag_name":"v0.3.0","html_url":"https://example/0.3.0","draft":true},
        {"tag_name":"nightly","html_url":"https://example/nightly","draft":false},
        {"tag_name":"v0.1.0","html_url":"https://example/0.1.0","draft":false}
    ]"#;

    #[test]
    fn picks_highest_non_draft_release_newer_than_current() {
        let update = pick_newer(RELEASES, "0.1.0").unwrap().unwrap();
        assert_eq!(update.version, "0.2.0-beta.1");
        assert_eq!(update.url, "https://example/0.2.0-beta.1");
    }

    #[test]
    fn none_when_already_up_to_date() {
        assert_eq!(pick_newer(RELEASES, "0.2.0-beta.1").unwrap(), None);
        assert_eq!(pick_newer(RELEASES, "1.0.0").unwrap(), None);
    }

    #[test]
    fn stable_release_is_newer_than_its_own_prerelease() {
        let json = r#"[{"tag_name":"v0.2.0","html_url":"u","draft":false}]"#;
        assert!(pick_newer(json, "0.2.0-beta.1").unwrap().is_some());
    }

    #[test]
    fn invalid_payload_is_an_error() {
        assert!(pick_newer("not json", "0.1.0").is_err());
    }
}
