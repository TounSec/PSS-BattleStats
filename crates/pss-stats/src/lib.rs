//! Combat statistics derived by crossing a `battleReport` (pss-model) with weapon base stats from
//! `RoomService/ListRoomDesigns2` (pss-designs).
//!
//! The battleReport never carries damage-per-shot values (see pss-model docs and
//! docs/pss_battlereport.pdf §10): only the shot volue per room and the damage *taken* per entity
//! are transmitted. Estimating damage dealt means multiplying a room's fire volume by its weapon's
//! base damage from the design table - this is an estimate, not a value read directly off the wire.

pub mod training;

use std::collections::HashMap;

use anyhow::Result;
use pss_designs::DesignsCache;
use pss_model::{
    CategoryType, CraftReport, ItemType, RoomReport, RoomType, ShipReport, SpecialAbilityType,
};
use pss_store::StoredBattle;

/// Client tick rate, sourced from community reverse-engineering (PSSCombat, HerzbubeWiki: "the
/// game engine runs on 40 frames per second"). Confirmed plausible against control captures (1706
/// frames -> 42.6s, 1917 frames -> 47.9s). Use this with `dps()` unless you have your own
/// calibrated value.
pub const DEFAULT_FRAMES_PER_SECOND: f64 = 40.0;

/// Real (or mono-weapon-calibrated) per-shot damage for a weapon, sourced from the standalone
/// `RoomService/ListMissileDesigns` endpoint (Missile ammo and Craft items - see pss-designs
/// module docs) or calibration's aggregate-observation fallback.
///
/// `ap` is "AP Damage" (verified against the Pixel Starships Fandom wiki: a distinct mechanic from
/// `system`, not an alias - it ignores Armor and never converts to hull damage on a destroyed
/// room). Calibration can't separate it from `system` (both manifest as the same observable
/// `RoomReport.damage_hp` delta on the defender's side), so it's always `0.0` on the calibration
/// fallback - a known gap, not a bug, same as every other calibration limitation in this project.
///
/// `shield` is real for direct-fire room weapons (Laser, via the room-nested `MissileDesignXml`),
/// but for `ItemType::Missile` ammo specifically, callers building this from a real
/// `MissileDamageRecord` (`item_based_missile_damage`, the Missile branch of `item_weapon_stats`)
/// force it to `0.0` after calling `weapon_damage_from_missile_design` - see
/// `MissileDesignFullXml`'s doc comment for the proof (a real in-game weapon stats panel for
/// Scarlet shows System/AP/Crew/Craft Dmg and no Shield Dmg row at all, despite a nonzero
/// `ShieldDamage` in the XML). Craft ammo is left alone (no equivalent counter-evidence for
/// drones).
///
/// `craft` is "Craft Dmg" in that same panel - damage dealt to enemy Craft/drones, a target
/// category distinct from hull/system/character/shield. Always `0.0` on the calibration fallback
/// (calibration only ever observes room-side `damage_hp` deltas, nothing about enemy craft).
///
/// `fire_ap`/`fire_character` are the separate "Fire Damage" damage-over-time effect some weapons
/// (confirmed: Scarlet Missile, Firehawk Craft) deal on top of the instant fields above - see
/// `MissileDesignFullXml::fire_length`'s doc comment for the mechanic and `fire_damage_over_burn`
/// for the formula. Kept apart from `character`/`ap` rather than folded in, so a UI can show "this
/// is a burn-over-time estimate" distinctly instead of silently blending it into a number that
/// looks like an ordinary instant per-shot hit - the whole point of adding it was a user report
/// that a design's small instant `shield`/`ap` values were being shown as its entire story when its
/// real (much larger) damage is almost all fire. Always `0.0` on the calibration fallback (no
/// `fire_length` signal at all without a real design match).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WeaponDamage {
    pub hull: f64,
    pub system: f64,
    pub character: f64,
    pub shield: f64,
    pub ap: f64,
    pub craft: f64,
    pub fire_ap: f64,
    pub fire_character: f64,
}

/// Frames per Fire Damage tick - one tick per real second at this project's 40fps tick rate
/// (`DEFAULT_FRAMES_PER_SECOND`), per the Fandom wiki's "Fire Damage Formulas" page.
const FIRE_FRAME_INTERVAL: f64 = 40.0;

/// Total AP + Crew damage a single, uninterrupted Fire Damage burn deals over its full duration.
///
/// Per the Fandom wiki ("Fire Damage", "Fire Damage Formulas", "Scarlet Missile"): Fire Damage is a
/// decaying damage-over-time effect, entirely separate from a weapon's instant hull/system/
/// character/shield/AP hit, that affects ONLY AP and Crew damage. It ticks once every
/// `FIRE_FRAME_INTERVAL` frames, each tick computing `AP = current_duration_frames / 500` and
/// `Crew = current_duration_frames / 200`, using the CURRENT remaining duration (which decays by
/// one Fire Frame per tick, not a flat per-tick rate) - not the initial one.
///
/// This is necessarily an upper bound, not an exact figure:
/// - It omits a target's Fire Resistance stat (multiplies the Crew term down further) and the
///   Sprinkler/Crew-Repair-stat duration reductions - none of these are observable from anything
///   this project's data has access to (battleReport, design caches).
/// - When an item's `quantity_deployed > 1`, callers multiply this per-ignition total by that
///   count, which assumes every deployment lands on a *different* room. The wiki states repeat
///   fire hits on the *same* room don't stack additively (the longer duration overrides the
///   shorter one, it doesn't add) - the battleReport has no per-hit room attribution for Missile
///   ammo to tell these cases apart (see CLAUDE.md), so that assumption can't be checked either.
///
/// Real totals are therefore <= this estimate, same spirit as every other "can't see the target's
/// mitigation" gap already documented in this project (calibration's missing `ap`, AOE not
/// multiplied into totals, etc.) - not a new kind of uncertainty, just a new instance of it.
fn fire_damage_over_burn(fire_length_frames: f64) -> (f64, f64) {
    if fire_length_frames <= 0.0 {
        return (0.0, 0.0);
    }

    let ticks = (fire_length_frames / FIRE_FRAME_INTERVAL).floor() as i64;
    let mut remaining = fire_length_frames;
    let mut ap_total = 0.0;
    let mut character_total = 0.0;
    for _ in 0..ticks {
        ap_total += remaining / 500.0;
        character_total += remaining / 200.0;
        remaining -= FIRE_FRAME_INTERVAL;
    }
    (ap_total, character_total)
}

/// Builds a `WeaponDamage` from a real `MissileDesignRecord` - the single place that also derives
/// the `fire_ap`/`fire_character` estimate from `fire_length`, so every caller (Missile ammo, Craft
/// ammo, the per-item breakdown) picks it up identically.
///
/// Negative design values (e.g. the Repair Drone's "Repair Beam": `SystemDamage = -0.1`) are
/// healing, not damage, so they're clamped to 0 here - left negative they'd subtract from the
/// damage totals and DPS (this showed up as `Carrier: -4.0 total damage`).
fn weapon_damage_from_missile_design(m: &pss_designs::MissileDamageRecord) -> WeaponDamage {
    let (fire_ap, fire_character) = fire_damage_over_burn(m.fire_length);
    WeaponDamage {
        hull: m.hull_damage.max(0.0),
        system: m.system_damage.max(0.0),
        character: m.character_damage.max(0.0),
        shield: m.shield_damage.max(0.0),
        ap: m.ap_damage.max(0.0),
        craft: m.craft_damage.max(0.0),
        fire_ap,
        fire_character,
    }
}

/// Estimated damage contribution of a single weapon room, derived from its fire volume (battleReport) crossed with its weapon's base stats (RoomDesign/MissileDesign).
#[derive(Debug, Clone, PartialEq)]
pub struct RoomWeaponStats {
    pub room_id: i32,
    pub room_design_id: i32,
    pub room_name: String,
    pub room_type: RoomType,
    pub shots_fired: i32,
    pub estimated_hull_damage: f64,
    pub estimated_system_damage: f64,
    pub estimated_character_damage: f64,
    pub estimated_shield_damage: f64,
    pub estimated_ap_damage: f64,
    /// See `WeaponDamage::craft`. Only ever nonzero for `Missile`/`Carrier` rooms (redistributed
    /// from the ship-wide item-based total, see `estimate_ship_damage`) - direct-fire weapon rooms
    /// (Laser, ...) have no equivalent "Craft Dmg" stat in their room-nested design.
    pub estimated_craft_damage: f64,
    /// See `WeaponDamage::fire_ap`/`fire_character`. Only ever nonzero for `Missile`/`Carrier`
    /// rooms (redistributed from the ship-wide item-based total, see `estimate_ship_damage`) -
    /// direct-fire weapon rooms (Laser, ...) have no `FireLength` concept at all.
    pub estimated_fire_ap_damage: f64,
    pub estimated_fire_character_damage: f64,
}

/// Estimated total damage dealt by one ship, broken down by room.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShipDamageEstimate {
    pub rooms: Vec<RoomWeaponStats>,
    pub total_hull_damage: f64,
    pub total_system_damage: f64,
    pub total_character_damage: f64,
    pub total_shield_damage: f64,
    pub total_ap_damage: f64,
    pub total_craft_damage: f64,
    pub total_fire_ap_damage: f64,
    pub total_fire_character_damage: f64,
}

impl ShipDamageEstimate {
    /// Every damage component summed - the one definition of "total estimated damage" used by the
    /// battle summary, the history chart and the CLI, so they can't drift apart again (the history
    /// chart once summed only four of the eight).
    pub fn total_damage(&self) -> f64 {
        self.total_hull_damage
            + self.total_system_damage
            + self.total_character_damage
            + self.total_shield_damage
            + self.total_ap_damage
            + self.total_craft_damage
            + self.total_fire_ap_damage
            + self.total_fire_character_damage
    }
}

/// Estimates the damage dealt by a single room, given its matching design record. Returns `None`
/// for non-weapon rooms (no fire-volume counter applies) or rooms with no known weapon design
/// (e.g. Bridge, Bedroom). `design`'s own damage fields are always absent (`None`) for
/// `Missile`/`Carrier` rooms - `estimate_ship_damage` fills their real per-room damage in
/// afterward, by redistributing a ship-wide item-based total (see its doc comment); this function
/// never sees their real per-shot stats directly.
///
/// `shots_override`, when given, replaces `room.shots_for(room.room_type)` as the fire-volume
/// count - used for `Carrier` rooms, where the room-level `RoomReport.activations` field turns out
/// to measure something else entirely (empirically: consistently far smaller than, and not a
/// constant ratio to, the sum of `CraftReport.attacks` for craft launched from that room -
/// `activations` is most likely a launch-cycle count, not a shot count). `estimate_ship_damage`
/// computes the real per-room shot count from `CraftReport.attacks` and passes it here.
///
/// `shield_hits` is how many of this room's projectiles hit the target's shield (the projectile
/// inventory tags each one with its `original_room_id` and `hit_shield`); only those deal the
/// design's `ShieldDamage`. `None` means unknown (no projectile inventory), in which case every
/// shot is assumed to. Checked against the observed shield damage over 23 laser-only ship-battles:
/// counting every shot overshot it 5x on average (up to 12x), counting only shield hits gave 1.05.
/// Hull/system/character/AP are deliberately NOT weighted by hit outcome: a projectile that hits
/// the shield still damages the rooms (all-shots system damage matched the observed room damage at
/// 1.02 +/- 0.11, hit-weighted only 0.86).
pub fn estimate_room_damage(
    room: &RoomReport,
    design: &pss_designs::RoomDesignRecord,
    shots_override: Option<i32>,
    shield_hits: Option<i32>,
) -> Option<RoomWeaponStats> {
    let shots_fired = shots_override.or_else(|| room.shots_for(room.room_type))?;

    let damage = WeaponDamage {
        hull: design.hull_damage.unwrap_or(0.0),
        system: design.system_damage.unwrap_or(0.0),
        character: design.character_damage.unwrap_or(0.0),
        shield: design.shield_damage.unwrap_or(0.0),
        ap: 0.0,
        craft: 0.0,
        fire_ap: 0.0,
        fire_character: 0.0,
    };

    Some(RoomWeaponStats {
        room_id: room.room_id,
        room_design_id: room.room_design_id,
        room_name: design.room_name.clone(),
        room_type: room.room_type,
        shots_fired,
        estimated_hull_damage: shots_fired as f64 * damage.hull,
        estimated_system_damage: shots_fired as f64 * damage.system,
        estimated_character_damage: shots_fired as f64 * damage.character,
        estimated_shield_damage: shield_hits.unwrap_or(shots_fired) as f64 * damage.shield,
        estimated_ap_damage: shots_fired as f64 * damage.ap,
        estimated_craft_damage: 0.0,
        estimated_fire_ap_damage: 0.0,
        estimated_fire_character_damage: 0.0,
    })
}

