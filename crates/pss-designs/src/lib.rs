//! Fetches and caches Pixel Starships design data from the public API (no authentification
//! required for these read-only endpoints). Follows the PixyShip pattern: a local SQLite cache,
//! refreshed only when the upstream data's `version` attribute changes.
//!
//! Five domains are covered:
//!     - `RoomDesign` (+ nested `MissileDesign` for built-in weapons like Laser) from
//!     `RoomService/ListRoomDesigns2`.
//!     - `ItemDesign` (equipement, modules, and ammo identification) from `ItemService/ListItemDesigns2`.
//!     - `CharacterDesign` (crew names, base stats, special ability details) from
//!     `CharacterService/ListAllCharacterDesigns2`.
//!     - `MissileDesign` (real per-shot hull/system/character/shield damage for Missile and Craft
//!     ammo) from `RoomService/ListMissileDesigns` - a standalone endpoint, NOT the same as the
//!     `<MissileDesign>` nested inside `RoomDesign` above. Despite the shared element name and
//!     the "MissileService doesn't exist in the IL2CPP dump" note that used to be here, this
//!     endpoint is real and lives under `RoomService`, same as `ListRoomDesigns2`/
//!     `ListCraftDesigns` - it was missed originally because the search was for a dedicated
//!     "MissileService", not this. Every `ItemDesign.MissileDesignId` (Missile ammo) and
//!     `CraftDesign.MissileDesignId` (Craft) resolves here: verified against a live capture,
//!     135/135 Missile ammo ids and 113/113 Craft ids found, zero misses.
//!     - `CraftDesign` (just enough to resolve a Craft item's `CraftDesignId` to its
//!     `MissileDesignId`) from `RoomService/ListCraftDesigns` - another standalone endpoint,
//!     previously untried.
//!
//! A sixth table, `calibrated_item_damage`, holds empirically-derived damage stats as a fallback
//! for the rare case a Missile/Craft ammo's design isn't in the two caches above (not yet synced,
//! or a genuinely unknown id) - it used to be the *only* path for these two room types before the
//! `MissileDesign`/`CraftDesign` endpoints above were found; now it's a safety net, not the
//! primary source. Android remains uncovered by any of this: its items carry a `CharacterDesignId`
//! instead of a `MissileDesignId`/`CraftDesignId` (an android is a synthetic crew member, not a
//! missile/craft weapon), a different lookup this module doesn't yet do.
//!
//! NOTE: `SettingService/getlatestversion3` currently returns an error response live
//! (`<GetLatestSetting errorMessage="An error occurred." />`), but `getlatestversion2` works and
//! carries every version field we need (`MissileDesignVersion`, `CraftDesignVersion` included) -
//! version checking still goes through the `version` attribute embedded in each `List` response
//! instead, for consistency with the other domains and to avoid a second HTTP round-trip.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Deserialize;

pub mod patch_notes;
pub mod prestige;

const ROOM_DESIGNS_URL: &str =
    "https://api.pixelstarships.com/RoomService/ListRoomDesigns2?languageKey=en";
const ITEM_DESIGNS_URL: &str =
    "https://api.pixelstarships.com/ItemService/ListItemDesigns2?languageKey=en";
const CHARACTER_DESIGNS_URL: &str =
    "https://api.pixelstarships.com/CharacterService/ListAllCharacterDesigns2?languageKey=en";
const MISSILE_DESIGNS_URL: &str =
    "https://api.pixelstarships.com/RoomService/ListMissileDesigns?languageKey=en";
const CRAFT_DESIGNS_URL: &str =
    "https://api.pixelstarships.com/RoomService/ListCraftDesigns?languageKey=en";

const ROOM_DESIGN_VERSION_KEY: &str = "room_design_version";
const ITEM_DESIGN_VERSION_KEY: &str = "item_design_version";
const CHARACTER_DESIGN_VERSION_KEY: &str = "character_design_version";
const MISSILE_DESIGN_VERSION_KEY: &str = "missile_design_version";
const CRAFT_DESIGN_VERSION_KEY: &str = "craft_design_version";

/// A single `<RoomDesign>` entry, with only the fields relevant to combat stats. Unrecognized XML
/// attributes are ignored by the deserializer.
#[derive(Debug, Clone, Deserialize)]
pub struct RoomDesignXml {
    #[serde(rename = "@RoomDesignId")]
    pub room_design_id: i32,
    #[serde(rename = "@RoomName")]
    pub room_name: String,
    #[serde(rename = "@RoomType")]
    pub room_type: String,
    #[serde(rename = "@ReloadTime")]
    pub reload_time: i32,
    #[serde(rename = "@MissileDesignId")]
    pub missile_design_id: i32,
    /// For `Shield`-type rooms, this room's contribution to the ship's total shield capacity
    /// (summed across all active Shield rooms — see `pss-stats::ship_shield_capacity`). The
    /// battleReport never transmits a max-shield value directly, so this is the only source for
    /// it. Meaningless for most other room types (still present in the XML, just not useful).
    #[serde(rename = "@Capacity")]
    pub capacity: i32,
    /// Present only for weapon rooms with a built-in weapon (Laser, ...).
    #[serde(rename = "MissileDesign", default)]
    pub missile_design: Option<MissileDesignXml>,
}

/// The nested `<MissileDesign>` element carrying the base damage values that the battleReport
/// itself never transmits (see pss-model docs).
#[derive(Debug, Clone, Deserialize)]
pub struct MissileDesignXml {
    #[serde(rename = "@MissileDesignId")]
    pub missile_design_id: i32,
    #[serde(rename = "@HullDamage")]
    pub hull_damage: f64,
    #[serde(rename = "@SystemDamage")]
    pub system_damage: f64,
    #[serde(rename = "@CharacterDamage")]
    pub character_damage: f64,
    #[serde(rename = "@ShieldDamage")]
    pub shield_damage: f64,
}

#[derive(Debug, Deserialize)]
struct RoomDesignsWrapper {
    #[serde(rename = "RoomDesign", default)]
    room_design: Vec<RoomDesignXml>,
}

#[derive(Debug, Deserialize)]
struct ListRoomDesigns {
    #[serde(rename = "@version")]
    version: i32,
    #[serde(rename = "RoomDesigns")]
    pub room_designs: RoomDesignsWrapper,
}

#[derive(Debug, Deserialize)]
struct RoomServiceEnvelope {
    #[serde(rename = "ListRoomDesigns")]
    list_room_designs: ListRoomDesigns,
}

/// Parses a `RoomService/ListRoomDesigns2` XML response body into its version number and room
/// designs. Pure/offline - no network access.
pub fn parse_room_designs_xml(xml: &str) -> Result<(i32, Vec<RoomDesignXml>)> {
    let envelope: RoomServiceEnvelope =
        quick_xml::de::from_str(xml).context("Failed to parse RomService/ListRoomDesigns2")?;

    Ok((
        envelope.list_room_designs.version,
        envelope.list_room_designs.room_designs.room_design,
    ))
}

/// A cached room design row, flattened from `RoomDesignXml` + `MissileDesignXml` for easy
/// consumption by pss-stats.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomDesignRecord {
    pub room_design_id: i32,
    pub room_name: String,
    pub room_type: String,
    pub reload_time: i32,
    pub missile_design_id: i32,
    pub capacity: i32,
    pub hull_damage: Option<f64>,
    pub system_damage: Option<f64>,
    pub character_damage: Option<f64>,
    pub shield_damage: Option<f64>,
}

// ----------------------------------------------------------------
//ItemDesign

/// A single `<ItemDesign>` entry. Covers three uses at once: naming loot and consumed resources
/// (`item_type`), interpreting equiped modules/gear (`enhancement_type`/`value`, `module_type`/`argument`), and identifying which weapon design an ammo item fires (`missile_design_id`/`craft_design_id`).
#[derive(Debug, Clone, Deserialize)]
pub struct ItemDesignXml {
    #[serde(rename = "@ItemDesignId")]
    pub item_design_id: i32,
    #[serde(rename = "@ItemDesignName")]
    pub item_design_name: String,
    #[serde(rename = "@ItemType")]
    pub item_type: String,
    #[serde(rename = "@ItemSubType")]
    pub item_sub_type: String,
    #[serde(rename = "@EnhancementType")]
    pub enhancement_type: String,
    #[serde(rename = "@EnhancementValue")]
    pub enhancement_value: f64,
    /// Real module behavior for `ItemSubType="Module"` items (e.g. "Trap"); `"None"` otherwise.
    #[serde(rename = "@ModuleType")]
    pub module_type: String,
    #[serde(rename = "@ModuleArgument")]
    pub module_argument: f64,
    /// Non-zero for `ItemType="Missile"` ammo - resolves directly against
    /// `DesignsCache::get_missile_design` (see module docs).
    #[serde(rename = "@MissileDesignId")]
    pub missile_design_id: i32,
    /// Non-zero for `ItemType="Craft"` drones - resolves via
    /// `DesignsCache::get_craft_design_missile_id` then `get_missile_design`.
    #[serde(rename = "@CraftDesignId")]
    pub craft_design_id: i32,
    /// Same scale as `CharacterDesignXml::rarity` (Common ... Legendary).
    #[serde(rename = "@Rarity")]
    pub rarity: String,
}

#[derive(Debug, Deserialize)]
struct ItemDesignsWrapper {
    #[serde(rename = "ItemDesign", default)]
    item_design: Vec<ItemDesignXml>,
}

