use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pss_designs::DesignsCache;
use pss_model::{FinaliseBattlePayload, ItemType};
use pss_stats::{DEFAULT_FRAMES_PER_SECOND, compute_ship_battle_stats, dps};
use pss_store::{BattleStore, InsertOutcome};

#[derive(Parser)]
#[command(
    name = "pss-cli",
    about = "PSS-BattleStats terminal tool: import battle captures and query stats"
)]
struct Cli {
    /// Path to the battle store SQLite database.
    #[arg(long, default_value_t = pss_store::paths::app_paths().battles_db().to_string_lossy().into_owned())]
    db: String,

    /// Path to the designs cache SQLite database.
    #[arg(long, default_value_t = pss_store::paths::app_paths().designs_db().to_string_lossy().into_owned())]
    designs_db: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode a captured FinaliseBattle request body and store it.
    Import {
        /// Path to a raw capture file (e.g. produced by pss-capture).
        path: PathBuf,
    },
    /// List stored battles, most recent first.
    List {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Show damage/hit-rate stats for one stored battle.
    Stats {
        battle_id: i64,
        /// Frames per second used to convert lastFrame into seconds for DPS.
        /// Defaults to the sourced PSS client rate (see pss_stats::DEFAULT_FRAMES_PER_SECOND).
        #[arg(long, default_value_t = DEFAULT_FRAMES_PER_SECOND)]
        fps: f64,
        /// Refresh the room designs cache from the live PSS API before computing stats.
        #[arg(long)]
        sync_designs: bool,
        /// Resolve trophies/alliance for any ship identity captured via CreateBattle but not yet
        /// profiled (UserService/SearchUsers, public, no access token needed).
        #[arg(long)]
        sync_identities: bool,
    },
    /// Attempt mono-weapon calibration (Missile/Craft) on one stored battle and persist any
    /// successful result into the designs cache, refining it if a prior calibration exists.
    Calibrate { battle_id: i64 },
    /// Multi-battle history: your win rate (by account) and average DPS by weapon type.
    History {
        #[arg(long, default_value_t = DEFAULT_FRAMES_PER_SECOND)]
        fps: f64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let _ = pss_store::paths::app_paths().ensure_dirs();

    match cli.command {
        Command::Import { path } => import(&cli.db, &path),
        Command::List { limit } => list(&cli.db, limit),
        Command::Stats {
            battle_id,
            fps,
            sync_designs,
            sync_identities,
        } => {
            stats(
                &cli.db,
                &cli.designs_db,
                battle_id,
                fps,
                sync_designs,
                sync_identities,
            )
            .await
        }
        Command::Calibrate { battle_id } => calibrate(&cli.db, &cli.designs_db, battle_id),
        Command::History { fps } => history(&cli.db, &cli.designs_db, fps),
    }
}

async fn stats(
    db_path: &str,
    designs_db_path: &str,
    battle_id: i64,
    fps: f64,
    sync_designs: bool,
    sync_identities: bool,
) -> Result<()> {
    let store = BattleStore::open(db_path)?;

    if sync_identities {
        let http = reqwest::Client::new();
        let resolved = store.sync_ship_profiles(&http).await?;
        println!("Ship identities resolved: {resolved}");
    }
    let stored = store.get_battle(battle_id)?.with_context(|| {
        format!(
            "no battle #{battle_id} in
  {db_path}"
        )
    })?;

    let mut designs = DesignsCache::open(designs_db_path)?;
    if sync_designs {
        let http = reqwest::Client::new();
        let rooms = pss_designs::sync_room_designs(&mut designs, &http).await?;
        let items = pss_designs::sync_item_designs(&mut designs, &http).await?;
        let characters = pss_designs::sync_character_designs(&mut designs, &http).await?;
        let missiles = pss_designs::sync_missile_designs(&mut designs, &http).await?;
        let crafts = pss_designs::sync_craft_designs(&mut designs, &http).await?;

        println!("Room designs sync: {rooms:?}");
        println!("Item designs sync: {items:?}");
        println!("Character designs sync: {characters:?}");
        println!("Missile designs sync: {missiles:?}");
        println!("Craft designs sync: {crafts:?}");
    }

    let report = &stored.payload.battle_report;
    println!(
        "Battle #{battle_id} - outcome: {:?}, duration: {} frames
  ({:.1}s @ {:.0} fps)",
        report.battle_outcome,
        report.last_frame,
        report.last_frame as f64 / fps,
        fps
    );
    println!();

    let win_loot = pss_stats::named_loot(&report.win_items, &designs)?;
    if !win_loot.is_empty() {
        print!("Win items:");

        for (name, qty) in &win_loot {
            print!(" {name}x{qty}");
        }
        println!();
    }

    let lose_loot = pss_stats::named_loot(&report.lose_items, &designs)?;
    if !lose_loot.is_empty() {
        print!("Lose items:");

        for (name, qty) in &lose_loot {
            print!(" {name}x{qty}");
        }
        println!();
    }
    println!();

    let sides = [
        (
            "Attacker",
            &report.attacking_ship_report,
            &report.defending_ship_report,
        ),
        (
            "Defender",
            &report.defending_ship_report,
            &report.attacking_ship_report,
        ),
    ];

    for (label, ship, enemy) in sides {
        let stats = compute_ship_battle_stats(ship, enemy, &designs)?;
        let total_damage = stats.damage.total_damage();

        println!(
            "{label} (ship #{}, design #{})",
            ship.ship_id, ship.ship_design_id
        );
        if let Some(identity) = store.get_ship_identity(ship.ship_id)? {
            print!(
                "   {} (niveau {}, race #{}, power {})",
                identity.ship_name,
                identity.ship_level,
                identity.original_race_id,
                identity.power_score
            );
            match (identity.trophy, identity.alliance_name.as_deref()) {
                (Some(trophy), Some(alliance)) => println!(" - {trophy} trophées, [{alliance}]"),
                (Some(trophy), None) => println!(" - {trophy} trophées, sans alliance"),
                _ => println!(" - trophées/alliance non résolus (--sync-identities)"),
            }
        }
        println!(
            "   hull: {:.0}/{:.0} HP, shield damage taken: {:.1}",
            ship.max_ship_hp / 100.0 - ship.hull_damage / 100.0,
            ship.max_ship_hp / 100.0,
            ship.shield_damage / 100.0
        );

        if stats.repair_ratio > 0.0 {
            println!("  repair ratio: {:.1}%", stats.repair_ratio * 100.0);
        }

        println!(
            "   estimated damage dealt - hull: {:.1}, system: {:.1}, shield: {:.1},
  character: {:.1}, AP: {:.1}, craft: {:.1} (across {} weapon rooms, {:.2} DPS)",
            stats.damage.total_hull_damage,
            stats.damage.total_system_damage,
            stats.damage.total_shield_damage,
            stats.damage.total_character_damage,
            stats.damage.total_ap_damage,
            stats.damage.total_craft_damage,
            stats.damage.rooms.len(),
            dps(total_damage, report.last_frame, fps)
        );
        if stats.damage.total_fire_ap_damage > 0.0 || stats.damage.total_fire_character_damage > 0.0
        {
            println!(
                "   + fire damage (est., see 'missiles'/'drones' below) - AP: {:.1}, character: {:.1}",
                stats.damage.total_fire_ap_damage, stats.damage.total_fire_character_damage
            );
        }
        println!(
            "   hit rate: {:.1}% ({} hit target, {} hit shield, {}
  missed, {} unresolved) - dodge rate: {:.1}%",
            stats.hit_rate.hit_rate() * 100.0,
            stats.hit_rate.hit_target,
            stats.hit_rate.hit_shield,
            stats.hit_rate.missed,
            stats.hit_rate.unresolved,
            stats.dodge_rate * 100.0
        );

        print!("   rooms by type:");
        for (room_type, count) in &stats.rooms_by_type {
            print!(
                " {room_type:?}={}/{}",
                count.active - count.destroyed,
                count.active
            );
        }
        println!();

        if stats.total_emp_duration > 0 {
            println!(
                "   EMP total: {} frames ({:.1}s)",
                stats.total_emp_duration,
                stats.total_emp_duration as f64 / fps
            );
        }

        if stats.modules.destroyed_count > 0 || stats.modules.total_damage > 0.0 {
            println!(
                "   modules destroyed: {}, total module damage:
  {:.1}",
                stats.modules.destroyed_count, stats.modules.total_damage
            );
        }

        if stats.crafts.total_attacks > 0 || stats.crafts.destroyed_count > 0 {
            println!(
                "   crafts destroyed: {}, total craft attacks: {},
  craft damage taken: {:.1}",
                stats.crafts.destroyed_count,
                stats.crafts.total_attacks,
                stats.crafts.total_damage_taken
            );
        }

        println!(
            "   crew deaths: {} human, {} android - teleports: {}",
            stats.crew.human_deaths, stats.crew.android_deaths, stats.crew.total_teleports
        );
        if !stats.crew.abilities_triggered.is_empty() {
            print!("    abilities triggered:");
            for (ability, count) in &stats.crew.abilities_triggered {
                print!("{ability:?}={count}");
            }
            println!();
        }

        for crew in &stats.named_crew {
            let status = if crew.is_dead { "dead" } else { "alive" };
            let kind = if crew.is_android { "android" } else { "human" };

            print!("    - {} [{kind}, {status}]", crew.name);
            if let Some(pct) = crew.damage_taken_pct {
                print!(", {pct:.0}% dmg taken ({:.1} HP)", crew.damage_taken);
            } else if crew.damage_taken > 0.0 {
                print!(", {:.1} HP dmg taken", crew.damage_taken);
            }
            if let Some(pct) = crew.healed_pct {
                if pct > 0.0 {
                    print!(", {pct:.0}% healed");
                }
            }
            if crew.teleports > 0 {
                print!(", {} teleports", crew.teleports);
            }
            if crew.ability_triggered {
                print!(", triggered {:?}", crew.special_ability_type);
            }
            println!();
        }

        let all_items = pss_stats::item_weapon_stats(ship, &designs);
        for (label, item_type) in [("missiles", ItemType::Missile), ("drones", ItemType::Craft)] {
            let items: Vec<_> = all_items
                .iter()
                .filter(|i| i.item_type == item_type)
                .collect();
            if items.is_empty() {
                continue;
            }
            println!("   {label}:");
            for item in items {
                let fire = item.damage.fire_ap + item.damage.fire_character;
                let t = item.total();
                let total = t.hull
                    + t.system
                    + t.character
                    + t.shield
                    + t.ap
                    + t.craft
                    + t.fire_ap
                    + t.fire_character;
                print!(
                    "    - {} x{} ({:.0} projectiles, volley {:.0}) - {:.1} total dmg (hull={:.2} system={:.2} shield={:.2} character={:.2} AP={:.2} craft={:.2} per projectile)",
                    item.item_name,
                    item.quantity_deployed,
                    item.projectiles,
                    item.volley,
                    total,
                    item.damage.hull,
                    item.damage.system,
                    item.damage.shield,
                    item.damage.character,
                    item.damage.ap,
                    item.damage.craft
                );
                if fire > 0.0 {
                    print!(
                        ", + fire (est.) AP={:.2} character={:.2} per projectile",
                        item.damage.fire_ap, item.damage.fire_character
                    );
                }
                if item.explosion_type != "Single" {
                    print!(
                        ", AOE {} (r={:.0})",
                        item.explosion_type, item.explosion_radius
                    );
                }
                println!();
            }
        }

        let items: Vec<_> = stats
            .items_deployed_by_type
            .iter()
            .filter(|(_, qty)| **qty > 0)
            .collect();
        if !items.is_empty() {
            print!("   items deployed:");
            for (item_type, qty) in items {
                print!(" {item_type:?}={qty}");
            }
            println!();
        }

        println!();
    }

    Ok(())
}

fn calibrate(db_path: &str, designs_db_path: &str, battle_id: i64) -> Result<()> {
    let store = BattleStore::open(db_path)?;
    let stored = store
        .get_battle(battle_id)?
        .with_context(|| format!("No battle #{battle_id} in {db_path}"))?;
    let designs = DesignsCache::open(designs_db_path)?;

    let report = &stored.payload.battle_report;
    let sides = [
        (
            "Attacker",
            &report.attacking_ship_report,
            &report.defending_ship_report,
        ),
        (
            "Defender",
            &report.defending_ship_report,
            &report.attacking_ship_report,
        ),
    ];

    for (label, ship, enemy) in sides {
        match pss_stats::calibrate_mono_missile_damage(ship, enemy) {
            Some(result) => apply_calibration(&designs, label, "Missile", result)?,
            None => println!(
                "{label}: not mono-missile (mixed weapons, or ambiguous/no missile ammo) - skipped"
            ),
        }

        match pss_stats::calibrate_mono_craft_damage(ship, enemy) {
            Some(result) => apply_calibration(&designs, label, "Craft", result)?,
            None => println!(
                "{label}: not mono-craft (mixed weapons, or ambiguous/no craft item) - skipped"
            ),
        }
    }

    Ok(())
}

/// Merges a new calibration result into any existing one for the same item, weighted by shot
/// count, then persists it. More calibration runs on more battles -> a more reliable average.
fn apply_calibration(
    designs: &DesignsCache,
    label: &str,
    kind: &str,
    result: pss_stats::WeaponCalibrationResult,
) -> Result<()> {
    let existing = designs.get_calibrated_item_damage(result.item_design_id)?;
    let merged = match existing {
        Some(e) => {
            let total_shots = e.calibrated_from_shots + result.shots;
            let (we, wn) = (e.calibrated_from_shots as f64, result.shots as f64);

            pss_designs::CalibratedItemDamageRecord {
                item_design_id: result.item_design_id,
                hull_damage: (e.hull_damage * we + result.hull_damage_per_shot * wn)
                    / total_shots as f64,
                system_damage: (e.system_damage * we + result.system_damage_per_shot * wn)
                    / total_shots as f64,
                character_damage: (e.character_damage * we + result.character_damage_per_shot * wn)
                    / total_shots as f64,
                shield_damage: (e.shield_damage * we + result.shield_damage_per_shot * wn)
                    / total_shots as f64,
                calibrated_from_shots: total_shots,
            }
        }
        None => pss_designs::CalibratedItemDamageRecord {
            item_design_id: result.item_design_id,
            hull_damage: result.hull_damage_per_shot,
            system_damage: result.system_damage_per_shot,
            character_damage: result.character_damage_per_shot,
            shield_damage: result.shield_damage_per_shot,
            calibrated_from_shots: result.shots,
        },
    };

    designs.upsert_calibrated_item_damage(&merged)?;
    println!(
        "{label}: calibrated {kind} item #{} from {} shots this battle (cumulative {} shots) -> hull={:.3} system={:.3} shield={:.3} character={:.3} per shot",
        merged.item_design_id,
        result.shots,
        merged.calibrated_from_shots,
        merged.hull_damage,
        merged.system_damage,
        merged.shield_damage,
        merged.character_damage
    );
    Ok(())
}

fn history(db_path: &str, designs_db_path: &str, fps: f64) -> Result<()> {
    let store = BattleStore::open(db_path)?;
    let designs = DesignsCache::open(designs_db_path)?;
    let battles = store.list_all_battles()?;

    if battles.is_empty() {
        println!("No battles stored yet. Run `pss-cli import <FILE>` first");
        return Ok(());
    }

    println!("=== Your win rate, by account ===");
    for record in store.win_rate_by_attacking_ship()? {
        let total = record.wins + record.losses;
        let pct = if total > 0 {
            record.wins as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  ship #{}: {} battles, {} wins, {} losses ({pct:.0}% win rate)",
            record.ship_id, record.appearances, record.wins, record.losses
        );
    }
    println!();

    println!(
        "=== Average DPS by weapon type (across {} battles) ===",
        battles.len()
    );
    let weapon_history: Vec<_> = pss_stats::weapon_type_history(&battles, &designs, fps)?
        .into_iter()
        .collect();
    for (room_type, hist) in weapon_history {
        println!(
            "  {room_type:?}: {:.2} DPS avg ({:.1} total damage over {} ship-battle
  appearances)",
            hist.dps(),
            hist.total_damage,
            hist.appearances
        );
    }
    println!();

