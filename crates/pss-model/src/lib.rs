//! Serde structs for the Pixel Starships `battleReport` payload.
//!
//! Field names and single-letter JSON key mappings come from the IL2CPP metadata dump
//! (GameAssembly.so, version 31, client 0.999.62), cross-checked against 14 paired combats. See
//! docs/pss_battlereport.pdf for the full reverse-engineering notes and reliability levels
//! (VERIFIED / DUMP / OPEN), and docs/battle_report.json "_enums" key for the numeric enum tables.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level envelope sent by the client to `BattleService/FinaliseBattle`.
/// See §3 "Chaîne de transport" - this is `BattleReportRequestApi`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinaliseBattlePayload {
    #[serde(rename = "battleReport")]
    pub battle_report: BattleReport,
    /// XML of manual commands issued during the fight.
    #[serde(rename = "manualCommands")]
    pub manual_commands: String,
    /// 32-char hex checksum.
    pub z: String,
}

/// Root of the battle result. See §5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BattleReport {
    #[serde(rename = "a")]
    pub attacking_ship_report: ShipReport,
    #[serde(rename = "b")]
    pub defending_ship_report: ShipReport,
    /// ItemDesignId -> quantity. PvE win rewards; empty in PvP (see §5.2).
    #[serde(rename = "c")]
    pub win_items: HashMap<String, i64>,
    /// Always empty on the 14 control battles (level: DUMP).
    #[serde(rename = "d")]
    pub lose_items: HashMap<String, i64>,
    #[serde(rename = "e")]
    pub battle_outcome: OutcomeType,
    #[serde(rename = "f")]
    pub last_frame: i32,
}

/// §5.1 + docs/battle_report.json `_enums.OutcomeType`. Surrenders are NOT reported as
/// `AttackerRetreated`: they surface as `DefenderWon` with `currentShipHp == 0` while hull damage
/// stayed low - see §6.2.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(from = "i32", into = "i32")]
pub enum OutcomeType {
    Playing,
    AttackerWon,
    DefenderWon,
    AttackerRetreated,
    Draw,
    Waiting,
    Processing,
    WaitingForResultToFinalise,
    Reviewing,
    Unknown(i32),
}

impl From<i32> for OutcomeType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Playing,
            1 => Self::AttackerWon,
            2 => Self::DefenderWon,
            3 => Self::AttackerRetreated,
            4 => Self::Draw,
            5 => Self::Waiting,
            6 => Self::Processing,
            7 => Self::WaitingForResultToFinalise,
            8 => Self::Reviewing,
            other => Self::Unknown(other),
        }
    }
}

impl From<OutcomeType> for i32 {
    fn from(value: OutcomeType) -> Self {
        match value {
            OutcomeType::Playing => 0,
            OutcomeType::AttackerWon => 1,
            OutcomeType::DefenderWon => 2,
            OutcomeType::AttackerRetreated => 3,
            OutcomeType::Draw => 4,
            OutcomeType::Waiting => 5,
            OutcomeType::Processing => 6,
            OutcomeType::WaitingForResultToFinalise => 7,
            OutcomeType::Reviewing => 8,
            OutcomeType::Unknown(other) => other,
        }
    }
}