/// Per-item damage breakdown for one Missile or Craft item design a ship deployed - "one row per
/// drone/missile type". These rows are the single source for both the per-item tables and the
/// ship-wide Missile/Craft totals (`estimate_ship_damage` sums `total()` over them), so the two can
/// never disagree.
///
/// `damage` is **per projectile**, never per launch: the live `ListMissileDesigns` damage fields
/// are the stats of one projectile, and one launch fires `volley` of them. Counting projectiles
/// right is what the rest hinges on - verified on all 35 captured battles:
/// - Missile: `ItemReport.quantity_deployed` counts LAUNCHES. Summed `quantity_deployed x Volley`
///   equals exactly the ship's Missile-room projectile count (e.g. 15 Rocket + 10 Scarlet + 8
///   "Rocket Barrage" (Volley 10) = 105 projectiles, not 33).
/// - Craft: `ItemReport.quantity_deployed` counts drones deployed, while `CraftReport.attacks`
///   counts their projectiles (13 drones, 64 attacks). The projectile inventory has exactly one
///   entry per attack, all with `original_room_id == 0`.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemWeaponStats {
    pub item_design_id: i32,
    pub item_name: String,
    pub item_type: ItemType,
    /// Launches (Missile) / drones deployed (Craft) - what the game's item counter shows.
    pub quantity_deployed: i32,
    /// Projectiles per launch (1.0 when the design isn't known).
    pub volley: f64,
    /// Projectiles this row accounts for. Missile: exactly `quantity_deployed x volley`. Craft: the
    /// ship's total `CraftReport.attacks` shared between its craft rows in proportion to
    /// `quantity_deployed` - an estimate, since a `CraftReport` carries no reference to the craft
    /// design it belongs to (a ship that deployed a single craft design gets the exact figure).
    pub projectiles: f64,
    /// How many of `projectiles` hit the target's shield (the only projectiles that deal
    /// `damage.shield`). Missile: always 0 - none of the 4000+ missiles in the captured battles ever
    /// hit a shield, matching the in-game panel that has no shield damage row for missiles. Craft:
    /// the ship's craft projectiles that registered `hit_shield`, shared like `projectiles`.
    pub shield_hit_projectiles: f64,
    /// Per projectile.
    pub damage: WeaponDamage,
    /// AOE mechanic, straight from the design - a radius (game units), not a damage amount, so
    /// never folded into `damage` (see `MissileDesignFullXml`'s doc comment for why: how a
    /// `damage_hp` observation splits across `ExplosionRadius`-hit targets isn't verified).
    /// `explosion_type` is `"Single"` for non-AOE weapons - the majority.
    pub explosion_type: String,
    pub explosion_radius: f64,
}

impl ItemWeaponStats {
    /// Estimated damage dealt by this row over the whole battle: per-projectile damage x the
    /// projectiles it fired - except shield damage, which only counts for the projectiles that
    /// actually hit the shield (checked against the observed `ShipReport.shield_damage`: counting
    /// it for every shot overshoots it ~5x, counting only shield hits lands within 5%).
    pub fn total(&self) -> WeaponDamage {
        let n = self.projectiles;
        WeaponDamage {
            hull: self.damage.hull * n,
            system: self.damage.system * n,
            character: self.damage.character * n,
            shield: self.damage.shield * self.shield_hit_projectiles,
            ap: self.damage.ap * n,
            craft: self.damage.craft * n,
            fire_ap: self.damage.fire_ap * n,
            fire_character: self.damage.fire_character * n,
        }
    }
}

/// Per-item breakdown for every `Missile`/`Craft` ammo/drone item this ship deployed (one row per
/// distinct item design, `quantity_deployed > 0`). Real design stats first, mono-weapon calibration
/// fallback (`ap`/AOE are always absent from calibration, see `WeaponDamage`'s doc comment).
pub fn item_weapon_stats(ship: &ShipReport, cache: &DesignsCache) -> Vec<ItemWeaponStats> {
    let mut stats = Vec::new();

    for item in ship.item_reports.values() {
        if !matches!(item.item_type, ItemType::Missile | ItemType::Craft)
            || item.quantity_deployed <= 0
        {
            continue;
        }

        let Ok(Some(design)) = cache.get_item_design(item.item_design_id) else {
            continue;
        };

        let missile_design_id = match item.item_type {
            ItemType::Missile => Some(design.missile_design_id),
            ItemType::Craft => cache
                .get_craft_design_missile_id(design.craft_design_id)
                .ok()
                .flatten(),
            _ => None,
        };

        let real = missile_design_id.and_then(|id| cache.get_missile_design(id).ok().flatten());
        let (mut damage, volley, explosion_type, explosion_radius) = match real {
            Some(ref m) => (
                weapon_damage_from_missile_design(m),
                m.volley.max(1.0),
                m.explosion_type.clone(),
                m.explosion_radius,
            ),
            None => {
                let calibrated = cache
                    .get_calibrated_item_damage(item.item_design_id)
                    .ok()
                    .flatten();
                let damage = calibrated
                    .map(|c| WeaponDamage {
                        hull: c.hull_damage,
                        system: c.system_damage,
                        character: c.character_damage,
                        shield: c.shield_damage,
                        ap: 0.0,
                        craft: 0.0,
                        fire_ap: 0.0,
                        fire_character: 0.0,
                    })
                    .unwrap_or_default();
                (damage, 1.0, "Single".to_string(), 0.0)
            }
        };

        let quantity = item.quantity_deployed as f64;
        let (projectiles, shield_hit_projectiles) = match item.item_type {
            ItemType::Missile => {
                // Missiles bypass shields (see doc above) whatever the raw design field says.
                damage.shield = 0.0;
                (quantity * volley, 0.0)
            }
            // Filled in below, once every craft row is known.
            _ => (0.0, 0.0),
        };

        stats.push(ItemWeaponStats {
            item_design_id: item.item_design_id,
            item_name: design.item_design_name,
            item_type: item.item_type,
            quantity_deployed: item.quantity_deployed,
            volley,
            projectiles,
            shield_hit_projectiles,
            damage,
            explosion_type,
            explosion_radius,
        });
    }

    // Craft: share the ship's real attack / shield-hit counts between the craft rows.
    let craft_deployed: i32 = stats
        .iter()
        .filter(|r| r.item_type == ItemType::Craft)
        .map(|r| r.quantity_deployed)
        .sum();
    if craft_deployed > 0 {
        let attacks: i32 = ship.craft_reports.values().map(|c| c.attacks).sum();
        // Craft projectiles are the ones not tied to any room. With no projectile inventory at all
        // (older payloads) there's nothing to count, so assume every attack hit the shield.
        let shield_hits = if ship.missile_reports.is_empty() {
            attacks as f64
        } else {
            ship.missile_reports
                .values()
                .filter(|m| m.hit_shield && !ship.room_reports.contains_key(&m.original_room_id))
                .count() as f64
        };
        for row in stats.iter_mut().filter(|r| r.item_type == ItemType::Craft) {
            let share = row.quantity_deployed as f64 / craft_deployed as f64;
            row.projectiles = attacks as f64 * share;
            row.shield_hit_projectiles = shield_hits * share;
        }
    }

    stats
}

/// Sums `ItemWeaponStats::total` over the rows of one item type.
fn sum_item_rows(rows: &[ItemWeaponStats], item_type: ItemType) -> WeaponDamage {
    let mut totals = WeaponDamage::default();
    for row in rows.iter().filter(|r| r.item_type == item_type) {
        let t = row.total();
        totals.hull += t.hull;
        totals.system += t.system;
        totals.character += t.character;
        totals.shield += t.shield;
        totals.ap += t.ap;
        totals.craft += t.craft;
        totals.fire_ap += t.fire_ap;
        totals.fire_character += t.fire_character;
    }
    totals
}

/// Estimates the total damage dealt by a ship, across all its weapon rooms. Rooms whose design
/// isn't in the cache are silently skipped. Missile's and Carrier's ship-wide totals come from
/// `item_based_missile_damage`/`item_based_craft_damage` (handle any number of distinct item
/// designs, see their doc comments), then get redistributed across that room type's own rooms
/// weighted by real per-room shot counts (Carrier's from `CraftReport.attacks`, not
/// `RoomReport.activations` - see `estimate_room_damage`'s `shots_override` doc comment for why).
/// Everything else falls back to the (always absent, for these two room types) RoomDesign-level
/// stats.
pub fn estimate_ship_damage(ship: &ShipReport, cache: &DesignsCache) -> Result<ShipDamageEstimate> {
    let mut estimate = ShipDamageEstimate::default();

    // Real per-room Carrier shot count - see `estimate_room_damage`'s `shots_override` doc comment
    // for why `RoomReport.activations` can't be used here.
    let mut craft_attacks_by_room: HashMap<i32, i32> = HashMap::new();
    for craft in ship.craft_reports.values() {
        *craft_attacks_by_room
            .entry(craft.original_room_id)
            .or_insert(0) += craft.attacks;
    }

    for room in ship.room_reports.values() {
        let Some(design) = cache.get_room_design(room.room_design_id)? else {
            continue;
        };
        let shots_override = match room.room_type {
            RoomType::Carrier => Some(
                craft_attacks_by_room
                    .get(&room.room_id)
                    .copied()
                    .unwrap_or(0),
            ),
            _ => None,
        };
        let shield_hits = if ship.missile_reports.is_empty() {
            None
        } else {
            Some(
                ship.missile_reports
                    .values()
                    .filter(|m| m.original_room_id == room.room_id && m.hit_shield)
                    .count() as i32,
            )
        };
        let Some(stats) = estimate_room_damage(room, &design, shots_override, shield_hits) else {
            continue;
        };

        if !matches!(room.room_type, RoomType::Missile | RoomType::Carrier) {
            estimate.total_hull_damage += stats.estimated_hull_damage;
            estimate.total_system_damage += stats.estimated_system_damage;
            estimate.total_character_damage += stats.estimated_character_damage;
            estimate.total_shield_damage += stats.estimated_shield_damage;
            estimate.total_ap_damage += stats.estimated_ap_damage;
            estimate.total_craft_damage += stats.estimated_craft_damage;
            estimate.total_fire_ap_damage += stats.estimated_fire_ap_damage;
            estimate.total_fire_character_damage += stats.estimated_fire_character_damage;
        }
        estimate.rooms.push(stats);
    }

    let item_rows = item_weapon_stats(ship, cache);
    let missile_damage = sum_item_rows(&item_rows, ItemType::Missile);
    estimate.total_hull_damage += missile_damage.hull;
    estimate.total_system_damage += missile_damage.system;
    estimate.total_character_damage += missile_damage.character;
    estimate.total_shield_damage += missile_damage.shield;
    estimate.total_ap_damage += missile_damage.ap;
    estimate.total_craft_damage += missile_damage.craft;
    estimate.total_fire_ap_damage += missile_damage.fire_ap;
    estimate.total_fire_character_damage += missile_damage.fire_character;

    let craft_damage = sum_item_rows(&item_rows, ItemType::Craft);
    estimate.total_hull_damage += craft_damage.hull;
    estimate.total_system_damage += craft_damage.system;
    estimate.total_character_damage += craft_damage.character;
    estimate.total_shield_damage += craft_damage.shield;
    estimate.total_ap_damage += craft_damage.ap;
    estimate.total_craft_damage += craft_damage.craft;
    estimate.total_fire_ap_damage += craft_damage.fire_ap;
    estimate.total_fire_character_damage += craft_damage.fire_character;

    // Distribute each ship-wide item-based total (Missile, Craft) across that room type's own
    // rooms, weighted by each room's share of total shots fired within that type. This is still an
    // estimate (which specific room fired which specific missile/launched which specific craft
    // isn't in the wire data), but unlike leaving every room's per-room fields at 0, it (a) gives
    // the room table a plausible number instead of a flatly wrong one, and (b) makes every
    // *per-room-type* aggregation (Historique's chart, `weapon_type_history`,
    // `damage_by_room_type` - anything that sums RoomWeaponStats grouped by RoomType, which all
    // only ever see `estimate.rooms`, never `estimate.total_*`) add back up to the correct total.
    for (room_type, damage) in [
        (RoomType::Missile, missile_damage),
        (RoomType::Carrier, craft_damage),
    ] {
        let total_shots: i32 = estimate
            .rooms
            .iter()
            .filter(|r| r.room_type == room_type)
            .map(|r| r.shots_fired)
            .sum();
        if total_shots > 0 {
            for room in estimate
                .rooms
                .iter_mut()
                .filter(|r| r.room_type == room_type)
            {
                let share = room.shots_fired as f64 / total_shots as f64;
                room.estimated_hull_damage = damage.hull * share;
                room.estimated_system_damage = damage.system * share;
                room.estimated_character_damage = damage.character * share;
                room.estimated_shield_damage = damage.shield * share;
                room.estimated_ap_damage = damage.ap * share;
                room.estimated_craft_damage = damage.craft * share;
                room.estimated_fire_ap_damage = damage.fire_ap * share;
                room.estimated_fire_character_damage = damage.fire_character * share;
            }
        }
    }

    Ok(estimate)
}