    println!("=== Damage per battle ===");
    for entry in pss_stats::battle_damage_history(&battles, &designs)? {
        println!(
            "  #{} [{}] {:?}: attacker(design #{}) dealt {:.1}, lost {:.0} hull |
  defender(design #{}) dealt {:.1}, lost {:.0} hull",
            entry.battle_id,
            entry.imported_at,
            entry.outcome,
            entry.attacker_ship_design_id,
            entry.attacker_damage_dealt,
            entry.attacker_hull_lost,
            entry.defender_ship_design_id,
            entry.defender_damage_dealt,
            entry.defender_hull_lost
        );
    }

    Ok(())
}

fn import(db_path: &str, capture_path: &PathBuf) -> Result<()> {
    let raw = std::fs::read(capture_path)
        .with_context(|| format!("Failed to read capture file {}", capture_path.display()))?;
    let decoded = pss_codec::decode_battle_report(&raw).context("Failed to decode capture")?;
    let payload: FinaliseBattlePayload = serde_json::from_str(&decoded)
        .context("Decoded capture did not match the expected battleReport shape")?;

    let store = BattleStore::open(db_path)?;
    match store.insert_battle(&payload)? {
        InsertOutcome::Inserted { id } => {
            println!("Imported battle #{id} (checksum {})", payload.z)
        }
        InsertOutcome::AlreadyExists { id } => {
            println!("Battle already imported as #{id} (checksum {})", payload.z)
        }
    }

    Ok(())
}

fn list(db_path: &str, limit: i64) -> Result<()> {
    let store = BattleStore::open(db_path)?;
    let battles = store.list_battles(limit, 0)?;

    if battles.is_empty() {
        println!("No battles stored yet. Run `pss-cli import <FILE>` first");
        return Ok(());
    }

    for battle in battles {
        println!(
            "#{:<5} {:<10} outcome={:<18?} frames={:<6} attacker={} defender={}",
            battle.id,
            battle.imported_at,
            battle.battle_outcome,
            battle.last_frame,
            battle.attacking_ship_id,
            battle.defending_ship_id
        );
    }

    Ok(())
}