/// One side of the battle. See §6.
///
/// `shield_damage`, `hull_damage`, `current_ship_hp` and `max_ship_hp` are stored in
/// **hundredths** of the in-game HP value, divide by 100 before display (§6.1).
/// Room/crew/module/craft damage fields are NOT scaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipReport {
    #[serde(rename = "a")]
    pub room_reports: HashMap<i32, RoomReport>,
    #[serde(rename = "b")]
    pub character_reports: HashMap<i32, CharacterReport>,
    /// Keyed by a composite "RoomId-index" string, not a bare id
    #[serde(rename = "c")]
    pub module_reports: HashMap<String, ModuleReport>,
    #[serde(rename = "d")]
    pub craft_reports: HashMap<i32, CraftReport>,
    #[serde(rename = "e")]
    pub item_reports: HashMap<i32, ItemReport>,
    #[serde(rename = "f")]
    pub ship_id: i32,
    #[serde(rename = "g")]
    pub ship_design_id: i32,
    /// Damage taken by the shield, in hundredths of HP.
    #[serde(rename = "h")]
    pub shield_damage: f64,
    /// Hull damage taken, in hundredths of HP.
    #[serde(rename = "i")]
    pub hull_damage: f64,
    ///Remaining hull HP, in hundredths of HP.
    #[serde(rename = "j")]
    pub current_ship_hp: f64,
    /// Max hull HP, in hundredths of HP (constant per shipDesignId).
    #[serde(rename = "k")]
    pub max_ship_hp: f64,
    /// Starting crew count, excluding androids.
    #[serde(rename = "l")]
    pub total_starting_crews: i32,
    #[serde(rename = "m")]
    pub missile_dodges: i32,
    /// Level: DUMP - only observed values are 0 and 1.
    #[serde(rename = "n")]
    pub manual_command_count: i32,
    /// Level: DUMP - always 0 on the 14 control battles.
    #[serde(rename = "o")]
    pub cloak_count: i32,
    #[serde(rename = "p")]
    pub missile_reports: HashMap<i32, MissileReport>,
}

impl ShipReport {
    /// Was this ship destroyed (as opposed to surrendered) ? See §6.2:
    /// both cases report `currentShipHp == 0`; only destruction also drives hull damage up to
    /// (approximately) `maxShipHp`.
    pub fn is_destroyed(&self) -> bool {
        self.current_ship_hp == 0.0 && (self.hull_damage - self.max_ship_hp).abs() < 1.0
    }

    /// Did this ship surrender rather than get destroyed ? See §6.2.
    pub fn is_surrendered(&self) -> bool {
        self.current_ship_hp == 0.0 && self.max_ship_hp - self.hull_damage > 0.0
    }
}

/// §7. Weapon rooms use a type-dependent fire counter - see §7.1: Laser -> `shots_fired`, Missile
/// -> `missiles_deployed`, AntiCraft -> `activations`. There is no single universal shot counter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomReport {
    #[serde(rename = "a")]
    pub room_id: i32,
    #[serde(rename = "b")]
    pub room_design_id: i32,
    #[serde(rename = "c")]
    pub room_type: RoomType,
    #[serde(rename = "d")]
    pub room_category: CategoryType,
    #[serde(rename = "e")]
    pub damage_hp: f64,
    #[serde(rename = "f")]
    pub repaired_hp: f64,
    #[serde(rename = "g")]
    pub is_destroyed: bool,
    /// Level: DUMP - non-null on 13/164 rooms in the corpus.
    #[serde(rename = "h")]
    pub emp_duration: i32,
    #[serde(rename = "i")]
    pub missiles_used: i32,
    #[serde(rename = "j")]
    pub craft_used: i32,
    #[serde(rename = "k")]
    pub androids_used: i32,
    /// Shot counter for AntiCraft/Android/Carrier rooms.
    #[serde(rename = "l")]
    pub activations: i32,
    #[serde(rename = "m")]
    pub charges_used: i32,
    #[serde(rename = "n")]
    pub original_charges: i32,
    /// Shot counter for Laser rooms.
    #[serde(rename = "o")]
    pub shots_fired: i32,
    /// Shot counter for Missile rooms.
    #[serde(rename = "p")]
    pub missiles_deployed: i32,
    #[serde(rename = "q")]
    pub crafts_deployed: i32,
    #[serde(rename = "r")]
    pub androids_deployed: i32,
}

impl RoomReport {
    /// Fire volume for this room, picking the counter that matches its weapon type (§7.1). Returns
    /// `None` for non-weapon rooms.
    pub fn shots_for(&self, room_type: RoomType) -> Option<i32> {
        match room_type {
            RoomType::Laser => Some(self.shots_fired),
            RoomType::Missile => Some(self.missiles_deployed),
            RoomType::AntiCraft | RoomType::Android | RoomType::Carrier => Some(self.activations),
            _ => None,
        }
    }
}

