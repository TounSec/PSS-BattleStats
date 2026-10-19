//! SQLite persistence for imported battle reports.
//!
//! Each battle is stored as its full decoded `FinaliseBattlePayload` (raw JSON, for full fidelity
//! and forward compatibility) alongside a handful of indexed columns for cheap listing/filtering.
//! Deduplication relies on the `z` checksum field (§3 of docs/pss_battlereport.pdf), which is
//! unique per client submission.

pub mod paths;

use anyhow::{Context, Result};
use pss_model::{FinaliseBattlePayload, OutcomeType};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

/// Outcome of an `insert_battle` call.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum InsertOutcome {
    Inserted {
        id: i64,
    },
    /// A battle with the same checksum was already stored; no new row was written.
    AlreadyExists {
        id: i64,
    },
}

/// Lightweight listing row - no JSON deserialization, for cheap browsing.
#[derive(Debug, Clone, PartialEq)]
pub struct BattleSummary {
    pub id: i64,
    pub checksum: String,
    pub imported_at: String,
    pub battle_outcome: OutcomeType,
    pub last_frame: i32,
    pub attacking_ship_id: i32,
    pub defending_ship_id: i32,
    /// `attacking_ship_report.is_surrendered()` (§6.2) - `battle_outcome` alone can't tell a
    /// clean rout from your ship bailing out early with HP to spare: a `DefenderWon` battle can
    /// mean either. Lets the GUI show those as a softer color than an actual destruction.
    pub attacking_ship_surrendered: bool,
    /// `defending_ship_report.is_surrendered()` - the mirror case: an `AttackerWon` battle where
    /// the defender bailed out rather than being destroyed ("Abandon adverse" in the GUI, not a
    /// clean "Victoire").
    pub defending_ship_surrendered: bool,
}

/// Win/loss record for one specific ship_id across every stored battle it appeared in.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipWinRate {
    pub ship_id: i32,
    pub appearances: i64,
    pub wins: i64,
    pub losses: i64,
}

/// A fully-loaded stored battle, with the complete decoded payload.
#[derive(Debug, Clone)]
pub struct StoredBattle {
    pub id: i64,
    pub checksum: String,
    pub imported_at: String,
    pub payload: FinaliseBattlePayload,
}

/// Player/ship identity as seen in a `BattleService/CreateBattle` response - this comes for free
/// alongside normal matchmaking, no extra API call. The `battleReport` itself (what `insert_battle`
/// stores) only ever carries a bare `ship_id`, nothing about the player behind it (§ battleReport
/// format notes in CLAUDE.md).
#[derive(Debug, Clone, PartialEq)]
pub struct ShipIdentitySeen {
    pub ship_id: i32,
    pub user_id: i32,
    pub ship_name: String,
    pub ship_level: i32,
    pub original_race_id: i32,
    pub power_score: i32,
}

/// A resolved player profile from `UserService/SearchUsers` (public, no access token needed) -
/// trophies and alliance aren't on the `CreateBattle` ship snapshot, only on the player's own
/// `User` record.
#[derive(Debug, Clone, PartialEq)]
pub struct UserProfile {
    pub user_id: i32,
    pub trophy: i32,
    /// `None` when the API reports `AllianceId="0"` - this codebase's usual sentinel for "no
    /// nested/associated record" (see e.g. `MissileDesignId="0"` in CLAUDE.md).
    pub alliance_id: Option<i32>,
    pub alliance_name: Option<String>,
}

