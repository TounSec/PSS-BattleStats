//! "New version available" check against the project's GitHub releases, plus the Windows
//! self-install of the MSI attached to a release.
//!
//! The installer is never run silently: Windows shows its usual UAC prompt and progress window,
//! and the MSI (Program Files, per-machine) stays the single way the app gets installed, so
//! Windows keeps an accurate record of the installed version.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const RELEASES_API: &str = "https://api.github.com/repos/TounSec/PSS-BattleStats/releases";
/// Assets are only ever downloaded from this project's own release URLs, whatever the API says.
const DOWNLOAD_PREFIX: &str = "https://github.com/TounSec/PSS-BattleStats/releases/download/";
const SUMS_ASSET: &str = "SHA256SUMS.txt";

/// The MSI of a release and the checksum file published next to it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerAsset {
    pub name: String,
    pub url: String,
    pub sums_url: String,
}

/// A published release newer than the running build
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    /// `None` when the release has no MSI (or no checksum file): the banner then only links to the
    /// release page
    pub installer: Option<InstallerAsset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// Names we are willing to write to the temp folder: no separators, no `..`, `.msi` only
fn is_safe_msi_name(name: &str) -> bool {
    name.ends_with(".msi")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn installer_of(assets: &[Asset]) -> Option<InstallerAsset> {
    let trusted = |a: &&Asset| a.browser_download_url.starts_with(DOWNLOAD_PREFIX);
    let msi = assets
        .iter()
        .filter(trusted)
        .find(|a| is_safe_msi_name(&a.name))?;
    let sums = assets
        .iter()
        .filter(trusted)
        .find(|a| a.name == SUMS_ASSET)?;

    Some(InstallerAsset {
        name: msi.name.clone(),
        url: msi.browser_download_url.clone(),
        sums_url: sums.browser_download_url.clone(),
    })
}

/// Picks the highest release strictly newer than `current` from a `/releases` JSON payload.
/// Pre-releases are included on purpose, tags that aren't valid semver (e.g. a stray "nightly")
/// are skipped. Pure/offline - no network access
pub fn pick_newer(json: &str, current: &str) -> Result<Option<UpdateInfo>> {
    let current = semver::Version::parse(current).context("Invalid current version")?;
    let releases: Vec<Release> =
        serde_json::from_str(json).context("Failed to parse GitHub releases response")?;

    Ok(releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = semver::Version::parse(r.tag_name.trim_start_matches('v')).ok()?;
            (version > current).then_some((version, r))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(version, r)| UpdateInfo {
            version: version.to_string(),
            installer: installer_of(&r.assets),
            url: r.html_url,
        }))
}

/// Asks GitHub for the release list and compares it with the running version
pub async fn check(http: &reqwest::Client) -> Result<Option<UpdateInfo>> {
    let body = http
        .get(RELEASES_API)
        // GitHub's API rejects requests without a User-Agent
        .header(reqwest::header::USER_AGENT, user_agent())
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

fn user_agent() -> &'static str {
    concat!("pss-battlestats/", env!("CARGO_PKG_VERSION"))
}