/// §7.2 + docs/battle_report.json `_enums.RoomType`. Full 0-27 range; `Unknown` only guards
/// against future additions.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[serde(from = "i32", into = "i32")]
pub enum RoomType {
    None,
    Laser,
    Bridge,
    Engine,
    Lift,
    Bedroom,
    Shield,
    Storage,
    Reactor,
    Missile,
    Teleport,
    Mineral,
    Gas,
    Command,
    Research,
    Wall,
    Council,
    Carrier,
    AntiCraft,
    Medical,
    Trap,
    Radar,
    Stealth,
    Training,
    Cannon,
    Corridor,
    StationMissile,
    Android,
    Unknown(i32),
}

impl From<i32> for RoomType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Laser,
            2 => Self::Bridge,
            3 => Self::Engine,
            4 => Self::Lift,
            5 => Self::Bedroom,
            6 => Self::Shield,
            7 => Self::Storage,
            8 => Self::Reactor,
            9 => Self::Missile,
            10 => Self::Teleport,
            11 => Self::Mineral,
            12 => Self::Gas,
            13 => Self::Command,
            14 => Self::Research,
            15 => Self::Wall,
            16 => Self::Council,
            17 => Self::Carrier,
            18 => Self::AntiCraft,
            19 => Self::Medical,
            20 => Self::Trap,
            21 => Self::Radar,
            22 => Self::Stealth,
            23 => Self::Training,
            24 => Self::Cannon,
            25 => Self::Corridor,
            26 => Self::StationMissile,
            27 => Self::Android,
            other => Self::Unknown(other),
        }
    }
}

impl From<RoomType> for i32 {
    fn from(value: RoomType) -> Self {
        match value {
            RoomType::None => 0,
            RoomType::Laser => 1,
            RoomType::Bridge => 2,
            RoomType::Engine => 3,
            RoomType::Lift => 4,
            RoomType::Bedroom => 5,
            RoomType::Shield => 6,
            RoomType::Storage => 7,
            RoomType::Reactor => 8,
            RoomType::Missile => 9,
            RoomType::Teleport => 10,
            RoomType::Mineral => 11,
            RoomType::Gas => 12,
            RoomType::Command => 13,
            RoomType::Research => 14,
            RoomType::Wall => 15,
            RoomType::Council => 16,
            RoomType::Carrier => 17,
            RoomType::AntiCraft => 18,
            RoomType::Medical => 19,
            RoomType::Trap => 20,
            RoomType::Radar => 21,
            RoomType::Stealth => 22,
            RoomType::Training => 23,
            RoomType::Cannon => 24,
            RoomType::Corridor => 25,
            RoomType::StationMissile => 26,
            RoomType::Android => 27,
            RoomType::Unknown(other) => other,
        }
    }
}

/// docs/battle_report.json `_enums.CategoryType` - this is what the JSON field `roomCategory`
/// encodes (named `CategoryType` in the dump, not `RoomCategory`).
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[serde(from = "i32", into = "i32")]
pub enum CategoryType {
    Weapon,
    Defence,
    Support,
    Ressources,
    Superweapon,
    None,
    Unknown(i32),
}

impl From<i32> for CategoryType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Weapon,
            1 => Self::Defence,
            2 => Self::Support,
            3 => Self::Ressources,
            4 => Self::Superweapon,
            5 => Self::None,
            other => Self::Unknown(other),
        }
    }
}

impl From<CategoryType> for i32 {
    fn from(value: CategoryType) -> Self {
        match value {
            CategoryType::Weapon => 0,
            CategoryType::Defence => 1,
            CategoryType::Support => 2,
            CategoryType::Ressources => 3,
            CategoryType::Superweapon => 4,
            CategoryType::None => 5,
            CategoryType::Unknown(other) => other,
        }
    }
}