/// Finds the single ammo item of a given type a ship deployed, if unambiguous (exactly one
/// distinct item design was used). Returns `None` when multiple different ammo designs of that
/// type were used. The report doesn't link ammo consumption to a specific room, so per-item
/// attribution isn't possible in that case.
fn find_ship_item_design(ship: &ShipReport, item_type: ItemType) -> Option<i32> {
    let mut ids: Vec<i32> = ship
        .item_reports
        .values()
        .filter(|item| item.item_type == item_type && item.quantity_deployed > 0)
        .map(|item| item.item_design_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    (ids.len() == 1).then(|| ids[0])
}

/// Result of a mono-weapon calibration attempt (§11.2): the empirical per-shot/per-attack damage
/// of an ammo item, derived from a battle where the attacker fired ONLY that ammo type.
#[derive(Debug, Clone, PartialEq)]
pub struct WeaponCalibrationResult {
    pub item_design_id: i32,
    pub shots: i32,
    pub hull_damage_per_shot: f64,
    pub system_damage_per_shot: f64,
    pub character_damage_per_shot: f64,
    pub shield_damage_per_shot: f64,
}

/// Damage the defender actually took, in real HP: hull, rooms ("system"), crew, shield. Every one of
/// these wire values is in hundredths: hull/shield per the pss-model docs, crew per CLAUDE.md, and
/// rooms per a check on the 35 captured battles (laser-only ships' estimated system damage matched
/// the defender's summed `RoomReport.damage_hp / 100` at 1.02 +/- 0.11).
fn defender_damage_taken(defender: &ShipReport) -> (f64, f64, f64, f64) {
    let hull = defender.hull_damage / 100.0; // centihp -> real HO, see pss-model docs
    let system: f64 = defender
        .room_reports
        .values()
        .map(|r| r.damage_hp)
        .sum::<f64>()
        / 100.0;
    let character: f64 = defender
        .character_reports
        .values()
        .map(|c| c.damage_hp)
        .sum::<f64>()
        / 100.0;
    let shield = defender.shield_damage / 100.0; // centihp -> real HP, same scale as hull

    (hull, system, character, shield)
}

/// Attempts to derive a missile ammo item's per-shot damage from one battle, by attributing 100%
/// of the defender's damage taken to the attacker's fire. Only works when the attacker's active
/// weapon rooms where EXCLUSIVELY `Missile` and exactly one missile ammo item design was deployed.
///
/// Caveat: attributes all of the defender's observed damage to the attacker's missiles, which
/// slightly overstates the estimate if the defender also self-damaged (e.g. a
/// `SetFire`/`PoisonRoom` ability side effect) during the same battle.
pub fn calibrate_mono_missile_damage(
    attacker: &ShipReport,
    defender: &ShipReport,
) -> Option<WeaponCalibrationResult> {
    let weapon_rooms: Vec<_> = attacker
        .room_reports
        .values()
        .filter(|room| room.shots_for(room.room_type).unwrap_or(0) > 0)
        .collect();
    if weapon_rooms.is_empty()
        || !weapon_rooms
            .iter()
            .all(|room| room.room_type == RoomType::Missile)
    {
        return None;
    }

    let item_design_id = find_ship_item_design(attacker, ItemType::Missile)?;
    let shots: i32 = weapon_rooms.iter().map(|room| room.missiles_deployed).sum();
    if shots == 0 {
        return None;
    }

    let (hull, system, character, shield) = defender_damage_taken(defender);
    Some(WeaponCalibrationResult {
        item_design_id,
        shots,
        hull_damage_per_shot: hull / shots as f64,
        system_damage_per_shot: system / shots as f64,
        character_damage_per_shot: character / shots as f64,
        shield_damage_per_shot: shield / shots as f64,
    })
}

/// Same idea as `calibrate_mono_missile_damage`, for Craft/drone damage: only works when the
/// attacker's active weapon rooms were EXCLUSIVELY `Carrier` and exactly one craft item design was
/// deployed. Shot volume is the sum of `craftReports.attacks` (reach deployed craft's own attack count) rather than a room-level counter.
pub fn calibrate_mono_craft_damage(
    attacker: &ShipReport,
    defender: &ShipReport,
) -> Option<WeaponCalibrationResult> {
    let weapon_rooms: Vec<_> = attacker
        .room_reports
        .values()
        .filter(|room| room.shots_for(room.room_type).unwrap_or(0) > 0)
        .collect();
    if weapon_rooms.is_empty()
        || !weapon_rooms
            .iter()
            .all(|room| room.room_type == RoomType::Carrier)
    {
        return None;
    }

    let item_design_id = find_ship_item_design(attacker, ItemType::Craft)?;
    let shots: i32 = attacker
        .craft_reports
        .values()
        .map(|craft| craft.attacks)
        .sum();
    if shots == 0 {
        return None;
    }

    let (hull, system, character, shield) = defender_damage_taken(defender);
    Some(WeaponCalibrationResult {
        item_design_id,
        shots,
        hull_damage_per_shot: hull / shots as f64,
        system_damage_per_shot: system / shots as f64,
        character_damage_per_shot: character / shots as f64,
        shield_damage_per_shot: shield / shots as f64,
    })
}

/// Damage-per-second for a given amount damage over a battle duration expressed in frames.
/// `frames_per_second` is caller-supplied because the PSS client's tick rate is not confirmed by
/// any source in docs/. Pass the value you've calibrated rather than assuming one here.
pub fn dps(damage: f64, duration_frames: i32, frames_per_second: f64) -> f64 {
    if duration_frames <= 0 || frames_per_second <= 0.0 {
        return 0.0;
    }

    let seconds = duration_frames as f64 / frames_per_second;

    damage / seconds
}

/// Hit-rate breakdown for a ship's fired projectiles, from its `missileReports` inventory (§9
/// "Warning": this is an inventory indexed by id, not an ordered event log - order carries no
/// meaning here).
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
pub struct HitRateStats {
    pub hit_target: u32,
    pub hit_shield: u32,
    pub missed: u32,
    /// Projectiles with none of `hitTarget`/`hitShield`/`missed` set - observed on the corpus for
    /// craft-originated shots with no `originalCraftId` (§9.1 note).
    pub unresolved: u32,
}

impl HitRateStats {
    pub fn total(&self) -> u32 {
        self.hit_target + self.hit_shield + self.missed + self.unresolved
    }

    /// Share of projectiles that connected (hit the hull or the shield), out of all fired
    /// projectiles. Returns 0.0 when nothing was fired.
    pub fn hit_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            0.0
        } else {
            (self.hit_target + self.hit_shield) as f64 / total as f64
        }
    }
}

/// Computes hit-rate stats for one ship from its missile report inventory.
pub fn compute_hit_rate(ship: &ShipReport) -> HitRateStats {
    let mut stats = HitRateStats::default();

    for missile in ship.missile_reports.values() {
        if missile.hit_target {
            stats.hit_target += 1;
        } else if missile.hit_shield {
            stats.hit_shield += 1;
        } else if missile.missed {
            stats.missed += 1;
        } else {
            stats.unresolved += 1;
        }
    }

    stats
}

/// Agregates a ship's estimated damage by room type (e.g. total damage dealt by all Laser rooms combined).
pub fn damage_by_room_type(estimate: &ShipDamageEstimate) -> HashMap<RoomType, f64> {
    let mut totals: HashMap<RoomType, f64> = HashMap::new();

    for room in &estimate.rooms {
        *totals.entry(room.room_type).or_insert(0.0) += room.estimated_hull_damage
            + room.estimated_system_damage
            + room.estimated_character_damage
            + room.estimated_shield_damage
            + room.estimated_ap_damage
            + room.estimated_craft_damage
            + room.estimated_fire_ap_damage
            + room.estimated_fire_character_damage;
    }

    totals
}

/// Share of incoming threats the defender evaded, vs. those that actually connected (hit hull or shield).
/// Doesn't count clean messes (`missed`, unrelated to active evasion) as either.
pub fn dodge_rate(attacker: &ShipReport, defender: &ShipReport) -> f64 {
    let attacker_hits = compute_hit_rate(attacker);
    let successful_hits = (attacker_hits.hit_target + attacker_hits.hit_shield) as f64;
    let dodges = defender.missile_dodges as f64;
    if dodges + successful_hits == 0.0 {
        0.0
    } else {
        dodges / (dodges + successful_hits)
    }
}

/// Active vs. destroyed room count, for a single `RoomType`/`CategoryType` bucket.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Default)]
pub struct RoomCount {
    pub active: u32,
    pub destroyed: u32,
}

/// Groups a ship's rooms by weapon/room type, counting active vs. destroyed.
pub fn rooms_by_type(ship: &ShipReport) -> HashMap<RoomType, RoomCount> {
    let mut counts: HashMap<RoomType, RoomCount> = HashMap::new();

    for room in ship.room_reports.values() {
        let entry = counts.entry(room.room_type).or_default();
        entry.active += 1;

        if room.is_destroyed {
            entry.destroyed += 1;
        }
    }

    counts
}

/// Groups a ship's rooms by category (Weapon/Defence/Support/...), counting active vs. destroyed.
pub fn rooms_by_category(ship: &ShipReport) -> HashMap<CategoryType, RoomCount> {
    let mut counts: HashMap<CategoryType, RoomCount> = HashMap::new();

    for room in ship.room_reports.values() {
        let entry = counts.entry(room.room_category).or_default();
        entry.active += 1;

        if room.is_destroyed {
            entry.destroyed += 1;
        }
    }

    counts
}

/// Total shield capacity for a ship, derived from `RoomDesign.Capacity` summed across its
/// `Shield`-type rooms that are still standing (destroyed shield generators contribute nothing).
/// The battleReport itself never transmits a max-shield value - this is the only way to get one,
/// since `Capacity` is otherwise unused for that room type.
pub fn ship_shield_capacity(ship: &ShipReport, cache: &DesignsCache) -> Result<f64> {
    let mut total = 0.0;
    for room in ship.room_reports.values() {
        if room.room_type != RoomType::Shield || room.is_destroyed {
            continue;
        }
        if let Some(design) = cache.get_room_design(room.room_design_id)? {
            total += design.capacity as f64;
        }
    }
    Ok(total)
}

/// EMP duration inflicted across all of a ship's rooms (raw frames; divide by `DEFAULT_FRAMES_PER_SECOND` for seconds).
pub fn total_emp_duration(ship: &ShipReport) -> i32 {
    ship.room_reports
        .values()
        .map(|room| room.emp_duration)
        .sum()
}

/// Aggregated module (equiped item) damage for a ship.
#[derive(Debug, Copy, Clone, PartialEq, Default)]
pub struct ModuleStats {
    pub destroyed_count: u32,
    pub total_damage: f64,
}

pub fn compute_module_stats(ship: &ShipReport) -> ModuleStats {
    let mut stats = ModuleStats::default();

    for module in ship.module_reports.values() {
        if module.is_destroyed {
            stats.destroyed_count += 1;
        }

        stats.total_damage += module.damage_hp;
    }

    stats
}

/// Aggregated craft/drone stats for a ship (attack count feeds `calibrate_mono_craft_damage`;
/// damage dealt is already in `ShipDamageEstimate` once a craft calibration exists).
#[derive(Debug, Copy, Clone, PartialEq, Default)]
pub struct CraftStats {
    pub destroyed_count: u32,
    pub total_attacks: i32,
    pub total_damage_taken: f64,
}

pub fn compute_craft_stats(ship: &ShipReport) -> CraftStats {
    let mut stats = CraftStats::default();

    for craft in ship.craft_reports.values() {
        if craft.is_destroyed {
            stats.destroyed_count += 1;
        }

        stats.total_attacks += craft.attacks;
        stats.total_damage_taken += craft.damage_hp;
    }

    stats
}

