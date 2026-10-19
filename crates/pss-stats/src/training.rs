//! Crew training simulation: a character's stat at a given level (interpolated from its
//! `CharacterDesign` level-1/`Final*` values) plus the gain from training points.
//!
//! Training rule (as played): `gain = base × points / 100`, `base` being the stat at the
//! character's current level. HP gains are rounded half-up to a whole number in game (a gain of
//! 0.5–0.9 counts as 1); every other stat keeps its decimals.

use pss_designs::CharacterDesignXml;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrewStat {
    Hp,
    Attack,
    Repair,
    Ability,
    Stamina,
    Pilot,
    Science,
    Engine,
    Weapon,
}

impl CrewStat {
    pub const ALL: [CrewStat; 9] = [
        CrewStat::Hp,
        CrewStat::Attack,
        CrewStat::Repair,
        CrewStat::Ability,
        CrewStat::Stamina,
        CrewStat::Pilot,
        CrewStat::Science,
        CrewStat::Engine,
        CrewStat::Weapon,
    ];

    fn index(self) -> usize {
        CrewStat::ALL.iter().position(|&s| s == self).unwrap()
    }

    /// Maps an item's `EnhancementType`/`BonusEnhancementType` to the stat it boosts. `None` for
    /// types that aren't a trainable stat (`FireResistance`, `FreezeAttackSkill`, `None`).
    pub fn from_enhancement(kind: &str) -> Option<CrewStat> {
        Some(match kind {
            "Hp" => CrewStat::Hp,
            "Attack" => CrewStat::Attack,
            "Repair" => CrewStat::Repair,
            "Ability" => CrewStat::Ability,
            "Stamina" => CrewStat::Stamina,
            "Pilot" => CrewStat::Pilot,
            "Science" => CrewStat::Science,
            "Engine" => CrewStat::Engine,
            "Weapon" => CrewStat::Weapon,
            _ => return None,
        })
    }

    /// The API's `EnhancementType` name for this stat (inverse of `from_enhancement`).
    pub fn enhancement_name(self) -> &'static str {
        match self {
            CrewStat::Hp => "Hp",
            CrewStat::Attack => "Attack",
            CrewStat::Repair => "Repair",
            CrewStat::Ability => "Ability",
            CrewStat::Stamina => "Stamina",
            CrewStat::Pilot => "Pilot",
            CrewStat::Science => "Science",
            CrewStat::Engine => "Engine",
            CrewStat::Weapon => "Weapon",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EquipmentSlot {
    Head,
    Body,
    Leg,
    Weapon,
    Accessory,
    Pet,
}

impl EquipmentSlot {
    /// Ordered by `EquipmentMask` bit: bit 0 Head ... bit 5 Pet.
    pub const ALL: [EquipmentSlot; 6] = [
        EquipmentSlot::Head,
        EquipmentSlot::Body,
        EquipmentSlot::Leg,
        EquipmentSlot::Weapon,
        EquipmentSlot::Accessory,
        EquipmentSlot::Pet,
    ];

    /// Slots a character can equip, from its design's `EquipmentMask`.
    pub fn from_mask(mask: i32) -> Vec<EquipmentSlot> {
        EquipmentSlot::ALL
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask >= 0 && mask & (1 << bit) != 0)
            .map(|(_, &slot)| slot)
            .collect()
    }

    /// Slot an equipment item goes in, from its design's `ItemSubType` (`EquipmentWeapon`...).
    pub fn from_item_sub_type(sub_type: &str) -> Option<EquipmentSlot> {
        Some(match sub_type {
            "EquipmentHead" => EquipmentSlot::Head,
            "EquipmentBody" => EquipmentSlot::Body,
            "EquipmentLeg" => EquipmentSlot::Leg,
            "EquipmentWeapon" => EquipmentSlot::Weapon,
            "EquipmentAccessory" => EquipmentSlot::Accessory,
            "EquipmentPet" => EquipmentSlot::Pet,
            _ => return None,
        })
    }
}

/// Training points spent per stat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Training {
    points: [i32; CrewStat::ALL.len()],
}

impl Training {
    pub fn get(&self, stat: CrewStat) -> i32 {
        self.points[stat.index()]
    }

    pub fn set(&mut self, stat: CrewStat, points: i32) {
        self.points[stat.index()] = points.max(0);
    }