/// §8. Crew member report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterReport {
    #[serde(rename = "a")]
    pub character_id: i32,
    #[serde(rename = "b")]
    pub character_design_id: i32,
    #[serde(rename = "c")]
    pub is_dead: bool,
    #[serde(rename = "d")]
    pub teleports: i32,
    #[serde(rename = "e")]
    pub damage_hp: f64,
    #[serde(rename = "f")]
    pub healed_hp: f64,
    /// Level: DUMP.
    #[serde(rename = "g")]
    pub collection_design_id: i32,
    /// Level: DUMP.
    #[serde(rename = "h")]
    pub is_in_active_collection: bool,
    /// True for synthetic crew (characterId >= 2e9); prefix encodes the side: 2000000xxx attacker,
    /// 2100000xxx defender (§8.1).
    #[serde(rename = "i")]
    pub is_android: bool,
    #[serde(rename = "j")]
    pub ability_triggered: bool,
    #[serde(rename = "k")]
    pub special_ability_type: SpecialAbilityType,
    /// Level: DUMP.
    #[serde(rename = "l")]
    pub original_room_id: i32,
}

/// docs/battle_report.json `_enums.SpecialAbilityType`.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[serde(from = "i32", into = "i32")]
pub enum SpecialAbilityType {
    None,
    HealSelfHp,
    HealSameRoomCharacters,
    AddReload,
    DeductReload,
    HealRoomHp,
    DamageToRoom,
    DamageToSameRoomCharacters,
    DamageToCurrentEnemy,
    SetFire,
    Teleport,
    Freeze,
    FireWalk,
    Clone,
    Polymorph,
    Mimic,
    Summon,
    Invulnerability,
    ProtectRoom,
    Bloodlust,
    PoisonRoom,
    RemoveDebuffsFromSameRoomCharacters,
    SelfDestruct,
    XmasExplosion,
    PoisonCrew,
    Unknown(i32),
}

impl From<i32> for SpecialAbilityType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::HealSelfHp,
            2 => Self::HealSameRoomCharacters,
            3 => Self::AddReload,
            4 => Self::DeductReload,
            5 => Self::HealRoomHp,
            6 => Self::DamageToRoom,
            7 => Self::DamageToSameRoomCharacters,
            8 => Self::DamageToCurrentEnemy,
            9 => Self::SetFire,
            10 => Self::Teleport,
            11 => Self::Freeze,
            12 => Self::FireWalk,
            13 => Self::Clone,
            14 => Self::Polymorph,
            15 => Self::Mimic,
            16 => Self::Summon,
            17 => Self::Invulnerability,
            18 => Self::ProtectRoom,
            19 => Self::Bloodlust,
            20 => Self::PoisonRoom,
            21 => Self::RemoveDebuffsFromSameRoomCharacters,
            22 => Self::SelfDestruct,
            23 => Self::XmasExplosion,
            24 => Self::PoisonCrew,
            other => Self::Unknown(other),
        }
    }
}

impl From<SpecialAbilityType> for i32 {
    fn from(value: SpecialAbilityType) -> Self {
        match value {
            SpecialAbilityType::None => 0,
            SpecialAbilityType::HealSelfHp => 1,
            SpecialAbilityType::HealSameRoomCharacters => 2,
            SpecialAbilityType::AddReload => 3,
            SpecialAbilityType::DeductReload => 4,
            SpecialAbilityType::HealRoomHp => 5,
            SpecialAbilityType::DamageToRoom => 6,
            SpecialAbilityType::DamageToSameRoomCharacters => 7,
            SpecialAbilityType::DamageToCurrentEnemy => 8,
            SpecialAbilityType::SetFire => 9,
            SpecialAbilityType::Teleport => 10,
            SpecialAbilityType::Freeze => 11,
            SpecialAbilityType::FireWalk => 12,
            SpecialAbilityType::Clone => 13,
            SpecialAbilityType::Polymorph => 14,
            SpecialAbilityType::Mimic => 15,
            SpecialAbilityType::Summon => 16,
            SpecialAbilityType::Invulnerability => 17,
            SpecialAbilityType::ProtectRoom => 18,
            SpecialAbilityType::Bloodlust => 19,
            SpecialAbilityType::PoisonRoom => 20,
            SpecialAbilityType::RemoveDebuffsFromSameRoomCharacters => 21,
            SpecialAbilityType::SelfDestruct => 22,
            SpecialAbilityType::XmasExplosion => 23,
            SpecialAbilityType::PoisonCrew => 24,
            SpecialAbilityType::Unknown(other) => other,
        }
    }
}