/// Drones launched from one Carrier room. `CraftReport.original_room_id` is the only link a drone
/// has to anything, so this is the finest breakdown that is exact (per craft *design* would be an
/// estimate, see `ItemWeaponStats::projectiles`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CarrierDroneStats {
    pub room_id: i32,
    pub deployed: u32,
    pub destroyed: u32,
    pub attacks: i32,
}

impl CarrierDroneStats {
    /// Share of the room's drones that survived the battle; `None` when it deployed none
    pub fn survival_rate(&self) -> Option<f64> {
        survival_rate(self.deployed, self.destroyed)
    }

    pub fn attacks_per_drone(&self) -> Option<f64> {
        (self.deployed > 0).then(|| self.attacks as f64 / self.deployed as f64)
    }
}

/// Survival and activity of a ship's drones, from `craftReports` (one entry per drone launched)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DroneSummary {
    pub deployed: u32,
    pub destroyed: u32,
    pub attacks: i32,
    /// One entry per Carrier room that launched drones, ordered by room id
    pub carriers: Vec<CarrierDroneStats>,
}

impl DroneSummary {
    pub fn survival_rate(&self) -> Option<f64> {
        survival_rate(self.deployed, self.destroyed)
    }

    pub fn attacks_per_drone(&self) -> Option<f64> {
        (self.deployed > 0).then(|| self.attacks as f64 / self.deployed as f64)
    }
}

fn survival_rate(deployed: u32, destroyed: u32) -> Option<f64> {
    (deployed > 0).then(|| (deployed - destroyed.min(deployed)) as f64 / deployed as f64)
}

pub fn drone_summary(ship: &ShipReport) -> DroneSummary {
    let mut by_room: HashMap<i32, CarrierDroneStats> = HashMap::new();

    for craft in ship.craft_reports.values() {
        let room = by_room
            .entry(craft.original_room_id)
            .or_insert_with(|| CarrierDroneStats {
                room_id: craft.original_room_id,
                ..CarrierDroneStats::default()
            });

        room.deployed += 1;
        room.attacks += craft.attacks;

        if craft.is_destroyed {
            room.destroyed += 1;
        }
    }

    let mut carriers: Vec<CarrierDroneStats> = by_room.into_values().collect();
    carriers.sort_by_key(|c| c.room_id);

    DroneSummary {
        deployed: carriers.iter().map(|c| c.deployed).sum(),
        destroyed: carriers.iter().map(|c| c.destroyed).sum(),
        attacks: carriers.iter().map(|c| c.attacks).sum(),
        carriers,
    }
}

/// Aggregated crew stats for a ship.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CrewStats {
    pub human_deaths: u32,
    pub android_deaths: u32,
    pub total_teleports: i32,
    pub abilities_triggered: HashMap<SpecialAbilityType, u32>,
}

pub fn compute_crew_stats(ship: &ShipReport) -> CrewStats {
    let mut stats = CrewStats::default();

    for crew in ship.character_reports.values() {
        if crew.is_dead {
            if crew.is_android {
                stats.android_deaths += 1;
            } else {
                stats.human_deaths += 1;
            }
        }

        stats.total_teleports += crew.teleports;

        if crew.ability_triggered {
            *stats
                .abilities_triggered
                .entry(crew.special_ability_type)
                .or_insert(0) += 1;
        }
    }

    stats
}

/// Total quantity deployed/consumed per item type (resources spent on repairs, ammo fired,
/// androids deployed, etc.) - see `itemReports` in pss-model.
pub fn item_consumption_by_type(ship: &ShipReport) -> HashMap<ItemType, i64> {
    let mut totals: HashMap<ItemType, i64> = HashMap::new();

    for item in ship.item_reports.values() {
        *totals.entry(item.item_type).or_insert(0) += item.quantity_deployed as i64;
    }

    totals
}

/// Ship-wide repair ratio: total HP repaired across all rooms, over total HP damage taken.
/// Returns 0.0 when no room took any damage.
pub fn ship_repair_ratio(ship: &ShipReport) -> f64 {
    let total_damage: f64 = ship.room_reports.values().map(|room| room.damage_hp).sum();
    let total_repaired: f64 = ship
        .room_reports
        .values()
        .map(|room| room.repaired_hp)
        .sum();

    if total_damage <= 0.0 {
        0.0
    } else {
        total_repaired / total_damage
    }
}

/// Per-room decomposition of `ship_repair_ratio`: each room's `repaired_hp` over the same
/// ship-wide total `damage_hp` denominator that function uses. Summing the returned values for
/// every room on a ship reconstructs `ship_repair_ratio` exactly. Only rooms that repaired
/// something are included - a room with no repair contribution isn't "0%", it's simply not part
/// of this ship's repair story, and callers should show that as absent rather than a bare 0.0.
pub fn room_repair_shares(ship: &ShipReport) -> HashMap<i32, f64> {
    let total_damage: f64 = ship.room_reports.values().map(|room| room.damage_hp).sum();
    if total_damage <= 0.0 {
        return HashMap::new();
    }

    ship.room_reports
        .values()
        .filter(|room| room.repaired_hp > 0.0)
        .map(|room| (room.room_id, room.repaired_hp / total_damage))
        .collect()
}

/// One crew member's stats, resolved against `CharacterDesign` for a name and a
/// percentage-of-max-HP reading. `FinalHp` is used as the reference (the report doesn't carry the
/// crew member's level, so the true max HP between `Hp` and `FinalHp` can't be pinned down exactly).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedCrewStat {
    pub character_id: i32,
    pub name: String,
    pub is_android: bool,
    pub is_dead: bool,
    /// In real HP units (already divided by 100 from the wire value - see CLAUDE.md: strong
    /// evidence CharacterReport.damageHp is centi-scaled like ship hull/shield, though full
    /// reconciliation against estimate_ship_damage's own totals isn't closed yet).
    pub damage_taken: f64,
    pub damage_taken_pct: Option<f64>,
    /// Same ÷100 scaling as `damage_taken`.
    pub healed: f64,
    pub healed_pct: Option<f64>,
    pub teleports: i32,
    pub ability_triggered: bool,
    pub special_ability_type: SpecialAbilityType,
}

/// Resolves every crew member on a ship against the `CharacterDesign` cache. Crew whose design
/// isn't cached (e.g. some androids) fall back to a placeholder name rather than being skipped.
pub fn named_crew_stats(ship: &ShipReport, cache: &DesignsCache) -> Result<Vec<NamedCrewStat>> {
    let mut stats = Vec::new();

    for crew in ship.character_reports.values() {
        let design = cache.get_character_design(crew.character_design_id)?;
        let (name, final_hp) = match design {
            Some(d) => (d.character_design_name, d.final_hp),
            None => (format!("Unknown crew #{}", crew.character_design_id), 0.0),
        };

        let damage_taken = crew.damage_hp / 100.0;
        let healed = crew.healed_hp / 100.0;
        let pct = |value: f64| (final_hp > 0.0).then(|| value / final_hp * 100.0);

        stats.push(NamedCrewStat {
            character_id: crew.character_id,
            name,
            is_android: crew.is_android,
            is_dead: crew.is_dead,
            damage_taken,
            damage_taken_pct: pct(damage_taken),
            healed,
            healed_pct: pct(healed),
            teleports: crew.teleports,
            ability_triggered: crew.ability_triggered,
            special_ability_type: crew.special_ability_type,
        });
    }

    Ok(stats)
}

/// Resolves a `winItems`/`loseItems` map (ItemDesignId as string -> quantity) against the `ItemDesign` cache for display. Unknown ids fall back to a placeholder name.
pub fn named_loot(
    items: &HashMap<String, i64>,
    cache: &DesignsCache,
) -> Result<Vec<(String, i64)>> {
    let mut named = Vec::with_capacity(items.len());

    for (item_design_id, quantity) in items {
        let id: i32 = item_design_id.parse().unwrap_or(0);
        let name = cache
            .get_item_design(id)?
            .map(|design| design.item_design_name)
            .unwrap_or_else(|| format!("Unknown item #{id}"));

        named.push((name, *quantity));
    }

    Ok(named)
}

/// Full per-ship battle report, bundling every stat this module can compute. `enemy` is the
/// opposing `ShipReport` (needed for `dodge_rate`, which crosses both sides).
#[derive(Debug, Clone, PartialEq)]
pub struct ShipBattleStats {
    pub damage: ShipDamageEstimate,
    pub hit_rate: HitRateStats,
    pub dodge_rate: f64,
    pub rooms_by_type: HashMap<RoomType, RoomCount>,
    pub rooms_by_category: HashMap<CategoryType, RoomCount>,
    pub total_emp_duration: i32,
    pub modules: ModuleStats,
    pub crafts: CraftStats,
    pub crew: CrewStats,
    pub items_deployed_by_type: HashMap<ItemType, i64>,
    pub repair_ratio: f64,
    pub named_crew: Vec<NamedCrewStat>,
}

pub fn compute_ship_battle_stats(
    ship: &ShipReport,
    enemy: &ShipReport,
    cache: &DesignsCache,
) -> Result<ShipBattleStats> {
    Ok(ShipBattleStats {
        damage: estimate_ship_damage(ship, cache)?,
        hit_rate: compute_hit_rate(ship),
        dodge_rate: dodge_rate(ship, enemy),
        rooms_by_type: rooms_by_type(ship),
        rooms_by_category: rooms_by_category(ship),
        total_emp_duration: total_emp_duration(ship),
        modules: compute_module_stats(ship),
        crafts: compute_craft_stats(ship),
        crew: compute_crew_stats(ship),
        items_deployed_by_type: item_consumption_by_type(ship),
        repair_ratio: ship_repair_ratio(ship),
        named_crew: named_crew_stats(ship, cache)?,
    })
}

/// Average DPS contribution of one room type, aggregated across many battles/ships.
#[derive(Debug, Copy, Clone, PartialEq, Default)]
pub struct WeaponTypeHistory {
    pub total_damage: f64,
    pub total_seconds: f64,
    /// Number of ship-battles where this room type dealt any damage (not room count).
    pub appearances: u32,
}

impl WeaponTypeHistory {
    pub fn dps(&self) -> f64 {
        if self.total_seconds <= 0.0 {
            0.0
        } else {
            self.total_damage / self.total_seconds
        }
    }
}

/// Aggregates estimated damage by room type across every stored battle, for a rolling "average DPS
/// per weapon type over time" view. Each ship-battle contributes its full battle duration once per
/// room type it used (not once per room), to avoid inflating the time base.
pub fn weapon_type_history(
    battles: &[StoredBattle],
    cache: &DesignsCache,
    frames_per_second: f64,
) -> Result<HashMap<RoomType, WeaponTypeHistory>> {
    let mut history: HashMap<RoomType, WeaponTypeHistory> = HashMap::new();

    for stored in battles {
        let report = &stored.payload.battle_report;
        let duration_seconds = report.last_frame as f64 / frames_per_second;

        // Only the attacker is "you" (see BattleStore::win_rate_by_attacking_ship's doc
        // comment) - the defender is always some opponent whose loadout shouldn't blend
        // into your own DPS.
        let ship = &report.attacking_ship_report;
        let estimate = estimate_ship_damage(ship, cache)?;
        let mut per_type_damage: HashMap<RoomType, f64> = HashMap::new();

        for room in &estimate.rooms {
            *per_type_damage.entry(room.room_type).or_insert(0.0) += room.estimated_hull_damage
                + room.estimated_system_damage
                + room.estimated_character_damage
                + room.estimated_shield_damage
                + room.estimated_ap_damage
                + room.estimated_craft_damage
                + room.estimated_fire_ap_damage
                + room.estimated_fire_character_damage;
        }

        for (room_type, damage) in per_type_damage {
            let entry = history.entry(room_type).or_default();
            entry.total_damage += damage;
            entry.total_seconds += duration_seconds;
            entry.appearances += 1;
        }
    }

    Ok(history)
}

/// One battle's DPS by weapon type, attacker side only (see `weapon_type_history`'s doc comment).
#[derive(Debug, Clone, PartialEq)]
pub struct BattleWeaponTypeDps {
    pub battle_id: i64,
    pub dps_by_type: HashMap<RoomType, f64>,
}

