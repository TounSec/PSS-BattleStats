//! Crew prestige (fusion) recipes from two public endpoints (no accessToken, verified live):
//! - `CharacterService/PrestigeCharacterTo?characterDesignId=X` - pairs that fuse INTO X
//! - `CharacterService/PrestigeCharacterFrom?characterDesignId=X` - what X fuses into, per partner
//!
//! Both answer with the same `<Prestige CharacterDesignId1 CharacterDesignId2
//! ToCharacterDesignId />` rows. Fetched on demand rather than cached: one call per character,
//! only when the user looks at it.

use anyhow::{Context, Result};
use serde::Deserialize;

const PRESTIGE_TO_URL: &str = "https://api.pixelstarships.com/CharacterService/PrestigeCharacterTo";
const PRESTIGE_FROM_URL: &str =
    "https://api.pixelstarships.com/CharacterService/PrestigeCharacterFrom";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Prestige {
    #[serde(rename = "@CharacterDesignId1")]
    pub character_design_id_1: i32,
    #[serde(rename = "@CharacterDesignId2")]
    pub character_design_id_2: i32,
    #[serde(rename = "@ToCharacterDesignId")]
    pub to_character_design_id: i32,
}

impl Prestige {
    /// The other ingredient when `character_design_id` is one of the two.
    pub fn partner_of(&self, character_design_id: i32) -> i32 {
        if self.character_design_id_1 == character_design_id {
            self.character_design_id_2
        } else {
            self.character_design_id_1
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CharacterPrestiges {
    /// Recipes producing the character.
    pub to: Vec<Prestige>,
    /// Recipes using the character as an ingredient.
    pub from: Vec<Prestige>,
}

#[derive(Deserialize)]
struct PrestigeRows {
    #[serde(rename = "Prestige", default)]
    prestige: Vec<Prestige>,
}

#[derive(Deserialize)]
struct PrestigeList {
    #[serde(rename = "Prestiges")]
    prestiges: PrestigeRows,
}

#[derive(Deserialize)]
struct PrestigeEnvelope {
    #[serde(rename = "PrestigeCharacterTo", alias = "PrestigeCharacterFrom")]
    list: PrestigeList,
}

/// Parses either endpoint's response body. Pure/offline.
pub fn parse_prestige_xml(xml: &str) -> Result<Vec<Prestige>> {
    let envelope: PrestigeEnvelope = quick_xml::de::from_str(xml)
        .context("Failed to parse CharacterService/PrestigeCharacter*")?;
    Ok(envelope.list.prestiges.prestige)
}

pub async fn fetch_character_prestiges(
    http: &reqwest::Client,
    character_design_id: i32,
) -> Result<CharacterPrestiges> {
    let get = |url: &'static str| async move {
        let xml = http
            .get(format!(
                "{url}?characterDesignId={character_design_id}&languageKey=en"
            ))
            .send()
            .await
            .with_context(|| format!("Failed to reach {url}"))?
            .text()
            .await
            .with_context(|| format!("Failed to read {url} response body"))?;
        parse_prestige_xml(&xml)
    };
    Ok(CharacterPrestiges {
        to: get(PRESTIGE_TO_URL).await?,
        from: get(PRESTIGE_FROM_URL).await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a live PrestigeCharacterFrom?characterDesignId=583 (Neptune Mariner) response
    const SAMPLE_FROM_XML: &str = r#"<CharacterService><PrestigeCharacterFrom timestamp="2026-09-28T12:04:10"><Prestiges><Prestige CharacterDesignId1="583" CharacterDesignId2="7" ToCharacterDesignId="350" /><Prestige CharacterDesignId1="583" CharacterDesignId2="10" ToCharacterDesignId="436" /></Prestiges></PrestigeCharacterFrom></CharacterService>"#;
    // Live PrestigeCharacterTo?characterDesignId=583 - no recipe produces Neptune Mariner
    const SAMPLE_EMPTY_TO_XML: &str = r#"<CharacterService><PrestigeCharacterTo timestamp="2026-09-28T12:04:10"><Prestiges></Prestiges></PrestigeCharacterTo></CharacterService>"#;

    #[test]
    fn parses_prestige_from_rows() {
        let rows = parse_prestige_xml(SAMPLE_FROM_XML).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].to_character_design_id, 350);
        assert_eq!(rows[0].partner_of(583), 7);
        assert_eq!(rows[1].partner_of(583), 10);
    }

    #[test]
    fn parses_empty_prestige_list() {
        assert!(parse_prestige_xml(SAMPLE_EMPTY_TO_XML).unwrap().is_empty());
    }

    #[test]
    #[ignore = "hits the live PSS API - run explicitly with `cargo test -- --ignored`"]
    fn fetches_against_the_real_api() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let prestiges = runtime
            .block_on(fetch_character_prestiges(&reqwest::Client::new(), 583))
            .unwrap();
        assert!(!prestiges.from.is_empty());
    }
}