    pub fn total(&self) -> i32 {
        self.points.iter().sum()
    }

    /// Raw per-stat points in `CrewStat::ALL` order, for persistence.
    pub fn points(&self) -> [i32; CrewStat::ALL.len()] {
        self.points
    }

    pub fn from_points(points: [i32; CrewStat::ALL.len()]) -> Self {
        Self {
            points: points.map(|p| p.max(0)),
        }
    }
}

/// Interpolates a stat between its level-1 value and its max-level value, using the design's
/// `ProgressionType` curve: `base + (final − base) × ((level − 1) / (max_level − 1))^p`, with
/// p = 1 (Linear), 2 (EaseIn), 0.5 (EaseOut). Same formula as PixyShip's `interpolateStat`
/// (frontend/src/views/CrewDetail.vue).
pub fn stat_at_level(
    base: f64,
    final_value: f64,
    level: i32,
    max_level: i32,
    progression: &str,
) -> f64 {
    let exponent = match progression {
        "EaseIn" => 2.0,
        "EaseOut" => 0.5,
        _ => 1.0,
    };
    let span = (max_level - 1).max(1) as f64;
    let t = ((level - 1) as f64 / span).clamp(0.0, 1.0);
    base + (final_value - base) * t.powf(exponent)
}

/// A stat's untrained value at `level`. Stamina has no design value (it only comes from training).
pub fn base_stat(design: &CharacterDesignXml, stat: CrewStat, level: i32) -> f64 {
    let (start, end) = match stat {
        CrewStat::Hp => (design.hp, design.final_hp),
        CrewStat::Attack => (design.attack, design.final_attack),
        CrewStat::Repair => (design.repair, design.final_repair),
        CrewStat::Ability => (
            design.special_ability_argument,
            design.special_ability_final_argument,
        ),
        CrewStat::Stamina => return 0.0,
        CrewStat::Pilot => (design.pilot, design.final_pilot),
        CrewStat::Science => (design.science, design.final_science),
        CrewStat::Engine => (design.engine, design.final_engine),
        CrewStat::Weapon => (design.weapon, design.final_weapon),
    };
    stat_at_level(
        start,
        end,
        level,
        design.max_character_level,
        &design.progression_type,
    )
}

/// Gain from `points` of training on a stat whose untrained value is `base`.
/// Stamina has no base to scale, so its points are the gain itself.
pub fn training_gain(stat: CrewStat, base: f64, points: i32) -> f64 {
    match stat {
        CrewStat::Stamina => points as f64,
        CrewStat::Hp => round_half_up(base * points as f64 / 100.0),
        _ => base * points as f64 / 100.0,
    }
}