/// §9 ".c" - equiped modules (not crafts/drones, despite the composite key looking that way).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleReport {
    /// Level: DUMP.
    #[serde(rename = "a")]
    pub is_destroyed: bool,
    #[serde(rename = "b")]
    pub damage_hp: f64,
    #[serde(rename = "c")]
    pub item_id: i32,
    #[serde(rename = "d")]
    pub item_design_id: i32,
    #[serde(rename = "e")]
    pub room_id: i32,
}

/// §9 ".d" - deployed crafts/drones, always originating from a Carrier room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraftReport {
    /// Level: DUMP.
    #[serde(rename = "a")]
    pub damage_hp: f64,
    #[serde(rename = "b")]
    pub is_destroyed: bool,
    /// Level: DUMP.
    #[serde(rename = "c")]
    pub attacks: i32,
    #[serde(rename = "d")]
    pub original_room_id: i32,
}

/// §9 ".p" - this is an inventory of fired projectiles indexed by id, NOT an ordered event log
/// (§9, "Attention" box).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissileReport {
    #[serde(rename = "a")]
    pub original_room_id: i32,
    /// Level: DUMP - always 0 across the 2060 projectiles in the corpus.
    #[serde(rename = "b")]
    pub original_craft_id: i32,
    #[serde(rename = "c")]
    pub hit_target: bool,
    #[serde(rename = "d")]
    pub hit_shield: bool,
    #[serde(rename = "e")]
    pub missed: bool,
}

/// §9 ".e" - cargo/inventory items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemReport {
    #[serde(rename = "a")]
    pub item_id: i32,
    #[serde(rename = "b")]
    pub item_design_id: i32,
    /// Level: DUMP - non-null on 6 items in the corpus.
    #[serde(rename = "c")]
    pub quantity_used: i32,
    #[serde(rename = "d")]
    pub original_quantity: i32,
    /// Level: DUMP - non-null on 6 items in the corpus.
    #[serde(rename = "e")]
    pub quantity_deployed: i32,
    #[serde(rename = "f")]
    pub item_type: ItemType,
    /// Level: OPEN - `ItemSubType` codes are only fully known for the equipment bitmask (Head=1,
    /// Body=2, Leg=4, Weapon=8, Accessory=16, Pet=32); beyond that they are sequential constants
    /// (GasPack=64, MineralPack=65, Module=72, ShipSkin=75...) with no exhaustive list in the
    /// dump, so this stays raw.
    #[serde(rename = "g")]
    pub item_sub_type: i32,
}

/// docs/battle_report.json `_enums.ItemType` - 9 values (0-8), wider than the 6 combat-relevant
/// ones in PDF §9.1 (Capacity, Starbux never appear in battle reports but are part of the real enum).
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[serde(from = "i32", into = "i32")]
pub enum ItemType {
    None,
    Mineral,
    Gas,
    Missile,
    Craft,
    Equipement,
    Android,
    Capacity,
    Starbux,
    Unknown(i32),
}

impl From<i32> for ItemType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Mineral,
            2 => Self::Gas,
            3 => Self::Missile,
            4 => Self::Craft,
            5 => Self::Equipement,
            6 => Self::Android,
            7 => Self::Capacity,
            8 => Self::Starbux,
            other => Self::Unknown(other),
        }
    }
}