#[derive(Debug, Deserialize)]
struct ListItemDesigns {
    #[serde(rename = "@version")]
    version: i32,
    #[serde(rename = "ItemDesigns")]
    item_designs: ItemDesignsWrapper,
}

#[derive(Debug, Deserialize)]
struct ItemServiceEnvelope {
    #[serde(rename = "ListItemDesigns")]
    list_item_designs: ListItemDesigns,
}

/// Parses an `ItemService/ListItemDesigns2` XML response body. Pure/offline.
pub fn parse_item_designs_xml(xml: &str) -> Result<(i32, Vec<ItemDesignXml>)> {
    let envelope: ItemServiceEnvelope =
        quick_xml::de::from_str(xml).context("Failed to parse ItemService/ListItemDesigns2")?;

    Ok((
        envelope.list_item_designs.version,
        envelope.list_item_designs.item_designs.item_design,
    ))
}

// ----------------------------------------------------------------
// CharacterDesign

/// A single `<CharacterDesign>` entry: crew naming, base/final stats (a crew member's live stats
/// fall somewhere between the two depending on level, which the battleReport doesn't carry), and
/// special ability details. Ignores the nested `<CharacterParts>` element (cosmetics only).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CharacterDesignXml {
    #[serde(rename = "@CharacterDesignId")]
    pub character_design_id: i32,
    #[serde(rename = "@CharacterDesignName")]
    pub character_design_name: String,
    #[serde(rename = "@Hp")]
    pub hp: f64,
    #[serde(rename = "@FinalHp")]
    pub final_hp: f64,
    #[serde(rename = "@Pilot")]
    pub pilot: f64,
    #[serde(rename = "@FinalPilot")]
    pub final_pilot: f64,
    #[serde(rename = "@Repair")]
    pub repair: f64,
    #[serde(rename = "@FinalRepair")]
    pub final_repair: f64,
    #[serde(rename = "@Weapon")]
    pub weapon: f64,
    #[serde(rename = "@FinalWeapon")]
    pub final_weapon: f64,
    #[serde(rename = "@Science")]
    pub science: f64,
    #[serde(rename = "@FinalScience")]
    pub final_science: f64,
    #[serde(rename = "@Engine")]
    pub engine: f64,
    #[serde(rename = "@FinalEngine")]
    pub final_engine: f64,
    #[serde(rename = "@Research")]
    pub research: f64,
    #[serde(rename = "@FinalResearch")]
    pub final_research: f64,
    #[serde(rename = "@Attack")]
    pub attack: f64,
    #[serde(rename = "@FinalAttack")]
    pub final_attack: f64,
    /// Matches the name in `_enums.SpecialAbilityType` (see pss-model)
    #[serde(rename = "@SpecialAbilityType")]
    pub special_ability_type: String,
    #[serde(rename = "@SpecialAbilityArgument")]
    pub special_ability_argument: f64,
    #[serde(rename = "@SpecialAbilityFinalArgument")]
    pub special_ability_final_argument: f64,
    #[serde(rename = "@Rarity")]
    pub rarity: String,
    /// `Linear`/`EaseIn`/`EaseOut` - curve used to interpolate every stat between its level-1
    /// value and its `Final*` value (see `pss-stats::stat_at_level`).
    #[serde(rename = "@ProgressionType")]
    pub progression_type: String,
    #[serde(rename = "@MaxCharacterLevel")]
    pub max_character_level: i32,
    /// Total training points this character can spend across all stats.
    #[serde(rename = "@TrainingCapacity")]
    pub training_capacity: i32,
    /// Bitmask of equipable slots: bit 0 Head, 1 Body, 2 Leg, 3 Weapon, 4 Accessory, 5 Pet
    /// (PixyShip's `parse_equipment_slots`; e.g. Neptune Mariner's 12 = Leg + Weapon).
    #[serde(rename = "@EquipmentMask")]
    pub equipment_mask: i32,
}

#[derive(Debug, Deserialize)]
struct CharacterDesignWrapper {
    #[serde(rename = "CharacterDesign", default)]
    character_design: Vec<CharacterDesignXml>,
}

#[derive(Debug, Deserialize)]
struct ListAllCharacterDesigns {
    #[serde(rename = "@version")]
    version: i32,
    #[serde(rename = "CharacterDesigns")]
    character_designs: CharacterDesignWrapper,
}

#[derive(Debug, Deserialize)]
struct CharacterServiceEnvelope {
    #[serde(rename = "ListAllCharacterDesigns")]
    list_all_character_designs: ListAllCharacterDesigns,
}

/// Parses a `CharacterService/ListAllCharacterDesigns2` XML response body. Pure/offline.
pub fn parse_character_designs_xml(xml: &str) -> Result<(i32, Vec<CharacterDesignXml>)> {
    let envelope: CharacterServiceEnvelope = quick_xml::de::from_str(xml)
        .context("Failed to parse CharacterService/ListAllCharacterDesigns2")?;

    Ok((
        envelope.list_all_character_designs.version,
        envelope
            .list_all_character_designs
            .character_designs
            .character_design,
    ))
}

const ITEM_DESIGN_COLUMNS: &str = "item_design_id, item_design_name, item_type, item_sub_type,
    enhancement_type, enhancement_value, module_type, module_argument,
    missile_design_id, craft_design_id, rarity";

fn item_design_from_row(row: &rusqlite::Row) -> rusqlite::Result<ItemDesignXml> {
    Ok(ItemDesignXml {
        item_design_id: row.get(0)?,
        item_design_name: row.get(1)?,
        item_type: row.get(2)?,
        item_sub_type: row.get(3)?,
        enhancement_type: row.get(4)?,
        enhancement_value: row.get(5)?,
        module_type: row.get(6)?,
        module_argument: row.get(7)?,
        missile_design_id: row.get(8)?,
        craft_design_id: row.get(9)?,
        rarity: row.get(10)?,
    })
}

/// Column order shared by every `character_designs` SELECT/INSERT - must match
/// `character_design_from_row`'s indices.
const CHARACTER_DESIGN_COLUMNS: &str = "character_design_id, character_design_name, hp, final_hp,
    pilot, final_pilot, repair, final_repair, weapon, final_weapon,
    science, final_science, engine, final_engine, research, final_research,
    attack, final_attack, special_ability_type, special_ability_argument,
    special_ability_final_argument, rarity, progression_type, max_character_level,
    training_capacity, equipment_mask";

fn character_design_from_row(row: &rusqlite::Row) -> rusqlite::Result<CharacterDesignXml> {
    Ok(CharacterDesignXml {
        character_design_id: row.get(0)?,
        character_design_name: row.get(1)?,
        hp: row.get(2)?,
        final_hp: row.get(3)?,
        pilot: row.get(4)?,
        final_pilot: row.get(5)?,
        repair: row.get(6)?,
        final_repair: row.get(7)?,
        weapon: row.get(8)?,
        final_weapon: row.get(9)?,
        science: row.get(10)?,
        final_science: row.get(11)?,
        engine: row.get(12)?,
        final_engine: row.get(13)?,
        research: row.get(14)?,
        final_research: row.get(15)?,
        attack: row.get(16)?,
        final_attack: row.get(17)?,
        special_ability_type: row.get(18)?,
        special_ability_argument: row.get(19)?,
        special_ability_final_argument: row.get(20)?,
        rarity: row.get(21)?,
        progression_type: row.get(22)?,
        max_character_level: row.get(23)?,
        training_capacity: row.get(24)?,
        equipment_mask: row.get(25)?,
    })
}

// ----------------------------------------------------------------
// MissileDesign (standalone - see module docs on why this is a separate endpoint from the
// <MissileDesign> nested inside RoomDesign)