/// Per-battle DPS by weapon type, in insertion order - unlike `weapon_type_history`'s single
/// all-time aggregate, this keeps each battle separate for a "how did each weapon type perform
/// battle-by-battle" trend view.
pub fn battle_weapon_type_dps(
    battles: &[StoredBattle],
    cache: &DesignsCache,
    frames_per_second: f64,
) -> Result<Vec<BattleWeaponTypeDps>> {
    let mut history = Vec::with_capacity(battles.len());

    for stored in battles {
        let report = &stored.payload.battle_report;
        let duration_seconds = report.last_frame as f64 / frames_per_second;
        let estimate = estimate_ship_damage(&report.attacking_ship_report, cache)?;

        let mut dps_by_type: HashMap<RoomType, f64> = HashMap::new();
        for room in &estimate.rooms {
            let damage = room.estimated_hull_damage
                + room.estimated_system_damage
                + room.estimated_character_damage
                + room.estimated_shield_damage
                + room.estimated_ap_damage
                + room.estimated_craft_damage
                + room.estimated_fire_ap_damage
                + room.estimated_fire_character_damage;
            *dps_by_type.entry(room.room_type).or_insert(0.0) += damage;
        }
        if duration_seconds > 0.0 {
            for value in dps_by_type.values_mut() {
                *value /= duration_seconds;
            }
        }

        history.push(BattleWeaponTypeDps {
            battle_id: stored.id,
            dps_by_type,
        });
    }

    Ok(history)
}

/// Per-battle damage dealt/taken summary for both ships, for a battle-log-style trend view.
#[derive(Debug, Clone, PartialEq)]
pub struct BattleDamageHistoryEntry {
    pub battle_id: i64,
    pub imported_at: String,
    pub outcome: pss_model::OutcomeType,
    pub attacker_ship_design_id: i32,
    pub attacker_damage_dealt: f64,
    pub attacker_hull_lost: f64,
    pub defender_ship_design_id: i32,
    pub defender_damage_dealt: f64,
    pub defender_hull_lost: f64,
}