/// Finds `file_name`'s hash in `sha256sum` output ("<hash> <name>" or <hash> *<name>)
fn expected_hash(sums: &str, file_name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        let valid = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
        (valid && name == file_name).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn get_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    Ok(http
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent())
        .send()
        .await
        .with_context(|| format!("Failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("Download of {url} returned an error"))?
        .bytes()
        .await
        .with_context(|| format!("Failed to read {url}"))?
        .to_vec())
}

/// Downloads the release's MSI into the temp folder after checking it against the release's
/// `Sha256SUMS.txt`. This catches truncated or currupted downloads; both files comme from the same
/// release, so it is not a defense against a compromised release
pub async fn download_installer(http: &reqwest::Client, asset: &InstallerAsset) -> Result<PathBuf> {
    if !is_safe_msi_name(&asset.name)
        || !asset.url.starts_with(DOWNLOAD_PREFIX)
        || !asset.sums_url.starts_with(DOWNLOAD_PREFIX)
    {
        bail!("Refusing to download an installer from an truncated location");
    }

    let sums = String::from_utf8(get_bytes(http, &asset.sums_url).await?)
        .context("Sha256SUMS.txt is not valid UTF-8")?;
    let expected = expected_hash(&sums, &asset.name)
        .with_context(|| format!("No checksum for {} in Sha256SUMS.txt", asset.name))?;

    let bytes = get_bytes(http, &asset.url).await?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!(
            "Checksum mismatch for {} (expected {expected}, got {actual})",
            asset.name
        );
    }

    let path = std::env::temp_dir().join(&asset.name);
    std::fs::write(&path, &bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Starts the Windows Installer on the downloaded MSI (normal UI, UAC prompt included). The caller
/// closes the app afterwards so the installer can replace the running executable
pub fn launch_installer(msi: &Path) -> Result<()> {
    if !cfg!(windows) {
        bail!("Installing an MSI is only supported on Windows");
    }

    std::process::Command::new("msiexec")
        .arg("/i")
        .arg(msi)
        .spawn()
        .context("Failed to start msiexec")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://github.com/TounSec/PSS-BattleStats/releases/download";

    fn releases() -> String {
        format!(
            r#"[
          {{"tag_name":"v0.2.0-beta.1","html_url":"https://example/0.2.0-beta.1","draft":false,
            "assets":[
              {{"name":"PSS-BattleStats-0.2.0-beta.1.msi","browser_download_url":"{BASE}/v0.2.0-beta.1/PSS-BattleStats-0.2.0-beta.1.msi"}},
              {{"name":"SHA256SUMS.txt","browser_download_url":"{BASE}/v0.2.0-beta.1/SHA256SUMS.txt"}}]}},
          {{"tag_name":"v0.1.1","html_url":"https://example/0.1.1","draft":false}},
          {{"tag_name":"v0.3.0","html_url":"https://example/0.3.0","draft":true}},
          {{"tag_name":"nightly","html_url":"https://example/nightly","draft":false}},
          {{"tag_name":"v0.1.0","html_url":"https://example/0.1.0","draft":false}}
      ]"#
        )
    }

    #[test]
    fn picks_highest_non_draft_release_newer_than_current() {
        let update = pick_newer(&releases(), "0.1.0").unwrap().unwrap();
        assert_eq!(update.version, "0.2.0-beta.1");
        assert_eq!(update.url, "https://example/0.2.0-beta.1");
    }

    #[test]
    fn exposes_the_msi_and_checksum_of_the_chosen_release() {
        let update = pick_newer(&releases(), "0.1.0").unwrap().unwrap();
        let installer = update.installer.unwrap();
        assert_eq!(installer.name, "PSS-BattleStats-0.2.0-beta.1.msi");
        assert!(installer.url.ends_with("/PSS-BattleStats-0.2.0-beta.1.msi"));
        assert!(installer.sums_url.ends_with("/SHA256SUMS.txt"));
    }

    #[test]
    fn release_without_msi_only_links_to_the_page() {
        let json = r#"[{"tag_name":"v0.2.0","html_url":"u","draft":false}]"#;
        assert_eq!(pick_newer(json, "0.1.0").unwrap().unwrap().installer, None);
    }

    #[test]
    fn assets_outside_the_project_releases_are_ignored() {
        let json = r#"[{"tag_name":"v0.2.0","html_url":"u","draft":false,"assets":[{"name":"x.msi","browser_download_url":"https://evil.example/x.msi"},{"name":"SHA256SUMS.txt","browser_download_url":"https://evil.example/SHA256SUMS.txt"}]}]"#;
        assert_eq!(pick_newer(json, "0.1.0").unwrap().unwrap().installer, None);
    }

    #[test]
    fn none_when_already_up_to_date() {
        assert_eq!(pick_newer(&releases(), "0.2.0-beta.1").unwrap(), None);
        assert_eq!(pick_newer(&releases(), "1.0.0").unwrap(), None);
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

    #[test]
    fn msi_file_names_are_restricted() {
        assert!(is_safe_msi_name("PSS-BattleStats-0.2.0-beta.1.msi"));
        assert!(!is_safe_msi_name("..\\evil.msi"));
        assert!(!is_safe_msi_name("a/b.msi"));
        assert!(!is_safe_msi_name("tool.exe"));
        assert!(!is_safe_msi_name("a..b.msi"));
    }

    #[test]
    fn finds_the_hash_in_sha256sum_output() {
        let h1 = "a".repeat(64);
        let h2 = "B".repeat(64);
        let sums = format!("{h1}    one.msi\n{h2} *two.msi\nnot-a-hash  three.msi\n");
        assert_eq!(expected_hash(&sums, "one.msi"), Some(h1));
        assert_eq!(expected_hash(&sums, "two.msi"), Some("b".repeat(64)));
        assert_eq!(expected_hash(&sums, "three.msi"), None);
        assert_eq!(expected_hash(&sums, "missing.msi"), None);
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