impl From<ItemType> for i32 {
    fn from(value: ItemType) -> Self {
        match value {
            ItemType::None => 0,
            ItemType::Mineral => 1,
            ItemType::Gas => 2,
            ItemType::Missile => 3,
            ItemType::Craft => 4,
            ItemType::Equipement => 5,
            ItemType::Android => 6,
            ItemType::Capacity => 7,
            ItemType::Starbux => 8,
            ItemType::Unknown(other) => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_room_report() -> RoomReport {
        RoomReport {
            room_id: 1,
            room_design_id: 1,
            room_type: RoomType::Laser,
            room_category: CategoryType::Weapon,
            damage_hp: 0.0,
            repaired_hp: 0.0,
            is_destroyed: false,
            emp_duration: 0,
            missiles_used: 0,
            craft_used: 0,
            androids_used: 0,
            activations: 0,
            charges_used: 0,
            original_charges: 0,
            shots_fired: 0,
            missiles_deployed: 0,
            crafts_deployed: 0,
            androids_deployed: 0,
        }
    }

    fn sample_ship_report() -> ShipReport {
        ShipReport {
            room_reports: HashMap::new(),
            character_reports: HashMap::new(),
            module_reports: HashMap::new(),
            craft_reports: HashMap::new(),
            item_reports: HashMap::new(),
            ship_id: 1,
            ship_design_id: 1,
            shield_damage: 0.0,
            hull_damage: 0.0,
            current_ship_hp: 0.0,
            max_ship_hp: 0.0,
            total_starting_crews: 6,
            missile_dodges: 0,
            manual_command_count: 0,
            cloak_count: 0,
            missile_reports: HashMap::new(),
        }
    }

    #[test]
    fn outcome_type_roundtrip() {
        assert_eq!(OutcomeType::from(2), OutcomeType::DefenderWon);
        assert_eq!(i32::from(OutcomeType::AttackerWon), 1);
        assert_eq!(OutcomeType::from(42), OutcomeType::Unknown(42));
    }

    #[test]
    fn room_type_shots_for_matches_weapon_kind() {
        let mut room = sample_room_report();
        room.shots_fired = 12;
        room.missiles_deployed = 7;
        room.activations = 3;

        assert_eq!(room.shots_for(RoomType::Laser), Some(12));
        assert_eq!(room.shots_for(RoomType::Missile), Some(7));
        assert_eq!(room.shots_for(RoomType::AntiCraft), Some(3));
        assert_eq!(room.shots_for(RoomType::Bridge), None);
    }

    #[test]
    fn special_ability_type_roundtrip() {
        assert_eq!(SpecialAbilityType::from(3), SpecialAbilityType::AddReload);
        assert_eq!(i32::from(SpecialAbilityType::PoisonCrew), 24);
    }

    #[test]
    fn ship_report_destroyed_vs_surrendered() {
        let mut ship = sample_ship_report();
        ship.max_ship_hp = 2200.0;
        ship.current_ship_hp = 0.0;
        ship.hull_damage = 2200.0;
        assert!(ship.is_destroyed());
        assert!(!ship.is_surrendered());

        ship.hull_damage = 82.0; // PvP-Loose-Surrender/1, §6.2
        assert!(!ship.is_destroyed());
        assert!(ship.is_surrendered());
    }

    #[test]
    fn deserializes_minimal_finalise_battle_payload() {
        use serde_json::json;

        let value = json!({
            "battleReport": {
                "a": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {},
                    "f": 1, "g": 2,
                    "h": 0.0, "i": 100.0, "j": 0.0, "k": 2200.0, "l": 6,
                    "m": 0, "n": 0, "o": 0, "p": {}
                },
                "b": {
                    "a": {}, "b": {}, "c": {}, "d": {}, "e": {},
                    "f": 3, "g": 4,
                    "h": 0.0, "i": 220000.0, "j": 0.0, "k": 2200.0, "l": 6,
                    "m": 0, "n": 0, "o": 0, "p": {}
                },
                "c": {}, "d": {}, "e": 2, "f": 1706
            },
            "manualCommands": "",
            "z": "abc"
        });

        let payload: FinaliseBattlePayload =
            serde_json::from_value(value).expect("payload should deserialize");
        assert_eq!(
            payload.battle_report.battle_outcome,
            OutcomeType::DefenderWon
        );
        assert_eq!(payload.battle_report.attacking_ship_report.ship_id, 1);
    }
}