/// Builds a damage-dealt/taken history entry for every stored battle, in insertion order.
pub fn battle_damage_history(
    battles: &[StoredBattle],
    cache: &DesignsCache,
) -> Result<Vec<BattleDamageHistoryEntry>> {
    let mut history = Vec::with_capacity(battles.len());

    for stored in battles {
        let report = &stored.payload.battle_report;
        let attacker_estimate = estimate_ship_damage(&report.attacking_ship_report, cache)?;
        let defender_estimate = estimate_ship_damage(&report.defending_ship_report, cache)?;

        history.push(BattleDamageHistoryEntry {
            battle_id: stored.id,
            imported_at: stored.imported_at.clone(),
            outcome: report.battle_outcome,
            attacker_ship_design_id: report.attacking_ship_report.ship_design_id,
            attacker_damage_dealt: attacker_estimate.total_damage(),
            attacker_hull_lost: report.attacking_ship_report.hull_damage / 100.0,
            defender_ship_design_id: report.defending_ship_report.ship_design_id,
            defender_damage_dealt: defender_estimate.total_damage(),
            defender_hull_lost: report.defending_ship_report.hull_damage / 100.0,
        });
    }

    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pss_designs::{DesignsCache, parse_room_designs_xml};
    use pss_model::{CategoryType, MissileReport};
    use std::collections::HashMap as StdHashMap;

    const SAMPLE_DESIGNS_XML: &str = r#"<RoomService><ListRoomDesigns
  version="1"><RoomDesigns>
          <RoomDesign RoomDesignId="6" RoomName="Mineral Mining Laser
  Lv1" RoomType="Laser" ReloadTime="240"
  MissileDesignId="1" Capacity="0"><MissileDesign MissileDesignId="1" HullDamage="10"
  SystemDamage="2" CharacterDamage="1" ShieldDamage="0.5" /></RoomDesign>
          <RoomDesign RoomDesignId="1" RoomName="Bridge Lv1"
  RoomType="Bridge" ReloadTime="0" MissileDesignId="0" Capacity="11" />
      </RoomDesigns></ListRoomDesigns></RoomService>"#;

    fn cache_with_sample_designs() -> DesignsCache {
        let mut cache = DesignsCache::open_in_memory().expect("Cache should open");
        let (version, designs) = parse_room_designs_xml(SAMPLE_DESIGNS_XML).expect("Should parse");

        cache.upsert_room_designs(&designs).expect("Should upsert");
        cache
            .set_room_design_version(version)
            .expect("Should set version");

        cache
    }

    fn laser_room_report(shots_fired: i32) -> RoomReport {
        RoomReport {
            room_id: 1,
            room_design_id: 6,
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
            shots_fired,
            missiles_deployed: 0,
            crafts_deployed: 0,
            androids_deployed: 0,
        }
    }

    fn full_ship_report(room_reports: StdHashMap<i32, RoomReport>) -> ShipReport {
        ShipReport {
            room_reports,
            character_reports: StdHashMap::new(),
            module_reports: StdHashMap::new(),
            craft_reports: StdHashMap::new(),
            item_reports: StdHashMap::new(),
            ship_id: 1,
            ship_design_id: 1,
            shield_damage: 0.0,
            hull_damage: 0.0,
            current_ship_hp: 0.0,
            max_ship_hp: 2200.0,
            total_starting_crews: 6,
            missile_dodges: 0,
            manual_command_count: 0,
            cloak_count: 0,
            missile_reports: StdHashMap::new(),
        }
    }

    #[test]
    fn estimate_room_damage_multiplies_shots_by_base_damage() {
        let cache = cache_with_sample_designs();
        let room = laser_room_report(5);
        let design = cache
            .get_room_design(6)
            .unwrap()
            .expect("Laser design should be cached");
        let stats = estimate_room_damage(&room, &design, None, None)
            .expect("Laser room should produce stats");

        assert_eq!(stats.shots_fired, 5);
        assert!(stats.room_name.contains("Laser"));
        assert_eq!(stats.estimated_hull_damage, 50.0);
        assert_eq!(stats.estimated_system_damage, 10.0);
        assert_eq!(stats.estimated_character_damage, 5.0);
        assert_eq!(stats.estimated_shield_damage, 2.5);
    }

    #[test]
    fn estimate_room_damage_none_for_non_weapon_room() {
        let cache = cache_with_sample_designs();
        let mut room = laser_room_report(0);
        room.room_id = 2;
        room.room_design_id = 1;
        room.room_type = RoomType::Bridge;

        let design = cache
            .get_room_design(1)
            .unwrap()
            .expect("Bridge design should be cached");

        assert!(estimate_room_damage(&room, &design, None, None).is_none());
    }

    #[test]
    fn estimate_ship_damage_sums_across_rooms_and_skips_unknown_designs() {
        let cache = cache_with_sample_designs();

        let mut room_reports = StdHashMap::new();
        room_reports.insert(1, laser_room_report(3));

        let mut unknown_room = laser_room_report(99);
        unknown_room.room_id = 2;
        unknown_room.room_design_id = 999; // not in the cache
        room_reports.insert(2, unknown_room);

        let ship = ShipReport {
            room_reports,
            character_reports: StdHashMap::new(),
            module_reports: StdHashMap::new(),
            craft_reports: StdHashMap::new(),
            item_reports: StdHashMap::new(),
            ship_id: 1,
            ship_design_id: 1,
            shield_damage: 0.0,
            hull_damage: 0.0,
            current_ship_hp: 0.0,
            max_ship_hp: 2200.0,
            total_starting_crews: 6,
            missile_dodges: 0,
            manual_command_count: 0,
            cloak_count: 0,
            missile_reports: StdHashMap::new(),
        };

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.rooms.len(), 1); // The unknown was skipped
        assert_eq!(estimate.total_hull_damage, 30.0);
        assert_eq!(estimate.total_system_damage, 6.0);
        assert_eq!(estimate.total_character_damage, 3.0);
        assert_eq!(estimate.total_shield_damage, 1.5);
    }

    #[test]
    fn estimate_ship_damage_sums_ap_damage_from_missile_items() {
        // AP Damage (DirectSystemDamage) is a distinct mechanic from regular System Damage - see
        // CLAUDE.md and MissileDesignFullXml's doc comment - it must flow through as its own
        // total, not get folded into `total_system_damage`.
        use pss_designs::{ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[ItemDesignXml {
                item_design_id: 80,
                item_design_name: "Javelin".to_string(),
                item_type: "Missile".to_string(),
                item_sub_type: "None".to_string(),
                enhancement_type: "None".to_string(),
                enhancement_value: 0.0,
                module_type: "None".to_string(),
                module_argument: 0.0,
                missile_design_id: 40,
                craft_design_id: 0,
                rarity: "Common".to_string(),
            }])
            .unwrap();
        cache
            .upsert_missile_designs(&[MissileDesignFullXml {
                missile_design_id: 40,
                missile_design_name: "Javelin Missile".to_string(),
                hull_damage: 0.0,
                system_damage: 0.3,
                character_damage: 0.0,
                shield_damage: 0.0,
                ap_damage: 2.0,
                explosion_type: "Single".to_string(),
                explosion_radius: 0.0,
                fire_length: 0.0,
                craft_damage: 0.0,
                volley: 1.0,
            }])
            .unwrap();

        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 80,
                quantity_used: 0,
                original_quantity: 5,
                quantity_deployed: 5,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_ap_damage, 10.0); // 5 * 2.0
        assert_eq!(estimate.total_system_damage, 1.5); // 5 * 0.3 - unaffected, separate field
    }

    #[test]
    fn estimate_ship_damage_prefers_real_missile_design_over_calibration() {
        use pss_designs::{CalibratedItemDamageRecord, ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();

        // Item #99 is Missile ammo referencing MissileDesignId=5 - deliberately absent from
        // SAMPLE_DESIGNS_XML's rooms, since a Missile room never has one of its own (see
        // pss-designs module docs).
        cache
            .upsert_item_designs(&[ItemDesignXml {
                item_design_id: 99,
                item_design_name: "Test Rocket".to_string(),
                item_type: "Missile".to_string(),
                item_sub_type: "None".to_string(),
                enhancement_type: "None".to_string(),
                enhancement_value: 0.0,
                module_type: "None".to_string(),
                module_argument: 0.0,
                missile_design_id: 5,
                craft_design_id: 0,
                rarity: "Common".to_string(),
            }])
            .unwrap();

        // Real stats: hull=100 per shot - a value nothing else in this test could produce, so
        // seeing it in the result proves this path was actually used.
        cache
            .upsert_missile_designs(&[MissileDesignFullXml {
                missile_design_id: 5,
                missile_design_name: "Test Rocket Warhead".to_string(),
                hull_damage: 100.0,
                system_damage: 0.0,
                character_damage: 0.0,
                shield_damage: 0.0,
                ap_damage: 0.0,
                explosion_type: "Single".to_string(),
                explosion_radius: 0.0,
                fire_length: 0.0,
                craft_damage: 0.0,
                volley: 1.0,
            }])
            .unwrap();

        // A stale/wrong calibration for the same item - if this wins instead, the test fails.
        cache
            .upsert_calibrated_item_damage(&CalibratedItemDamageRecord {
                item_design_id: 99,
                hull_damage: 1.0,
                system_damage: 0.0,
                character_damage: 0.0,
                shield_damage: 0.0,
                calibrated_from_shots: 3,
            })
            .unwrap();

        let mut missile_room = laser_room_report(4);
        missile_room.room_type = RoomType::Missile;
        missile_room.missiles_deployed = 4;
        let mut room_reports = StdHashMap::new();
        room_reports.insert(1, missile_room);

        let mut ship = full_ship_report(room_reports);
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 99,
                quantity_used: 0,
                original_quantity: 4,
                quantity_deployed: 4,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_hull_damage, 400.0); // 4 shots * 100, not 4 * 1
    }

    #[test]
    fn estimate_ship_damage_sums_multiple_distinct_missile_items() {
        // Regression test: a real capture (battle #3 in this project's own store) has an attacker
        // deploying several different Missile item designs at once, which made
        // `find_ship_item_design` return None (ambiguous) and the whole Missile contribution
        // silently come out as 0 - even though we have real per-item damage for every one of them.
        // `estimate_ship_damage` must not depend on "exactly one Missile item type" at all.
        use pss_designs::{ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();

        cache
            .upsert_item_designs(&[
                ItemDesignXml {
                    item_design_id: 50,
                    item_design_name: "Rocket".to_string(),
                    item_type: "Missile".to_string(),
                    item_sub_type: "None".to_string(),
                    enhancement_type: "None".to_string(),
                    enhancement_value: 0.0,
                    module_type: "None".to_string(),
                    module_argument: 0.0,
                    missile_design_id: 10,
                    craft_design_id: 0,
                    rarity: "Common".to_string(),
                },
                ItemDesignXml {
                    item_design_id: 51,
                    item_design_name: "Scarlet".to_string(),
                    item_type: "Missile".to_string(),
                    item_sub_type: "None".to_string(),
                    enhancement_type: "None".to_string(),
                    enhancement_value: 0.0,
                    module_type: "None".to_string(),
                    module_argument: 0.0,
                    missile_design_id: 11,
                    craft_design_id: 0,
                    rarity: "Common".to_string(),
                },
            ])
            .unwrap();

        cache
            .upsert_missile_designs(&[
                MissileDesignFullXml {
                    missile_design_id: 10,
                    missile_design_name: "Rocket Warhead".to_string(),
                    hull_damage: 10.0,
                    system_damage: 0.0,
                    character_damage: 0.0,
                    shield_damage: 2.0,
                    ap_damage: 0.0,
                    explosion_type: "Single".to_string(),
                    explosion_radius: 0.0,
                    fire_length: 0.0,
                    craft_damage: 1.0,
                    volley: 1.0,
                },
                MissileDesignFullXml {
                    missile_design_id: 11,
                    missile_design_name: "Scarlet Warhead".to_string(),
                    hull_damage: 0.0,
                    system_damage: 3.0,
                    character_damage: 1.0,
                    shield_damage: 5.0,
                    ap_damage: 0.0,
                    explosion_type: "Single".to_string(),
                    explosion_radius: 0.0,
                    fire_length: 0.0,
                    craft_damage: 0.5,
                    volley: 1.0,
                },
            ])
            .unwrap();

        // No Missile rooms at all here on purpose - proves the ship-wide total doesn't depend on
        // room shots_fired for Missile, only on item consumption.
        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 50,
                quantity_used: 0,
                original_quantity: 6,
                quantity_deployed: 6,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );
        ship.item_reports.insert(
            2,
            ItemReport {
                item_id: 2,
                item_design_id: 51,
                quantity_used: 0,
                original_quantity: 2,
                quantity_deployed: 2,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_hull_damage, 60.0); // 6 Rocket * 10
        assert_eq!(estimate.total_system_damage, 6.0); // 2 Scarlet * 3
        assert_eq!(estimate.total_character_damage, 2.0); // 2 Scarlet * 1
        assert_eq!(estimate.total_craft_damage, 7.0); // 6*1.0 + 2*0.5
        // Missile ammo never deals real shield damage (see WeaponDamage's doc comment) even
        // though both designs carry a nonzero ShieldDamage field in the cache.
        assert_eq!(estimate.total_shield_damage, 0.0);
    }

    fn carrier_room_report(room_id: i32, activations: i32) -> RoomReport {
        RoomReport {
            room_id,
            room_design_id: 6,
            room_type: RoomType::Carrier,
            room_category: CategoryType::Weapon,
            damage_hp: 0.0,
            repaired_hp: 0.0,
            is_destroyed: false,
            emp_duration: 0,
            missiles_used: 0,
            craft_used: 0,
            androids_used: 0,
            activations,
            charges_used: 0,
            original_charges: 0,
            shots_fired: 0,
            missiles_deployed: 0,
            crafts_deployed: 0,
            androids_deployed: 0,
        }
    }

    fn craft_report(attacks: i32, original_room_id: i32) -> CraftReport {
        CraftReport {
            damage_hp: 0.0,
            is_destroyed: false,
            attacks,
            original_room_id,
        }
    }

    #[test]
    fn drone_summary_groups_drones_by_carrier_room() {
        let mut ship = full_ship_report(StdHashMap::new());
        let mut destroyed = craft_report(2, 10);

        destroyed.is_destroyed = true;
        ship.craft_reports.insert(1, craft_report(5, 10));
        ship.craft_reports.insert(2, destroyed);
        ship.craft_reports.insert(3, craft_report(4, 20));

        let summary = drone_summary(&ship);

        assert_eq!(summary.deployed, 3);
        assert_eq!(summary.destroyed, 1);
        assert_eq!(summary.attacks, 11);
        assert_eq!(summary.survival_rate(), Some(2.0 / 3.0));
        assert_eq!(summary.carriers.len(), 2);
        assert_eq!(summary.carriers[0].room_id, 10);
        assert_eq!(summary.carriers[0].deployed, 2);
        assert_eq!(summary.carriers[0].destroyed, 1);
        assert_eq!(summary.carriers[0].attacks_per_drone(), Some(3.5));
        assert_eq!(summary.carriers[1].survival_rate(), Some(1.0));
    }

    #[test]
    fn drone_summary_of_a_ship_without_drones_has_no_rates() {
        let summary = drone_summary(&full_ship_report(StdHashMap::new()));

        assert_eq!(summary, DroneSummary::default());
        assert_eq!(summary.survival_rate(), None);
        assert_eq!(summary.attacks_per_drone(), None);
    }

    #[test]
    fn estimate_ship_damage_sums_multiple_distinct_craft_items() {
        // Regression test: real captures in this project's own store show a ship deploying
        // several distinct Craft item designs at once is the *common* case (16 of ~60
        // ship-appearances), not an edge case - the old `find_ship_item_design`-gated
        // `resolve_craft_damage` made every one of those silently report 0 Craft damage.
        use pss_designs::{CraftDesignXml, ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();

        cache
            .upsert_item_designs(&[
                ItemDesignXml {
                    item_design_id: 60,
                    item_design_name: "Interceptor".to_string(),
                    item_type: "Craft".to_string(),
                    item_sub_type: "None".to_string(),
                    enhancement_type: "None".to_string(),
                    enhancement_value: 0.0,
                    module_type: "None".to_string(),
                    module_argument: 0.0,
                    missile_design_id: 0,
                    craft_design_id: 6,
                    rarity: "Common".to_string(),
                },
                ItemDesignXml {
                    item_design_id: 61,
                    item_design_name: "Firehawk".to_string(),
                    item_type: "Craft".to_string(),
                    item_sub_type: "None".to_string(),
                    enhancement_type: "None".to_string(),
                    enhancement_value: 0.0,
                    module_type: "None".to_string(),
                    module_argument: 0.0,
                    missile_design_id: 0,
                    craft_design_id: 7,
                    rarity: "Common".to_string(),
                },
            ])
            .unwrap();
        cache
            .upsert_craft_designs(&[
                CraftDesignXml {
                    craft_design_id: 6,
                    missile_design_id: 30,
                },
                CraftDesignXml {
                    craft_design_id: 7,
                    missile_design_id: 31,
                },
            ])
            .unwrap();
        cache
            .upsert_missile_designs(&[
                MissileDesignFullXml {
                    missile_design_id: 30,
                    missile_design_name: "Interceptor Laser".to_string(),
                    hull_damage: 0.0,
                    system_damage: 2.0,
                    character_damage: 0.0,
                    shield_damage: 0.0,
                    ap_damage: 0.0,
                    explosion_type: "Single".to_string(),
                    explosion_radius: 0.0,
                    fire_length: 0.0,
                    craft_damage: 0.0,
                    volley: 1.0,
                },
                MissileDesignFullXml {
                    missile_design_id: 31,
                    missile_design_name: "Firehawk Missile".to_string(),
                    hull_damage: 0.0,
                    system_damage: 0.0,
                    character_damage: 0.0,
                    shield_damage: 3.0,
                    ap_damage: 0.0,
                    explosion_type: "Single".to_string(),
                    explosion_radius: 0.0,
                    fire_length: 0.0,
                    craft_damage: 0.0,
                    volley: 1.0,
                },
            ])
            .unwrap();

        // No Carrier rooms at all here on purpose - proves the ship-wide total doesn't depend on
        // room activity at all, only on item consumption, same as Missile above.
        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 60,
                quantity_used: 0,
                original_quantity: 3,
                quantity_deployed: 3,
                item_type: ItemType::Craft,
                item_sub_type: 0,
            },
        );
        ship.item_reports.insert(
            2,
            ItemReport {
                item_id: 2,
                item_design_id: 61,
                quantity_used: 0,
                original_quantity: 2,
                quantity_deployed: 2,
                item_type: ItemType::Craft,
                item_sub_type: 0,
            },
        );

        // 5 drones deployed (3 Interceptor + 2 Firehawk) that made 10 attacks in total: the damage
        // follows the ATTACKS (the projectile count), shared between the craft rows by deployed
        // quantity - not the 5 deployed drones.
        for id in 1..=5 {
            ship.craft_reports.insert(id, craft_report(2, 99));
        }

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_system_damage, 12.0); // 10 attacks * 3/5 = 6 Interceptor shots * 2
        // Unlike Missile ammo, Craft's ShieldDamage is NOT forced to 0 - no equivalent real-panel
        // counter-evidence exists for drones (see WeaponDamage's doc comment). No projectile
        // inventory in this fixture, so every attack is assumed to have hit the shield.
        assert_eq!(estimate.total_shield_damage, 12.0); // 10 attacks * 2/5 = 4 Firehawk shots * 3

        let mut items = item_weapon_stats(&ship, &cache);
        items.sort_by_key(|i| i.item_design_id);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item_name, "Interceptor");
        assert_eq!(items[0].quantity_deployed, 3);
        assert_eq!(items[0].projectiles, 6.0);
        assert_eq!(items[0].damage.system, 2.0);
        assert_eq!(items[1].item_name, "Firehawk");
        assert_eq!(items[1].quantity_deployed, 2);
        assert_eq!(items[1].projectiles, 4.0);
        assert_eq!(items[1].damage.shield, 3.0);
    }

    #[test]
    fn item_weapon_stats_reports_aoe_as_a_fixed_stat_not_folded_into_damage() {
        // "Bombshell Strike" (a real Craft missile design) is both AP damage AND an AOE weapon
        // (DirectSystemDamage=1.5, ExplosionType=Radius, ExplosionRadius=200 in a live capture) -
        // both must show up as their own fields on the same row, and the AOE radius must not
        // change the computed `damage` total (see `ItemWeaponStats::explosion_type` doc comment).
        use pss_designs::{CraftDesignXml, ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[ItemDesignXml {
                item_design_id: 90,
                item_design_name: "Bombshell".to_string(),
                item_type: "Craft".to_string(),
                item_sub_type: "None".to_string(),
                enhancement_type: "None".to_string(),
                enhancement_value: 0.0,
                module_type: "None".to_string(),
                module_argument: 0.0,
                missile_design_id: 0,
                craft_design_id: 9,
                rarity: "Common".to_string(),
            }])
            .unwrap();
        cache
            .upsert_craft_designs(&[CraftDesignXml {
                craft_design_id: 9,
                missile_design_id: 115,
            }])
            .unwrap();
        cache
            .upsert_missile_designs(&[MissileDesignFullXml {
                missile_design_id: 115,
                missile_design_name: "Bombshell Strike".to_string(),
                hull_damage: 0.0,
                system_damage: 1.0,
                character_damage: 3.0,
                shield_damage: 0.0,
                ap_damage: 1.5,
                explosion_type: "Radius".to_string(),
                explosion_radius: 200.0,
                fire_length: 0.0,
                craft_damage: 0.0,
                volley: 1.0,
            }])
            .unwrap();

        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 90,
                quantity_used: 0,
                original_quantity: 2,
                quantity_deployed: 2,
                item_type: ItemType::Craft,
                item_sub_type: 0,
            },
        );

        // 2 Bombshells that made 4 attacks in total.
        ship.craft_reports.insert(1, craft_report(2, 99));
        ship.craft_reports.insert(2, craft_report(2, 99));

        let items = item_weapon_stats(&ship, &cache);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].projectiles, 4.0);
        assert_eq!(items[0].explosion_type, "Radius");
        assert_eq!(items[0].explosion_radius, 200.0);
        assert_eq!(items[0].damage.ap, 1.5); // per-shot, NOT multiplied by explosion_radius
        assert_eq!(items[0].damage.system, 1.0);
        assert_eq!(items[0].damage.character, 3.0);

        // Ship-wide total is still just projectiles * per-projectile damage - AOE never inflates it.
        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_ap_damage, 6.0); // 4 * 1.5
        assert_eq!(estimate.total_character_damage, 12.0); // 4 * 3.0
    }

    #[test]
    fn fire_damage_over_burn_matches_real_scarlet_lv3_fire_length() {
        // Regression test: real live-API value for "Scarlet Level 3" is FireLength=400 (verified
        // against RoomService/ListMissileDesigns this session). 10 ticks (400/40), remaining
        // duration per tick 400,360,...,40 - sum = 2200 frames total across the burn.
        // AP = 2200/500 = 4.4, Crew = 2200/200 = 11.0.
        let (ap, character) = fire_damage_over_burn(400.0);
        assert_eq!(ap, 4.4);
        assert_eq!(character, 11.0);
    }

    #[test]
    fn fire_damage_over_burn_is_zero_for_non_fire_weapons() {
        assert_eq!(fire_damage_over_burn(0.0), (0.0, 0.0));
    }

    #[test]
    fn item_weapon_stats_and_ship_totals_add_fire_damage_on_top_of_instant_fields() {
        // Regression test for a direct user report, backed by the real in-game weapon stats panel
        // for this exact design (a screenshot of "Scarlet Level 4"): it lists System Dmg 0.00, AP
        // Dmg 0.10, Crew Dmg 0.00, Craft Dmg 1.00, Incendiary 11.0s - there is NO Shield Dmg row at
        // all, despite the API's `ShieldDamage="5"` being nonzero (proof it's a vestigial field for
        // Missile ammo, not real damage - System/Crew being shown as "0.00" proves zero-valued
        // stats aren't simply hidden). Two corrections bundled here: (1) Fire Damage - a decaying
        // AP+Crew damage-over-time effect this project didn't compute at all before, added ON TOP
        // of the instant fields (never substituted - other real weapons, e.g. "Big Flying Rocket
        // Attack", combine large instant damage with a nonzero FireLength too); (2) Missile ammo's
        // `ShieldDamage` must never be counted as real damage, and the real `CraftDamage` field
        // must be. Both `item_weapon_stats` (per-item breakdown) and `estimate_ship_damage`
        // (ship-wide + per-room totals) must reflect all of this identically.
        use pss_designs::{ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[ItemDesignXml {
                item_design_id: 23,
                item_design_name: "Scarlet Lv4".to_string(),
                item_type: "Missile".to_string(),
                item_sub_type: "None".to_string(),
                enhancement_type: "None".to_string(),
                enhancement_value: 0.0,
                module_type: "None".to_string(),
                module_argument: 0.0,
                missile_design_id: 23,
                craft_design_id: 0,
                rarity: "Common".to_string(),
            }])
            .unwrap();
        // Real live-API values for MissileDesignId=23 ("Scarlet Level 4") - matches the screenshot
        // exactly: System=0, AP(DirectSystemDamage)=0.1, Crew=0, Craft=1, FireLength=440 (11.0s).
        cache
            .upsert_missile_designs(&[MissileDesignFullXml {
                missile_design_id: 23,
                missile_design_name: "Scarlet Level 4".to_string(),
                hull_damage: 0.0,
                system_damage: 0.0,
                character_damage: 0.0,
                shield_damage: 5.0,
                ap_damage: 0.1,
                explosion_type: "Single".to_string(),
                explosion_radius: 0.0,
                fire_length: 440.0,
                craft_damage: 1.0,
                volley: 1.0,
            }])
            .unwrap();

        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 23,
                quantity_used: 0,
                original_quantity: 2,
                quantity_deployed: 2,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );

        let items = item_weapon_stats(&ship, &cache);
        assert_eq!(items.len(), 1);
        // Per-shot (not multiplied by quantity_deployed) - real instant fields untouched...
        assert_eq!(items[0].damage.ap, 0.1);
        assert_eq!(items[0].damage.hull, 0.0);
        assert_eq!(items[0].damage.character, 0.0);
        assert_eq!(items[0].damage.craft, 1.0);
        // ...ShieldDamage forced to 0 despite the design's nonzero raw value...
        assert_eq!(items[0].damage.shield, 0.0);
        // ...and the fire-derived estimate added as its own separate fields. 11 ticks (440/40),
        // remaining duration per tick 440,400,...,40 - sum = 2640 frames. AP = 2640/500 = 5.28,
        // Crew = 2640/200 = 13.2. (float division, so compare with a tolerance)
        assert!((items[0].damage.fire_ap - 5.28).abs() < 1e-9);
        assert!((items[0].damage.fire_character - 13.2).abs() < 1e-9);

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_shield_damage, 0.0); // never 2 * 5.0
        assert_eq!(estimate.total_ap_damage, 0.2); // 2 * 0.1, unaffected
        assert_eq!(estimate.total_craft_damage, 2.0); // 2 * 1.0
        assert!((estimate.total_fire_ap_damage - 10.56).abs() < 1e-9); // 2 * 5.28
        assert!((estimate.total_fire_character_damage - 26.4).abs() < 1e-9); // 2 * 13.2
    }

    #[test]
    fn estimate_ship_damage_uses_craft_attacks_not_activations_for_redistribution() {
        // Regression test: `RoomReport.activations` is NOT a shot count for Carrier rooms
        // (empirically confirmed against real captures - see `estimate_room_damage`'s doc
        // comment), so redistributing by it would give each room the wrong share. This sets up
        // two Carrier rooms whose `activations` values would give the *opposite* share ordering
        // from their real `CraftReport.attacks` totals, so a bug reusing `activations` here would
        // fail this test's exact numbers rather than just being slightly off.
        use pss_designs::{CraftDesignXml, ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[ItemDesignXml {
                item_design_id: 70,
                item_design_name: "Interceptor".to_string(),
                item_type: "Craft".to_string(),
                item_sub_type: "None".to_string(),
                enhancement_type: "None".to_string(),
                enhancement_value: 0.0,
                module_type: "None".to_string(),
                module_argument: 0.0,
                missile_design_id: 0,
                craft_design_id: 8,
                rarity: "Common".to_string(),
            }])
            .unwrap();
        cache
            .upsert_craft_designs(&[CraftDesignXml {
                craft_design_id: 8,
                missile_design_id: 32,
            }])
            .unwrap();
        cache
            .upsert_missile_designs(&[MissileDesignFullXml {
                missile_design_id: 32,
                missile_design_name: "Interceptor Laser".to_string(),
                hull_damage: 0.0,
                system_damage: 10.0,
                character_damage: 0.0,
                shield_damage: 0.0,
                ap_damage: 0.0,
                explosion_type: "Single".to_string(),
                explosion_radius: 0.0,
                fire_length: 0.0,
                craft_damage: 0.0,
                volley: 1.0,
            }])
            .unwrap();

        // Room 1: high activations (5), low real attacks (1). Room 2: low activations (1), high
        // real attacks (9). A bug using `activations` would give room 1 the larger share (5/6);
        // using `attacks` correctly gives room 2 the larger share (9/10).
        let mut room_reports = StdHashMap::new();
        room_reports.insert(1, carrier_room_report(1, 5));
        room_reports.insert(2, carrier_room_report(2, 1));

        let mut ship = full_ship_report(room_reports);
        ship.craft_reports.insert(1, craft_report(1, 1));
        ship.craft_reports.insert(2, craft_report(9, 2));
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 70,
                quantity_used: 0,
                original_quantity: 4,
                quantity_deployed: 4,
                item_type: ItemType::Craft,
                item_sub_type: 0,
            },
        );

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_system_damage, 100.0); // 10 attacks (1 + 9) * 10, not 4 drones * 10

        let room1 = estimate.rooms.iter().find(|r| r.room_id == 1).unwrap();
        let room2 = estimate.rooms.iter().find(|r| r.room_id == 2).unwrap();
        assert_eq!(room1.shots_fired, 1);
        assert_eq!(room2.shots_fired, 9);
        assert_eq!(room1.estimated_system_damage, 10.0); // 100 * 1/10
        assert_eq!(room2.estimated_system_damage, 90.0); // 100 * 9/10
    }

    #[test]
    fn estimate_ship_damage_redistributes_missile_total_across_rooms_by_shot_share() {
        // Regression test: `battle_weapon_type_dps`/`weapon_type_history`/`damage_by_room_type`
        // (Historique's chart, `pss-cli history`, the GUI's room table) all group by RoomType by
        // summing `estimate.rooms`, never `estimate.total_*` - so fixing only the ship-wide total
        // (as the first version of this fix did) left every one of those views still reporting
        // Missile as 0, which is what was actually visible to a user. Each Missile room must carry
        // a share of the real total so grouping by RoomType recovers the correct number too.
        use pss_designs::{ItemDesignXml, MissileDesignFullXml};
        use pss_model::ItemReport;

        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[ItemDesignXml {
                item_design_id: 60,
                item_design_name: "Rocket".to_string(),
                item_type: "Missile".to_string(),
                item_sub_type: "None".to_string(),
                enhancement_type: "None".to_string(),
                enhancement_value: 0.0,
                module_type: "None".to_string(),
                module_argument: 0.0,
                missile_design_id: 20,
                craft_design_id: 0,
                rarity: "Common".to_string(),
            }])
            .unwrap();
        cache
            .upsert_missile_designs(&[MissileDesignFullXml {
                missile_design_id: 20,
                missile_design_name: "Rocket Warhead".to_string(),
                hull_damage: 10.0,
                system_damage: 0.0,
                character_damage: 0.0,
                shield_damage: 0.0,
                ap_damage: 0.0,
                explosion_type: "Single".to_string(),
                explosion_radius: 0.0,
                fire_length: 0.0,
                craft_damage: 0.0,
                volley: 1.0,
            }])
            .unwrap();

        // Two Missile rooms, 3:1 shot split - room A should get 75% of the total, room B 25%.
        let mut room_a = laser_room_report(3);
        room_a.room_id = 1;
        room_a.room_type = RoomType::Missile;
        room_a.missiles_deployed = 3;
        let mut room_b = laser_room_report(1);
        room_b.room_id = 2;
        room_b.room_type = RoomType::Missile;
        room_b.missiles_deployed = 1;
        let mut room_reports = StdHashMap::new();
        room_reports.insert(1, room_a);
        room_reports.insert(2, room_b);

        let mut ship = full_ship_report(room_reports);
        ship.item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 60,
                quantity_used: 0,
                original_quantity: 4,
                quantity_deployed: 4,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_hull_damage, 40.0); // 4 shots * 10, ship-wide

        let missile_rooms: Vec<_> = estimate
            .rooms
            .iter()
            .filter(|r| r.room_type == RoomType::Missile)
            .collect();
        assert_eq!(missile_rooms.len(), 2);
        let room_a_stats = missile_rooms.iter().find(|r| r.room_id == 1).unwrap();
        let room_b_stats = missile_rooms.iter().find(|r| r.room_id == 2).unwrap();
        assert_eq!(room_a_stats.estimated_hull_damage, 30.0); // 3/4 share of 40
        assert_eq!(room_b_stats.estimated_hull_damage, 10.0); // 1/4 share of 40

        // Grouping by RoomType (what Historique/history/the room table actually do) must recover
        // the exact ship-wide total, not 0.
        let by_type = damage_by_room_type(&estimate);
        assert_eq!(by_type[&RoomType::Missile], 40.0);
    }

    #[test]
    fn dps_divides_damage_by_elapsed_seconds() {
        // 1706 frames at a  hypothetical 10 fps = 170.6s
        assert_eq!(dps(1706.0, 1706, 10.0), 10.0);
        assert_eq!(dps(100.0, 0, 10.0), 0.0);
        assert_eq!(dps(100.0, 100, 0.0), 0.0);
    }

    #[test]
    fn compute_hit_rate_classifies_by_flag() {
        let mut missile_reports = StdHashMap::new();
        missile_reports.insert(
            1,
            MissileReport {
                original_room_id: 1,
                original_craft_id: 0,
                hit_target: true,
                hit_shield: false,
                missed: false,
            },
        );
        missile_reports.insert(
            2,
            MissileReport {
                original_room_id: 1,
                original_craft_id: 0,
                hit_target: false,
                hit_shield: true,
                missed: false,
            },
        );
        missile_reports.insert(
            3,
            MissileReport {
                original_room_id: 1,
                original_craft_id: 0,
                hit_target: false,
                hit_shield: false,
                missed: true,
            },
        );
        missile_reports.insert(
            4,
            MissileReport {
                original_room_id: 0,
                original_craft_id: 0,
                hit_target: false,
                hit_shield: false,
                missed: false,
            },
        );

        let ship = ShipReport {
            room_reports: StdHashMap::new(),
            character_reports: StdHashMap::new(),
            module_reports: StdHashMap::new(),
            craft_reports: StdHashMap::new(),
            item_reports: StdHashMap::new(),
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
            missile_reports,
        };

        let stats = compute_hit_rate(&ship);
        assert_eq!(stats.hit_target, 1);
        assert_eq!(stats.hit_shield, 1);
        assert_eq!(stats.missed, 1);
        assert_eq!(stats.unresolved, 1);
        assert_eq!(stats.total(), 4);
        assert_eq!(stats.hit_rate(), 0.5);
    }

    #[test]
    fn dodge_rate_weighs_dodges_against_successful_enemy_hits() {
        let mut defender = full_ship_report(StdHashMap::new());
        defender.missile_dodges = 3;

        let mut attacker_hits = StdHashMap::new();
        attacker_hits.insert(
            1,
            MissileReport {
                original_room_id: 1,
                original_craft_id: 0,
                hit_target: true,
                hit_shield: false,
                missed: false,
            },
        );

        let mut attacker = full_ship_report(StdHashMap::new());
        attacker.missile_reports = attacker_hits;

        // 3 dodges, 1 successful hit -> 3 / (3 + 1) = 0.75
        assert_eq!(dodge_rate(&attacker, &defender), 0.75)
    }

    #[test]
    fn rooms_by_type_counts_active_and_destroyed() {
        let mut destroyed_room = laser_room_report(0);
        destroyed_room.is_destroyed = true;

        let mut room_reports = StdHashMap::new();
        room_reports.insert(1, laser_room_report(0));
        room_reports.insert(2, destroyed_room);

        let ship = full_ship_report(room_reports);
        let counts = rooms_by_type(&ship);
        let laser_count = counts
            .get(&RoomType::Laser)
            .expect("Laser bucket should exist");

        assert_eq!(laser_count.active, 2);
        assert_eq!(laser_count.destroyed, 1);
    }

    fn item_row(id: i32, ty: &str, name: &str, md: i32, cd: i32) -> pss_designs::ItemDesignXml {
        pss_designs::ItemDesignXml {
            item_design_id: id,
            item_design_name: name.to_string(),
            item_type: ty.to_string(),
            item_sub_type: "None".to_string(),
            enhancement_type: "None".to_string(),
            enhancement_value: 0.0,
            module_type: "None".to_string(),
            module_argument: 0.0,
            missile_design_id: md,
            craft_design_id: cd,
            rarity: "Common".to_string(),
        }
    }

    fn missile_design(
        id: i32,
        name: &str,
        system: f64,
        shield: f64,
        volley: f64,
    ) -> pss_designs::MissileDesignFullXml {
        pss_designs::MissileDesignFullXml {
            missile_design_id: id,
            missile_design_name: name.to_string(),
            hull_damage: 0.0,
            system_damage: system,
            character_damage: 0.0,
            shield_damage: shield,
            ap_damage: 0.0,
            explosion_type: "Single".to_string(),
            explosion_radius: 0.0,
            fire_length: 0.0,
            craft_damage: 0.0,
            volley,
        }
    }

    fn item_report(item_design_id: i32, item_type: ItemType, qty: i32) -> pss_model::ItemReport {
        pss_model::ItemReport {
            item_id: item_design_id,
            item_design_id,
            quantity_used: 0,
            original_quantity: qty,
            quantity_deployed: qty,
            item_type,
            item_sub_type: 0,
        }
    }

    fn projectile(room_id: i32, hit_target: bool, hit_shield: bool, missed: bool) -> MissileReport {
        MissileReport {
            original_room_id: room_id,
            original_craft_id: 0,
            hit_target,
            hit_shield,
            missed,
        }
    }

    #[test]
    fn missile_items_fire_quantity_times_volley_projectiles() {
        // Real capture (battle #27): 15 Rocket + 10 Scarlet + 8 "Rocket Barrage" (Volley 10) made the
        // ship's Missile rooms report 105 projectiles = 15 + 10 + 8 * 10, not 33 launches. The
        // damage fields are per projectile, so a Barrage launch deals ten times its listed damage.
        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[
                item_row(50, "Missile", "Rocket", 10, 0),
                item_row(51, "Missile", "Rocket Barrage", 11, 0),
            ])
            .unwrap();
        cache
            .upsert_missile_designs(&[
                missile_design(10, "Rocket", 1.0, 5.0, 1.0),
                missile_design(11, "Rocket Barrage", 1.3, 1.0, 10.0),
            ])
            .unwrap();

        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports
            .insert(1, item_report(50, ItemType::Missile, 15));
        ship.item_reports
            .insert(2, item_report(51, ItemType::Missile, 8));

        let mut rows = item_weapon_stats(&ship, &cache);
        rows.sort_by_key(|r| r.item_design_id);
        assert_eq!(rows[0].projectiles, 15.0);
        assert_eq!(rows[1].quantity_deployed, 8); // launches, as the game's counter shows them
        assert_eq!(rows[1].volley, 10.0);
        assert_eq!(rows[1].projectiles, 80.0);

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert!((estimate.total_system_damage - (15.0 * 1.0 + 80.0 * 1.3)).abs() < 1e-9);
        // Missiles never hit a shield, whatever the raw ShieldDamage field says.
        assert_eq!(estimate.total_shield_damage, 0.0);
    }

    #[test]
    fn laser_shield_damage_counts_only_projectiles_that_hit_the_shield() {
        // Real captures: counting ShieldDamage for every shot overshot the observed shield damage
        // ~5x; counting only the projectiles tagged `hit_shield` for the room matched it (1.05).
        // System damage still counts every shot (it matched the observed room damage as is).
        // The sample Laser design (id 6) deals system 2 / shield 0.5 per shot.
        let mut ship = full_ship_report({
            let mut rooms = StdHashMap::new();
            rooms.insert(1, laser_room_report(4));
            rooms
        });
        ship.missile_reports
            .insert(1, projectile(1, false, true, false)); // hit the shield
        ship.missile_reports
            .insert(2, projectile(1, true, false, false)); // hit the target
        ship.missile_reports
            .insert(3, projectile(1, true, false, false));
        ship.missile_reports
            .insert(4, projectile(1, false, false, true)); // missed
        // a projectile from another room must not be counted for this one
        ship.missile_reports
            .insert(5, projectile(2, false, true, false));

        let estimate = estimate_ship_damage(&ship, &cache_with_sample_designs())
            .expect("Estimate should succeed");
        assert_eq!(estimate.total_system_damage, 8.0); // 4 shots * 2
        assert_eq!(estimate.total_shield_damage, 0.5); // 1 shield hit * 0.5, not 4 * 0.5
        assert_eq!(estimate.rooms[0].estimated_shield_damage, 0.5);

        // Without any projectile inventory there is nothing to weight by: every shot is assumed to
        // hit the shield (the pre-existing behaviour) instead of silently reporting 0.
        ship.missile_reports.clear();
        let estimate = estimate_ship_damage(&ship, &cache_with_sample_designs())
            .expect("Estimate should succeed");
        assert_eq!(estimate.total_shield_damage, 2.0); // 4 * 0.5
    }

    #[test]
    fn craft_shield_damage_counts_only_craft_projectiles_that_hit_the_shield() {
        // Craft projectiles are the ones with no matching room (`original_room_id` 0 in real
        // captures); their count equals the craft attacks exactly.
        use pss_designs::CraftDesignXml;
        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[item_row(70, "Craft", "Shieldbuster", 0, 8)])
            .unwrap();
        cache
            .upsert_craft_designs(&[CraftDesignXml {
                craft_design_id: 8,
                missile_design_id: 32,
            }])
            .unwrap();
        cache
            .upsert_missile_designs(&[missile_design(32, "Shieldbuster Beam", 0.0, 4.0, 3.0)])
            .unwrap();

        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports
            .insert(1, item_report(70, ItemType::Craft, 2));
        ship.craft_reports.insert(1, craft_report(3, 99));
        ship.craft_reports.insert(2, craft_report(3, 99));
        for (id, (hit_target, hit_shield)) in [
            (false, true),
            (false, true),
            (true, false),
            (true, false),
            (true, false),
            (true, false),
        ]
        .into_iter()
        .enumerate()
        {
            ship.missile_reports
                .insert(id as i32, projectile(0, hit_target, hit_shield, false));
        }

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_shield_damage, 8.0); // 2 shield hits * 4, not 6 attacks * 4
    }

    #[test]
    fn healing_is_not_negative_damage() {
        // The Repair Drone's "Repair Beam" has SystemDamage = -0.1 in the live API. Left negative it
        // subtracted from the totals (`Carrier: -4.0 total damage`); a heal isn't damage.
        use pss_designs::CraftDesignXml;
        let mut cache = cache_with_sample_designs();
        cache
            .upsert_item_designs(&[item_row(80, "Craft", "Repair Drone", 0, 9)])
            .unwrap();
        cache
            .upsert_craft_designs(&[CraftDesignXml {
                craft_design_id: 9,
                missile_design_id: 33,
            }])
            .unwrap();
        cache
            .upsert_missile_designs(&[missile_design(33, "Repair Beam", -0.1, 0.0, 10.0)])
            .unwrap();

        let mut ship = full_ship_report(StdHashMap::new());
        ship.item_reports
            .insert(1, item_report(80, ItemType::Craft, 2));
        ship.craft_reports.insert(1, craft_report(10, 99));

        let estimate = estimate_ship_damage(&ship, &cache).expect("Estimate should succeed");
        assert_eq!(estimate.total_system_damage, 0.0);
        assert_eq!(estimate.total_damage(), 0.0);
    }

    #[test]
    fn total_damage_sums_every_component_and_history_uses_it() {
        let estimate = ShipDamageEstimate {
            rooms: Vec::new(),
            total_hull_damage: 1.0,
            total_system_damage: 2.0,
            total_character_damage: 3.0,
            total_shield_damage: 4.0,
            total_ap_damage: 5.0,
            total_craft_damage: 6.0,
            total_fire_ap_damage: 7.0,
            total_fire_character_damage: 8.0,
        };
        assert_eq!(estimate.total_damage(), 36.0);
    }

    #[test]
    fn calibrate_mono_missile_damage_requires_pure_missile_attacker() {
        use pss_model::ItemReport;

        let mut missile_room = laser_room_report(0);
        missile_room.room_type = RoomType::Missile;
        missile_room.missiles_deployed = 10;

        let mut room_reports = StdHashMap::new();
        room_reports.insert(1, missile_room);

        let mut item_reports = StdHashMap::new();
        item_reports.insert(
            1,
            ItemReport {
                item_id: 1,
                item_design_id: 23,
                quantity_used: 0,
                original_quantity: 10,
                quantity_deployed: 10,
                item_type: ItemType::Missile,
                item_sub_type: 0,
            },
        );

        let mut attacker = full_ship_report(room_reports);
        attacker.item_reports = item_reports;

        let mut defender = full_ship_report(StdHashMap::new());
        defender.hull_damage = 500.0; // 5.0 real HP after /100
        defender.shield_damage = 50.0; // 0.5 real HP after /100

        let mut damaged_room = laser_room_report(0);
        damaged_room.damage_hp = 2000.0; // 20.0 real HP after /100 (rooms are in hundredths too)

        let mut defender_rooms = StdHashMap::new();
        defender_rooms.insert(1, damaged_room);
        defender.room_reports = defender_rooms;

        let result = calibrate_mono_missile_damage(&attacker, &defender)
            .expect("Pure-missile attacker should calibrate");
        assert_eq!(result.item_design_id, 23);
        assert_eq!(result.shots, 10);
        assert_eq!(result.hull_damage_per_shot, 0.5);
        assert_eq!(result.system_damage_per_shot, 2.0);
        assert_eq!(result.shield_damage_per_shot, 0.05);
    }

    #[test]
    fn compute_crew_stats_separates_human_and_android_deaths() {
        use pss_model::CharacterReport;

        let mut character_reports = StdHashMap::new();
        character_reports.insert(
            1,
            CharacterReport {
                character_id: 1,
                character_design_id: 1,
                is_dead: true,
                teleports: 2,
                damage_hp: 10.0,
                healed_hp: 0.0,
                collection_design_id: 0,
                is_in_active_collection: false,
                is_android: false,
                ability_triggered: true,
                special_ability_type: SpecialAbilityType::AddReload,
                original_room_id: 1,
            },
        );
        character_reports.insert(
            2000000001,
            CharacterReport {
                character_id: 2000000001,
                character_design_id: 2,
                is_dead: true,
                teleports: 0,
                damage_hp: 5.0,
                healed_hp: 0.0,
                collection_design_id: 0,
                is_in_active_collection: false,
                is_android: true,
                ability_triggered: false,
                special_ability_type: SpecialAbilityType::None,
                original_room_id: 1,
            },
        );

        let ship = full_ship_report(StdHashMap::new()).clone();
        let mut ship = ship;
        ship.character_reports = character_reports;

        let stats = compute_crew_stats(&ship);
        assert_eq!(stats.human_deaths, 1);
        assert_eq!(stats.android_deaths, 1);
        assert_eq!(stats.total_teleports, 2);
        assert_eq!(
            stats
                .abilities_triggered
                .get(&SpecialAbilityType::AddReload),
            Some(&1)
        );
    }
}
