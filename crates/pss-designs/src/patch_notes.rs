//! "Galaxy Patch Notes" scraping/caching - blog.pixelstarships.com, not the game API.
//!
//! There is no structured API for this: it's a WordPress blog, and a single post's changelog is
//! one flat bullet list with no sub-headings ("Balance"/"QoL"/"Bug Fixes" don't exist in the
//! markup) - verified against a real post (V0.999.60, 83 list items). Categorization is therefore
//! a best-effort text heuristic, not a guarantee: "Fixed a bug..." reliably means a bug fix
//! (15/15 on the sample post), a handful of verb phrases ("increased from/to", "now has"...)
//! reliably mean a numeric balance change, everything else falls into a catch-all bucket rather
//! than being mis-classified with false confidence.
//!
//! The WordPress RSS feed (`/feed/`) is used as the fetch source rather than scraping rendered
//! HTML pages: `<content:encoded>` already carries the full post body (verified byte-identical in
//! shape to the live page's `<article>` content), and `<dc:creator>`/`<pubDate>` give a reliable
//! per-post author/date without hunting through page `<meta>` tags. The feed only keeps a rolling
//! window of recent posts (WordPress default), so older versions need `?paged=N` on the feed URL,
//! tried up to a small limit and stopped once a page comes back empty.
//!
//! Phase 1 (this module, for now): version list, UGC designs with real hosted images, changelog
//! entries bucketed into Balance/Fix/Other as plain text. Deliberately NOT yet implemented:
//! structured before/after parsing cross-referenced against the `room_designs` cache (needs a
//! second pass once the plain-text version is in front of real usage), and "your own rooms"
//! cross-referencing (needs a definition of "your current ship" - the most recent captured
//! battle's attacker, since pss-capture only ever sees the local player's own outgoing traffic).