/// Full stored identity row: the always-present `CreateBattle` fields, plus whatever
/// `UserService/SearchUsers` has resolved so far (`None` until `sync_ship_profiles` succeeds for
/// that name).
#[derive(Debug, Clone, PartialEq)]
pub struct ShipIdentity {
    pub ship_id: i32,
    pub user_id: i32,
    pub ship_name: String,
    pub ship_level: i32,
    pub original_race_id: i32,
    pub power_score: i32,
    pub trophy: Option<i32>,
    pub alliance_id: Option<i32>,
    pub alliance_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShipSnapshotXml {
    #[serde(rename = "@ShipId")]
    ship_id: i32,
    #[serde(rename = "@UserId")]
    user_id: i32,
    #[serde(rename = "@ShipName")]
    ship_name: String,
    #[serde(rename = "@ShipLevel")]
    ship_level: i32,
    #[serde(rename = "@OriginalRaceId")]
    original_race_id: i32,
    #[serde(rename = "@PowerScore")]
    power_score: i32,
}

impl From<ShipSnapshotXml> for ShipIdentitySeen {
    fn from(xml: ShipSnapshotXml) -> Self {
        ShipIdentitySeen {
            ship_id: xml.ship_id,
            user_id: xml.user_id,
            ship_name: xml.ship_name,
            ship_level: xml.ship_level,
            original_race_id: xml.original_race_id,
            power_score: xml.power_score,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BattleElementXml {
    /// XML-attribute-escaped nested `<Ship .../>` document, not a plain string - unescaped by the
    /// outer XML parse itself (attribute value un-escaping is a base part of the XML spec), so this
    /// is already valid XML text ready for a second `quick_xml::de::from_str` pass.
    #[serde(rename = "@AttackingShipXml")]
    attacking_ship_xml: String,
    #[serde(rename = "@DefendingShipXml")]
    defending_ship_xml: String,
}

#[derive(Debug, Deserialize)]
struct CreateBattleXml {
    #[serde(rename = "Battle")]
    battle: BattleElementXml,
}

#[derive(Debug, Deserialize)]
struct BattleServiceEnvelope {
    #[serde(rename = "CreateBattle")]
    create_battle: CreateBattleXml,
}

/// Parses a `BattleService/CreateBattle` response body into both sides' ship identity. Pure/offline
/// - no network access.
pub fn parse_create_battle_ships(xml: &str) -> Result<(ShipIdentitySeen, ShipIdentitySeen)> {
    let envelope: BattleServiceEnvelope =
        quick_xml::de::from_str(xml).context("Failed to parse BattleService/CreateBattle")?;
    let battle = envelope.create_battle.battle;

    let attacker: ShipSnapshotXml = quick_xml::de::from_str(&battle.attacking_ship_xml)
        .context("Failed to parse nested AttackingShipXml")?;
    let defender: ShipSnapshotXml = quick_xml::de::from_str(&battle.defending_ship_xml)
        .context("Failed to parse nested DefendingShipXml")?;

    Ok((attacker.into(), defender.into()))
}

#[derive(Debug, Deserialize)]
struct SearchUserXml {
    #[serde(rename = "@Id")]
    id: i32,
    #[serde(rename = "@Name")]
    name: String,
    #[serde(rename = "@Trophy", default)]
    trophy: i32,
    #[serde(rename = "@AllianceId", default)]
    alliance_id: i32,
    #[serde(rename = "@AllianceName", default)]
    alliance_name: String,
}

#[derive(Debug, Deserialize, Default)]
struct UsersWrapperXml {
    #[serde(rename = "User", default)]
    user: Vec<SearchUserXml>,
}

#[derive(Debug, Deserialize)]
struct SearchUsersXml {
    #[serde(rename = "Users")]
    users: UsersWrapperXml,
}

#[derive(Debug, Deserialize)]
struct UserServiceEnvelope {
    #[serde(rename = "SearchUsers")]
    search_users: SearchUsersXml,
}

/// Parses a `UserService/SearchUsers?searchstring=...` response, returning the profile of the
/// first result whose `Name` matches `ship_name` exactly (the search can return near-matches).
/// Pure/offline - no network access.
pub fn parse_search_users_xml(xml: &str, ship_name: &str) -> Result<Option<UserProfile>> {
    let envelope: UserServiceEnvelope =
        quick_xml::de::from_str(xml).context("Failed to parse UserService/SearchUsers")?;

    Ok(envelope
        .search_users
        .users
        .user
        .into_iter()
        .find(|u| u.name == ship_name)
        .map(|u| UserProfile {
            user_id: u.id,
            trophy: u.trophy,
            alliance_id: (u.alliance_id != 0).then_some(u.alliance_id),
            alliance_name: (u.alliance_id != 0).then_some(u.alliance_name),
        }))
}

/// SQLite-backed store of imported battles.
pub struct BattleStore {
    conn: Connection,
}

impl BattleStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open battle store database")?;
        Self::init_schema(&conn)?;

        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory battle store")?;
        Self::init_schema(&conn)?;

        Ok(Self { conn })
    }

    /// Stores a decoded battle. Re-importing the same capture (same `z` checksum) is a no-op that
    /// returns the existing row's id instead of inserting a duplicate.
    pub fn insert_battle(&self, payload: &FinaliseBattlePayload) -> Result<InsertOutcome> {
        let raw_json =
            serde_json::to_string(payload).context("Failed to serialize battle payload")?;
        let outcome_code: i32 = payload.battle_report.battle_outcome.into();
        let attacking_ship_surrendered =
            payload.battle_report.attacking_ship_report.is_surrendered();
        let defending_ship_surrendered =
            payload.battle_report.defending_ship_report.is_surrendered();
        let rows_changed = self.conn.execute(
            "
            INSERT INTO battles (checksum, battle_outcome, last_frame,
                attacking_ship_id, attacking_ship_design_id,
                defending_ship_id, defending_ship_design_id,
                attacking_ship_surrendered, defending_ship_surrendered,
                raw_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(checksum) DO NOTHING
            ",
            params![
                payload.z,
                outcome_code,
                payload.battle_report.last_frame,
                payload.battle_report.attacking_ship_report.ship_id,
                payload.battle_report.attacking_ship_report.ship_design_id,
                payload.battle_report.defending_ship_report.ship_id,
                payload.battle_report.defending_ship_report.ship_design_id,
                attacking_ship_surrendered,
                defending_ship_surrendered,
                raw_json
            ],
        )?;

        if rows_changed == 0 {
            let id: i64 = self.conn.query_row(
                "SELECT id FROM battles WHERE checksum = ?1",
                params![payload.z],
                |row| row.get(0),
            )?;

            Ok(InsertOutcome::AlreadyExists { id })
        } else {
            Ok(InsertOutcome::Inserted {
                id: self.conn.last_insert_rowid(),
            })
        }
    }

    /// The stored JSON text of a battle - decoded `FinaliseBattle` payload as this app keeps it
    /// (minified keys, see `docs/battle_report.json` for the key -> field mapping), for exporting
    /// a report to a file. `None` if no battle has this id.
    pub fn get_battle_json(&self, id: i64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT raw_json FROM battles WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("Failed to read the stored battle JSON")
    }

    /// Loads a stored battle with its full payload.
    pub fn get_battle(&self, id: i64) -> Result<Option<StoredBattle>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, checksum, imported_at, raw_json FROM battles WHERE id = ?1")?;
        let row = stmt
            .query_row(params![id], |row| {
                let raw_json: String = row.get(3)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    raw_json,
                ))
            })
            .ok();
        let Some((id, checksum, imported_at, raw_json)) = row else {
            return Ok(None);
        };
        let payload: FinaliseBattlePayload = serde_json::from_str(&raw_json)
            .context("Failed to deserialize stored battle payload")?;