// The epsilon absorbs float noise (e.g. 0.49999999 from 10 × 5 / 100 computed as 0.5 - ε)
fn round_half_up(value: f64) -> f64 {
    (value + 1e-9).floor()
        + if (value + 1e-9).fract() >= 0.5 {
            1.0
        } else {
            0.0
        }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainedStat {
    pub stat: CrewStat,
    pub base: f64,
    pub training_gain: f64,
    pub equipment_bonus: f64,
    pub final_value: f64,
}

/// Every trainable stat for `design` at `level`: `final = base(level) + training + equipment`.
/// `equipment` lists flat bonuses (item enhancement and per-item random bonus alike); the
/// training gain scales off the untrained base only, never off equipment.
pub fn crew_stats(
    design: &CharacterDesignXml,
    level: i32,
    training: &Training,
    equipment: &[(CrewStat, f64)],
) -> Vec<TrainedStat> {
    CrewStat::ALL
        .iter()
        .map(|&stat| {
            let base = base_stat(design, stat, level);
            let training_gain = training_gain(stat, base, training.get(stat));
            let equipment_bonus: f64 = equipment
                .iter()
                .filter(|(s, _)| *s == stat)
                .map(|(_, v)| v)
                .sum();
            TrainedStat {
                stat,
                base,
                training_gain,
                equipment_bonus,
                final_value: base + training_gain + equipment_bonus,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_is_base_times_training_percent() {
        // 10 base × 1% = 0.1
        assert!((training_gain(CrewStat::Attack, 10.0, 1) - 0.1).abs() < 1e-9);
        assert!((training_gain(CrewStat::Weapon, 24.0, 10) - 2.4).abs() < 1e-9);
    }

    #[test]
    fn hp_gain_rounds_half_up_to_whole_number() {
        assert_eq!(training_gain(CrewStat::Hp, 10.0, 5), 1.0); // 0.5 -> 1
        assert_eq!(training_gain(CrewStat::Hp, 10.0, 9), 1.0); // 0.9 -> 1
        assert_eq!(training_gain(CrewStat::Hp, 10.0, 4), 0.0); // 0.4 -> 0
        assert_eq!(training_gain(CrewStat::Hp, 10.0, 15), 2.0); // 1.5 -> 2
        assert_eq!(training_gain(CrewStat::Hp, 10.0, 12), 1.0); // 1.2 -> 1
    }

    #[test]
    fn stamina_points_are_the_gain() {
        assert_eq!(training_gain(CrewStat::Stamina, 0.0, 7), 7.0);
    }

    #[test]
    fn equipment_mask_maps_bits_to_slots() {
        // Real designs: Neptune Mariner 12 = Leg + Weapon, Laura 62 = everything but Head
        assert_eq!(
            EquipmentSlot::from_mask(12),
            vec![EquipmentSlot::Leg, EquipmentSlot::Weapon]
        );
        assert_eq!(
            EquipmentSlot::from_mask(62),
            vec![
                EquipmentSlot::Body,
                EquipmentSlot::Leg,
                EquipmentSlot::Weapon,
                EquipmentSlot::Accessory,
                EquipmentSlot::Pet
            ]
        );
        assert!(EquipmentSlot::from_mask(-1).is_empty());
    }

    #[test]
    fn crew_stats_adds_equipment_flat_without_scaling_training() {
        let mut design = sample_design();
        design.weapon = 24.0;
        design.final_weapon = 24.0;
        let mut training = Training::default();
        training.set(CrewStat::Weapon, 10);

        let stats = crew_stats(
            &design,
            40,
            &training,
            &[(CrewStat::Weapon, 4.0), (CrewStat::Weapon, 1.5)],
        );
        let weapon = stats.iter().find(|s| s.stat == CrewStat::Weapon).unwrap();
        assert!((weapon.training_gain - 2.4).abs() < 1e-9); // 24 × 10%, not (24 + 5.5) × 10%
        assert!((weapon.equipment_bonus - 5.5).abs() < 1e-9);
        assert!((weapon.final_value - 31.9).abs() < 1e-9);
    }

    fn sample_design() -> CharacterDesignXml {
        CharacterDesignXml {
            character_design_id: 583,
            character_design_name: "Neptune Mariner".to_string(),
            hp: 5.0,
            final_hp: 9.0,
            pilot: 1.0,
            final_pilot: 3.0,
            repair: 1.6,
            final_repair: 2.7,
            weapon: 12.0,
            final_weapon: 24.0,
            science: 1.0,
            final_science: 3.0,
            engine: 2.0,
            final_engine: 4.0,
            research: 0.0,
            final_research: 1.0,
            attack: 1.2,
            final_attack: 2.2,
            special_ability_type: "AddReload".to_string(),
            special_ability_argument: 25.0,
            special_ability_final_argument: 50.0,
            rarity: "Hero".to_string(),
            progression_type: "Linear".to_string(),
            max_character_level: 40,
            training_capacity: 90,
            equipment_mask: 12,
        }
    }

    #[test]
    fn stat_at_level_follows_progression_curve() {
        // Neptune Mariner (real design #583): Weapon 12 -> 24, Linear, max level 40
        assert_eq!(stat_at_level(12.0, 24.0, 1, 40, "Linear"), 12.0);
        assert_eq!(stat_at_level(12.0, 24.0, 40, 40, "Linear"), 24.0);
        let mid = (20 - 1) as f64 / 39.0;
        assert!((stat_at_level(0.0, 10.0, 20, 40, "Linear") - 10.0 * mid).abs() < 1e-9);
        assert!((stat_at_level(0.0, 10.0, 20, 40, "EaseIn") - 10.0 * mid * mid).abs() < 1e-9);
        assert!((stat_at_level(0.0, 10.0, 20, 40, "EaseOut") - 10.0 * mid.sqrt()).abs() < 1e-9);
    }
}