/// A single `<MissileDesign>` entry from `RoomService/ListMissileDesigns` - real per-shot damage
/// for Missile/Craft ammo, keyed by the same `MissileDesignId` that `ItemDesign`/`CraftDesign`
/// reference. The live response also carries `StunLength`, `EMPLength`, `Volley`... - real data,
/// just not wired into anything yet.
///
/// `shield_damage` is NOT real per-shot shield damage for `ItemType::Missile` ammo (it IS real for
/// the room-nested `MissileDesignXml` used by Laser rooms - a different, smaller struct, see
/// below - this only concerns the standalone endpoint's Missile/Craft ammo). Directly disproven by
/// the player-facing in-game weapon stats panel for a real Missile (Scarlet): it lists exactly
/// System Dmg, AP Dmg, Crew Dmg, Craft Dmg (and shows `0.00` for System/Crew, proving zero-valued
/// stats aren't just hidden) - there is no Shield Dmg row at all, despite this record's
/// `ShieldDamage="5"` being nonzero. Matches the wiki's "Like all Missiles [...] bypasses shields"
/// - not just a defense-evasion footnote as this project first assumed, but evidence the field
/// itself is vestigial for this weapon category. `pss-stats` zeroes `shield` for `ItemType::Missile`
/// consumption specifically (not `Craft` - no equivalent counter-evidence for drones, so left
/// alone) rather than dropping the column here, since the raw value is still worth caching/
/// inspecting even though it isn't applied as damage.
///
/// `craft_damage` is the real stat shown in that same panel as "Craft Dmg" (verified: Scarlet
/// Level 4's `CraftDamage="1"` matches the panel's "Craft Dmg 1.00" exactly, alongside
/// `DirectSystemDamage="0.1"`/"AP Dmg 0.10" and `SystemDamage="0"`/"System Dmg 0.00") - damage
/// dealt to enemy Craft/drones, a target category distinct from hull/system/character/shield.
///
/// `direct_system_damage` and `explosion_*` were verified against the Pixel Starships Fandom wiki
/// and cross-checked against real weapon names in a live response:
/// - `DirectSystemDamage` is "AP Damage" (wiki: "a special form of System Damage" that ignores
///   Armor, including innate room armor, and never converts to hull damage on a destroyed room -
///   genuinely a different mechanic from `SystemDamage`, not an alias). Confirmed non-zero
///   exclusively on Javelin/Railgun-family designs, matching the wiki's own "AP Damage Weaponry"
///   list exactly.
/// - `ExplosionType`/`ExplosionRadius` are the AOE mechanic (6 real values seen: Single, Radius,
///   RandomUnique, All, RandomOnce, Random). Confirmed against a real patch note: "Irradiator Room
///   now has ... a Random AOE of 200" matches `ExplosionType="Random" ExplosionRadius="200"`
///   exactly on the Irradiator Beam design. Stored for reference only, NOT factored into any damage
///   total - how a `damage_hp` observation splits across `ExplosionRadius`-hit targets isn't
///   verified, so multiplying by it would be a fabricated number, not an estimate.
/// - `FireLength` (frames) drives a wholly separate damage-over-time mechanic ("Fire Damage" on
///   the wiki) on top of whatever instant hull/system/character/shield/AP this design also deals -
///   confirmed on real weapons that clearly do both (e.g. "Big Flying Rocket Attack" has
///   `HullDamage=7 CharacterDamage=60` AND `FireLength=200`). Ticks once per Fire Frame (40 frames
///   = 1s at this project's 40fps), duration decreasing by one Fire Frame each tick, using the
///   CURRENT remaining duration (not the initial one) in per-tick formulas that hit ONLY AP and
///   Crew damage - explicitly never shield ("Like all Missiles, [...] bypasses shields" is a
///   separate, unrelated defense-evasion mechanic per the wiki, not evidence this field is unused)
///   and, per the wiki, never stacking additively on repeat hits to the same room (a longer fire
///   just overrides a shorter one already burning there). See `pss-stats::fire_damage_over_burn`
///   for the derived closed-form sum used here, and its doc comment for why it's necessarily an
///   unmitigated upper bound (target Fire Resistance/Sprinkler/Repair stats aren't observable from
///   this project's data) and why it's added on top of - not substituted for - the instant fields.
///
/// `HullPercentageDamage` (the wiki's "Direct Hull Damage", percentage variant e.g. Cruise
/// Missile - the flat variant, Penetrator/EMP Cannon, is already `hull_damage` itself, see
/// CLAUDE.md's Laser-HullDamage=0 note) is deliberately NOT added yet: whether it's a flat
/// per-shot amount (like every other field here) or a true percentage of the target's max hull HP
/// requires knowing which computation the game applies, which isn't verified.
#[derive(Debug, Clone, Deserialize)]
pub struct MissileDesignFullXml {
    #[serde(rename = "@MissileDesignId")]
    pub missile_design_id: i32,
    #[serde(rename = "@MissileDesignName")]
    pub missile_design_name: String,
    #[serde(rename = "@HullDamage")]
    pub hull_damage: f64,
    #[serde(rename = "@SystemDamage")]
    pub system_damage: f64,
    #[serde(rename = "@CharacterDamage")]
    pub character_damage: f64,
    #[serde(rename = "@ShieldDamage")]
    pub shield_damage: f64,
    #[serde(rename = "@DirectSystemDamage")]
    pub ap_damage: f64,
    #[serde(rename = "@ExplosionType")]
    pub explosion_type: String,
    #[serde(rename = "@ExplosionRadius")]
    pub explosion_radius: f64,
    #[serde(rename = "@FireLength")]
    pub fire_length: f64,
    #[serde(rename = "@CraftDamage")]
    pub craft_damage: f64,
    /// Projectiles per launch (min 1 in the live data, up to 13 on missiles - "Rocket Barrage" is
    /// 10). Verified on all 35 captured battles: for every ship, the number of projectiles its
    /// Missile rooms report (`RoomReport.missiles_deployed`, and the projectile inventory) equals
    /// exactly the sum of `ItemReport.quantity_deployed x Volley` over its Missile items - so
    /// `quantity_deployed` counts LAUNCHES and each damage field here is per PROJECTILE.
    #[serde(rename = "@Volley")]
    pub volley: f64,
}

#[derive(Debug, Deserialize)]
struct MissileDesignsWrapper {
    #[serde(rename = "MissileDesign", default)]
    missile_design: Vec<MissileDesignFullXml>,
}

#[derive(Debug, Deserialize)]
struct ListMissileDesigns {
    #[serde(rename = "@version")]
    version: i32,
    #[serde(rename = "MissileDesigns")]
    missile_designs: MissileDesignsWrapper,
}

#[derive(Debug, Deserialize)]
struct RoomServiceMissileEnvelope {
    #[serde(rename = "ListMissileDesigns")]
    list_missile_designs: ListMissileDesigns,
}

/// Parses a `RoomService/ListMissileDesigns` XML response body. Pure/offline.
pub fn parse_missile_designs_xml(xml: &str) -> Result<(i32, Vec<MissileDesignFullXml>)> {
    let envelope: RoomServiceMissileEnvelope =
        quick_xml::de::from_str(xml).context("Failed to parse RoomService/ListMissileDesigns")?;

    Ok((
        envelope.list_missile_designs.version,
        envelope.list_missile_designs.missile_designs.missile_design,
    ))
}

/// A cached row from `missile_designs` - the real, exact per-shot damage for a Missile/Craft
/// weapon. Returned by `DesignsCache::get_missile_design`.
#[derive(Debug, Clone, PartialEq)]
pub struct MissileDamageRecord {
    pub missile_design_id: i32,
    pub missile_design_name: String,
    pub hull_damage: f64,
    pub system_damage: f64,
    pub character_damage: f64,
    pub shield_damage: f64,
    pub ap_damage: f64,
    pub explosion_type: String,
    pub explosion_radius: f64,
    pub fire_length: f64,
    pub craft_damage: f64,
    pub volley: f64,
}

// ----------------------------------------------------------------
// CraftDesign (standalone - just enough to resolve a Craft item's CraftDesignId to the
// MissileDesignId that carries its actual damage stats, see MissileDesign above)

/// A single `<CraftDesign>` entry from `RoomService/ListCraftDesigns`. The live response carries
/// plenty more (Hp, FlightSpeed, Reload, CraftAttackType...) but nothing here needs it yet - only
/// the `MissileDesignId` resolution matters for damage estimation.
#[derive(Debug, Clone, Deserialize)]
pub struct CraftDesignXml {
    #[serde(rename = "@CraftDesignId")]
    pub craft_design_id: i32,
    #[serde(rename = "@MissileDesignId")]
    pub missile_design_id: i32,
}

#[derive(Debug, Deserialize)]
struct CraftDesignsWrapper {
    #[serde(rename = "CraftDesign", default)]
    craft_design: Vec<CraftDesignXml>,
}

#[derive(Debug, Deserialize)]
struct ListCraftDesigns {
    #[serde(rename = "@version")]
    version: i32,
    #[serde(rename = "CraftDesigns")]
    craft_designs: CraftDesignsWrapper,
}

#[derive(Debug, Deserialize)]
struct RoomServiceCraftEnvelope {
    #[serde(rename = "ListCraftDesigns")]
    list_craft_designs: ListCraftDesigns,
}

/// Parses a `RoomService/ListCraftDesigns` XML response body. Pure/offline.
pub fn parse_craft_designs_xml(xml: &str) -> Result<(i32, Vec<CraftDesignXml>)> {
    let envelope: RoomServiceCraftEnvelope =
        quick_xml::de::from_str(xml).context("Failed to parse RoomService/ListCraftDesigns")?;

    Ok((
        envelope.list_craft_designs.version,
        envelope.list_craft_designs.craft_designs.craft_design,
    ))
}

// ----------------------------------------------------------------
// Calibrated damage

/// Empirically-derived per-shot/per-attack damage for a Missile/Craft ammo item, derived from
/// mono-weapon combat analysis. Was the only source for these stats before the `MissileDesign`/
/// `CraftDesign` endpoints above were found; now only used as a fallback when an item's design
/// isn't in either cache (not yet synced, or a genuinely unresolvable id - still possible for
/// Android, which this project has no equivalent lookup for at all, see module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct CalibratedItemDamageRecord {
    pub item_design_id: i32,
    pub hull_damage: f64,
    pub system_damage: f64,
    pub character_damage: f64,
    pub shield_damage: f64,
    /// How many shots the calibration that produced this record was based on - more shots means a
    /// more reliable average.
    pub calibrated_from_shots: i32,
}

/// Row counts currently cached for each design domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DesignsCounts {
    pub room_designs: i64,
    pub item_designs: i64,
    pub character_designs: i64,
    pub missile_designs: i64,
    pub craft_designs: i64,
}

// ----------------------------------------------------------------
// SyncOutcome

/// Result of a `sync_*` call.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SyncOutcome {
    /// The cache already matched the upstream version; nothing was fetched beyond the
    /// version-carrying payload itself.
    UpToDate { version: i32 },
    /// The cache was refreshed from a newer upstream version.
    Updated { version: i32, room_count: usize },
}

/// SQLite-backed cache of PSS designs.
pub struct DesignsCache {
    conn: Connection,
}