        Ok(Some(StoredBattle {
            id,
            checksum,
            imported_at,
            payload,
        }))
    }

    pub fn list_battles(&self, limit: i64, offset: i64) -> Result<Vec<BattleSummary>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT id, checksum, imported_at, battle_outcome, last_frame, attacking_ship_id, defending_ship_id, attacking_ship_surrendered, defending_ship_surrendered FROM battles ORDER BY id DESC LIMIT ?1 OFFSET ?2
            "
        )?;

        let rows = stmt.query_map(params![limit, offset], |row| {
            let outcome_code: i32 = row.get(3)?;
            Ok(BattleSummary {
                id: row.get(0)?,
                checksum: row.get(1)?,
                imported_at: row.get(2)?,
                battle_outcome: OutcomeType::from(outcome_code),
                last_frame: row.get(4)?,
                attacking_ship_id: row.get(5)?,
                defending_ship_id: row.get(6)?,
                attacking_ship_surrendered: row.get(7)?,
                defending_ship_surrendered: row.get(8)?,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to list battles")
    }

    /// Loads every stored battle with its fully payload
    pub fn list_all_battles(&self) -> Result<Vec<StoredBattle>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, checksum, imported_at, raw_json FROM battles ORDER BY id ASC")?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut battles = Vec::new();

        for row in rows {
            let (id, checksum, imported_at, raw_json) = row?;
            let payload: FinaliseBattlePayload = serde_json::from_str(&raw_json)
                .context("Failed to deserialize stored battle payload")?;
            battles.push(StoredBattle {
                id,
                checksum,
                imported_at,
                payload,
            });
        }

        Ok(battles)
    }

    /// Win/loss record per attacking ship_id. Battles can only be captured while your own
    /// client is online (you can't be attacked while connected), so the attacker is always
    /// you - grouping by attacking_ship_id separates multiple accounts with zero ambiguity,
    /// no auto-detection needed.
    pub fn win_rate_by_attacking_ship(&self) -> Result<Vec<ShipWinRate>> {
        let mut stmt = self.conn.prepare(
            "SELECT attacking_ship_id AS ship_id,
                    COUNT(*) AS appearances,
                    SUM(CASE WHEN battle_outcome = 1 THEN 1 ELSE 0 END) AS wins,
                    SUM(CASE WHEN battle_outcome = 2 THEN 1 ELSE 0 END) AS losses
             FROM battles
             GROUP BY attacking_ship_id
             ORDER BY appearances DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ShipWinRate {
                ship_id: row.get(0)?,
                appearances: row.get(1)?,
                wins: row.get(2)?,
                losses: row.get(3)?,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to aggregate win rate by attacking ship")
    }

    /// Records/refreshes a ship's `CreateBattle`-sourced identity fields. Never touches
    /// `trophy`/`alliance_id`/`alliance_name` - those are only known once `sync_ship_profiles` has
    /// resolved that ship's name, and a ship reappearing in a new `CreateBattle` capture shouldn't
    /// wipe a profile already resolved earlier.
    pub fn upsert_ship_identity(&self, seen: &ShipIdentitySeen) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO ship_identities
                    (ship_id, user_id, ship_name, ship_level, original_race_id, power_score)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(ship_id) DO UPDATE SET
                    user_id = excluded.user_id,
                    ship_name = excluded.ship_name,
                    ship_level = excluded.ship_level,
                    original_race_id = excluded.original_race_id,
                    power_score = excluded.power_score,
                    identity_updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                params![
                    seen.ship_id,
                    seen.user_id,
                    seen.ship_name,
                    seen.ship_level,
                    seen.original_race_id,
                    seen.power_score,
                ],
            )
            .context("Failed to upsert ship identity")?;

        Ok(())
    }

    pub fn get_ship_identity(&self, ship_id: i32) -> Result<Option<ShipIdentity>> {
        self.conn
            .query_row(
                "SELECT ship_id, user_id, ship_name, ship_level, original_race_id, power_score,
                        trophy, alliance_id, alliance_name
                 FROM ship_identities WHERE ship_id = ?1",
                params![ship_id],
                |row| {
                    Ok(ShipIdentity {
                        ship_id: row.get(0)?,
                        user_id: row.get(1)?,
                        ship_name: row.get(2)?,
                        ship_level: row.get(3)?,
                        original_race_id: row.get(4)?,
                        power_score: row.get(5)?,
                        trophy: row.get(6)?,
                        alliance_id: row.get(7)?,
                        alliance_name: row.get(8)?,
                    })
                },
            )
            .optional()
            .context("Failed to load ship identity")
    }

    /// Distinct `(ship_id, ship_name)` pairs seen via `CreateBattle` that `sync_ship_profiles`
    /// hasn't resolved yet - i.e. `UserService/SearchUsers` has never been called for that name.
    pub fn ship_names_missing_profile(&self) -> Result<Vec<(i32, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ship_id, ship_name FROM ship_identities WHERE profile_updated_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to list ship identities missing a profile")
    }

    /// Applies a resolved `UserService/SearchUsers` profile to every stored ship identity sharing
    /// that `user_id` (in practice, exactly one - a PSS account has a single ship).
    pub fn update_ship_profile(&self, profile: &UserProfile) -> Result<()> {
        self.conn
            .execute(
                "UPDATE ship_identities SET
                    trophy = ?1,
                    alliance_id = ?2,
                    alliance_name = ?3,
                    profile_updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE user_id = ?4",
                params![
                    profile.trophy,
                    profile.alliance_id,
                    profile.alliance_name,
                    profile.user_id,
                ],
            )
            .context("Failed to update ship profile")?;

        Ok(())
    }

    /// Resolves every not-yet-profiled ship name via the public `UserService/SearchUsers`
    /// endpoint (no access token needed - see CLAUDE.md). Returns how many names were newly
    /// resolved; a name with no matching result (renamed since, or a search-index lag) is left
    /// for the next call rather than erroring the whole batch.
    pub async fn sync_ship_profiles(&self, http: &reqwest::Client) -> Result<usize> {
        let pending = self.ship_names_missing_profile()?;
        let mut resolved = 0;

        for (_ship_id, ship_name) in pending {
            let xml = http
                .get("https://api.pixelstarships.com/UserService/SearchUsers")
                .query(&[("searchstring", ship_name.as_str())])
                .send()
                .await
                .context("Failed to call UserService/SearchUsers")?
                .text()
                .await
                .context("Failed to read UserService/SearchUsers response body")?;

            match parse_search_users_xml(&xml, &ship_name) {
                Ok(Some(profile)) => {
                    self.update_ship_profile(&profile)?;
                    resolved += 1;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(?err, ship = %ship_name, "Skipping unparsable SearchUsers response");
                }
            }
        }

        Ok(resolved)
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS battles (
                id                          INTEGER PRIMARY KEY AUTOINCREMENT,
                checksum                    TEXT NOT NULL UNIQUE,
                imported_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                battle_outcome              INTEGER NOT NULL,
                last_frame                  INTEGER NOT NULL,
                attacking_ship_id           INTEGER NOT NULL,
                attacking_ship_design_id    INTEGER NOT NULL,
                defending_ship_id           INTEGER NOT NULL,
                defending_ship_design_id    INTEGER NOT NULL,
                attacking_ship_surrendered  INTEGER,
                defending_ship_surrendered  INTEGER,
                raw_json                    TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ship_identities (
                ship_id             INTEGER PRIMARY KEY,
                user_id             INTEGER NOT NULL,
                ship_name           TEXT NOT NULL,
                ship_level          INTEGER NOT NULL,
                original_race_id    INTEGER NOT NULL,
                power_score         INTEGER NOT NULL,
                identity_updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                trophy              INTEGER,
                alliance_id         INTEGER,
                alliance_name       TEXT,
                profile_updated_at  TEXT
            );
            "
        )?;

        // `attacking_ship_surrendered` was added after this store shipped. Unlike pss-designs'
        // caches, there's no external API to re-fetch here - every pre-existing row's real value
        // is already sitting in its own `raw_json`, so this backfills it directly instead of
        // needing a version-gate/force-resync dance. Left NULL (not NOT NULL/DEFAULT) so this
        // query can tell "never computed" apart from a real `false`
        for column in ["attacking_ship_surrendered", "defending_ship_surrendered"] {
            if let Err(err) = conn.execute(
                &format!("ALTER TABLE battles ADD COLUMN {column} INTEGER"),
                [],
            ) {
                let is_duplicate_column = matches!(
                    &err,
                    rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("duplicate column name")
                );
                if !is_duplicate_column {
                    return Err(err).context(format!("Failed to migrate battles.{column} column"));
                }
            }
        }

        let mut stmt = conn.prepare(
            "SELECT id, raw_json FROM battles
             WHERE attacking_ship_surrendered IS NULL OR defending_ship_surrendered IS NULL",
        )?;
        let stale: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for (id, raw_json) in stale {
            let payload: FinaliseBattlePayload = serde_json::from_str(&raw_json)
                .context("Failed to deserialize stored battle payload during migration")?;
            let attacking_surrendered =
                payload.battle_report.attacking_ship_report.is_surrendered();
            let defending_surrendered =
                payload.battle_report.defending_ship_report.is_surrendered();
            conn.execute(
                "UPDATE battles SET attacking_ship_surrendered = ?1, defending_ship_surrendered = ?2
                 WHERE id = ?3",
                params![attacking_surrendered, defending_surrendered, id],
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_payload(checksum: &str) -> FinaliseBattlePayload {
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
            "z": checksum
        });

        serde_json::from_value(value).expect("Sample payload should deserialize")
    }

    #[test]
    fn battle_json_export_round_trips_to_same_payload() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        let payload = sample_payload("checksum-export");
        let InsertOutcome::Inserted { id } = store.insert_battle(&payload).unwrap() else {
            panic!("Expected a fresh insert");
        };

        let json = store
            .get_battle_json(id)
            .expect("Read should succeed")
            .expect("Battle should exist");
        let reparsed: FinaliseBattlePayload =
            serde_json::from_str(&json).expect("Exported JSON should be a valid payload");

        assert_eq!(reparsed.z, "checksum-export");
        assert_eq!(reparsed.battle_report.last_frame, 1706);
        assert_eq!(store.get_battle_json(id + 1000).unwrap(), None);
    }

    #[test]
    fn insert_and_get_battle_round_trip() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        let payload = sample_payload("checksum-1");
        let outcome = store
            .insert_battle(&payload)
            .expect("Insert should succeed");
        let InsertOutcome::Inserted { id } = outcome else {
            panic!("Expected a fresh insert, got {outcome:?}");
        };
        let stored = store
            .get_battle(id)
            .expect("Get should succeed")
            .expect("Battle should exist");

        assert_eq!(stored.checksum, "checksum-1");
        assert_eq!(stored.payload.battle_report.last_frame, 1706);
        assert_eq!(
            stored.payload.battle_report.attacking_ship_report.ship_id,
            1
        );
    }

    #[test]
    fn reimporting_same_checksum_does_not_duplicate() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        let payload = sample_payload("checksum-dedup");
        let first = store
            .insert_battle(&payload)
            .expect("First insert should succeed");
        let second = store
            .insert_battle(&payload)
            .expect("Second insert should succeed");
        let all = store.list_battles(10, 0).expect("list should succeed");
        let InsertOutcome::Inserted { id: first_id } = first else {
            panic!("Expected first insert to be fresh");
        };
        let InsertOutcome::AlreadyExists { id: second_id } = second else {
            panic!("Expected second insert to be a duplicate");
        };

        assert_eq!(first_id, second_id);
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn list_battles_orders_most_recent_first() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        store.insert_battle(&sample_payload("a")).unwrap();
        store.insert_battle(&sample_payload("b")).unwrap();
        store.insert_battle(&sample_payload("c")).unwrap();

        let page = store.list_battles(2, 0).expect("List should succeed");
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].checksum, "c");
        assert_eq!(page[1].checksum, "b");
        assert_eq!(page[0].battle_outcome, OutcomeType::DefenderWon);
        // sample_payload's attacker has currentShipHp=0 but hullDamage far below maxShipHp -
        // surrendered, not destroyed (see ShipReport::is_surrendered)
        assert!(page[0].attacking_ship_surrendered);
        // The defender's hullDamage (220000) far exceeds maxShipHp (2200) - a real destruction,
        // not a surrender
        assert!(!page[0].defending_ship_surrendered);
    }

    #[test]
    fn migration_backfills_surrendered_flag_from_existing_raw_json() {
        // Simulates a database file from before `attacking_ship_surrendered` existed: the column
        // is absent and a row is inserted directly, bypassing `insert_battle` (which already sets
        // it correctly for new rows - this test is specifically about *old* rows). Opening that
        // database must backfill the real value from the row's own `raw_json`, not from an
        // external source (there isn't one here, unlike pss-designs' caches)
        let conn = Connection::open_in_memory().expect("In-memory connection should open");
        conn.execute_batch(
            "CREATE TABLE battles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                checksum TEXT NOT NULL UNIQUE,
                imported_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                battle_outcome INTEGER NOT NULL,
                last_frame INTEGER NOT NULL,
                attacking_ship_id INTEGER NOT NULL,
                attacking_ship_design_id INTEGER NOT NULL,
                defending_ship_id INTEGER NOT NULL,
                defending_ship_design_id INTEGER NOT NULL,
                raw_json TEXT NOT NULL
            );",
        )
        .unwrap();

        let payload = sample_payload("pre-migration");
        let raw_json = serde_json::to_string(&payload).unwrap();
        conn.execute(
            "INSERT INTO battles (checksum, battle_outcome, last_frame, attacking_ship_id,
                attacking_ship_design_id, defending_ship_id, defending_ship_design_id, raw_json)
             VALUES ('pre-migration', 2, 1706, 1, 2, 3, 4, ?1)",
            params![raw_json],
        )
        .unwrap();

        BattleStore::init_schema(&conn).expect("Migration should succeed");

        let (attacking_surrendered, defending_surrendered): (bool, bool) = conn
            .query_row(
                "SELECT attacking_ship_surrendered, defending_ship_surrendered
                 FROM battles WHERE checksum = 'pre-migration'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(attacking_surrendered);
        assert!(!defending_surrendered);
    }

    #[test]
    fn end_to_end_real_capture_decodes_and_stores() {
        use std::path::Path;

        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/pss_31_req.bin");
        let raw = std::fs::read(&path).expect("Failed to read docs/pss_31_req.bin");
        let decoded = pss_codec::decode_battle_report(&raw).expect("Capture should decode");
        let payload: FinaliseBattlePayload =
            serde_json::from_str(&decoded).expect("Decoded capture should match pss-model");
        let store = BattleStore::open_in_memory().expect("Store should open");
        let outcome = store
            .insert_battle(&payload)
            .expect("Insert should succeed");
        let InsertOutcome::Inserted { id } = outcome else {
            panic!("Expected a fresh insert");
        };
        let stored = store
            .get_battle(id)
            .expect("Get should succeed")
            .expect("battle should exist");

        assert_eq!(
            stored.payload.battle_report.last_frame,
            payload.battle_report.last_frame
        );
    }

    // Nested ship attributes trimmed from a real BattleService/CreateBattle capture (values kept
    // as observed: attacker ShipId 9397907/UserId 12363942 "Electouns2", defender ShipId 8594857/
    // UserId 11259701 "Dark_01" - see docs/pss_data_correlation/PvP-Win/1/create_battle.resp)
    const SAMPLE_CREATE_BATTLE_XML: &str = r#"<BattleService><CreateBattle ChanceToFindStarBattles="0"><Battle BattleId="438225" AttackingShipId="9397907" DefendingShipId="8594857" RandomSeed="754" AttackingShipXml="&lt;Ship ShipId=&quot;9397907&quot; ShipDesignId=&quot;23&quot; ShipLevel=&quot;7&quot; PowerScore=&quot;900&quot; UserId=&quot;12363942&quot; OriginalRaceId=&quot;3&quot; ShipName=&quot;Electouns2&quot; /&gt;" DefendingShipXml="&lt;Ship ShipId=&quot;8594857&quot; ShipDesignId=&quot;21&quot; ShipLevel=&quot;7&quot; PowerScore=&quot;876&quot; UserId=&quot;11259701&quot; OriginalRaceId=&quot;1&quot; ShipName=&quot;Dark_01&quot; /&gt;" /></CreateBattle></BattleService>"#;

    #[test]
    fn parses_create_battle_ships() {
        let (attacker, defender) =
            parse_create_battle_ships(SAMPLE_CREATE_BATTLE_XML).expect("should parse");

        assert_eq!(
            attacker,
            ShipIdentitySeen {
                ship_id: 9397907,
                user_id: 12363942,
                ship_name: "Electouns2".to_string(),
                ship_level: 7,
                original_race_id: 3,
                power_score: 900,
            }
        );
        assert_eq!(
            defender,
            ShipIdentitySeen {
                ship_id: 8594857,
                user_id: 11259701,
                ship_name: "Dark_01".to_string(),
                ship_level: 7,
                original_race_id: 1,
                power_score: 876,
            }
        );
    }

    // Trimmed from a real UserService/SearchUsers response (public endpoint, no access token) -
    // extra unrelated attributes kept to prove they're tolerated, not just absent
    const SAMPLE_SEARCH_USERS_XML: &str = r#"<UserService><SearchUsers><Users><User Id="12363942" Name="Electouns2" GenderType="Unknown" Trophy="2150" AllianceId="45809" AllianceName="Trek Ad Astra" /></Users></SearchUsers></UserService>"#;
    const SAMPLE_SEARCH_USERS_NO_ALLIANCE_XML: &str = r#"<UserService><SearchUsers><Users><User Id="1" Name="Solo" Trophy="10" AllianceId="0" AllianceName="" /></Users></SearchUsers></UserService>"#;
    const SAMPLE_SEARCH_USERS_EMPTY_XML: &str =
        r#"<UserService><SearchUsers><Users></Users></SearchUsers></UserService>"#;

    #[test]
    fn parses_search_users_with_alliance() {
        let profile = parse_search_users_xml(SAMPLE_SEARCH_USERS_XML, "Electouns2")
            .expect("should parse")
            .expect("Electouns2 should be found");

        assert_eq!(profile.user_id, 12363942);
        assert_eq!(profile.trophy, 2150);
        assert_eq!(profile.alliance_id, Some(45809));
        assert_eq!(profile.alliance_name.as_deref(), Some("Trek Ad Astra"));
    }

    #[test]
    fn search_users_without_alliance_name_attribute_parses() {
        let xml = r#"<UserService><SearchUsers><Users><User Id="9581" Name="Solo" Trophy="1073" AllianceId="0" /></Users></SearchUsers></UserService>"#;
        let profile = parse_search_users_xml(xml, "Solo").unwrap().unwrap();

        assert_eq!(profile.user_id, 9581);
        assert_eq!(profile.alliance_id, None);
        assert_eq!(profile.alliance_name, None);
    }

    #[test]
    fn parses_search_users_without_alliance_as_none() {
        let profile = parse_search_users_xml(SAMPLE_SEARCH_USERS_NO_ALLIANCE_XML, "Solo")
            .expect("should parse")
            .expect("Solo should be found");

        assert_eq!(profile.trophy, 10);
        assert_eq!(profile.alliance_id, None);
        assert_eq!(profile.alliance_name, None);
    }

    #[test]
    fn parses_search_users_with_no_match_returns_none() {
        assert_eq!(
            parse_search_users_xml(SAMPLE_SEARCH_USERS_EMPTY_XML, "Nobody").unwrap(),
            None
        );
        // A result set that doesn't contain an exact name match (near-match search) also counts
        // as "not found" - resolving the wrong player's profile would be worse than skipping it
        assert_eq!(
            parse_search_users_xml(SAMPLE_SEARCH_USERS_XML, "Electouns3").unwrap(),
            None
        );
    }

    fn sample_ship_identity_seen(ship_id: i32, user_id: i32, name: &str) -> ShipIdentitySeen {
        ShipIdentitySeen {
            ship_id,
            user_id,
            ship_name: name.to_string(),
            ship_level: 7,
            original_race_id: 3,
            power_score: 900,
        }
    }

    #[test]
    fn upsert_and_get_ship_identity_round_trip() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        store
            .upsert_ship_identity(&sample_ship_identity_seen(9397907, 12363942, "Electouns2"))
            .expect("Upsert should succeed");

        let identity = store
            .get_ship_identity(9397907)
            .expect("Get should succeed")
            .expect("Identity should exist");

        assert_eq!(identity.user_id, 12363942);
        assert_eq!(identity.ship_name, "Electouns2");
        assert_eq!(identity.trophy, None);
        assert_eq!(identity.alliance_id, None);
    }

    #[test]
    fn get_ship_identity_returns_none_for_unknown_ship() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        assert_eq!(store.get_ship_identity(123).unwrap(), None);
    }

    #[test]
    fn reidentifying_a_ship_does_not_wipe_its_resolved_profile() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        store
            .upsert_ship_identity(&sample_ship_identity_seen(9397907, 12363942, "Electouns2"))
            .expect("Upsert should succeed");
        store
            .update_ship_profile(&UserProfile {
                user_id: 12363942,
                trophy: 2150,
                alliance_id: Some(45809),
                alliance_name: Some("Trek Ad Astra".to_string()),
            })
            .expect("Profile update should succeed");

        // The same ship reappears in a later CreateBattle capture, maybe with a leveled-up ship
        store
            .upsert_ship_identity(&sample_ship_identity_seen(9397907, 12363942, "Electouns2"))
            .expect("Re-upsert should succeed");

        let identity = store
            .get_ship_identity(9397907)
            .expect("Get should succeed")
            .expect("Identity should exist");
        assert_eq!(identity.trophy, Some(2150));
        assert_eq!(identity.alliance_name.as_deref(), Some("Trek Ad Astra"));
    }

    #[test]
    fn ship_names_missing_profile_excludes_already_resolved() {
        let store = BattleStore::open_in_memory().expect("Store should open");
        store
            .upsert_ship_identity(&sample_ship_identity_seen(9397907, 12363942, "Electouns2"))
            .expect("Upsert should succeed");
        store
            .upsert_ship_identity(&sample_ship_identity_seen(8594857, 11259701, "Dark_01"))
            .expect("Upsert should succeed");
        store
            .update_ship_profile(&UserProfile {
                user_id: 12363942,
                trophy: 2150,
                alliance_id: Some(45809),
                alliance_name: Some("Trek Ad Astra".to_string()),
            })
            .expect("Profile update should succeed");

        let pending = store.ship_names_missing_profile().unwrap();
        assert_eq!(pending, vec![(8594857, "Dark_01".to_string())]);
    }
}