use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub const BLOG_FEED_URL: &str = "https://blog.pixelstarships.com/feed/";
const MAX_FEED_PAGES: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UgcKind {
    Skin,
    Sticker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UgcDesign {
    pub name: String,
    pub creator: String,
    pub image_url: Option<String>,
    pub kind: UgcKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangelogCategory {
    Balance,
    Fix,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangelogEntry {
    pub category: ChangelogCategory,
    pub text: String,
    /// Only ever `Some` for `category == Balance`, and only when a numeric before/after value
    /// could actually be pulled out of the sentence - a Balance-classified entry with no
    /// structured value still displays (as plain text, via `text`), it just can't show
    /// avant/après/Δ columns.
    pub balance: Option<BalanceChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceChange {
    /// Room/craft name + stat name as they appeared in the sentence (e.g. "Ion Cannon Room Reload
    /// Time") - not cross-referenced against `room_designs` at parse time; matching against the
    /// cache (fuzzy, by substring) happens at render time, once "your current ship" is known.
    pub subject: String,
    /// `None` when the sentence didn't state a prior value (e.g. "Volley increased **to** 10") -
    /// different from `Some(0.0)`, which means the sentence said "now has" (a stat introduced
    /// where there previously was none at all, e.g. a Room gaining a Cooldown for the first time).
    pub before: Option<f64>,
    pub after: f64,
    pub unit: Option<String>,
}

impl BalanceChange {
    /// `None` when there's no `before` to compare against (unstated - not the same as a `now has`
    /// introduction, which returns `None` here too since a percentage change from zero is
    /// undefined; the GUI shows "nouveau" for that case instead of a percentage).
    pub fn delta_percent(&self) -> Option<f64> {
        let before = self.before?;
        if before == 0.0 {
            return None;
        }
        Some((self.after - before) / before * 100.0)
    }

    /// The `now has`/`gains` phrasing this was parsed from stated no prior value at all, i.e.
    /// this stat is brand new on this room/craft - shown as "nouveau" instead of a percentage.
    pub fn is_new(&self) -> bool {
        self.before == Some(0.0)
    }
}

fn parse_balance_change(text: &str) -> Option<BalanceChange> {
    // Ordered most-specific-first; the first pattern that matches wins. Real phrasing varies too
    // much to catch with one regex - see module docs
    static FROM_TO: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static TO_ONLY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NOW_HAS_VALUE_UNIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NOW_HAS_NOUN_OF_VALUE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

    let from_to = FROM_TO.get_or_init(|| {
        Regex::new(
            r"(?i)^(.*?)\s+(?:increased|reduced|decreased)\s+from\s+(-?[\d.]+)\s*(%|sec)?\s+to\s+(-?[\d.]+)\s*(%|sec)?",
        )
        .unwrap()
    });
    if let Some(c) = from_to.captures(text) {
        let before: f64 = c[2].parse().ok()?;
        let after: f64 = c[4].parse().ok()?;
        let unit = c
            .get(5)
            .or_else(|| c.get(3))
            .map(|m| m.as_str().to_string());
        return Some(BalanceChange {
            subject: c[1].trim().to_string(),
            before: Some(before),
            after,
            unit,
        });
    }

    let to_only = TO_ONLY.get_or_init(|| {
        Regex::new(r"(?i)^(.*?)\s+(?:increased|reduced|decreased)\s+to\s+(-?[\d.]+)\s*(%|sec)?")
            .unwrap()
    });
    if let Some(c) = to_only.captures(text) {
        let after: f64 = c[2].parse().ok()?;
        return Some(BalanceChange {
            subject: c[1].trim().to_string(),
            before: None,
            after,
            unit: c.get(3).map(|m| m.as_str().to_string()),
        });
    }

    let now_has_value_unit = NOW_HAS_VALUE_UNIT.get_or_init(|| {
        Regex::new(r"(?i)^(.+?)\s+now (?:has|have)\s+(?:an?\s+)?(?:minimum\s+)?(-?[\d.]+)\s*(%|sec)\s*(.*)$").unwrap()
    });
    if let Some(c) = now_has_value_unit.captures(text) {
        let after: f64 = c[2].parse().ok()?;
        let prefix = c[1].trim();
        let suffix = c[4].trim();
        let subject = if suffix.is_empty() {
            prefix.to_string()
        } else {
            format!("{prefix} {suffix}")
        };
        return Some(BalanceChange {
            subject,
            before: Some(0.0),
            after,
            unit: Some(c[3].to_string()),
        });
    }

    let now_has_noun_of_value = NOW_HAS_NOUN_OF_VALUE.get_or_init(|| {
        Regex::new(r"(?i)^(.+?)\s+now (?:has|have)\s+(?:an?\s+)?([A-Za-z ]+?)\s+of\s+(-?[\d.]+)")
            .unwrap()
    });
    if let Some(c) = now_has_noun_of_value.captures(text) {
        let after: f64 = c[3].parse().ok()?;
        return Some(BalanceChange {
            subject: format!("{} {}", c[1].trim(), c[2].trim()),
            before: Some(0.0),
            after,
            unit: None,
        });
    }

    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchNoteVersion {
    pub version: String,
    pub post_url: String,
    pub post_title: String,
    /// `YYYY-MM-DD`, parsed from the feed's RFC 2822 `pubDate` - sortable, displayable, not a
    /// full timestamp (the source only ever needs day-level precision here).
    pub published_at: String,
    pub author: String,
    pub ugc_designs: Vec<UgcDesign>,
    pub entries: Vec<ChangelogEntry>,
}

impl PatchNoteVersion {
    pub fn count(&self, category: ChangelogCategory) -> usize {
        self.entries
            .iter()
            .filter(|e| e.category == category)
            .count()
    }
}

// ---------------------------------------------------------------- RSS feed parsing

struct FeedItem {
    title: String,
    link: String,
    pub_date: String,
    creator: String,
    content_html: String,
}

fn strip_cdata(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(trimmed)
        .to_string()
}

fn extract_tag(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = item.find(&open)? + open.len();
    let rest = &item[start..];
    let end = rest.find(&close)?;
    Some(strip_cdata(&rest[..end]))
}

fn split_items(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<item>") {
        let after = &rest[start + "<item>".len()..];
        let Some(end) = after.find("</item>") else {
            break;
        };
        out.push(&after[..end]);
        rest = &after[end + "</item>".len()..];
    }
    out
}

/// Parses a WordPress RSS feed body into its items. Pure/offline - no network access.
fn parse_feed_items(xml: &str) -> Vec<FeedItem> {
    split_items(xml)
        .into_iter()
        .filter_map(|raw| {
            Some(FeedItem {
                title: extract_tag(raw, "title")?,
                link: extract_tag(raw, "link")?,
                pub_date: extract_tag(raw, "pubDate")?,
                creator: extract_tag(raw, "dc:creator").unwrap_or_default(),
                content_html: extract_tag(raw, "content:encoded").unwrap_or_default(),
            })
        })
        .collect()
}

/// `"Mon, 17 Aug 2026 01:35:29 +0000"` -> `"2026-08-17"`. No `chrono` dependency for one fixed,
/// always-consistent format (WordPress always emits RFC 2822 here).
fn parse_rfc2822_date(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month_name, year, ..] = parts[..] else {
        return None;
    };
    let month = match month_name {
        "Jan" => "01",
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => return None,
    };
    Some(format!("{year}-{month}-{day:0>2}"))
}

/// Decodes numeric character references (`&#8217;`, `&#x2019;` - what WordPress emits for almost
/// everything non-ASCII, including UGC creator names in CJK/other scripts) plus the handful of
/// named entities that show up in practice. Not a general HTML-entity decoder (no full named-
/// entity table) - deliberately narrow to what this specific feed actually emits.
fn html_unescape(s: &str) -> String {
    let numeric_re = Regex::new(r"&#(x?)([0-9a-fA-F]+);").unwrap();
    let decoded = numeric_re.replace_all(s, |c: &regex::Captures| {
        let is_hex = &c[1] == "x";
        let digits = &c[2];
        let code = u32::from_str_radix(digits, if is_hex { 16 } else { 10 }).ok();
        code.and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_default()
    });
    decoded
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_unescape(out.trim())
}

// ---------------------------------------------------------------- content parsing

fn classify_entry(text: &str) -> ChangelogCategory {
    let lower = text.to_lowercase();
    if lower.trim_start().starts_with("fixed") {
        return ChangelogCategory::Fix;
    }
    const BALANCE_MARKERS: &[&str] = &[
        "increased from",
        "reduced from",
        "decreased from",
        "increased to",
        "reduced to",
        "decreased to",
        "now has",
        "now have",
        "gains a",
        "gain a",
    ];
    if BALANCE_MARKERS.iter().any(|m| lower.contains(m)) {
        return ChangelogCategory::Balance;
    }
    ChangelogCategory::Other
}

fn parse_changelog_entries(list_html: &str) -> Vec<ChangelogEntry> {
    let li_re = Regex::new(r"(?s)<li[^>]*>(.*?)</li>").unwrap();
    li_re
        .captures_iter(list_html)
        .map(|c| {
            let text = strip_tags(&c[1]);
            let category = classify_entry(&text);
            let balance = (category == ChangelogCategory::Balance)
                .then(|| parse_balance_change(&text))
                .flatten();
            ChangelogEntry {
                category,
                text,
                balance,
            }
        })
        .filter(|e| !e.text.is_empty())
        .collect()
}

fn parse_ugc_designs(section_html: &str) -> Vec<UgcDesign> {
    let img_re = Regex::new(r#"<img[^>]*\ssrc="([^"]+)""#).unwrap();
    let image_urls: Vec<&str> = img_re
        .captures_iter(section_html)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();

    let Some(credits_start) = section_html.find("Congratulations to") else {
        return Vec::new();
    };
    let credits_end = section_html[credits_start..]
        .find("</p>")
        .map(|i| credits_start + i)
        .unwrap_or(section_html.len());
    let credits_html = &section_html[credits_start..credits_end];

    let credit_re = Regex::new(r"(?s)<strong>([^<]+)</strong>\s*\(([^)]+)\)").unwrap();
    credit_re
        .captures_iter(credits_html)
        .enumerate()
        .map(|(i, c)| {
            let creator = html_unescape(c[1].trim());
            let raw_name = html_unescape(c[2].trim());
            let kind = if raw_name.to_lowercase().contains("sticker") {
                UgcKind::Sticker
            } else {
                UgcKind::Skin
            };
            // Cosmetic only: "Miru Sticker" -> "Miru" - the "Sticker" suffix is what `kind`
            // detection above keys on, but it's redundant once the kind is already a badge/label
            let name = if kind == UgcKind::Sticker {
                raw_name
                    .split_whitespace()
                    .rev()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                raw_name
            };
            UgcDesign {
                name,
                creator,
                image_url: image_urls.get(i).map(|s| s.to_string()),
                kind,
            }
        })
        .collect()
}

/// Parses every `V<version> Changelogs:` section out of one post's full HTML content, sharing
/// that post's UGC design list across all of them (a single post can cover more than one
/// version, e.g. a hotfix rolled into the next patch's post - see `V0.999.58`/`V0.999.60` both
/// appearing in the same real post).
fn parse_post_versions(item: &FeedItem) -> Vec<PatchNoteVersion> {
    let published_at = parse_rfc2822_date(&item.pub_date).unwrap_or_default();
    let author = html_unescape(&item.creator);

    let ugc_section = item
        .content_html
        .find("New UGC Designs")
        .map(|start| {
            let after = &item.content_html[start..];
            let end = after
                .find("<h4")
                .or_else(|| after.find("<h5"))
                .filter(|&i| i > 0)
                .unwrap_or(after.len());
            &after[..end]
        })
        .unwrap_or("");
    let ugc_designs = parse_ugc_designs(ugc_section);

    let heading_re = Regex::new(r"(?i)(V[\d.]+)\s+Changelogs:").unwrap();
    let headings: Vec<(String, usize)> = heading_re
        .captures_iter(&item.content_html)
        .map(|c| (c[1].to_string(), c.get(0).unwrap().start()))
        .collect();

    if headings.is_empty() {
        return Vec::new();
    }

    headings
        .iter()
        .enumerate()
        .map(|(i, (version, start))| {
            let end = headings
                .get(i + 1)
                .map(|(_, s)| *s)
                .unwrap_or(item.content_html.len());
            let section = &item.content_html[*start..end];
            PatchNoteVersion {
                version: version.clone(),
                post_url: item.link.clone(),
                post_title: html_unescape(&item.title),
                published_at: published_at.clone(),
                author: author.clone(),
                ugc_designs: ugc_designs.clone(),
                entries: parse_changelog_entries(section),
            }
        })
        .collect()
}

/// Fetches and parses every "Galaxy Patch Notes" version found in the blog's RSS feed, paging
/// back through `?paged=N` up to `MAX_FEED_PAGES` times to reach further back than the feed's
/// default rolling window. Pure fetch+parse - does not touch the cache; call `upsert_patch_notes`
/// with the result to persist it.
pub async fn fetch_patch_notes(http: &reqwest::Client) -> Result<Vec<PatchNoteVersion>> {
    let mut versions = Vec::new();
    for page in 1..=MAX_FEED_PAGES {
        let url = if page == 1 {
            BLOG_FEED_URL.to_string()
        } else {
            format!("{BLOG_FEED_URL}?paged={page}")
        };
        let response = http
            .get(&url)
            .send()
            .await
            .context("Failed to fetch blog.pixelstarships.com feed")?;
        if !response.status().is_success() {
            break; // ran past the last page
        }
        let xml = response.text().await.context("Failed to read feed body")?;
        let items = parse_feed_items(&xml);
        if items.is_empty() {
            break;
        }
        let mut found_any_patch_note = false;
        for item in &items {
            if item.title.to_lowercase().contains("patch notes") {
                found_any_patch_note = true;
                versions.extend(parse_post_versions(item));
            }
        }
        // Once a whole feed page contains no patch-notes-titled post at all, older pages are
        // overwhelmingly likely to be even less relevant (other blog content) - stop early rather
        // than paging back through the entire blog history on every refresh
        if !found_any_patch_note && page > 1 {
            break;
        }
    }
    Ok(versions)
}

// ---------------------------------------------------------------- cache (SQLite)

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS patch_notes (
            version         TEXT PRIMARY KEY,
            post_url        TEXT NOT NULL,
            post_title      TEXT NOT NULL,
            published_at    TEXT NOT NULL,
            author          TEXT NOT NULL,
            ugc_json        TEXT NOT NULL,
            entries_json    TEXT NOT NULL,
            fetched_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );
        ",
    )?;
    Ok(())
}

pub fn upsert_patch_notes(conn: &Connection, versions: &[PatchNoteVersion]) -> Result<()> {
    for v in versions {
        let ugc_json = serde_json::to_string(&v.ugc_designs)?;
        let entries_json = serde_json::to_string(&v.entries)?;
        conn.execute(
            "INSERT INTO patch_notes
                (version, post_url, post_title, published_at, author, ugc_json, entries_json, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(version) DO UPDATE SET
                post_url = excluded.post_url,
                post_title = excluded.post_title,
                published_at = excluded.published_at,
                author = excluded.author,
                ugc_json = excluded.ugc_json,
                entries_json = excluded.entries_json,
                fetched_at = excluded.fetched_at",
            params![v.version, v.post_url, v.post_title, v.published_at, v.author, ugc_json, entries_json],
        )?;
    }
    Ok(())
}

fn row_to_version(
    version: String,
    post_url: String,
    post_title: String,
    published_at: String,
    author: String,
    ugc_json: String,
    entries_json: String,
) -> Result<PatchNoteVersion> {
    Ok(PatchNoteVersion {
        version,
        post_url,
        post_title,
        published_at,
        author,
        ugc_designs: serde_json::from_str(&ugc_json).context("Failed to parse cached ugc_json")?,
        entries: serde_json::from_str(&entries_json)
            .context("Failed to parse cached entries_json")?,
    })
}

/// Most recent versions first (by `published_at`, falling back to the version string itself for
/// same-day posts covering more than one version).
pub fn list_patch_notes(conn: &Connection) -> Result<Vec<PatchNoteVersion>> {
    let mut stmt = conn.prepare(
        "SELECT version, post_url, post_title, published_at, author, ugc_json, entries_json
         FROM patch_notes ORDER BY published_at DESC, version DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to list cached patch notes")?
        .into_iter()
        .map(
            |(version, post_url, post_title, published_at, author, ugc_json, entries_json)| {
                row_to_version(
                    version,
                    post_url,
                    post_title,
                    published_at,
                    author,
                    ugc_json,
                    entries_json,
                )
            },
        )
        .collect()
}

/// The most recent `fetched_at` across all cached versions - "dernier fetch" in the GUI. `None`
/// if the cache is empty (never fetched).
pub fn last_fetched_at(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("SELECT MAX(fetched_at) FROM patch_notes", [], |row| {
        row.get(0)
    })
    .optional()
    .map(|opt| opt.flatten())
    .context("Failed to read patch notes last-fetch time")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a real blog.pixelstarships.com/feed/ response (one <item>, content_html cut
    // down to the two sections this module actually reads - UGC credits/images and one
    // "Changelogs:" list - full real post had 83 list items and 15 UGC designs)
    const SAMPLE_FEED_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:dc="http://purl.org/dc/elements/1.1/"><channel><item>
<title>Galaxy Patch Notes V0.999.60</title>
<link>https://blog.pixelstarships.com/2026/08/17/galaxy-patch-notes-v0-999-60/</link>
<dc:creator><![CDATA[Toma]]></dc:creator>
<pubDate>Mon, 17 Aug 2026 01:35:29 +0000</pubDate>
<category><![CDATA[Balance Changes]]></category>
<content:encoded><![CDATA[<h4 class="wp-block-heading">New UGC Designs</h4>
<p>Today's patch includes 2 Room Skins and 1 Ship Sticker designed by players:</p>
<img decoding="async" width="224" height="129" src="https://blog.pixelstarships.com/wp-content/uploads/2026/08/Mechanical-Bunny-Girl.gif" alt=""/>
<img decoding="async" width="130" height="130" src="https://blog.pixelstarships.com/wp-content/uploads/2026/08/Cat-Ear-Clinic.gif" alt=""/>
<img decoding="async" width="130" height="130" src="https://blog.pixelstarships.com/wp-content/uploads/2026/08/Miru.gif" alt=""/>
<p>Congratulations to <strong>cheong&#26085; </strong>(Mechanical Bunny Girl), <strong>Someone</strong> (Cat Ear Clinic), <strong>yasusu </strong>(Miru Sticker).</p>
<h5 class="wp-block-heading">V0.999.60 Changelogs:</h5>
<ul>
<li>Recall Beacon Cooldown increased from 1 to 2 sec</li>
<li>Fixed a bug causing inaccurate Manual &#8216;drag and drop&#8217; Targeting</li>
<li>Google Play Sidekick now available on Android</li>
<li>Repair Drone Craft Volley increased to 10</li>
</ul>
<h5 class="wp-block-heading">V0.999.58 Changelogs:</h5>
<ul>
<li>Older patch entry</li>
<li>Fixed a bug preventing delivery of Tapjoy Rewards</li>
</ul>
]]></content:encoded>
</item></channel></rss>"#;

    #[test]
    fn parses_feed_items() {
        let items = parse_feed_items(SAMPLE_FEED_XML);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Galaxy Patch Notes V0.999.60");
        assert_eq!(items[0].creator, "Toma");
        assert_eq!(items[0].pub_date, "Mon, 17 Aug 2026 01:35:29 +0000");
        assert!(items[0].content_html.contains("New UGC Designs"));
    }

    #[test]
    fn parses_rfc2822_dates() {
        assert_eq!(
            parse_rfc2822_date("Mon, 17 Aug 2026 01:35:29 +0000"),
            Some("2026-08-17".to_string())
        );
        assert_eq!(
            parse_rfc2822_date("Wed, 01 Jan 2025 00:00:00 +0000"),
            Some("2025-01-01".to_string())
        );
    }

    #[test]
    fn splits_one_post_into_multiple_version_sections() {
        let items = parse_feed_items(SAMPLE_FEED_XML);
        let versions = parse_post_versions(&items[0]);
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, "V0.999.60");
        assert_eq!(versions[1].version, "V0.999.58");
        // Both sections share the same post-level UGC list
        assert_eq!(versions[0].ugc_designs.len(), 3);
        assert_eq!(versions[1].ugc_designs.len(), 3);
        assert_eq!(versions[0].published_at, "2026-08-17");
        assert_eq!(versions[0].author, "Toma");
    }

    #[test]
    fn classifies_changelog_entries() {
        let items = parse_feed_items(SAMPLE_FEED_XML);
        let versions = parse_post_versions(&items[0]);
        let entries = &versions[0].entries;
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].category, ChangelogCategory::Balance);
        assert_eq!(
            entries[0].text,
            "Recall Beacon Cooldown increased from 1 to 2 sec"
        );
        assert_eq!(entries[1].category, ChangelogCategory::Fix);
        assert!(entries[1].text.contains("drag and drop"));
        assert_eq!(entries[2].category, ChangelogCategory::Other);
        assert_eq!(entries[3].category, ChangelogCategory::Balance);

        let older = &versions[1].entries;
        assert_eq!(older.len(), 2);
        assert_eq!(older[0].category, ChangelogCategory::Other);
        assert_eq!(older[1].category, ChangelogCategory::Fix);
    }

    #[test]
    fn parses_structured_balance_from_to() {
        let change = parse_balance_change("Recall Beacon Cooldown increased from 1 to 2 sec")
            .expect("should parse");
        assert_eq!(change.subject, "Recall Beacon Cooldown");
        assert_eq!(change.before, Some(1.0));
        assert_eq!(change.after, 2.0);
        assert_eq!(change.unit.as_deref(), Some("sec"));
        assert_eq!(change.delta_percent(), Some(100.0));
        assert!(!change.is_new());
    }

    #[test]
    fn parses_structured_balance_percent_unit() {
        let change = parse_balance_change("Engine Room Evasion Regeneration reduced from 5% to 4%")
            .expect("should parse");
        assert_eq!(change.before, Some(5.0));
        assert_eq!(change.after, 4.0);
        assert_eq!(change.unit.as_deref(), Some("%"));
        assert_eq!(change.delta_percent(), Some(-20.0));
    }

    #[test]
    fn parses_structured_balance_to_only_as_unstated_before() {
        let change = parse_balance_change("Repair Drone Craft Volley increased to 10")
            .expect("should parse");
        assert_eq!(change.subject, "Repair Drone Craft Volley");
        assert_eq!(change.before, None);
        assert_eq!(change.after, 10.0);
        assert_eq!(change.delta_percent(), None);
        assert!(!change.is_new()); // unstated, not "brand new"
    }

    #[test]
    fn parses_structured_balance_now_has_as_new() {
        let change = parse_balance_change("Laser Blaster Room now has 1 sec Cooldown Time")
            .expect("should parse");
        assert_eq!(change.subject, "Laser Blaster Room Cooldown Time");
        assert_eq!(change.before, Some(0.0));
        assert_eq!(change.after, 1.0);
        assert!(change.is_new());
        assert_eq!(change.delta_percent(), None); // "nouveau", not a percentage

        let change2 =
            parse_balance_change("Irradiator Room now has a Volley of 3 and a Random AOE of 200")
                .expect("should parse");
        assert_eq!(change2.subject, "Irradiator Room Volley");
        assert!(change2.is_new());
        assert_eq!(change2.after, 3.0);
    }

    #[test]
    fn parses_negative_percentages_and_now_have_plural() {
        let change =
            parse_balance_change("Herald Craft Reload Modifier increased from -60% to -50%")
                .expect("should parse");
        assert_eq!(change.before, Some(-60.0));
        assert_eq!(change.after, -50.0);

        let change2 =
            parse_balance_change("All Hangar Rooms now have a minimum 1 sec Cooldown Time")
                .expect("should parse");
        assert!(change2.is_new());
        assert_eq!(change2.after, 1.0);
    }

    #[test]
    fn unparseable_balance_text_returns_none() {
        assert_eq!(
            parse_balance_change("Gamma Beam Emitter Room now categorized as a Cannon"),
            None
        );
    }

    #[test]
    fn parses_ugc_designs_with_kind_and_images() {
        let items = parse_feed_items(SAMPLE_FEED_XML);
        let versions = parse_post_versions(&items[0]);
        let ugc = &versions[0].ugc_designs;
        assert_eq!(ugc.len(), 3);

        assert_eq!(ugc[0].name, "Mechanical Bunny Girl");
        assert_eq!(ugc[0].creator, "cheong日");
        assert_eq!(ugc[0].kind, UgcKind::Skin);
        assert_eq!(
            ugc[0].image_url.as_deref(),
            Some(
                "https://blog.pixelstarships.com/wp-content/uploads/2026/08/Mechanical-Bunny-Girl.gif"
            )
        );

        assert_eq!(ugc[2].name, "Miru"); // "Miru Sticker" -> "Miru", suffix stripped
        assert_eq!(ugc[2].creator, "yasusu");
        assert_eq!(ugc[2].kind, UgcKind::Sticker);
    }

    #[test]
    fn count_helper_matches_category() {
        let items = parse_feed_items(SAMPLE_FEED_XML);
        let versions = parse_post_versions(&items[0]);
        assert_eq!(versions[0].count(ChangelogCategory::Balance), 2);
        assert_eq!(versions[0].count(ChangelogCategory::Fix), 1);
        assert_eq!(versions[0].count(ChangelogCategory::Other), 1);
    }

    #[test]
    fn cache_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(list_patch_notes(&conn).unwrap().is_empty());
        assert_eq!(last_fetched_at(&conn).unwrap(), None);

        let items = parse_feed_items(SAMPLE_FEED_XML);
        let versions = parse_post_versions(&items[0]);
        upsert_patch_notes(&conn, &versions).unwrap();

        let cached = list_patch_notes(&conn).unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].version, "V0.999.60");
        assert_eq!(cached[0].entries.len(), 4);
        assert!(last_fetched_at(&conn).unwrap().is_some());
    }

    #[test]
    fn parses_real_captured_feed_item_without_panicking() {
        // A real, unedited blog.pixelstarships.com/feed/ <item> for the same V0.999.60 post used
        // as the basis for SAMPLE_FEED_XML above, but untrimmed - full 83-entry changelog, real
        // CJK/emoji creator names, real apostrophes/dashes as WordPress actually emits them. This
        // is what proves the regexes survive real messy data, not just the clean synthetic sample
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/blog_patch_notes_v0_999_60.xml");
        let xml = std::fs::read_to_string(&path).expect("Failed to read real feed fixture");

        let items = parse_feed_items(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Galaxy Patch Notes V0.999.60");
        assert_eq!(items[0].creator, "Toma");
        assert_eq!(items[0].pub_date, "Mon, 17 Aug 2026 01:35:29 +0000");

        let versions = parse_post_versions(&items[0]);
        assert_eq!(
            versions.len(),
            2,
            "expected V0.999.60 and V0.999.58 sections"
        );
        assert_eq!(versions[0].version, "V0.999.60");
        assert_eq!(versions[0].published_at, "2026-08-17");
        assert_eq!(versions[1].version, "V0.999.58");

        // 15 real UGC designs (9 skins + 6 stickers) - every one must have resolved a name,
        // creator, and image url; no silent gaps from the unicode-heavy creator names
        assert_eq!(versions[0].ugc_designs.len(), 15);
        for design in &versions[0].ugc_designs {
            assert!(!design.name.is_empty());
            assert!(!design.creator.is_empty());
            assert!(design.image_url.is_some());
        }
        assert_eq!(
            versions[0]
                .ugc_designs
                .iter()
                .filter(|d| d.kind == UgcKind::Sticker)
                .count(),
            6
        );
        assert_eq!(
            versions[0]
                .ugc_designs
                .iter()
                .filter(|d| d.kind == UgcKind::Skin)
                .count(),
            9
        );

        // Real post has 83 flat list items under "V0.999.60 Changelogs:" - every one must
        // classify into some bucket and end up with non-empty, tag-free text
        assert_eq!(versions[0].entries.len(), 83);
        for entry in &versions[0].entries {
            assert!(!entry.text.is_empty());
            assert!(!entry.text.contains('<'));
        }
        assert_eq!(versions[0].count(ChangelogCategory::Fix), 15);
        assert!(versions[0].count(ChangelogCategory::Balance) >= 40);

        // Structured (subject/before/after) parsing should cover the vast majority of real
        // Balance-classified entries, not just the clean synthetic sample - 47/48 on this post
        let balance_entries: Vec<&ChangelogEntry> = versions[0]
            .entries
            .iter()
            .filter(|e| e.category == ChangelogCategory::Balance)
            .collect();
        let structured = balance_entries
            .iter()
            .filter(|e| e.balance.is_some())
            .count();
        assert!(
            structured * 100 >= balance_entries.len() * 90,
            "expected >=90% structured coverage, got {structured}/{}",
            balance_entries.len()
        );
    }

    #[test]
    fn upserting_again_overwrites_same_version() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let items = parse_feed_items(SAMPLE_FEED_XML);
        let versions = parse_post_versions(&items[0]);
        upsert_patch_notes(&conn, &versions).unwrap();
        upsert_patch_notes(&conn, &versions).unwrap();
        assert_eq!(list_patch_notes(&conn).unwrap().len(), 2);
    }

    #[tokio::test]
    #[ignore = "hits the live blog.pixelstarships.com feed - run explicitly with `cargo test -- --ignored`"]
    async fn fetches_against_the_real_blog() {
        let http = reqwest::Client::new();
        let versions = fetch_patch_notes(&http)
            .await
            .expect("Fetch should succeed");
        assert!(!versions.is_empty());
        for v in &versions {
            assert!(!v.version.is_empty());
            assert!(!v.entries.is_empty());
        }
    }
}