impl DesignsCache {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open designs cache database")?;

        Self::init_schema(&conn)?;
        patch_notes::init_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().context("Failed to open in-memory designs cache")?;

        Self::init_schema(&conn)?;
        patch_notes::init_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn upsert_patch_notes(&self, versions: &[patch_notes::PatchNoteVersion]) -> Result<()> {
        patch_notes::upsert_patch_notes(&self.conn, versions)
    }

    /// Most recent first.
    pub fn list_patch_notes(&self) -> Result<Vec<patch_notes::PatchNoteVersion>> {
        patch_notes::list_patch_notes(&self.conn)
    }

    /// `None` if the cache has never been populated.
    pub fn patch_notes_last_fetched_at(&self) -> Result<Option<String>> {
        patch_notes::last_fetched_at(&self.conn)
    }

    pub fn get_room_design(&self, room_design_id: i32) -> Result<Option<RoomDesignRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT room_design_id, room_name, room_type, reload_time, missile_design_id, capacity, hull_damage, system_damage, character_damage, shield_damage FROM room_designs WHERE room_design_id = ?1"
        )?;
        let record = stmt
            .query_row(params![room_design_id], |row| {
                Ok(RoomDesignRecord {
                    room_design_id: row.get(0)?,
                    room_name: row.get(1)?,
                    room_type: row.get(2)?,
                    reload_time: row.get(3)?,
                    missile_design_id: row.get(4)?,
                    capacity: row.get(5)?,
                    hull_damage: row.get(6)?,
                    system_damage: row.get(7)?,
                    character_damage: row.get(8)?,
                    shield_damage: row.get(9)?,
                })
            })
            .ok();

        Ok(record)
    }

    pub fn set_room_design_version(&self, version: i32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO designs_meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ROOM_DESIGN_VERSION_KEY, version.to_string()]
        )?;

        Ok(())
    }

    pub fn upsert_room_designs(&mut self, designs: &[RoomDesignXml]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM room_designs", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO room_designs (room_design_id, room_name, room_type, reload_time, missile_design_id, capacity, hull_damage, system_damage, character_damage, shield_damage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
            )?;

            for design in designs {
                let (hull, system, character, shield) = design
                    .missile_design
                    .as_ref()
                    .map(|m| {
                        (
                            Some(m.hull_damage),
                            Some(m.system_damage),
                            Some(m.character_damage),
                            Some(m.shield_damage),
                        )
                    })
                    .unwrap_or((None, None, None, None));

                stmt.execute(params![
                    design.room_design_id,
                    design.room_name,
                    design.room_type,
                    design.reload_time,
                    design.missile_design_id,
                    design.capacity,
                    hull,
                    system,
                    character,
                    shield
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
                CREATE TABLE IF NOT EXISTS designs_meta (
                    key     TEXT PRIMARY KEY,
                    value   TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS room_designs (
                    room_design_id      INTEGER PRIMARY KEY,
                    room_name           TEXT NOT NULL,
                    room_type           TEXT NOT NULL,
                    reload_time         INTEGER NOT NULL,
                    missile_design_id   INTEGER NOT NULL,
                    capacity            INTEGER NOT NULL DEFAULT 0,
                    hull_damage         REAL,
                    system_damage       REAL,
                    character_damage    REAL,
                    shield_damage       REAL
                );
                CREATE TABLE IF NOT EXISTS item_designs (
                    item_design_id      INTEGER PRIMARY KEY,
                    item_design_name    TEXT NOT NULL,
                    item_type           TEXT NOT NULL,
                    item_sub_type       TEXT NOT NULL,
                    enhancement_type    TEXT NOT NULL,
                    enhancement_value   REAL NOT NULL,
                    module_type         TEXT NOT NULL,
                    module_argument     REAL NOT NULL,
                    missile_design_id   INTEGER NOT NULL,
                    craft_design_id     INTEGER NOT NULL,
                    rarity              TEXT NOT NULL DEFAULT ''
                );
                CREATE TABLE IF NOT EXISTS character_designs (
                    character_design_id             INTEGER PRIMARY KEY,
                    character_design_name           TEXT NOT NULL,
                    hp                              REAL NOT NULL,
                    final_hp                        REAL NOT NULL,
                    pilot                           REAL NOT NULL,
                    final_pilot                     REAL NOT NULL,
                    repair                          REAL NOT NULL,
                    final_repair                    REAL NOT NULL,
                    weapon                          REAL NOT NULL,
                    final_weapon                    REAL NOT NULL,
                    science                         REAL NOT NULL,
                    final_science                   REAL NOT NULL,
                    engine                          REAL NOT NULL,
                    final_engine                    REAL NOT NULL,
                    research                        REAL NOT NULL,
                    final_research                  REAL NOT NULL,
                    attack                          REAL NOT NULL,
                    final_attack                    REAL NOT NULL,
                    special_ability_type            TEXT NOT NULL,
                    special_ability_argument        REAL NOT NULL,
                    special_ability_final_argument  REAL NOT NULL,
                    rarity                          TEXT NOT NULL DEFAULT '',
                    progression_type                TEXT NOT NULL DEFAULT '',
                    max_character_level             INTEGER NOT NULL DEFAULT 40,
                    training_capacity               INTEGER NOT NULL DEFAULT 0,
                    equipment_mask                  INTEGER NOT NULL DEFAULT -1
                );
                CREATE TABLE IF NOT EXISTS calibrated_item_damage (
                    item_design_id          INTEGER PRIMARY KEY,
                    hull_damage             REAL NOT NULL,
                    system_damage           REAL NOT NULL,
                    character_damage        REAL NOT NULL,
                    shield_damage           REAL NOT NULL DEFAULT 0.0,
                    calibrated_from_shots   INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS missile_designs (
                    missile_design_id      INTEGER PRIMARY KEY,
                    missile_design_name    TEXT NOT NULL,
                    hull_damage            REAL NOT NULL,
                    system_damage          REAL NOT NULL,
                    character_damage       REAL NOT NULL,
                    shield_damage          REAL NOT NULL,
                    ap_damage              REAL NOT NULL DEFAULT 0.0,
                    explosion_type         TEXT NOT NULL DEFAULT '',
                    explosion_radius       REAL NOT NULL DEFAULT 0.0,
                    fire_length            REAL NOT NULL DEFAULT 0.0,
                    craft_damage           REAL NOT NULL DEFAULT 0.0,
                    volley                 REAL NOT NULL DEFAULT 0.0
                );
                CREATE TABLE IF NOT EXISTS craft_designs (
                    craft_design_id     INTEGER PRIMARY KEY,
                    missile_design_id   INTEGER NOT NULL
                );
                ",
        )?;

        // Same idea as room_designs.capacity/shield_damage above, but unlike those (re-fetched
        // automatically from the API), calibration has no re-fetch to force: it only updates when
        // the user reruns calibration (`pss-cli calibrate` / the GUI's "Calibrer..." button), which
        // then overwrites the row via `upsert_calibrated_item_damage`'s ON CONFLICT. So an existing
        // cache's pre-migration rows are just left at the DEFAULT 0.0 shield_damage - an honest
        // "unknown, treated as 0" placeholder consistent with how this app already treats every
        // other not-yet-calibrated damage value (see CLAUDE.md) - until the user recalibrates.
        if let Err(err) = conn.execute(
            "ALTER TABLE calibrated_item_damage ADD COLUMN shield_damage REAL NOT NULL DEFAULT 0.0",
            [],
        ) {
            let is_duplicate_column = matches!(
                &err,
                rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
            );
            if !is_duplicate_column {
                return Err(err)
                    .context("Failed to migrate calibrated_item_damage.shield_damage column");
            }
        }

        // `capacity` was added to room_designs after this cache shipped. `CREATE TABLE IF NOT
        // EXISTS` above is a no-op against a database file created before that, so a cache from
        // an earlier version needs the column added explicitly or every query listing it
        // (get_room_design, upsert_room_designs) fails with "no such column: capacity" - this is
        // exactly what happened opening this repo's own pre-existing pss-designs-cache.db.
        // SQLite has no "ADD COLUMN IF NOT EXISTS", so the expected failure (column already
        // present, e.g. a fresh database that just created it above) is swallowed; anything else
        // is a real problem and still surfaces.
        if let Err(err) = conn.execute(
            "ALTER TABLE room_designs ADD COLUMN capacity INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            let is_duplicate_column = matches!(
                &err,
                rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
            );
            if !is_duplicate_column {
                return Err(err).context("Failed to migrate room_designs.capacity column");
            }
        }

        // Same situation as `capacity` above: `shield_damage` was added to the nested
        // MissileDesign parsing after this cache shipped, so an existing database file needs the
        // column added explicitly.
        if let Err(err) = conn.execute("ALTER TABLE room_designs ADD COLUMN shield_damage REAL", [])
        {
            let is_duplicate_column = matches!(
                &err,
                rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
            );
            if !is_duplicate_column {
                return Err(err).context("Failed to migrate room_designs.shield_damage column");
            }
        }

        // The ALTER above leaves every pre-existing row's shield_damage NULL - only a real
        // re-fetch from the API populates the true value. But `sync_room_designs` only re-fetches
        // when the live version differs from the stored one, so on its own this cache would keep
        // reporting no shield_damage forever (the stored version still matches, nothing looks
        // "changed"). Detect that placeholder state - every cached row with a real hull_damage
        // (i.e. an actual weapon room, not a room without a MissileDesign at all) still missing
        // shield_damage despite some existing - and force the next sync by forgetting the stored
        // version. Same logic covers the analogous `capacity` staleness this migration already
        // had to detect (every Shield room still at the placeholder 0 default).
        let stale_shield_damage: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM room_designs WHERE hull_damage IS NOT NULL)
                AND NOT EXISTS (SELECT 1 FROM room_designs WHERE hull_damage IS NOT NULL AND shield_damage IS NOT NULL)",
            [],
            |row| row.get(0),
        )?;
        let stale_shield_capacity: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM room_designs WHERE room_type = 'Shield')
                AND NOT EXISTS (SELECT 1 FROM room_designs WHERE room_type = 'Shield' AND capacity != 0)",
            [],
            |row| row.get(0),
        )?;
        if stale_shield_capacity || stale_shield_damage {
            conn.execute(
                "DELETE FROM designs_meta WHERE key = ?1",
                params![ROOM_DESIGN_VERSION_KEY],
            )?;
        }

        // `ap_damage`/`explosion_type`/`explosion_radius` were added to `missile_designs` after
        // this cache shipped - same ALTER-then-detect-staleness dance as `shield_damage` above.
        // `missile_designs` is fully DELETE+re-INSERTed on every sync (not a per-row UPDATE), so
        // existing rows won't naturally pick up these columns until the next real fetch - which,
        // same problem as above, `sync_missile_designs` only does when the live version differs
        // from the stored one.
        for (column, ddl) in [
            (
                "ap_damage",
                "ALTER TABLE missile_designs ADD COLUMN ap_damage REAL NOT NULL DEFAULT 0.0",
            ),
            (
                "explosion_type",
                "ALTER TABLE missile_designs ADD COLUMN explosion_type TEXT NOT NULL DEFAULT ''",
            ),
            (
                "explosion_radius",
                "ALTER TABLE missile_designs ADD COLUMN explosion_radius REAL NOT NULL DEFAULT 0.0",
            ),
            (
                "fire_length",
                "ALTER TABLE missile_designs ADD COLUMN fire_length REAL NOT NULL DEFAULT 0.0",
            ),
            (
                "craft_damage",
                "ALTER TABLE missile_designs ADD COLUMN craft_damage REAL NOT NULL DEFAULT 0.0",
            ),
            // 0.0 = "never fetched": no real design has a Volley below 1, so it doubles as the
            // staleness marker checked below.
            (
                "volley",
                "ALTER TABLE missile_designs ADD COLUMN volley REAL NOT NULL DEFAULT 0.0",
            ),
        ] {
            if let Err(err) = conn.execute(ddl, []) {
                let is_duplicate_column = matches!(
                    &err,
                    rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
                );
                if !is_duplicate_column {
                    return Err(err)
                        .context(format!("Failed to migrate missile_designs.{column} column"));
                }
            }
        }
        // Same dance for the training/progression columns on `character_designs`, added after
        // this cache shipped: `sync_character_designs` only re-fetches on a version change, so
        // rows still carrying the '' placeholder force a resync by forgetting the stored version.
        for (column, ddl) in [
            (
                "rarity",
                "ALTER TABLE character_designs ADD COLUMN rarity TEXT NOT NULL DEFAULT ''",
            ),
            (
                "progression_type",
                "ALTER TABLE character_designs ADD COLUMN progression_type TEXT NOT NULL DEFAULT ''",
            ),
            (
                "max_character_level",
                "ALTER TABLE character_designs ADD COLUMN max_character_level INTEGER NOT NULL DEFAULT 40",
            ),
            (
                "training_capacity",
                "ALTER TABLE character_designs ADD COLUMN training_capacity INTEGER NOT NULL DEFAULT 0",
            ),
            // -1, not 0: a mask of 0 is a legitimate "no slots" value, so it can't double as the
            // "never fetched" placeholder the staleness check below looks for.
            (
                "equipment_mask",
                "ALTER TABLE character_designs ADD COLUMN equipment_mask INTEGER NOT NULL DEFAULT -1",
            ),
        ] {
            if let Err(err) = conn.execute(ddl, []) {
                let is_duplicate_column = matches!(
                    &err,
                    rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
                );
                if !is_duplicate_column {
                    return Err(err).context(format!(
                        "Failed to migrate character_designs.{column} column"
                    ));
                }
            }
        }
        // Same for item_designs.rarity: '' marks a row synced before the column existed.
        if let Err(err) = conn.execute(
            "ALTER TABLE item_designs ADD COLUMN rarity TEXT NOT NULL DEFAULT ''",
            [],
        ) {
            let is_duplicate_column = matches!(
                &err,
                rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
            );
            if !is_duplicate_column {
                return Err(err).context("Failed to migrate item_designs.rarity column");
            }
        }
        let stale_item_designs: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM item_designs WHERE rarity = '')",
            [],
            |row| row.get(0),
        )?;
        if stale_item_designs {
            conn.execute(
                "DELETE FROM designs_meta WHERE key = ?1",
                params![ITEM_DESIGN_VERSION_KEY],
            )?;
        }

        let stale_character_designs: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM character_designs
                WHERE progression_type = '' OR equipment_mask = -1)",
            [],
            |row| row.get(0),
        )?;
        if stale_character_designs {
            conn.execute(
                "DELETE FROM designs_meta WHERE key = ?1",
                params![CHARACTER_DESIGN_VERSION_KEY],
            )?;
        }

        let stale_missile_designs: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM missile_designs)
                AND EXISTS (SELECT 1 FROM missile_designs WHERE explosion_type = '' OR volley = 0)",
            [],
            |row| row.get(0),
        )?;
        if stale_missile_designs {
            conn.execute(
                "DELETE FROM designs_meta WHERE key = ?1",
                params![MISSILE_DESIGN_VERSION_KEY],
            )?;
        }

        Ok(())
    }

    fn cached_room_design_version(&self) -> Result<Option<i32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM designs_meta WHERE key = ?1")?;
        let value: Option<String> = stmt
            .query_row(params![ROOM_DESIGN_VERSION_KEY], |row| row.get(0))
            .ok();

        Ok(value.and_then(|v| v.parse().ok()))
    }

    fn cached_item_design_version(&self) -> Result<Option<i32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM designs_meta WHERE key = ?1")?;
        let value: Option<String> = stmt
            .query_row(params![ITEM_DESIGN_VERSION_KEY], |row| row.get(0))
            .ok();

        Ok(value.and_then(|v| v.parse().ok()))
    }

    pub fn set_item_design_version(&self, version: i32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO designs_meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ITEM_DESIGN_VERSION_KEY, version.to_string()],
        )?;
        Ok(())
    }

    fn cached_character_design_version(&self) -> Result<Option<i32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM designs_meta WHERE key = ?1")?;
        let value: Option<String> = stmt
            .query_row(params![CHARACTER_DESIGN_VERSION_KEY], |row| row.get(0))
            .ok();

        Ok(value.and_then(|v| v.parse().ok()))
    }

    pub fn set_character_design_version(&self, version: i32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO designs_meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![CHARACTER_DESIGN_VERSION_KEY, version.to_string()],
        )?;
        Ok(())
    }

    pub fn get_item_design(&self, item_design_id: i32) -> Result<Option<ItemDesignXml>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_DESIGN_COLUMNS} FROM item_designs WHERE item_design_id = ?1"
        ))?;
        let record = stmt
            .query_row(params![item_design_id], item_design_from_row)
            .ok();
        Ok(record)
    }

    /// Crew equipment (`ItemType="Equipment"`, `ItemSubType="Equipment{Slot}"`), sorted by name.
    /// Excludes the other `Equipment`-typed items (skins, stickers, modules...), which never go
    /// in a crew slot.
    pub fn list_equipment_designs(&self) -> Result<Vec<ItemDesignXml>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_DESIGN_COLUMNS} FROM item_designs
             WHERE item_type = 'Equipment' AND item_sub_type LIKE 'Equipment%'
             ORDER BY item_design_name"
        ))?;
        let rows = stmt.query_map([], item_design_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_missile_design(
        &self,
        missile_design_id: i32,
    ) -> Result<Option<MissileDamageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT missile_design_id, missile_design_name, hull_damage, system_damage, character_damage, shield_damage, ap_damage, explosion_type, explosion_radius, fire_length, craft_damage, volley
             FROM missile_designs WHERE missile_design_id = ?1",
        )?;
        let record = stmt
            .query_row(params![missile_design_id], |row| {
                Ok(MissileDamageRecord {
                    missile_design_id: row.get(0)?,
                    missile_design_name: row.get(1)?,
                    hull_damage: row.get(2)?,
                    system_damage: row.get(3)?,
                    character_damage: row.get(4)?,
                    shield_damage: row.get(5)?,
                    ap_damage: row.get(6)?,
                    explosion_type: row.get(7)?,
                    explosion_radius: row.get(8)?,
                    fire_length: row.get(9)?,
                    craft_damage: row.get(10)?,
                    volley: row.get(11)?,
                })
            })
            .ok();
        Ok(record)
    }

    pub fn upsert_missile_designs(&mut self, designs: &[MissileDesignFullXml]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM missile_designs", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO missile_designs (missile_design_id, missile_design_name, hull_damage, system_damage, character_damage, shield_damage, ap_damage, explosion_type, explosion_radius, fire_length, craft_damage, volley)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for design in designs {
                stmt.execute(params![
                    design.missile_design_id,
                    design.missile_design_name,
                    design.hull_damage,
                    design.system_damage,
                    design.character_damage,
                    design.shield_damage,
                    design.ap_damage,
                    design.explosion_type,
                    design.explosion_radius,
                    design.fire_length,
                    design.craft_damage,
                    design.volley,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_missile_design_version(&self, version: i32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO designs_meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![MISSILE_DESIGN_VERSION_KEY, version.to_string()],
        )?;
        Ok(())
    }

    fn cached_missile_design_version(&self) -> Result<Option<i32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM designs_meta WHERE key = ?1")?;
        let value: Option<String> = stmt
            .query_row(params![MISSILE_DESIGN_VERSION_KEY], |row| row.get(0))
            .ok();
        Ok(value.and_then(|v| v.parse().ok()))
    }

    pub fn get_craft_design_missile_id(&self, craft_design_id: i32) -> Result<Option<i32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT missile_design_id FROM craft_designs WHERE craft_design_id = ?1")?;
        let value = stmt
            .query_row(params![craft_design_id], |row| row.get(0))
            .ok();
        Ok(value)
    }

    pub fn upsert_craft_designs(&mut self, designs: &[CraftDesignXml]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM craft_designs", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO craft_designs (craft_design_id, missile_design_id) VALUES (?1, ?2)",
            )?;
            for design in designs {
                stmt.execute(params![design.craft_design_id, design.missile_design_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_craft_design_version(&self, version: i32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO designs_meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![CRAFT_DESIGN_VERSION_KEY, version.to_string()],
        )?;
        Ok(())
    }

    fn cached_craft_design_version(&self) -> Result<Option<i32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM designs_meta WHERE key = ?1")?;
        let value: Option<String> = stmt
            .query_row(params![CRAFT_DESIGN_VERSION_KEY], |row| row.get(0))
            .ok();
        Ok(value.and_then(|v| v.parse().ok()))
    }

    pub fn upsert_item_designs(&mut self, designs: &[ItemDesignXml]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM item_designs", [])?;
        {
            let mut stmt = tx.prepare(&format!(
                "INSERT INTO item_designs ({ITEM_DESIGN_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ))?;
            for design in designs {
                stmt.execute(params![
                    design.item_design_id,
                    design.item_design_name,
                    design.item_type,
                    design.item_sub_type,
                    design.enhancement_type,
                    design.enhancement_value,
                    design.module_type,
                    design.module_argument,
                    design.missile_design_id,
                    design.craft_design_id,
                    design.rarity,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_character_design(
        &self,
        character_design_id: i32,
    ) -> Result<Option<CharacterDesignXml>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHARACTER_DESIGN_COLUMNS} FROM character_designs WHERE character_design_id = ?1"
        ))?;
        let record = stmt
            .query_row(params![character_design_id], character_design_from_row)
            .ok();
        Ok(record)
    }

    /// Every cached character design, sorted by name.
    pub fn list_character_designs(&self) -> Result<Vec<CharacterDesignXml>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHARACTER_DESIGN_COLUMNS} FROM character_designs ORDER BY character_design_name"
        ))?;
        let rows = stmt.query_map([], character_design_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn upsert_character_designs(&mut self, designs: &[CharacterDesignXml]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM character_designs", [])?;
        {
            let mut stmt = tx.prepare(&format!(
                "INSERT INTO character_designs ({CHARACTER_DESIGN_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)"
            ))?;
            for design in designs {
                stmt.execute(params![
                    design.character_design_id,
                    design.character_design_name,
                    design.hp,
                    design.final_hp,
                    design.pilot,
                    design.final_pilot,
                    design.repair,
                    design.final_repair,
                    design.weapon,
                    design.final_weapon,
                    design.science,
                    design.final_science,
                    design.engine,
                    design.final_engine,
                    design.research,
                    design.final_research,
                    design.attack,
                    design.final_attack,
                    design.special_ability_type,
                    design.special_ability_argument,
                    design.special_ability_final_argument,
                    design.rarity,
                    design.progression_type,
                    design.max_character_level,
                    design.training_capacity,
                    design.equipment_mask,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores (or refines, if called again with more shots) a calibrated ammo damage estimate for
    /// a Missile or Craft item design.
    pub fn upsert_calibrated_item_damage(&self, record: &CalibratedItemDamageRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO calibrated_item_damage
                (item_design_id, hull_damage, system_damage, character_damage, shield_damage, calibrated_from_shots)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(item_design_id) DO UPDATE SET
                hull_damage = excluded.hull_damage,
                system_damage = excluded.system_damage,
                character_damage = excluded.character_damage,
                shield_damage = excluded.shield_damage,
                calibrated_from_shots = excluded.calibrated_from_shots",
            params![
                record.item_design_id,
                record.hull_damage,
                record.system_damage,
                record.character_damage,
                record.shield_damage,
                record.calibrated_from_shots,
            ],
        )?;
        Ok(())
    }

    pub fn get_calibrated_item_damage(
        &self,
        item_design_id: i32,
    ) -> Result<Option<CalibratedItemDamageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT item_design_id, hull_damage, system_damage, character_damage, shield_damage, calibrated_from_shots
             FROM calibrated_item_damage WHERE item_design_id = ?1",
        )?;
        let record = stmt
            .query_row(params![item_design_id], |row| {
                Ok(CalibratedItemDamageRecord {
                    item_design_id: row.get(0)?,
                    hull_damage: row.get(1)?,
                    system_damage: row.get(2)?,
                    character_damage: row.get(3)?,
                    shield_damage: row.get(4)?,
                    calibrated_from_shots: row.get(5)?,
                })
            })
            .ok();
        Ok(record)
    }

    /// Every calibrated ammo item currently on record (Missile/Craft/Android ammo whose damage was
    /// derived via mono-weapon calibration rather than read from a `MissileDesign`).
    pub fn list_calibrated_item_damage(&self) -> Result<Vec<CalibratedItemDamageRecord>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT item_design_id, hull_damage, system_damage, character_damage, shield_damage, calibrated_from_shots FROM calibrated_item_damage
            "
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CalibratedItemDamageRecord {
                item_design_id: row.get(0)?,
                hull_damage: row.get(1)?,
                system_damage: row.get(2)?,
                character_damage: row.get(3)?,
                shield_damage: row.get(4)?,
                calibrated_from_shots: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to list calibrated damage")
    }

    /// Row counts currently cached for each design domain (room, item, character).
    pub fn counts(&self) -> Result<DesignsCounts> {
        let room_designs = self
            .conn
            .query_row("SELECT COUNT(*) FROM room_designs", [], |row| row.get(0))?;
        let item_designs =
            self.conn
                .query_row("SELECT COUNT(*) FROM  item_designs", [], |row| row.get(0))?;
        let character_designs =
            self.conn
                .query_row("SELECT COUNT(*) FROM character_designs", [], |row| {
                    row.get(0)
                })?;
        let missile_designs =
            self.conn
                .query_row("SELECT COUNT(*) FROM missile_designs", [], |row| row.get(0))?;
        let craft_designs =
            self.conn
                .query_row("SELECT COUNT(*) FROM craft_designs", [], |row| row.get(0))?;

        Ok(DesignsCounts {
            room_designs,
            item_designs,
            character_designs,
            missile_designs,
            craft_designs,
        })
    }

    /// True when a domain has cached rows but no stored version: a migration added columns and
    /// forgot the version on purpose (see `init_schema`) so the rows are placeholders until the
    /// next sync. The caller should sync right away - otherwise the placeholders are what gets
    /// read (e.g. every character with `equipment_mask = -1`, `training_capacity = 0`).
    pub fn has_invalidated_domains(&self) -> Result<bool> {
        for (table, key) in [
            ("room_designs", ROOM_DESIGN_VERSION_KEY),
            ("item_designs", ITEM_DESIGN_VERSION_KEY),
            ("character_designs", CHARACTER_DESIGN_VERSION_KEY),
            ("missile_designs", MISSILE_DESIGN_VERSION_KEY),
            ("craft_designs", CRAFT_DESIGN_VERSION_KEY),
        ] {
            let invalidated: bool = self.conn.query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM {table})
                        AND NOT EXISTS (SELECT 1 FROM designs_meta WHERE key = ?1)"
                ),
                params![key],
                |row| row.get(0),
            )?;
            if invalidated {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Fetches `RoomService/ListRoomDesigns2` and refreshes the cache only if its `version` attribute
/// differs from what's stored locally.
pub async fn sync_room_designs(
    cache: &mut DesignsCache,
    http: &reqwest::Client,
) -> Result<SyncOutcome> {
    let xml = http
        .get(ROOM_DESIGNS_URL)
        .send()
        .await
        .context("Failed to reach RoomService/ListRoomDesigns2")?
        .text()
        .await
        .context("Failed to read RoomService/ListRoomDesigns2 response body")?;

    let (version, designs) = parse_room_designs_xml(&xml)?;

    if cache.cached_room_design_version()? == Some(version) {
        return Ok(SyncOutcome::UpToDate { version });
    }

    let room_count = designs.len();

    cache.upsert_room_designs(&designs)?;
    cache.set_room_design_version(version)?;
    Ok(SyncOutcome::Updated {
        version,
        room_count,
    })
}

/// Fetches `ItemService/ListItemDesigns2` and refreshes the cache only if its `version` attribute
/// differs from what's stored locally.
pub async fn sync_item_designs(
    cache: &mut DesignsCache,
    http: &reqwest::Client,
) -> Result<SyncOutcome> {
    let xml = http
        .get(ITEM_DESIGNS_URL)
        .send()
        .await
        .context("Failed to reach ItemService/ListItemDesigns2")?
        .text()
        .await
        .context("Failed to read ItemService/ListItemDesigns2 response body")?;

    let (version, designs) = parse_item_designs_xml(&xml)?;

    if cache.cached_item_design_version()? == Some(version) {
        return Ok(SyncOutcome::UpToDate { version });
    }

    let room_count = designs.len();

    cache.upsert_item_designs(&designs)?;
    cache.set_item_design_version(version)?;
    Ok(SyncOutcome::Updated {
        version,
        room_count,
    })
}

/// Fetches `CharacterService/ListAllCharacterDesigns2` and refreshes the cache only if its
/// `version` attribute differs from what's stored locally.
pub async fn sync_character_designs(
    cache: &mut DesignsCache,
    http: &reqwest::Client,
) -> Result<SyncOutcome> {
    let xml = http
        .get(CHARACTER_DESIGNS_URL)
        .send()
        .await
        .context("Failed to reach CharacterService/ListAllCharacterDesigns2")?
        .text()
        .await
        .context("Failed to read CharacterService/ListAllCharacterDesigns2 response body")?;

    let (version, designs) = parse_character_designs_xml(&xml)?;

    if cache.cached_character_design_version()? == Some(version) {
        return Ok(SyncOutcome::UpToDate { version });
    }

    let room_count = designs.len();

    cache.upsert_character_designs(&designs)?;
    cache.set_character_design_version(version)?;
    Ok(SyncOutcome::Updated {
        version,
        room_count,
    })
}

/// Fetches `RoomService/ListMissileDesigns` and refreshes the cache only if its `version`
/// attribute differs from what's stored locally. See module docs: this is the real, exact per-shot
/// damage source for Missile/Craft ammo, not the calibration fallback.
pub async fn sync_missile_designs(
    cache: &mut DesignsCache,
    http: &reqwest::Client,
) -> Result<SyncOutcome> {
    let xml = http
        .get(MISSILE_DESIGNS_URL)
        .send()
        .await
        .context("Failed to reach RoomService/ListMissileDesigns")?
        .text()
        .await
        .context("Failed to read RoomService/ListMissileDesigns response body")?;

    let (version, designs) = parse_missile_designs_xml(&xml)?;

    if cache.cached_missile_design_version()? == Some(version) {
        return Ok(SyncOutcome::UpToDate { version });
    }

    let room_count = designs.len();

    cache.upsert_missile_designs(&designs)?;
    cache.set_missile_design_version(version)?;
    Ok(SyncOutcome::Updated {
        version,
        room_count,
    })
}

/// Fetches `RoomService/ListCraftDesigns` and refreshes the cache only if its `version` attribute
/// differs from what's stored locally.
pub async fn sync_craft_designs(
    cache: &mut DesignsCache,
    http: &reqwest::Client,
) -> Result<SyncOutcome> {
    let xml = http
        .get(CRAFT_DESIGNS_URL)
        .send()
        .await
        .context("Failed to reach RoomService/ListCraftDesigns")?
        .text()
        .await
        .context("Failed to read RoomService/ListCraftDesigns response body")?;

    let (version, designs) = parse_craft_designs_xml(&xml)?;

    if cache.cached_craft_design_version()? == Some(version) {
        return Ok(SyncOutcome::UpToDate { version });
    }

    let room_count = designs.len();

    cache.upsert_craft_designs(&designs)?;
    cache.set_craft_design_version(version)?;
    Ok(SyncOutcome::Updated {
        version,
        room_count,
    })
}

/// Fetches and caches every "Galaxy Patch Notes" version currently reachable from
/// `blog.pixelstarships.com`'s RSS feed. Unlike the game-API `sync_*` functions above, there's no
/// single incrementing version to gate on - every call re-fetches and upserts (cheap: a handful of
/// small HTTP requests, not a whole design catalog), returning how many versions were found.
pub async fn sync_patch_notes(cache: &mut DesignsCache, http: &reqwest::Client) -> Result<usize> {
    let versions = patch_notes::fetch_patch_notes(http).await?;
    let count = versions.len();
    cache.upsert_patch_notes(&versions)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a real RoomService/ListRoomDesigns2 response: one support room (no weapon) and
    // one Laser room with a nested MissileDesign.
    const SAMPLE_XML: &str = r#"<RoomService><ListRoomDesigns
  version="1312"><RoomDesigns>
          <RoomDesign RoomDesignId="1" RoomName="Bridge Lv1" RoomType="Bridge"
  ReloadTime="0" MissileDesignId="0" Capacity="11" />
          <RoomDesign RoomDesignId="6" RoomName="Mineral Mining Laser Lv1"
  RoomType="Laser" ReloadTime="240" MissileDesignId="1" Capacity="0"><MissileDesign
  MissileDesignId="1" HullDamage="0" SystemDamage="1" CharacterDamage="1" ShieldDamage="0.5"
  /></RoomDesign>
      </RoomDesigns></ListRoomDesigns></RoomService>"#;

    // Trimmed from a real ItemService/ListItemDesigns2 response: one missile ammo item and one
    // equipped module.
    const SAMPLE_ITEM_XML: &str = r#"<ItemService><ListItemDesigns version="166567"><ItemDesigns>
        <ItemDesign ItemDesignId="4" ItemDesignName="Rocket" ItemType="Missile" ItemSubType="None" EnhancementType="None" EnhancementValue="0" ModuleType="None" ModuleArgument="0" MissileDesignId="2" CraftDesignId="0" Rarity="Common" />
        <ItemDesign ItemDesignId="139" ItemDesignName="Proximity Mine" ItemType="Equipment" ItemSubType="Module" EnhancementType="Attack" EnhancementValue="4" ModuleType="Trap" ModuleArgument="4" MissileDesignId="0" CraftDesignId="0" Rarity="Common" />
        <ItemDesign ItemDesignId="536" ItemDesignName="Snow Launcher" ItemType="Equipment" ItemSubType="EquipmentWeapon" EnhancementType="Weapon" EnhancementValue="6" ModuleType="None" ModuleArgument="0" MissileDesignId="0" CraftDesignId="0" Rarity="Unique" />
    </ItemDesigns></ListItemDesigns></ItemService>"#;

    // Trimmed from a real CharacterService/ListAllCharacterDesigns2 response, including the
    // nested <CharacterParts> that we ignore.
    const SAMPLE_CHARACTER_XML: &str = r#"<CharacterService><ListAllCharacterDesigns version="1356"><CharacterDesigns>
        <CharacterDesign CharacterDesignId="1" CharacterDesignName="Laura" Hp="2" FinalHp="6" Pilot="0" FinalPilot="18" Attack="1" FinalAttack="1.9" Repair="1" FinalRepair="1.7" Weapon="1" FinalWeapon="4" Science="1" FinalScience="7" Engine="1" FinalEngine="3" Research="1" FinalResearch="4" SpecialAbilityType="DeductReload" SpecialAbilityArgument="240" SpecialAbilityFinalArgument="460" Rarity="Special" ProgressionType="Linear" MaxCharacterLevel="40" TrainingCapacity="200" EquipmentMask="62"><CharacterParts><CharacterPart CharacterPartId="1" /></CharacterParts></CharacterDesign>
    </CharacterDesigns></ListAllCharacterDesigns></CharacterService>"#;

    // Trimmed from a real RoomService/ListMissileDesigns response - MissileDesignId=2 is exactly
    // what SAMPLE_ITEM_XML's "Rocket" item (MissileDesignId="2") references, and MissileDesignId=47
    // is what SAMPLE_CRAFT_XML's "Interceptor Lv1" craft references - both verified reachable here
    // against a live capture (see module docs), unlike the room-nested MissileDesign above.
    // AP/explosion values are illustrative test data, not copied from the real Rocket/Interceptor
    // designs (both of which have DirectSystemDamage=0, ExplosionType=Single in a live capture) -
    // picked to exercise both fields being parsed and threaded through, not to model these two
    // specific weapons accurately.
    const SAMPLE_MISSILE_XML: &str = r#"<RoomService><ListMissileDesigns version="796"><MissileDesigns>
        <MissileDesign MissileDesignId="2" MissileDesignName="Rocket" SystemDamage="1" HullDamage="0" CharacterDamage="1" ShieldDamage="5" DirectSystemDamage="1" ExplosionType="Single" ExplosionRadius="0" FireLength="0" CraftDamage="1" Volley="1" />
        <MissileDesign MissileDesignId="47" MissileDesignName="Interceptor Attack" SystemDamage="0.5" HullDamage="2" CharacterDamage="0.2" ShieldDamage="1" DirectSystemDamage="0" ExplosionType="RandomOnce" ExplosionRadius="150" FireLength="0" CraftDamage="0" Volley="3" />
    </MissileDesigns></ListMissileDesigns></RoomService>"#;

    // Trimmed from a real RoomService/ListCraftDesigns response.
    const SAMPLE_CRAFT_XML: &str = r#"<RoomService><ListCraftDesigns version="775"><CraftDesigns>
        <CraftDesign CraftDesignId="1" CraftName="Interceptor Lv1" MissileDesignId="47" />
    </CraftDesigns></ListCraftDesigns></RoomService>"#;

    #[test]
    fn parse_room_designs_with_and_without_missile() {
        let (version, designs) = parse_room_designs_xml(SAMPLE_XML).expect("should parse");
        assert_eq!(version, 1312);
        assert_eq!(designs.len(), 2);

        let bridge = &designs[0];
        assert_eq!(bridge.room_design_id, 1);
        assert!(bridge.missile_design.is_none());

        let laser = &designs[1];
        assert_eq!(laser.room_design_id, 6);

        let missile = laser
            .missile_design
            .as_ref()
            .expect("Laser room should have a missile design");
        assert_eq!(missile.hull_damage, 0.0);
        assert_eq!(missile.system_damage, 1.0);
        assert_eq!(missile.character_damage, 1.0);
    }

    #[test]
    fn detects_rows_left_without_a_version_by_a_migration() {
        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        assert!(!cache.has_invalidated_domains().unwrap()); // empty, never synced

        let (version, designs) = parse_character_designs_xml(SAMPLE_CHARACTER_XML).unwrap();
        cache.upsert_character_designs(&designs).unwrap();
        cache.set_character_design_version(version).unwrap();
        assert!(!cache.has_invalidated_domains().unwrap());

        // What the character_designs migration does to a pre-existing cache.
        cache
            .conn
            .execute(
                "DELETE FROM designs_meta WHERE key = ?1",
                params![CHARACTER_DESIGN_VERSION_KEY],
            )
            .unwrap();
        assert!(cache.has_invalidated_domains().unwrap());
    }

    #[test]
    fn cache_round_trip_skips_reupsert_on_same_version() {
        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        let (version, designs) = parse_room_designs_xml(SAMPLE_XML).expect("Should parse");

        assert_eq!(cache.cached_room_design_version().unwrap(), None);
        cache.upsert_room_designs(&designs).unwrap();
        cache.set_room_design_version(version).unwrap();

        let laser = cache
            .get_room_design(6)
            .unwrap()
            .expect("Laser room should be cached");
        assert_eq!(laser.room_name, "Mineral Mining Laser Lv1");
        assert_eq!(laser.system_damage, Some(1.0));
        assert_eq!(laser.character_damage, Some(1.0));

        let bridge = cache
            .get_room_design(1)
            .unwrap()
            .expect("Bridge room should be cached");
        assert_eq!(bridge.hull_damage, None);
        assert_eq!(cache.cached_room_design_version().unwrap(), Some(1312));
    }

    #[test]
    fn parses_and_caches_item_designs() {
        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        let (version, designs) = parse_item_designs_xml(SAMPLE_ITEM_XML).expect("Should parse");
        assert_eq!(version, 166567);
        assert_eq!(designs.len(), 3);

        cache.upsert_item_designs(&designs).unwrap();
        cache.set_item_design_version(version).unwrap();

        let rocket = cache
            .get_item_design(4)
            .unwrap()
            .expect("Rocket should be cached");
        assert_eq!(rocket.item_type, "Missile");
        assert_eq!(rocket.missile_design_id, 2);

        let mine = cache
            .get_item_design(139)
            .unwrap()
            .expect("Mine should be cached");
        assert_eq!(mine.module_type, "Trap");
        assert_eq!(mine.enhancement_value, 4.0);

        // Only crew-slot gear: the Module-subtyped mine is Equipment too, but never a crew slot.
        let equipment = cache.list_equipment_designs().unwrap();
        assert_eq!(equipment.len(), 1);
        assert_eq!(equipment[0].item_design_name, "Snow Launcher");
        assert_eq!(equipment[0].item_sub_type, "EquipmentWeapon");
        assert_eq!(equipment[0].rarity, "Unique");
    }

    #[test]
    fn parses_and_caches_character_designs_ignoring_nested_parts() {
        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        let (version, designs) = parse_character_designs_xml(SAMPLE_CHARACTER_XML)
            .expect("Should parse, ignoring <CharacterParts>");
        assert_eq!(version, 1356);
        assert_eq!(designs.len(), 1);

        cache.upsert_character_designs(&designs).unwrap();
        cache.set_character_design_version(version).unwrap();

        let laura = cache
            .get_character_design(1)
            .unwrap()
            .expect("Laura should be cached");
        assert_eq!(laura.character_design_name, "Laura");
        assert_eq!(laura.final_hp, 6.0);
        assert_eq!(laura.special_ability_type, "DeductReload");
        assert_eq!(laura.special_ability_final_argument, 460.0);
        assert_eq!(laura.rarity, "Special");
        assert_eq!(laura.progression_type, "Linear");
        assert_eq!(laura.max_character_level, 40);
        assert_eq!(laura.training_capacity, 200);
        assert_eq!(laura.equipment_mask, 62);

        let all = cache.list_character_designs().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], laura);
    }

    #[test]
    fn calibrated_item_damage_round_trip_and_refine() {
        let cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        assert_eq!(cache.get_calibrated_item_damage(23).unwrap(), None);

        cache
            .upsert_calibrated_item_damage(&CalibratedItemDamageRecord {
                item_design_id: 23,
                hull_damage: 10.0,
                system_damage: 2.0,
                character_damage: 1.0,
                shield_damage: 0.5,
                calibrated_from_shots: 6,
            })
            .unwrap();

        // A later, larger calibration refines the estimate in place.
        cache
            .upsert_calibrated_item_damage(&CalibratedItemDamageRecord {
                item_design_id: 23,
                hull_damage: 11.0,
                system_damage: 2.2,
                character_damage: 1.1,
                shield_damage: 0.55,
                calibrated_from_shots: 20,
            })
            .unwrap();

        let record = cache
            .get_calibrated_item_damage(23)
            .unwrap()
            .expect("Should be cached");
        assert_eq!(record.calibrated_from_shots, 20);
        assert_eq!(record.hull_damage, 11.0);
    }

    #[tokio::test]
    #[ignore = "hits the live PSS API - run explicitly with `cargo test -- --ignored`"]
    async fn syncs_against_the_real_api() {
        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        let http = reqwest::Client::new();
        let outcome = sync_room_designs(&mut cache, &http)
            .await
            .expect("Sync should succeed");

        match outcome {
            SyncOutcome::Updated {
                version,
                room_count,
            } => {
                assert!(version > 0);
                assert!(room_count > 100);
            }
            SyncOutcome::UpToDate { version } => {
                assert!(version > 0);
            }
        }

        let laser = cache
            .get_room_design(6)
            .unwrap()
            .expect("Mineral Mining Laser Lv1 should exist");

        assert_eq!(laser.room_type, "Laser");
        assert!(laser.character_damage.is_some());
    }

    #[test]
    fn parses_and_caches_missile_designs() {
        let (version, designs) =
            parse_missile_designs_xml(SAMPLE_MISSILE_XML).expect("should parse");
        assert_eq!(version, 796);
        assert_eq!(designs.len(), 2);

        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        assert_eq!(cache.get_missile_design(2).unwrap(), None);

        cache.upsert_missile_designs(&designs).unwrap();
        cache.set_missile_design_version(version).unwrap();

        let rocket = cache
            .get_missile_design(2)
            .unwrap()
            .expect("Rocket's MissileDesign should be cached");
        assert_eq!(rocket.missile_design_name, "Rocket");
        assert_eq!(rocket.hull_damage, 0.0);
        assert_eq!(rocket.system_damage, 1.0);
        assert_eq!(rocket.character_damage, 1.0);
        assert_eq!(rocket.shield_damage, 5.0);
        assert_eq!(rocket.ap_damage, 1.0);
        assert_eq!(rocket.explosion_type, "Single");
        assert_eq!(rocket.explosion_radius, 0.0);
        assert_eq!(rocket.fire_length, 0.0);
        assert_eq!(rocket.craft_damage, 1.0);
        assert_eq!(rocket.volley, 1.0);

        let interceptor = cache
            .get_missile_design(47)
            .unwrap()
            .expect("Interceptor's MissileDesign should be cached");
        assert_eq!(interceptor.ap_damage, 0.0);
        assert_eq!(interceptor.explosion_type, "RandomOnce");
        assert_eq!(interceptor.explosion_radius, 150.0);
        assert_eq!(interceptor.fire_length, 0.0);
        assert_eq!(interceptor.craft_damage, 0.0);
        assert_eq!(interceptor.volley, 3.0);
    }

    #[test]
    fn resolves_a_craft_item_to_its_missile_design_damage() {
        let (_, missile_designs) =
            parse_missile_designs_xml(SAMPLE_MISSILE_XML).expect("should parse");
        let (craft_version, craft_designs) =
            parse_craft_designs_xml(SAMPLE_CRAFT_XML).expect("should parse");
        assert_eq!(craft_version, 775);
        assert_eq!(craft_designs.len(), 1);

        let mut cache = DesignsCache::open_in_memory().expect("In-memory cache should open");
        cache.upsert_missile_designs(&missile_designs).unwrap();
        cache.upsert_craft_designs(&craft_designs).unwrap();

        // The full real resolution chain: a Craft item's CraftDesignId -> CraftDesign's own
        // MissileDesignId -> the MissileDesign that actually carries damage stats.
        let missile_id = cache
            .get_craft_design_missile_id(1)
            .unwrap()
            .expect("Interceptor Lv1's craft design should be cached");
        assert_eq!(missile_id, 47);

        let damage = cache
            .get_missile_design(missile_id)
            .unwrap()
            .expect("Interceptor's MissileDesign should be cached");
        assert_eq!(damage.hull_damage, 2.0);
        assert_eq!(damage.shield_damage, 1.0);
    }
}
