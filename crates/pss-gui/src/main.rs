#[macro_use]
mod i18n;
mod crew;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use eframe::egui;
use tracing_subscriber::layer::SubscriberExt;

use pss_capture::CaptureHandle;
use pss_designs::{CalibratedItemDamageRecord, DesignsCache, DesignsCounts};
use pss_model::{ItemType, OutcomeType, RoomType, ShipReport, SpecialAbilityType};
use pss_stats::{NamedCrewStat, ShipBattleStats};
use pss_store::paths::app_paths;
use pss_store::{BattleStore, BattleSummary, ShipIdentity, ShipWinRate};

rust_i18n::i18n!("locales", fallback = "en");

const CAPTURE_LISTEN: &str = "127.0.0.1:8484";
/// Steam app id of Pixel Starships, written to `steam_appid.txt` for launches outside Steam.
const STEAM_APP_ID: &str = "378760";
const LOG_CAPACITY: usize = 1000;

/// Shared semantic colors so every tab agrees on what a color means (mirrors docs/Ui: green =
/// good/synced/calibrated, orange = calibration-sourced/caution, red = bad/destroyed, blue =
/// pss-designs-sourced data, grey = neutral/no data). Kept in one place after the GUI shipped with
/// several near-duplicate colors picked ad hoc per call site, some of which didn't actually match
/// the mockups (e.g. "calibré" was green instead of the mockup's orange).
mod palette {
    use eframe::egui::Color32;

    pub const GOOD: Color32 = Color32::from_rgb(72, 187, 120);
    pub const WARN: Color32 = Color32::from_rgb(214, 158, 46);
    pub const BAD: Color32 = Color32::from_rgb(245, 101, 101);
    pub const INFO_BLUE: Color32 = Color32::from_rgb(99, 179, 237);
    pub const NEUTRAL: Color32 = Color32::from_rgb(160, 174, 192);
}

/// Logical query language for the battle sidebar's search box: `field: value`/`field != value`/
/// `field > value` terms combined with `&&`/`||`/`!`/parens, plus a bare-word fallback that still
/// does the old plain substring search. See CLAUDE.md-adjacent design discussion in chat history -
/// no dedicated parser crate, the grammar is small enough to hand-roll.
mod filter {
    use std::collections::HashMap;

    use pss_model::OutcomeType;
    use pss_store::{BattleSummary, ShipIdentity};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Side {
        Either,
        Attacker,
        Defender,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Field {
        PlayerId,
        PlayerName,
        Alliance,
        ShipId,
        Level,
        Trophy,
        Race,
        BattleId,
        Outcome,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Op {
        Eq,
        Gt,
        Lt,
        Ge,
        Le,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Text(String),
        Number(f64),
    }

    impl Value {
        fn from_word(w: &str) -> Value {
            match w.parse::<f64>() {
                Ok(n) => Value::Number(n),
                Err(_) => Value::Text(w.to_string()),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Expr {
        And(Vec<Expr>),
        Or(Vec<Expr>),
        Not(Box<Expr>),
        FreeText(String),
        Field {
            field: Field,
            side: Side,
            op: Op,
            value: Value,
        },
    }

    // ------------------------------------------------------------------ tokenizer

    #[derive(Debug, Clone, PartialEq)]
    enum Token {
        LParen,
        RParen,
        And,
        Or,
        Bang,
        NotEq,
        Colon,
        Ge,
        Le,
        Gt,
        Lt,
        Word(String),
        Quoted(String),
    }

    fn tokenize(input: &str) -> Vec<Token> {
        let chars: Vec<char> = input.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            match c {
                '(' => {
                    tokens.push(Token::LParen);
                    i += 1;
                }
                ')' => {
                    tokens.push(Token::RParen);
                    i += 1;
                }
                '"' => {
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != '"' {
                        i += 1;
                    }
                    tokens.push(Token::Quoted(chars[start..i].iter().collect()));
                    if i < chars.len() {
                        i += 1; // closing quote
                    }
                }
                '!' if chars.get(i + 1) == Some(&'=') => {
                    tokens.push(Token::NotEq);
                    i += 2;
                }
                '!' => {
                    tokens.push(Token::Bang);
                    i += 1;
                }
                '&' if chars.get(i + 1) == Some(&'&') => {
                    tokens.push(Token::And);
                    i += 2;
                }
                '|' if chars.get(i + 1) == Some(&'|') => {
                    tokens.push(Token::Or);
                    i += 2;
                }
                ':' => {
                    tokens.push(Token::Colon);
                    i += 1;
                }
                '>' if chars.get(i + 1) == Some(&'=') => {
                    tokens.push(Token::Ge);
                    i += 2;
                }
                '<' if chars.get(i + 1) == Some(&'=') => {
                    tokens.push(Token::Le);
                    i += 2;
                }
                '>' => {
                    tokens.push(Token::Gt);
                    i += 1;
                }
                '<' => {
                    tokens.push(Token::Lt);
                    i += 1;
                }
                // Stray '&'/'|' (not doubled) and anything else start/continue a bare word.
                _ => {
                    let start = i;
                    while i < chars.len()
                        && !chars[i].is_whitespace()
                        && !"()!&|:<>\"".contains(chars[i])
                    {
                        i += 1;
                    }
                    if i == start {
                        i += 1; // isolated '&' or '|' - skip, not meaningful alone
                    } else {
                        tokens.push(Token::Word(chars[start..i].iter().collect()));
                    }
                }
            }
        }
        tokens
    }

    // ------------------------------------------------------------------ parser

    struct Parser<'a> {
        tokens: &'a [Token],
        pos: usize,
    }

    impl<'a> Parser<'a> {
        fn peek(&self) -> Option<&Token> {
            self.tokens.get(self.pos)
        }

        fn advance(&mut self) -> Option<&'a Token> {
            let t = self.tokens.get(self.pos);
            self.pos += 1;
            t
        }

        fn parse_or(&mut self) -> Option<Expr> {
            let mut terms = vec![self.parse_and()?];
            while self.peek() == Some(&Token::Or) {
                self.advance();
                terms.push(self.parse_and()?);
            }
            Some(if terms.len() == 1 {
                terms.pop().unwrap()
            } else {
                Expr::Or(terms)
            })
        }

        fn parse_and(&mut self) -> Option<Expr> {
            let mut terms = vec![self.parse_unary()?];
            loop {
                match self.peek() {
                    Some(Token::And) => {
                        self.advance();
                        terms.push(self.parse_unary()?);
                    }
                    Some(Token::Or) | Some(Token::RParen) | None => break,
                    _ => terms.push(self.parse_unary()?), // implicit AND
                }
            }
            Some(if terms.len() == 1 {
                terms.pop().unwrap()
            } else {
                Expr::And(terms)
            })
        }

        fn parse_unary(&mut self) -> Option<Expr> {
            match self.peek() {
                Some(Token::Bang) => {
                    self.advance();
                    Some(Expr::Not(Box::new(self.parse_unary()?)))
                }
                Some(Token::LParen) => {
                    self.advance();
                    let inner = self.parse_or()?;
                    if self.peek() != Some(&Token::RParen) {
                        return None;
                    }
                    self.advance();
                    Some(inner)
                }
                _ => self.parse_term(),
            }
        }

        fn parse_term(&mut self) -> Option<Expr> {
            match self.advance()? {
                Token::Word(w) => {
                    let negate = match self.peek() {
                        Some(Token::Colon) => false,
                        Some(Token::NotEq) => true,
                        Some(Token::Ge) | Some(Token::Le) | Some(Token::Gt) | Some(Token::Lt) => {
                            false
                        }
                        _ => return Some(Expr::FreeText(w.to_lowercase())),
                    };
                    let (field, side) = resolve_field(w)?;
                    let op = match self.advance()? {
                        Token::Colon | Token::NotEq => Op::Eq,
                        Token::Ge => Op::Ge,
                        Token::Le => Op::Le,
                        Token::Gt => Op::Gt,
                        Token::Lt => Op::Lt,
                        _ => return None,
                    };
                    let value = match self.advance()? {
                        Token::Word(v) => Value::from_word(v),
                        Token::Quoted(v) => Value::Text(v.clone()),
                        _ => return None,
                    };
                    let field_expr = Expr::Field {
                        field,
                        side,
                        op,
                        value,
                    };
                    Some(if negate {
                        Expr::Not(Box::new(field_expr))
                    } else {
                        field_expr
                    })
                }
                Token::Quoted(s) => Some(Expr::FreeText(s.to_lowercase())),
                _ => None,
            }
        }
    }

    fn resolve_field(name: &str) -> Option<(Field, Side)> {
        let lower = name.to_lowercase();
        let (side, base) = if let Some(rest) = lower.strip_prefix("atk_") {
            (Side::Attacker, rest)
        } else if let Some(rest) = lower.strip_prefix("def_") {
            (Side::Defender, rest)
        } else {
            (Side::Either, lower.as_str())
        };
        let field = match base {
            "player_id" => Field::PlayerId,
            "player_name" => Field::PlayerName,
            "alliance" => Field::Alliance,
            "ship_id" => Field::ShipId,
            "level" => Field::Level,
            "trophy" => Field::Trophy,
            "race" => Field::Race,
            "battle_id" | "id" => Field::BattleId,
            "outcome" => Field::Outcome,
            _ => return None,
        };
        // battle_id/outcome are battle-level, not per-side - an atk_/def_ prefix on them is
        // meaningless, treat it as an unrecognized field rather than silently ignoring it.
        if matches!(field, Field::BattleId | Field::Outcome) && side != Side::Either {
            return None;
        }
        Some((field, side))
    }

    /// Whether `name` (with an optional `atk_`/`def_` prefix) is a recognized filter field - used
    /// by the search box's syntax-highlighting layouter to badge valid field names as they're
    /// typed, independent of whether the rest of the query around it currently parses.
    pub fn is_known_field(name: &str) -> bool {
        resolve_field(name).is_some()
    }

    /// Parses a query. Any malformed input (unclosed paren, `field:` with no value, unknown field
    /// name...) falls back to a plain substring search over the whole input, same as before this
    /// query language existed - so a query that's still being typed never hides everything.
    pub fn parse(input: &str) -> Expr {
        if input.trim().is_empty() {
            return Expr::And(Vec::new());
        }
        let tokens = tokenize(input);
        let mut parser = Parser {
            tokens: &tokens,
            pos: 0,
        };
        match parser.parse_or() {
            Some(expr) if parser.pos == tokens.len() => expr,
            _ => Expr::FreeText(input.to_lowercase()),
        }
    }

    // ------------------------------------------------------------------ evaluation

    fn normalize_keyword(s: &str) -> String {
        s.to_lowercase()
            .replace('é', "e")
            .replace('è', "e")
            .replace(' ', "_")
    }

    fn outcome_keyword(battle: &BattleSummary) -> &'static str {
        match battle.battle_outcome {
            OutcomeType::AttackerWon if battle.defending_ship_surrendered => "abandon_adverse",
            OutcomeType::AttackerWon => "victoire",
            OutcomeType::DefenderWon if battle.attacking_ship_surrendered => "abandon",
            OutcomeType::DefenderWon => "defaite",
            OutcomeType::Draw => "egalite",
            _ => "autre",
        }
    }

    fn compare_number(actual: f64, op: Op, value: &Value) -> bool {
        let Value::Number(target) = value else {
            return false;
        };
        match op {
            Op::Eq => actual == *target,
            Op::Gt => actual > *target,
            Op::Lt => actual < *target,
            Op::Ge => actual >= *target,
            Op::Le => actual <= *target,
        }
    }

    fn compare_text(actual: &str, op: Op, value: &Value) -> bool {
        let needle = match value {
            Value::Text(t) => t.to_lowercase(),
            Value::Number(n) => n.to_string(),
        };
        let haystack = actual.to_lowercase();
        match op {
            Op::Eq => haystack.contains(&needle),
            Op::Gt => haystack > needle,
            Op::Lt => haystack < needle,
            Op::Ge => haystack >= needle,
            Op::Le => haystack <= needle,
        }
    }

    fn single_side_match(
        field: Field,
        side: Side,
        op: Op,
        value: &Value,
        battle: &BattleSummary,
        identities: &HashMap<i32, ShipIdentity>,
    ) -> bool {
        let ship_id = match side {
            Side::Attacker => battle.attacking_ship_id,
            Side::Defender => battle.defending_ship_id,
            Side::Either => unreachable!("resolved to a concrete side before this point"),
        };
        match field {
            Field::ShipId => compare_number(ship_id as f64, op, value),
            Field::PlayerId => identities
                .get(&ship_id)
                .is_some_and(|i| compare_number(i.user_id as f64, op, value)),
            Field::PlayerName => identities
                .get(&ship_id)
                .is_some_and(|i| compare_text(&i.ship_name, op, value)),
            Field::Alliance => identities.get(&ship_id).is_some_and(|i| {
                i.alliance_name
                    .as_deref()
                    .is_some_and(|name| compare_text(name, op, value))
            }),
            Field::Level => identities
                .get(&ship_id)
                .is_some_and(|i| compare_number(i.ship_level as f64, op, value)),
            Field::Trophy => identities.get(&ship_id).is_some_and(|i| {
                i.trophy
                    .is_some_and(|t| compare_number(t as f64, op, value))
            }),
            Field::Race => identities
                .get(&ship_id)
                .is_some_and(|i| compare_number(i.original_race_id as f64, op, value)),
            Field::BattleId | Field::Outcome => {
                unreachable!("battle-level fields never reach single_side_match")
            }
        }
    }

    pub fn matches(
        expr: &Expr,
        battle: &BattleSummary,
        identities: &HashMap<i32, ShipIdentity>,
    ) -> bool {
        match expr {
            Expr::And(terms) => terms.iter().all(|e| matches(e, battle, identities)),
            Expr::Or(terms) => terms.iter().any(|e| matches(e, battle, identities)),
            Expr::Not(inner) => !matches(inner, battle, identities),
            Expr::FreeText(needle) => {
                let haystack = format!(
                    "{} {:?} {} {}",
                    battle.id,
                    battle.battle_outcome,
                    battle.attacking_ship_id,
                    battle.defending_ship_id
                )
                .to_lowercase();
                haystack.contains(needle)
            }
            Expr::Field {
                field,
                side,
                op,
                value,
            } => match field {
                Field::BattleId => compare_number(battle.id as f64, *op, value),
                Field::Outcome => {
                    *op == Op::Eq
                        && matches!(value, Value::Text(t) if normalize_keyword(t) == outcome_keyword(battle))
                }
                _ => {
                    let sides: &[Side] = match side {
                        Side::Either => &[Side::Attacker, Side::Defender],
                        Side::Attacker => &[Side::Attacker],
                        Side::Defender => &[Side::Defender],
                    };
                    sides
                        .iter()
                        .any(|&s| single_side_match(*field, s, *op, value, battle, identities))
                }
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn battle(
            id: i64,
            outcome: OutcomeType,
            atk_id: i32,
            def_id: i32,
            atk_surrendered: bool,
            def_surrendered: bool,
        ) -> BattleSummary {
            BattleSummary {
                id,
                checksum: format!("c{id}"),
                imported_at: "2026-01-01T00:00:00Z".to_string(),
                battle_outcome: outcome,
                last_frame: 1000,
                attacking_ship_id: atk_id,
                defending_ship_id: def_id,
                attacking_ship_surrendered: atk_surrendered,
                defending_ship_surrendered: def_surrendered,
            }
        }

        fn identity(ship_id: i32, user_id: i32, name: &str) -> ShipIdentity {
            ShipIdentity {
                ship_id,
                user_id,
                ship_name: name.to_string(),
                ship_level: 10,
                original_race_id: 1,
                power_score: 900,
                trophy: Some(2000),
                alliance_id: Some(1),
                alliance_name: Some("Trek Ad Astra".to_string()),
            }
        }

        #[test]
        fn empty_query_matches_everything() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            assert!(matches(&parse(""), &b, &HashMap::new()));
            assert!(matches(&parse("   "), &b, &HashMap::new()));
        }

        #[test]
        fn free_text_matches_ship_id_substring() {
            let b = battle(1, OutcomeType::AttackerWon, 9397907, 20, false, false);
            assert!(matches(&parse("9397907"), &b, &HashMap::new()));
            assert!(!matches(&parse("nope"), &b, &HashMap::new()));
        }

        #[test]
        fn player_name_matches_either_side() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            let mut identities = HashMap::new();
            identities.insert(20, identity(20, 999, "Dark_01"));
            assert!(matches(&parse("player_name: dark"), &b, &identities));
            assert!(matches(&parse("player_name: Dark_01"), &b, &identities));
            assert!(!matches(&parse("player_name: Nobody"), &b, &identities));
        }

        #[test]
        fn side_prefix_restricts_to_one_side() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            let mut identities = HashMap::new();
            identities.insert(20, identity(20, 999, "Dark_01"));
            assert!(matches(&parse("def_player_name: Dark_01"), &b, &identities));
            assert!(!matches(
                &parse("atk_player_name: Dark_01"),
                &b,
                &identities
            ));
        }

        #[test]
        fn not_equal_excludes_battles_involving_either_side() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            let mut identities = HashMap::new();
            identities.insert(20, identity(20, 999, "Dark_01"));
            assert!(!matches(&parse("player_name != Dark_01"), &b, &identities));
            assert!(matches(
                &parse("player_name != Somebody_Else"),
                &b,
                &identities
            ));
        }

        #[test]
        fn bang_prefix_negates_a_group() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            assert!(matches(&parse("!outcome: defaite"), &b, &HashMap::new()));
            assert!(!matches(&parse("!outcome: victoire"), &b, &HashMap::new()));
        }

        #[test]
        fn and_or_precedence_and_parens() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            let mut identities = HashMap::new();
            identities.insert(20, identity(20, 999, "Dark_01"));
            identities.insert(10, identity(10, 1, "Me"));

            assert!(matches(
                &parse("player_name: Dark_01 && outcome: victoire"),
                &b,
                &identities
            ));
            assert!(matches(
                &parse("player_name: Nobody || outcome: victoire"),
                &b,
                &identities
            ));
            assert!(matches(
                &parse("(player_name: Nobody || player_name: Dark_01) && atk_player_id > 0"),
                &b,
                &identities
            ));
        }

        #[test]
        fn implicit_and_between_bare_terms() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            let mut identities = HashMap::new();
            identities.insert(20, identity(20, 999, "Dark_01"));
            assert!(matches(
                &parse("player_name: Dark_01 outcome: victoire"),
                &b,
                &identities
            ));
            assert!(!matches(
                &parse("player_name: Dark_01 outcome: defaite"),
                &b,
                &identities
            ));
        }

        #[test]
        fn comparison_operators_on_trophy() {
            let b = battle(1, OutcomeType::AttackerWon, 10, 20, false, false);
            let mut identities = HashMap::new();
            identities.insert(20, identity(20, 999, "Dark_01"));
            assert!(matches(&parse("def_trophy > 1000"), &b, &identities));
            assert!(!matches(&parse("def_trophy > 5000"), &b, &identities));
            assert!(matches(&parse("def_trophy >= 2000"), &b, &identities));
        }

        #[test]
        fn abandon_adverse_outcome_keyword() {
            let win_via_surrender = battle(1, OutcomeType::AttackerWon, 10, 20, false, true);
            let clean_win = battle(2, OutcomeType::AttackerWon, 10, 20, false, false);
            assert!(matches(
                &parse("outcome: abandon_adverse"),
                &win_via_surrender,
                &HashMap::new()
            ));
            assert!(!matches(
                &parse("outcome: abandon_adverse"),
                &clean_win,
                &HashMap::new()
            ));
            assert!(matches(
                &parse("outcome: victoire"),
                &clean_win,
                &HashMap::new()
            ));
        }

        #[test]
        fn malformed_query_falls_back_to_free_text() {
            // Unclosed paren, a dangling "field:" with no value, and an unknown field name are
            // all malformed - each falls back to a plain substring search over the raw (lowered)
            // input rather than panicking or matching nothing.
            assert_eq!(
                parse("(player_name: Foo"),
                Expr::FreeText("(player_name: foo".to_string())
            );
            assert_eq!(
                parse("player_name:"),
                Expr::FreeText("player_name:".to_string())
            );
            assert_eq!(
                parse("unknown_field: 5"),
                Expr::FreeText("unknown_field: 5".to_string())
            );
        }
    }
}

/// Baked into the binary rather than read from disk at startup - a window icon is a fixed,
/// always-present asset, not something that varies at runtime or should be able to go missing
/// depending on the working directory the binary happens to be launched from.
const APP_ICON_BYTES: &[u8] = include_bytes!("../../../assets/pss_battlestats_icon.png");

fn main() -> eframe::Result<()> {
    let log_buffer = Arc::new(Mutex::new(VecDeque::new()));
    init_logging(log_buffer.clone());

    // `with_icon` alone has no effect on Wayland (winit/xdg-shell limitation): the compositor
    // sources the window/taskbar icon from a `.desktop` file matched by `app_id`, not from the
    // app at runtime. `with_app_id` must match the desktop file's basename
    // (~/.local/share/applications/pss-battlestats.desktop) for that lookup to succeed.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1300.0, 850.0])
        .with_app_id("pss-battlestats");
    match eframe::icon_data::from_png_bytes(APP_ICON_BYTES) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(err) => eprintln!("warning: failed to load app icon: {err}"),
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "pss-gui — PSS-BattleStats",
        options,
        Box::new(move |cc| {
            // Registers the HTTP+image-decode loaders behind `egui::Image::from_uri` - used to
            // render Patch notes' UGC design thumbnails straight from their blog-hosted URLs.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            match PssGuiApp::new(log_buffer) {
                Ok(app) => Ok(Box::new(app) as Box<dyn eframe::App>),
                Err(err) => Err(err.to_string().into()),
            }
        }),
    )
}

// ---------------------------------------------------------------- logging

#[derive(Debug, Clone)]
struct LogEntry {
    time: String,
    level: tracing::Level,
    message: String,
}

struct GuiLogLayer {
    buffer: Arc<Mutex<VecDeque<LogEntry>>>,
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for GuiLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Without this, every event from hyper/rustls/tokio/hudsucker (very chatty while a
        // capture is running - every intercepted request, not just FinaliseBattle, goes through
        // them) floods the capacity-bounded buffer and evicts real entries within seconds. That's
        // the "entries disappear shortly after" bug: they were never lost, just pushed out of the
        // ring buffer by noise the Journal never even displays (it's filtered at render time by
        // level, but was being counted against capacity at ingestion time). Our own crates' event
        // targets are always their crate name (module path defaults to it), so this keeps the
        // buffer scoped to what the Journal is meant to show.
        if !event.metadata().target().starts_with("pss_") {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        push_log_entry(&self.buffer, *event.metadata().level(), visitor.message);
    }
}

fn push_log_entry(buffer: &Arc<Mutex<VecDeque<LogEntry>>>, level: tracing::Level, message: String) {
    let mut buffer = buffer.lock().unwrap();
    buffer.push_back(LogEntry {
        time: now_hms(),
        level,
        message,
    });
    if buffer.len() > LOG_CAPACITY {
        buffer.pop_front();
    }
}

fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Installs a global `tracing` subscriber that feeds the Journal tab. If this fails (e.g.
/// something else already installed one), we print to stderr instead of swallowing it silently
/// — the previous version discarded the `Result`, which made a broken Journal impossible to
/// diagnose. Even if this fails, `PssGuiApp::log` still lets the GUI's own actions (import, sync,
/// capture toggle) reach the Journal directly, bypassing `tracing` entirely.
fn init_logging(buffer: Arc<Mutex<VecDeque<LogEntry>>>) {
    let subscriber = tracing_subscriber::registry().with(GuiLogLayer { buffer });
    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("warning: failed to install global tracing subscriber: {err}");
    }
}

// ---------------------------------------------------------------- CA setup

/// Persisted across launches so the app remembers a custom CA location instead of asking again
/// every time. Lives next to the SQLite files, in the working directory.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct GuiConfig {
    ca_cert_path: Option<PathBuf>,
    ca_key_path: Option<PathBuf>,
    #[serde(default = "default_lang")]
    lang: String,
    /// Game executable to launch (through the capture proxy) at startup. `None` = auto-detect
    #[serde(default)]
    game_path: Option<PathBuf>,
}

/// Candidate install locations of the Steam build, per OS. Only the Linux ones are confirmed;
/// the Windows/macOS file names are best guesses, hence `game_path` in the config as an override.
fn game_path_candidates() -> Vec<PathBuf> {
    const GAME_DIR: &str = "steamapps/common/Pixel Starships";
    let mut out = Vec::new();

    #[cfg(target_os = "linux")]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for steam in [
            ".steam/debian-installation",
            ".steam/steam",
            ".local/share/Steam",
        ] {
            out.push(
                home.join(steam)
                    .join(GAME_DIR)
                    .join("PixelStarships.x86_64"),
            );
        }
    }

    #[cfg(target_os = "windows")]
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
            out.push(base.join("Steam").join(GAME_DIR).join("PixelStarships.exe"));
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        out.push(
            home.join("Library/Application Support/Steam")
                .join(GAME_DIR)
                .join("PixelStarships.app"),
        );
    }

    out
}

fn detect_game_path() -> Option<PathBuf> {
    game_path_candidates().into_iter().find(|p| p.exists())
}

/// A macOS `.app` is a folder: the runnable file is inside `Contents/MacOS`.
fn resolve_game_executable(path: &std::path::Path) -> Option<PathBuf> {
    if path.extension().is_some_and(|e| e == "app") {
        let macos_dir = path.join("Contents/MacOS");
        return std::fs::read_dir(macos_dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_file());
    }
    Some(path.to_path_buf())
}

fn default_lang() -> String {
    i18n::DEFAULT_LANG.to_string()
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            ca_cert_path: None,
            ca_key_path: None,
            lang: default_lang(),
            game_path: None,
        }
    }
}

impl GuiConfig {
    fn load() -> Self {
        std::fs::read_to_string(app_paths().config_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(app_paths().config_file(), json);
        }
    }
}

/// Where the CA setup flow currently stands. Non-technical users (the GUI's audience, unlike
/// `pss-cli generate-ca` which stays a manual command for people comfortable with a terminal)
/// get walked through generating a certificate instead of being told to run a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaSetupState {
    /// Cert + key both exist on disk — capture can be used.
    Ready,
    /// Neither found (or one is missing) — show the "generate one" prompt.
    NeedsSetup,
    /// Just generated — show OS-specific trust instructions until dismissed.
    GeneratedInstructions,
}

// ---------------------------------------------------------------- app state

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Combats,
    Equipage,
    Historique,
    Designs,
    PatchNotes,
    Journal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeFilter {
    All,
    Wins,
    Losses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    fn toggled(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }

    /// Plain ASCII, not "▲"/"▼": the bundled egui font doesn't reliably include those glyphs,
    /// which rendered as an empty tofu box (same class of issue as the battle-list marker, which
    /// used a painted rect instead of "▍" for the same reason - inline sort-header text can't use
    /// that trick, so this uses a guaranteed-available fallback instead).
    fn arrow(self) -> &'static str {
        match self {
            SortDirection::Ascending => "^",
            SortDirection::Descending => "v",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoomSortKey {
    Room,
    Shots,
    Hull,
    System,
    Shield,
    Character,
    Ap,
    Dps,
    Repair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrewSortKey {
    Name,
    State,
    DamageTaken,
    Healed,
    Teleports,
    Ability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DroneSortKey {
    Name,
    Quantity,
    Hull,
    System,
    Shield,
    Character,
    Ap,
    Craft,
    Fire,
    Dps,
}

/// One ammo/craft/android item this side deployed, resolved for the "Consommés" chip row.
struct ConsumedAmmo {
    name: String,
    quantity: i64,
}

struct SideDetail {
    is_attacker: bool,
    ship_id: i32,
    ship_design_id: i32,
    max_hp: f64,
    hull_remaining: f64,
    /// Sum of `Capacity` across this ship's surviving Shield rooms - NOT a "max shield pool" to
    /// compare `shield_damage_taken` against. Cross-checking real captures showed a ship can take
    /// more cumulative shield damage than this number while its shield room stays intact and hull
    /// damage stays at 0 (battle #1: 9.5 cumulative shield dmg vs. capacity 8) - shields recharge
    /// during the fight (RoomDesign's `ManufactureType="Restore"`), so `shield_damage_taken` is a
    /// cumulative total like every other damage field, not a depletion of this capacity. This is
    /// the generator's instantaneous recharge-buffer size, shown as its own stat, not a max.
    shield_capacity: f64,
    shield_damage_taken: f64,
    /// Only set once the ship actually left the battle destroyed or surrendered - a still
    /// standing ship has nothing to report here (see mockup: only the losing side shows "Issue").
    status: Option<(OutcomeKind, egui::Color32)>,
    stats: ShipBattleStats,
    consumed_ammo: Vec<ConsumedAmmo>,
    /// Kept alongside `stats.damage.rooms` (weapon rooms only) so the room table can also show
    /// non-weapon rooms that matter, like Repair Bay's `repaired_hp` contribution.
    ship: ShipReport,
    /// Pseudo/niveau/faction/trophées/alliance - only known if pss-capture saw this ship_id's
    /// `BattleService/CreateBattle` at some point (battles imported before that capture existed,
    /// or fought without pss-capture running, have no identity to show here).
    identity: Option<pss_store::ShipIdentity>,
    /// One row per distinct Missile/Craft item design deployed - "which drone/missile dealt how
    /// much", same idea as the room table but keyed by ammo identity instead of room identity.
    drones: Vec<pss_stats::ItemWeaponStats>,
}

/// "Issue" label for a side's summary panel - only `Some` when the ship was destroyed or
/// surrendered (see `ShipReport::is_destroyed`/`is_surrendered`, §6.2). Colors match the same
/// outcome coding already used for the battle-list marker (red = destroyed, orange = surrendered).
fn ship_status_label(ship: &ShipReport) -> Option<(OutcomeKind, egui::Color32)> {
    if ship.is_destroyed() {
        Some((OutcomeKind::Destroyed, palette::BAD))
    } else if ship.is_surrendered() {
        Some((OutcomeKind::Surrender, palette::WARN))
    } else {
        None
    }
}

impl SideDetail {
    /// "Attaquant"/"Défenseur", resolved at render time so it follows the UI language.
    fn label(&self) -> String {
        if self.is_attacker {
            tr!("side.attacker")
        } else {
            tr!("side.defender")
        }
    }
}

/// Language-independent outcome/status, translated only when drawn (`text`) so the labels follow
/// the UI language and tests can assert on the kind instead of on a localized string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    Win,
    Loss,
    Draw,
    Other,
    Surrender,
    SurrenderAdverse,
    Destroyed,
}

impl OutcomeKind {
    fn text(self) -> String {
        match self {
            OutcomeKind::Win => tr!("outcome.win"),
            OutcomeKind::Loss => tr!("outcome.loss"),
            OutcomeKind::Draw => tr!("outcome.draw"),
            OutcomeKind::Other => tr!("outcome.other"),
            OutcomeKind::Surrender => tr!("outcome.surrender"),
            OutcomeKind::SurrenderAdverse => tr!("outcome.surrender_adverse"),
            OutcomeKind::Destroyed => tr!("outcome.destroyed"),
        }
    }
}

/// Battle-list row label - same 4-state coding as `ship_status_label`/the row color, but from the
/// win/loss side: a win where the defender bailed out reads as "Abandon adverse" (not a clean
/// "Victoire"), and a loss where you bailed out reads as "Abandon" (not "Défaite").
fn outcome_label(battle: &BattleSummary) -> OutcomeKind {
    match battle.battle_outcome {
        OutcomeType::AttackerWon if battle.defending_ship_surrendered => {
            OutcomeKind::SurrenderAdverse
        }
        OutcomeType::AttackerWon => OutcomeKind::Win,
        OutcomeType::DefenderWon if battle.attacking_ship_surrendered => OutcomeKind::Surrender,
        OutcomeType::DefenderWon => OutcomeKind::Loss,
        OutcomeType::Draw => OutcomeKind::Draw,
        _ => OutcomeKind::Other,
    }
}

/// Space-grouped thousands, e.g. `7441203` -> `"7 441 203"` - matches the mockup's ship id
/// formatting in the battle list.
fn format_thousands(n: i32) -> String {
    let digits = n.unsigned_abs().to_string();
    let grouped: Vec<&str> = {
        let mut groups = Vec::new();
        let bytes = digits.as_bytes();
        let mut end = bytes.len();
        while end > 3 {
            groups.push(std::str::from_utf8(&bytes[end - 3..end]).unwrap());
            end -= 3;
        }
        groups.push(std::str::from_utf8(&bytes[..end]).unwrap());
        groups.reverse();
        groups
    };
    let sign = if n < 0 { "-" } else { "" };
    format!("{sign}{}", grouped.join(" "))
}

/// Byte ranges of `text` that look like a recognized filter field name plus its trailing operator
/// (`:`/`!=`/`>`/`<`/`>=`/`<=`), e.g. `player_id:` - pure/no egui dependency so it's testable
/// without a live font context. Shared by the syntax highlighter below and its tests.
fn find_field_highlight_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let start = i;
        while i < text.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i > start {
            let ident = &text[start..i];
            let mut j = i;
            while j < text.len() && bytes[j] == b' ' {
                j += 1;
            }
            let op_len = if text[j..].starts_with("!=")
                || text[j..].starts_with(">=")
                || text[j..].starts_with("<=")
            {
                Some(2)
            } else if j < text.len() && matches!(bytes[j], b':' | b'>' | b'<') {
                Some(1)
            } else {
                None
            };
            if let Some(len) = op_len {
                if filter::is_known_field(ident) {
                    spans.push((start, j + len));
                    i = j + len;
                }
            }
            continue;
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    spans
}

/// `TextEdit` layouter for the sidebar's search box: badges a recognized filter field name (with
/// its trailing `:`/`!=`/`>`/`<`/`>=`/`<=`) as it's typed, e.g. `player_id:` gets its own pill
/// background - independent of whether the rest of the query currently parses, so the badge
/// appears the instant a valid field name is typed, not only once a whole well-formed term is
/// complete.
fn highlight_filter_query(
    ui: &egui::Ui,
    text: &str,
    wrap_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    let base_color = ui.visuals().text_color();
    let field_color = ui.visuals().strong_text_color();
    let field_bg = ui.visuals().selection.bg_fill;

    let mut pos = 0;
    for (start, end) in find_field_highlight_spans(text) {
        if start > pos {
            job.append(
                &text[pos..start],
                0.0,
                egui::TextFormat {
                    color: base_color,
                    ..Default::default()
                },
            );
        }
        job.append(
            &text[start..end],
            0.0,
            egui::TextFormat {
                color: field_color,
                background: field_bg,
                ..Default::default()
            },
        );
        pos = end;
    }
    if pos < text.len() {
        job.append(
            &text[pos..],
            0.0,
            egui::TextFormat {
                color: base_color,
                ..Default::default()
            },
        );
    }
    job.wrap.max_width = wrap_width;
    ui.painter().layout_job(job)
}

/// Cheat-sheet bubble for the sidebar's filter query language (see the `filter` module) - shown
/// while the search box has keyboard focus, hidden otherwise, so it doesn't clutter the sidebar
/// the rest of the time.
fn render_filter_help_popup(search_response: &egui::Response) {
    egui::Popup::from_response(search_response)
        .open(search_response.has_focus())
        .width(420.0)
        .show(|ui| {
            ui.strong(tr!("filter.free_text"));
            ui.label(tr!("filter.free_text_desc"));
            ui.add_space(6.0);

            ui.strong(tr!("filter.fields"));
            egui::Grid::new("filter_help_fields")
                .num_columns(2)
                .spacing([12.0, 2.0])
                .show(ui, |ui| {
                    ui.label("player_id / player_name");
                    ui.weak(tr!("filter.f_player"));
                    ui.end_row();
                    ui.label("alliance");
                    ui.weak(tr!("filter.f_alliance"));
                    ui.end_row();
                    ui.label("ship_id");
                    ui.weak(tr!("filter.f_ship"));
                    ui.end_row();
                    ui.label("level");
                    ui.weak(tr!("filter.f_level"));
                    ui.end_row();
                    ui.label("trophy");
                    ui.weak(tr!("filter.f_trophy"));
                    ui.end_row();
                    ui.label("race");
                    ui.weak(tr!("filter.f_race"));
                    ui.end_row();
                    ui.label("battle_id (ou id)");
                    ui.weak(tr!("filter.f_battle"));
                    ui.end_row();
                    ui.label("outcome");
                    ui.weak(tr!("filter.f_outcome"));
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.label(tr!("filter.side_prefix"));
            ui.add_space(6.0);

            ui.strong(tr!("filter.operators"));
            ui.label(tr!("filter.operators_text"));
            ui.add_space(6.0);

            ui.strong(tr!("filter.examples"));
            ui.code("player_name: Dark_01 && outcome: victoire");
            ui.code("def_trophy > 2000 || alliance: \"Trek Ad Astra\"");
            ui.code("!outcome: victoire");
        });
}

struct BattleDetail {
    outcome: OutcomeType,
    last_frame: i32,
    win_loot: Vec<(String, i64)>,
    lose_loot: Vec<(String, i64)>,
    attacker: SideDetail,
    defender: SideDetail,
}

/// Where one design type's sync currently stands. There's no live item-by-item counter here (the
/// XML endpoints return one whole document per request - a real per-record count would need a
/// streaming XML parse instead of pss-designs' current parse-the-whole-response approach), so this
/// tracks real per-endpoint phases instead of faking a fractional progress number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SyncPhase {
    #[default]
    Idle,
    InProgress,
    Done,
    Failed,
}

#[derive(Default)]
struct SyncState {
    room: SyncPhase,
    item: SyncPhase,
    character: SyncPhase,
    missile: SyncPhase,
    craft: SyncPhase,
    /// Final counts on success, error text on failure - shown as the menu bar's one combined
    /// status line, alongside the five per-type phases used for the Designs tab's progress rows.
    message: String,
}

struct PssGuiApp {
    store: BattleStore,
    designs: DesignsCache,
    fps: f64,

    tab: Tab,
    filter: OutcomeFilter,
    search: String,

    battles: Vec<BattleSummary>,
    /// Keyed by ship_id, built once per `reload()` from `battles`' own attacker/defender ids -
    /// feeds the search box's `player_name:`/`alliance:`/`level:`/`trophy:` filters (see the
    /// `filter` module) without re-querying `pss-store` on every keystroke/frame.
    ship_identities: HashMap<i32, ShipIdentity>,
    win_rates: Vec<ShipWinRate>,
    battle_weapon_dps: Vec<pss_stats::BattleWeaponTypeDps>,
    battle_damage: Vec<pss_stats::BattleDamageHistoryEntry>,
    design_counts: DesignsCounts,
    calibrations: Vec<(CalibratedItemDamageRecord, String)>, // (record, item name)

    patch_notes: Vec<pss_designs::patch_notes::PatchNoteVersion>,
    patch_notes_last_fetched_at: Option<String>,
    selected_patch_note_version: Option<String>,
    /// (room_name, active-instance count) on the attacking ship of the most recently captured
    /// battle - "your current ship", since the attacker is always the local player (pss-capture
    /// only ever sees traffic leaving this machine). Powers Patch notes' "vos salles" column via
    /// fuzzy substring matching against a `BalanceChange`'s `subject` text (no clean id-based join
    /// exists between free-form blog prose and `room_designs`).
    my_current_rooms: Vec<(String, usize)>,

    selected_battle_id: Option<i64>,
    selected_detail: Option<BattleDetail>,
    /// Shared by both sides' room/crew tables in the Combats tab - sorting one side sorts the
    /// other the same way, since independent per-side sort state would make the two tables harder
    /// to compare at a glance for no real benefit.
    room_sort: (RoomSortKey, SortDirection),
    crew_sort: (CrewSortKey, SortDirection),
    drone_sort: (DroneSortKey, SortDirection),
    /// Which page of battles the Historique tab's two charts show - 0 is the most recent window,
    /// higher values page further back in time. Shared by both charts so they always show the
    /// same time slice together.
    historique_page: usize,

    sync_state: Arc<Mutex<SyncState>>,
    /// Edge-detector for the syncing->done transition: `design_counts` (and everything derived
    /// from it - progress rows, the sync button label) is a snapshot taken at startup/reload, not
    /// a live query, so nothing refreshes it once a background sync completes on its own thread.
    designs_syncing: bool,
    capture_handle: Option<CaptureHandle>,
    game_path: Option<PathBuf>,
    game_process: Option<std::process::Child>,

    ca_cert_path: PathBuf,
    ca_key_path: PathBuf,
    ca_setup: CaSetupState,

    log_buffer: Arc<Mutex<VecDeque<LogEntry>>>,
    log_min_level: tracing::Level,

    crew: crew::CrewTab,
    /// Code of the active UI language, one of `i18n::LANGUAGES`.
    lang: &'static str,
}

impl PssGuiApp {
    fn new(log_buffer: Arc<Mutex<VecDeque<LogEntry>>>) -> Result<Self> {
        let paths = app_paths();
        paths
            .ensure_dirs()
            .context("Failed to create the app's data folders")?;
        for (from, to) in paths.migrate_legacy(std::path::Path::new(".")) {
            tracing::info!("migrated {} -> {}", from.display(), to.display());
        }
        let store = BattleStore::open(&paths.battles_db().to_string_lossy())?;
        let designs = DesignsCache::open(&paths.designs_db().to_string_lossy())?;

        let config = GuiConfig::load();
        let lang = i18n::normalize(&config.lang);
        i18n::set(lang);
        let game_path = config.game_path.clone().or_else(detect_game_path);
        let ca_cert_path = config
            .ca_cert_path
            .unwrap_or_else(|| app_paths().default_ca_cert());
        let ca_key_path = config
            .ca_key_path
            .unwrap_or_else(|| app_paths().default_ca_key());
        let ca_setup = if ca_cert_path.exists() && ca_key_path.exists() {
            CaSetupState::Ready
        } else {
            CaSetupState::NeedsSetup
        };

        let mut app = Self {
            store,
            designs,
            fps: pss_stats::DEFAULT_FRAMES_PER_SECOND,
            tab: Tab::Combats,
            filter: OutcomeFilter::All,
            search: String::new(),
            battles: Vec::new(),
            ship_identities: HashMap::new(),
            win_rates: Vec::new(),
            battle_weapon_dps: Vec::new(),
            battle_damage: Vec::new(),
            design_counts: DesignsCounts::default(),
            calibrations: Vec::new(),
            patch_notes: Vec::new(),
            patch_notes_last_fetched_at: None,
            selected_patch_note_version: None,
            my_current_rooms: Vec::new(),
            selected_battle_id: None,
            selected_detail: None,
            room_sort: (RoomSortKey::Hull, SortDirection::Descending),
            crew_sort: (CrewSortKey::DamageTaken, SortDirection::Descending),
            drone_sort: (DroneSortKey::Dps, SortDirection::Descending),
            historique_page: 0,
            sync_state: Arc::new(Mutex::new(SyncState::default())),
            designs_syncing: false,
            capture_handle: None,
            game_path,
            game_process: None,
            ca_cert_path,
            ca_key_path,
            ca_setup,
            log_buffer,
            log_min_level: tracing::Level::INFO,
            crew: crew::CrewTab::load(),
            lang,
        };

        app.reload()?;
        app.log(tracing::Level::INFO, tr!("log.started"));
        if app.ca_setup == CaSetupState::Ready {
            // The proxy must be listening before the game starts, or its first requests bypass it.
            app.toggle_capture();
            app.launch_game();
        }
        if app.designs.has_invalidated_domains()? {
            app.log(tracing::Level::INFO, tr!("log.migration"));
            app.start_designs_sync();
        }
        Ok(app)
    }

    /// Pushes a line straight into the Journal buffer, independent of the global `tracing`
    /// subscriber. Used for the GUI's own actions so the Journal stays useful even if the
    /// cross-crate `tracing` plumbing (used by pss-capture's background thread) isn't working.
    fn log(&self, level: tracing::Level, message: impl Into<String>) {
        push_log_entry(&self.log_buffer, level, message.into());
    }

    fn reload(&mut self) -> Result<()> {
        self.battles = self.store.list_battles(500, 0)?;
        self.win_rates = self.store.win_rate_by_attacking_ship()?;

        let mut ship_ids = HashSet::new();
        for battle in &self.battles {
            ship_ids.insert(battle.attacking_ship_id);
            ship_ids.insert(battle.defending_ship_id);
        }
        self.ship_identities.clear();
        for ship_id in ship_ids {
            if let Some(identity) = self.store.get_ship_identity(ship_id)? {
                self.ship_identities.insert(ship_id, identity);
            }
        }

        let all_battles = self.store.list_all_battles()?;
        self.battle_weapon_dps =
            pss_stats::battle_weapon_type_dps(&all_battles, &self.designs, self.fps)?;

        self.battle_damage = pss_stats::battle_damage_history(&all_battles, &self.designs)?;
        self.design_counts = self.designs.counts()?;
        self.crew.characters = self.designs.list_character_designs()?;
        self.crew.equipment = self.designs.list_equipment_designs()?;

        self.calibrations = self
            .designs
            .list_calibrated_item_damage()?
            .into_iter()
            .map(|record| {
                let name = self
                    .designs
                    .get_item_design(record.item_design_id)
                    .ok()
                    .flatten()
                    .map(|d| d.item_design_name)
                    .unwrap_or_else(|| format!("item #{}", record.item_design_id));
                (record, name)
            })
            .collect();

        self.patch_notes = self.designs.list_patch_notes()?;
        self.patch_notes_last_fetched_at = self.designs.patch_notes_last_fetched_at()?;
        if self.selected_patch_note_version.is_none() {
            self.selected_patch_note_version = self.patch_notes.first().map(|v| v.version.clone());
        }

        let most_recent_battle_id = self.battles.first().map(|b| b.id);
        self.my_current_rooms = most_recent_battle_id
            .and_then(|id| self.store.get_battle(id).ok().flatten())
            .map(|stored| {
                let mut counts: HashMap<i32, usize> = HashMap::new();
                for room in stored
                    .payload
                    .battle_report
                    .attacking_ship_report
                    .room_reports
                    .values()
                {
                    if !room.is_destroyed {
                        *counts.entry(room.room_design_id).or_insert(0) += 1;
                    }
                }
                counts
                    .into_iter()
                    .filter_map(|(design_id, count)| {
                        self.designs
                            .get_room_design(design_id)
                            .ok()
                            .flatten()
                            .map(|d| (d.room_name, count))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(())
    }

    fn select_battle(&mut self, id: i64) {
        self.selected_battle_id = Some(id);
        self.selected_detail = self.load_detail(id).ok();
    }

    fn consumed_ammo(&self, ship: &ShipReport) -> Vec<ConsumedAmmo> {
        let mut totals: HashMap<i32, i64> = HashMap::new();
        for item in ship.item_reports.values() {
            if matches!(
                item.item_type,
                ItemType::Missile | ItemType::Craft | ItemType::Android
            ) && item.quantity_deployed > 0
            {
                *totals.entry(item.item_design_id).or_insert(0) += item.quantity_deployed as i64;
            }
        }
        totals
            .into_iter()
            .map(|(id, quantity)| {
                let name = self
                    .designs
                    .get_item_design(id)
                    .ok()
                    .flatten()
                    .map(|d| d.item_design_name)
                    .unwrap_or_else(|| format!("item #{id}"));
                ConsumedAmmo { name, quantity }
            })
            .collect()
    }

    fn load_detail(&self, id: i64) -> Result<BattleDetail> {
        let stored = self.store.get_battle(id)?.context("battle not found")?;
        let report = &stored.payload.battle_report;

        let win_loot = pss_stats::named_loot(&report.win_items, &self.designs)?;
        let lose_loot = pss_stats::named_loot(&report.lose_items, &self.designs)?;

        let attacker = SideDetail {
            is_attacker: true,
            ship_id: report.attacking_ship_report.ship_id,
            ship_design_id: report.attacking_ship_report.ship_design_id,
            max_hp: report.attacking_ship_report.max_ship_hp / 100.0,
            hull_remaining: report.attacking_ship_report.max_ship_hp / 100.0
                - report.attacking_ship_report.hull_damage / 100.0,
            shield_capacity: pss_stats::ship_shield_capacity(
                &report.attacking_ship_report,
                &self.designs,
            )?,
            shield_damage_taken: report.attacking_ship_report.shield_damage / 100.0,
            status: ship_status_label(&report.attacking_ship_report),
            stats: pss_stats::compute_ship_battle_stats(
                &report.attacking_ship_report,
                &report.defending_ship_report,
                &self.designs,
            )?,
            consumed_ammo: self.consumed_ammo(&report.attacking_ship_report),
            ship: report.attacking_ship_report.clone(),
            identity: self
                .store
                .get_ship_identity(report.attacking_ship_report.ship_id)?,
            drones: pss_stats::item_weapon_stats(&report.attacking_ship_report, &self.designs),
        };
        let defender = SideDetail {
            is_attacker: false,
            ship_id: report.defending_ship_report.ship_id,
            ship_design_id: report.defending_ship_report.ship_design_id,
            max_hp: report.defending_ship_report.max_ship_hp / 100.0,
            hull_remaining: report.defending_ship_report.max_ship_hp / 100.0
                - report.defending_ship_report.hull_damage / 100.0,
            shield_capacity: pss_stats::ship_shield_capacity(
                &report.defending_ship_report,
                &self.designs,
            )?,
            shield_damage_taken: report.defending_ship_report.shield_damage / 100.0,
            status: ship_status_label(&report.defending_ship_report),
            stats: pss_stats::compute_ship_battle_stats(
                &report.defending_ship_report,
                &report.attacking_ship_report,
                &self.designs,
            )?,
            consumed_ammo: self.consumed_ammo(&report.defending_ship_report),
            ship: report.defending_ship_report.clone(),
            identity: self
                .store
                .get_ship_identity(report.defending_ship_report.ship_id)?,
            drones: pss_stats::item_weapon_stats(&report.defending_ship_report, &self.designs),
        };

        Ok(BattleDetail {
            outcome: report.battle_outcome,
            last_frame: report.last_frame,
            win_loot,
            lose_loot,
            attacker,
            defender,
        })
    }

    fn start_designs_sync(&self) {
        let state = self.sync_state.clone();
        {
            let mut guard = state.lock().unwrap();
            let already_running = guard.room == SyncPhase::InProgress
                || guard.item == SyncPhase::InProgress
                || guard.character == SyncPhase::InProgress
                || guard.missile == SyncPhase::InProgress
                || guard.craft == SyncPhase::InProgress;
            if already_running {
                return;
            }
            guard.room = SyncPhase::InProgress;
            guard.item = SyncPhase::Idle;
            guard.character = SyncPhase::Idle;
            guard.missile = SyncPhase::Idle;
            guard.craft = SyncPhase::Idle;
            guard.message = tr!("log.syncing");
        }
        self.log(tracing::Level::INFO, tr!("log.sync_started"));

        std::thread::spawn(move || {
            let result: Result<String> = (|| {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(async {
                    let mut designs = DesignsCache::open(&app_paths().designs_db().to_string_lossy())?;
                    let http = reqwest::Client::new();

                    // Each phase flips to InProgress/Done/Failed right before and after its own
                    // await, so the Designs tab's three rows update live as the sequence runs
                    // instead of jumping from "nothing happened" straight to a single final
                    // combined message once all three calls have returned.
                    let rooms = pss_designs::sync_room_designs(&mut designs, &http).await;
                    state.lock().unwrap().room = if rooms.is_ok() { SyncPhase::Done } else { SyncPhase::Failed };
                    let rooms = rooms?;

                    state.lock().unwrap().item = SyncPhase::InProgress;
                    let items = pss_designs::sync_item_designs(&mut designs, &http).await;
                    state.lock().unwrap().item = if items.is_ok() { SyncPhase::Done } else { SyncPhase::Failed };
                    let items = items?;

                    state.lock().unwrap().character = SyncPhase::InProgress;
                    let characters = pss_designs::sync_character_designs(&mut designs, &http).await;
                    state.lock().unwrap().character =
                        if characters.is_ok() { SyncPhase::Done } else { SyncPhase::Failed };
                    let characters = characters?;

                    state.lock().unwrap().missile = SyncPhase::InProgress;
                    let missiles = pss_designs::sync_missile_designs(&mut designs, &http).await;
                    state.lock().unwrap().missile =
                        if missiles.is_ok() { SyncPhase::Done } else { SyncPhase::Failed };
                    let missiles = missiles?;

                    state.lock().unwrap().craft = SyncPhase::InProgress;
                    let crafts = pss_designs::sync_craft_designs(&mut designs, &http).await;
                    state.lock().unwrap().craft =
                        if crafts.is_ok() { SyncPhase::Done } else { SyncPhase::Failed };
                    let crafts = crafts?;

                    Ok::<_, anyhow::Error>(format!(
                        "rooms {rooms:?} · items {items:?} · characters {characters:?} · missiles {missiles:?} · crafts {crafts:?}"
                    ))
                })
            })();

            let mut guard = state.lock().unwrap();
            guard.message = match result {
                Ok(msg) => msg,
                Err(err) => tr!("log.sync_failed", err = err.to_string()),
            };
        });
    }

    /// Opens a native "save file" dialog for the cert location, derives the key's path next to
    /// it, generates a fresh CA, writes both files, and remembers the choice for next launch.
    fn generate_ca_interactive(&mut self) {
        let dialog = rfd::FileDialog::new()
            .set_file_name("pss-capture-ca.pem")
            .set_directory(app_paths().ca_dir())
            .add_filter(tr!("ca.pem_filter"), &["pem"]);

        let Some(cert_path) = dialog.save_file() else {
            return; // user cancelled - stay on the "needs setup" screen
        };

        let stem = cert_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pss-capture-ca");
        let key_path = cert_path.with_file_name(format!("{stem}-key.pem"));

        match pss_capture::generate_ca() {
            Ok(ca) => {
                let wrote_cert = std::fs::write(&cert_path, &ca.cert_pem);
                let wrote_key = std::fs::write(&key_path, &ca.key_pem);
                if wrote_cert.is_ok() && wrote_key.is_ok() {
                    self.ca_cert_path = cert_path.clone();
                    self.ca_key_path = key_path.clone();
                    GuiConfig {
                        ca_cert_path: Some(cert_path),
                        ca_key_path: Some(key_path),
                        lang: self.lang.to_string(),
                        game_path: self.game_path.clone(),
                    }
                    .save();
                    self.ca_setup = CaSetupState::GeneratedInstructions;
                    self.log(tracing::Level::INFO, tr!("log.cert_generated"));
                } else {
                    self.log(tracing::Level::WARN, tr!("log.cert_write_failed"));
                }
            }
            Err(err) => self.log(
                tracing::Level::WARN,
                tr!("log.cert_gen_failed", err = err.to_string()),
            ),
        }
    }

    fn toggle_capture(&mut self) {
        if let Some(mut handle) = self.capture_handle.take() {
            handle.stop();
            self.log(tracing::Level::INFO, tr!("log.capture_stopped"));
            return;
        }

        if self.ca_setup != CaSetupState::Ready {
            self.log(tracing::Level::WARN, tr!("log.cert_first"));
            return;
        }

        let (Ok(cert), Ok(key)) = (
            std::fs::read_to_string(&self.ca_cert_path),
            std::fs::read_to_string(&self.ca_key_path),
        ) else {
            self.log(tracing::Level::WARN, tr!("log.cert_read_failed"));
            return;
        };

        let Ok(listen_addr) = CAPTURE_LISTEN.parse() else {
            return;
        };

        self.capture_handle = Some(pss_capture::start(
            listen_addr,
            cert,
            key,
            app_paths().battles_db().to_string_lossy().into_owned(),
        ));
        self.log(
            tracing::Level::INFO,
            tr!("log.capture_started", addr = CAPTURE_LISTEN),
        );
    }

    /// Starts the game with its HTTP(S) traffic routed through the capture proxy.
    fn launch_game(&mut self) {
        if let Some(child) = self.game_process.as_mut() {
            // Already running: don't start a second instance.
            if matches!(child.try_wait(), Ok(None)) {
                return;
            }
        }
        let Some(path) = self.game_path.clone() else {
            self.log(tracing::Level::WARN, tr!("log.game_not_found"));
            return;
        };

        let Some(path) = resolve_game_executable(&path) else {
            self.log(tracing::Level::WARN, tr!("log.game_not_found"));
            return;
        };

        // Launched outside Steam, the Steamworks SDK needs this file next to the executable to
        // know which app it is; without it the game refuses to start.
        if let Some(dir) = path.parent() {
            let appid_file = dir.join("steam_appid.txt");
            if !appid_file.exists()
                && let Err(err) = std::fs::write(&appid_file, STEAM_APP_ID)
            {
                self.log(
                    tracing::Level::WARN,
                    tr!("log.appid_write_failed", err = err.to_string()),
                );
            }
        }

        let proxy = format!("http://{CAPTURE_LISTEN}");
        let mut cmd = std::process::Command::new(&path);
        if let Some(dir) = path.parent() {
            cmd.current_dir(dir);
        }
        cmd.env("http_proxy", &proxy)
            .env("https_proxy", &proxy)
            .env("HTTP_PROXY", &proxy)
            .env("HTTPS_PROXY", &proxy);

        match cmd.spawn() {
            Ok(child) => {
                self.game_process = Some(child);
                self.log(tracing::Level::INFO, tr!("log.game_launched"));
            }
            Err(err) => self.log(
                tracing::Level::WARN,
                tr!("log.game_launch_failed", err = err.to_string()),
            ),
        }
    }

    /// Resolves trophies/alliance for any ship identity `pss-capture` has already seen via
    /// `CreateBattle` but never profiled (`UserService/SearchUsers`, public, no access token).
    /// Blocking/synchronous - a handful of quick HTTP GETs, not worth a background thread and
    /// shared state the way the Designs tab's much larger catalog sync needs.
    fn resolve_ship_identities(&mut self) {
        let result: Result<usize> = (|| {
            let rt = tokio::runtime::Runtime::new()?;
            let http = reqwest::Client::new();
            rt.block_on(self.store.sync_ship_profiles(&http))
        })();

        match result {
            Ok(resolved) => {
                self.log(tracing::Level::INFO, tr!("log.identities_ok", n = resolved));
                self.selected_detail = self
                    .selected_battle_id
                    .and_then(|id| self.load_detail(id).ok());
            }
            Err(err) => self.log(
                tracing::Level::WARN,
                tr!("log.identities_failed", err = err.to_string()),
            ),
        }
    }

    /// Re-fetches "Galaxy Patch Notes" from blog.pixelstarships.com (a handful of small HTTP
    /// requests - a WordPress feed, not a design catalog - so this is blocking/synchronous like
    /// `resolve_ship_identities`, not worth a background thread).
    fn refresh_patch_notes(&mut self) {
        let result: Result<usize> = (|| {
            let rt = tokio::runtime::Runtime::new()?;
            let http = reqwest::Client::new();
            rt.block_on(pss_designs::sync_patch_notes(&mut self.designs, &http))
        })();

        match result {
            Ok(count) => {
                self.log(tracing::Level::INFO, tr!("log.patch_ok", n = count));
                let _ = self.reload();
            }
            Err(err) => self.log(
                tracing::Level::WARN,
                tr!("log.patch_failed", err = err.to_string()),
            ),
        }
    }

    fn import_file(&mut self, path: PathBuf) {
        let result: Result<String> = (|| {
            let raw = std::fs::read(&path)?;
            let decoded = pss_codec::decode_battle_report(&raw).map_err(|e| anyhow::anyhow!(e))?;
            let payload: pss_model::FinaliseBattlePayload = serde_json::from_str(&decoded)?;
            let message = match self.store.insert_battle(&payload)? {
                pss_store::InsertOutcome::Inserted { id } => {
                    tr!("log.imported", id = id, path = path.display().to_string())
                }
                pss_store::InsertOutcome::AlreadyExists { id } => {
                    tr!("log.already", id = id, path = path.display().to_string())
                }
            };
            self.reload()?;
            Ok(message)
        })();

        match result {
            Ok(message) => self.log(tracing::Level::INFO, message),
            Err(err) => self.log(
                tracing::Level::WARN,
                tr!("log.import_failed", err = err.to_string()),
            ),
        }
    }

    /// Saves a battle's stored report (the decoded `FinaliseBattle` payload, pretty-printed JSON)
    /// to a file chosen in a native "save" dialog.
    fn export_battle_json(&mut self, id: i64) {
        let Some(path) = rfd::FileDialog::new()
            .set_file_name(format!("battle_{id}.json"))
            .add_filter("JSON", &["json"])
            .save_file()
        else {
            return; // cancelled
        };

        let result: Result<()> = (|| {
            let raw = self
                .store
                .get_battle_json(id)?
                .context("battle not found")?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            std::fs::write(&path, serde_json::to_string_pretty(&value)?)?;
            Ok(())
        })();

        match result {
            Ok(()) => self.log(
                tracing::Level::INFO,
                tr!("log.export_ok", id = id, path = path.display().to_string()),
            ),
            Err(err) => self.log(
                tracing::Level::WARN,
                tr!("log.export_failed", err = err.to_string()),
            ),
        }
    }
}

// ---------------------------------------------------------------- rendering

impl eframe::App for PssGuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.ca_setup != CaSetupState::Ready {
            self.render_ca_setup(ui);
        }

        let current_sync_phase_name: Option<&'static str>;
        {
            let guard = self.sync_state.lock().unwrap();
            let syncing_now = guard.room == SyncPhase::InProgress
                || guard.item == SyncPhase::InProgress
                || guard.character == SyncPhase::InProgress
                || guard.missile == SyncPhase::InProgress
                || guard.craft == SyncPhase::InProgress;
            current_sync_phase_name = if guard.room == SyncPhase::InProgress {
                Some("RoomDesign")
            } else if guard.item == SyncPhase::InProgress {
                Some("ItemDesign")
            } else if guard.character == SyncPhase::InProgress {
                Some("CharacterDesign")
            } else if guard.missile == SyncPhase::InProgress {
                Some("MissileDesign")
            } else if guard.craft == SyncPhase::InProgress {
                Some("CraftDesign")
            } else {
                None
            };
            let message = guard.message.clone();
            drop(guard);
            if syncing_now {
                // Keeps this status pill (and the Designs tab's progress rows) animating without
                // user input - eframe doesn't repaint on a timer by default, only on OS events.
                ui.ctx().request_repaint();
            }
            if self.designs_syncing && !syncing_now {
                self.log(
                    tracing::Level::INFO,
                    tr!("log.sync_done", message = message),
                );
                let _ = self.reload();
            }
            self.designs_syncing = syncing_now;
        }

        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button(tr!("menu.file"), |ui| {
                    if ui.button(tr!("menu.import_bin")).clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            self.import_file(path);
                        }
                        ui.close();
                    }
                    if ui.button(tr!("menu.refresh")).clicked() {
                        let _ = self.reload();
                        ui.close();
                    }
                    if ui.button(tr!("menu.quit")).clicked() {
                        std::process::exit(0);
                    }
                });
                ui.menu_button(tr!("menu.battles"), |_ui| {});
                ui.menu_button(tr!("menu.designs"), |_ui| {});
                ui.menu_button(tr!("menu.help"), |_ui| {});
                ui.menu_button(tr!("menu.language"), |ui| {
                    for (code, name) in i18n::LANGUAGES {
                        if ui.selectable_label(self.lang == code, name).clicked() {
                            self.lang = code;
                            i18n::set(code);
                            let mut config = GuiConfig::load();
                            config.lang = code.to_string();
                            config.save();
                            ui.close();
                        }
                    }
                });

                if let Some(phase_name) = current_sync_phase_name {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().color(palette::INFO_BLUE));
                                ui.label(tr!("status.sync_phase", phase = phase_name));
                            });
                        });
                    });
                }
            });
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let capturing = self.capture_handle.is_some();
                // Painted dot, not "●" - same reason as the battle-list marker (see below): the
                // bundled egui font doesn't reliably include it, which showed up as a tofu box.
                let (dot_rect, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(
                    dot_rect.center(),
                    4.0,
                    if capturing {
                        palette::GOOD
                    } else {
                        palette::NEUTRAL
                    },
                );
                ui.label(if capturing {
                    tr!("status.capture_on", addr = CAPTURE_LISTEN)
                } else {
                    tr!("status.capture_off")
                });
                ui.separator();
                ui.label(tr!("status.battles", n = self.battles.len()));
                ui.separator();
                ui.label(tr!("status.fps", v = format!("{:.1}", self.fps)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dt = ui.ctx().input(|i| i.unstable_dt);
                    ui.label(tr!("status.frame_ms", v = format!("{:.1}", dt * 1000.0)));
                });
            });
        });

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Combats, tr!("tab.battles"));
                ui.selectable_value(&mut self.tab, Tab::Equipage, tr!("tab.crew"));
                ui.selectable_value(&mut self.tab, Tab::Historique, tr!("tab.history"));
                ui.selectable_value(&mut self.tab, Tab::Designs, tr!("tab.designs"));
                ui.selectable_value(&mut self.tab, Tab::PatchNotes, tr!("tab.patch_notes"));
                ui.selectable_value(&mut self.tab, Tab::Journal, tr!("tab.journal"));
            });
        });

        if self.tab == Tab::Combats {
            egui::Panel::left("battle_list")
                .min_size(320.0)
                .show(ui, |ui| {
                    self.render_battle_sidebar(ui);
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Combats => self.render_combats_tab(ui),
                Tab::Equipage => crew::render(ui, &mut self.crew),
                Tab::Historique => self.render_historique_tab(ui),
                Tab::Designs => self.render_designs_tab(ui),
                Tab::PatchNotes => self.render_patch_notes_tab(ui),
                Tab::Journal => self.render_journal_tab(ui),
            });
        });
    }
}

impl PssGuiApp {
    /// Walks a non-technical user through generating a MITM certificate. Only generates the
    /// files and explains how to trust them — never touches the system trust store itself.
    fn render_ca_setup(&mut self, ui: &mut egui::Ui) {
        egui::Window::new(tr!("ca.title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.set_max_width(480.0);
                match self.ca_setup {
                    CaSetupState::NeedsSetup => {
                        ui.label(tr!("ca.intro"));
                        ui.add_space(8.0);
                        if ui.button(tr!("ca.generate")).clicked() {
                            self.generate_ca_interactive();
                        }
                    }
                    CaSetupState::GeneratedInstructions => {
                        ui.colored_label(palette::GOOD, tr!("ca.success"));
                        ui.label(tr!(
                            "ca.location",
                            path = self.ca_cert_path.display().to_string()
                        ));
                        ui.add_space(8.0);
                        ui.label(tr!("ca.last_step"));
                        ui.separator();
                        if cfg!(target_os = "windows") {
                            ui.label(tr!("ca.windows"));
                        } else if cfg!(target_os = "macos") {
                            ui.label(tr!("ca.macos"));
                        } else {
                            ui.label(tr!("ca.linux"));
                        }
                        ui.add_space(8.0);
                        if ui.button(tr!("ca.ack")).clicked() {
                            self.ca_setup = CaSetupState::Ready;
                        }
                    }
                    CaSetupState::Ready => {}
                }
            });
    }

    fn render_battle_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            highlight_filter_query(ui, buf.as_str(), wrap_width)
        };
        let search_response = ui
            .add(egui::TextEdit::singleline(&mut self.search).layouter(&mut layouter))
            .on_hover_text(tr!("filter.hover"));
        render_filter_help_popup(&search_response);

        let wins = self
            .battles
            .iter()
            .filter(|b| b.battle_outcome == OutcomeType::AttackerWon)
            .count();
        let losses = self
            .battles
            .iter()
            .filter(|b| b.battle_outcome == OutcomeType::DefenderWon)
            .count();
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.filter,
                OutcomeFilter::All,
                tr!("battle.all", n = self.battles.len()),
            );
            ui.selectable_value(
                &mut self.filter,
                OutcomeFilter::Wins,
                tr!("battle.wins", n = wins),
            );
            ui.selectable_value(
                &mut self.filter,
                OutcomeFilter::Losses,
                tr!("battle.losses", n = losses),
            );
        });
        ui.separator();

        let query = filter::parse(&self.search);
        let mut clicked: Option<i64> = None;

        egui::ScrollArea::vertical()
            .id_salt("battle_list_scroll")
            .show(ui, |ui| {
                for battle in &self.battles {
                    let matches_filter = match self.filter {
                        OutcomeFilter::All => true,
                        OutcomeFilter::Wins => battle.battle_outcome == OutcomeType::AttackerWon,
                        OutcomeFilter::Losses => battle.battle_outcome == OutcomeType::DefenderWon,
                    };
                    if !matches_filter || !filter::matches(&query, battle, &self.ship_identities) {
                        continue;
                    }

                    // `battle_outcome` alone can't distinguish a clean rout from either side
                    // bailing out early with HP to spare - both surrender cases come back as a
                    // plain win/loss. Red/green are reserved for an actual destruction; either
                    // side's surrender reads as orange, same severity as a draw/retreat/other
                    // non-terminal outcome (the `_` arm below).
                    let color = match battle.battle_outcome {
                        OutcomeType::AttackerWon if battle.defending_ship_surrendered => {
                            palette::WARN
                        }
                        OutcomeType::AttackerWon => palette::GOOD,
                        OutcomeType::DefenderWon if battle.attacking_ship_surrendered => {
                            palette::WARN
                        }
                        OutcomeType::DefenderWon => palette::BAD,
                        _ => palette::WARN,
                    };
                    let duration_secs = battle.last_frame as f64 / self.fps;
                    let label = tr!(
                        "battle.row",
                        id = battle.id,
                        outcome = outcome_label(battle).text(),
                        date = battle.imported_at.as_str(),
                        secs = format!("{duration_secs:.1}"),
                        atk = format_thousands(battle.attacking_ship_id)
                    );
                    let selected = self.selected_battle_id == Some(battle.id);

                    ui.horizontal(|ui| {
                        // A painted rect instead of a Unicode block glyph: the bundled egui font
                        // doesn't reliably include "▍", which showed up as an empty tofu box.
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(4.0, 34.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 0.0, color);
                        if ui.selectable_label(selected, label).clicked() {
                            clicked = Some(battle.id);
                        }
                    });
                }
            });

        if let Some(id) = clicked {
            self.select_battle(id);
        }
    }

    fn render_combats_tab(&mut self, ui: &mut egui::Ui) {
        let Some(detail) = &self.selected_detail else {
            ui.heading(tr!("battle.none_selected"));
            return;
        };
        let battle_id = self.selected_battle_id.unwrap_or_default();

        let mut resolve_identities_clicked = false;
        let mut export_json_clicked = false;
        ui.horizontal(|ui| {
            ui.heading(tr!("battle.title", id = battle_id));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(tr!("battle.export_json")).clicked() {
                    export_json_clicked = true;
                }
                if ui.button(tr!("battle.resolve_identities")).clicked() {
                    resolve_identities_clicked = true;
                }
            });
        });

        ui.label(format!(
            "battleOutcome = {:?} · lastFrame {} → {:.1} s @ {:.0} fps",
            detail.outcome,
            detail.last_frame,
            detail.last_frame as f64 / self.fps,
            self.fps
        ));
        ui.add_space(8.0);

        // Sum of every damage type the report distinguishes (hull/system/character/AP/craft from
        // MissileDesign, shield from MissileDesign.ShieldDamage, fire from FireLength) - NOT
        // hull-only, see the card title below. Each type's own number is broken out per-room in
        // the table further down.
        let total_dealt = detail.attacker.stats.damage.total_damage();
        let hull_taken_pct = if detail.defender.max_hp > 0.0 {
            (detail.defender.max_hp - detail.defender.hull_remaining) / detail.defender.max_hp
                * 100.0
        } else {
            0.0
        };

        ui.columns(4, |cols| {
            stat_card(
                &mut cols[0],
                &tr!("battle.total_dealt"),
                &format!("{total_dealt:.0}"),
                &tr!("battle.hull_real", pct = format!("{hull_taken_pct:.0}")),
                palette::GOOD,
            );
            stat_card(
                &mut cols[1],
                &tr!("battle.hull_taken"),
                &format!(
                    "{:.0}",
                    detail.attacker.max_hp - detail.attacker.hull_remaining
                ),
                &tr!("battle.of_your_hull"),
                palette::WARN,
            );
            stat_card(
                &mut cols[2],
                &tr!("battle.hit_rate"),
                &format!("{:.1} %", detail.attacker.stats.hit_rate.hit_rate() * 100.0),
                &tr!(
                    "battle.shots",
                    hit = detail.attacker.stats.hit_rate.hit_target
                        + detail.attacker.stats.hit_rate.hit_shield,
                    total = detail.attacker.stats.hit_rate.total()
                ),
                palette::NEUTRAL,
            );
            stat_card(
                &mut cols[3],
                &tr!("battle.emp_taken"),
                &format!(
                    "{:.1} s",
                    detail.attacker.stats.total_emp_duration as f64 / self.fps
                ),
                &tr!("battle.emp_sum"),
                palette::NEUTRAL,
            );
        });
        ui.add_space(8.0);

        ui.columns(2, |cols| {
            Self::render_side_summary(&mut cols[0], &detail.attacker);
            Self::render_side_summary(&mut cols[1], &detail.defender);
        });

        if !detail.win_loot.is_empty() || !detail.lose_loot.is_empty() {
            ui.add_space(8.0);
            if !detail.win_loot.is_empty() {
                ui.label(tr!("battle.loot_won"));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for (name, qty) in &detail.win_loot {
                        let mut job = egui::text::LayoutJob::default();
                        job.append(
                            &format!("{name} ×{qty}"),
                            0.0,
                            egui::TextFormat {
                                color: ui.visuals().text_color(),
                                ..Default::default()
                            },
                        );
                        draw_chip(ui, job);
                    }
                });
            }
            if !detail.lose_loot.is_empty() {
                ui.label(tr!("battle.loot_lost"));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for (name, qty) in &detail.lose_loot {
                        let mut job = egui::text::LayoutJob::default();
                        job.append(
                            &format!("{name} ×{qty}"),
                            0.0,
                            egui::TextFormat {
                                color: ui.visuals().text_color(),
                                ..Default::default()
                            },
                        );
                        draw_chip(ui, job);
                    }
                });
            }
        }

        // Full-width, always-visible tables below the summary cards — not nested tiny
        // accordions inside each side's column, so they can't be missed or get an ID collision
        // between the two sides.
        ui.add_space(12.0);
        ui.separator();
        let duration_secs = detail.last_frame as f64 / self.fps;
        ui.heading(tr!("section.rooms", side = detail.attacker.label()));
        render_room_table(
            ui,
            &detail.attacker,
            duration_secs,
            &mut self.room_sort,
            &self.designs,
            "rooms_attacker",
        );
        ui.add_space(8.0);
        ui.heading(tr!("section.rooms", side = detail.defender.label()));
        render_room_table(
            ui,
            &detail.defender,
            duration_secs,
            &mut self.room_sort,
            &self.designs,
            "rooms_defender",
        );

        ui.add_space(12.0);
        ui.separator();
        ui.heading(tr!("section.crew", side = detail.attacker.label()));
        render_crew_table(ui, &detail.attacker, &mut self.crew_sort, "crew_attacker");
        ui.add_space(8.0);
        ui.heading(tr!("section.crew", side = detail.defender.label()));
        render_crew_table(ui, &detail.defender, &mut self.crew_sort, "crew_defender");

        ui.add_space(12.0);
        ui.separator();
        ui.heading(tr!("section.missiles", side = detail.attacker.label()));
        render_drone_table(
            ui,
            &detail.attacker,
            ItemType::Missile,
            duration_secs,
            &mut self.drone_sort,
            "missiles_attacker",
        );
        ui.add_space(8.0);
        ui.heading(tr!("section.missiles", side = detail.defender.label()));
        render_drone_table(
            ui,
            &detail.defender,
            ItemType::Missile,
            duration_secs,
            &mut self.drone_sort,
            "missiles_defender",
        );

        ui.add_space(12.0);
        ui.separator();
        ui.heading(tr!("section.drones", side = detail.attacker.label()));
        render_drone_table(
            ui,
            &detail.attacker,
            ItemType::Craft,
            duration_secs,
            &mut self.drone_sort,
            "drones_attacker",
        );
        ui.add_space(8.0);
        ui.heading(tr!("section.drones", side = detail.defender.label()));
        render_drone_table(
            ui,
            &detail.defender,
            ItemType::Craft,
            duration_secs,
            &mut self.drone_sort,
            "drones_defender",
        );

        if resolve_identities_clicked {
            self.resolve_ship_identities();
        }
        if export_json_clicked {
            self.export_battle_json(battle_id);
        }
    }

    fn render_side_summary(ui: &mut egui::Ui, side: &SideDetail) {
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                // Defender heading gets a red tint in the mockup; attacker stays plain - this is
                // the "you vs. them" framing, not a win/loss judgement (a losing attacker's own
                // panel is still shown plain, its destroyed/surrendered status is the "Issue" row
                // below instead).
                let heading = if side.is_attacker {
                    tr!("side.you", label = side.label())
                } else {
                    tr!("side.opponent", label = side.label())
                };
                if side.is_attacker {
                    ui.strong(heading);
                } else {
                    ui.colored_label(palette::BAD, egui::RichText::new(heading).strong());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(tr!(
                        "side.ship_design",
                        ship = side.ship_id,
                        design = side.ship_design_id
                    ));
                });
            });

            if let Some(identity) = &side.identity {
                ui.label(tr!(
                    "side.identity",
                    name = identity.ship_name.as_str(),
                    level = identity.ship_level,
                    race = identity.original_race_id,
                    power = identity.power_score
                ));
                match (identity.trophy, identity.alliance_name.as_deref()) {
                    (Some(trophy), Some(alliance)) => {
                        ui.label(tr!(
                            "side.trophies_alliance",
                            trophy = trophy,
                            alliance = alliance
                        ));
                    }
                    (Some(trophy), None) => {
                        ui.label(tr!("side.trophies_no_alliance", trophy = trophy));
                    }
                    _ => {
                        ui.weak(tr!("side.unresolved"));
                    }
                }
            }

            if let Some((label, color)) = side.status {
                ui.horizontal(|ui| {
                    ui.label(tr!("side.outcome"));
                    ui.colored_label(color, label.text());
                });
            }

            ui.label(tr!(
                "side.hull",
                cur = format!("{:.0}", side.hull_remaining),
                max = format!("{:.0}", side.max_hp)
            ));
            let hp_color = if side.hull_remaining <= 0.0 {
                palette::BAD
            } else {
                palette::GOOD
            };
            ui.add(
                egui::ProgressBar::new((side.hull_remaining / side.max_hp.max(1.0)) as f32)
                    .fill(hp_color),
            );

            // Not a "remaining/max" gauge: shield_capacity (RoomDesign.Capacity, surviving Shield
            // rooms only) is the generator's instantaneous recharge-buffer size, while
            // shield_damage_taken is a cumulative battle-long total that can exceed it many times
            // over as the shield recharges mid-fight (see SideDetail::shield_capacity doc). The
            // bar shows the generator's capacity as a flat reference stat - full/blue while any
            // shield room survives, empty/red once they're all destroyed - not a depletion level.
            ui.label(tr!("side.shield"));
            let shield_color = if side.shield_capacity <= 0.0 {
                palette::BAD
            } else {
                palette::INFO_BLUE
            };
            ui.add(
                egui::ProgressBar::new(if side.shield_capacity > 0.0 { 1.0 } else { 0.0 })
                    .fill(shield_color)
                    .text(tr!(
                        "side.shield_capacity",
                        v = format!("{:.0}", side.shield_capacity)
                    )),
            );
            ui.label(tr!(
                "side.absorbed",
                v = format!("{:.1}", side.shield_damage_taken)
            ));

            ui.label(tr!(
                "side.repair_ratio",
                v = format!("{:.2}", side.stats.repair_ratio)
            ));
            ui.label(tr!(
                "side.dodge",
                v = format!("{:.1}", side.stats.dodge_rate * 100.0)
            ));

            ui.label(tr!(
                "side.rooms_destroyed",
                a = side
                    .stats
                    .rooms_by_type
                    .values()
                    .map(|c| c.destroyed)
                    .sum::<u32>(),
                b = side
                    .stats
                    .rooms_by_type
                    .values()
                    .map(|c| c.active)
                    .sum::<u32>()
            ));
            ui.label(tr!(
                "side.modules_destroyed",
                n = side.stats.modules.destroyed_count,
                dmg = format!("{:.0}", side.stats.modules.total_damage)
            ));
            ui.label(tr!(
                "side.crafts",
                lost = side.stats.crafts.destroyed_count,
                attacks = side.stats.crafts.total_attacks
            ));
            ui.label(tr!(
                "side.crew_deaths",
                h = side.stats.crew.human_deaths,
                a = side.stats.crew.android_deaths
            ));
            ui.label(tr!("side.teleports", n = side.stats.crew.total_teleports));
            ui.label(tr!(
                "side.abilities",
                n = side.stats.crew.abilities_triggered.values().sum::<u32>()
            ));

            if !side.consumed_ammo.is_empty() {
                ui.add_space(4.0);
                ui.label(tr!("side.consumed"));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let base_color = ui.visuals().text_color();
                    for ammo in &side.consumed_ammo {
                        let mut job = egui::text::LayoutJob::default();
                        job.append(
                            &format!("{} ×{}", ammo.name, ammo.quantity),
                            0.0,
                            egui::TextFormat {
                                color: base_color,
                                ..Default::default()
                            },
                        );
                        draw_chip(ui, job);
                    }
                });
            }
        });
    }

    fn render_historique_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading(tr!("history.title"));
        ui.add_space(8.0);

        const BATTLES_PER_PAGE: usize = 8;
        let total_battles = self.battle_weapon_dps.len();
        let max_page = total_battles.saturating_sub(1) / BATTLES_PER_PAGE;
        if self.historique_page > max_page {
            self.historique_page = max_page;
        }

        ui.horizontal(|ui| {
            let at_oldest = self.historique_page >= max_page;
            if ui
                .add_enabled(!at_oldest, egui::Button::new(tr!("history.older")))
                .clicked()
            {
                self.historique_page += 1;
            }
            ui.label(tr!(
                "history.page",
                cur = self.historique_page + 1,
                total = max_page + 1
            ));
            let at_newest = self.historique_page == 0;
            if ui
                .add_enabled(!at_newest, egui::Button::new(tr!("history.newer")))
                .clicked()
            {
                self.historique_page -= 1;
            }
        });
        ui.add_space(4.0);

        let skip = self.historique_page * BATTLES_PER_PAGE;
        let recent_dps: Vec<&pss_stats::BattleWeaponTypeDps> = self
            .battle_weapon_dps
            .iter()
            .rev()
            .skip(skip)
            .take(BATTLES_PER_PAGE)
            .collect();

        ui.horizontal(|ui| {
            ui.label(tr!("history.dps_by_type", n = BATTLES_PER_PAGE));
            // Legend + bar colors are keyed by weapon identity, not by sort rank, so the same
            // weapon type always gets the same color. Ordered Laser/Missile/Craft first to match
            // docs/Ui's legend, then anything else actually present in this window.
            //
            // Dedup via a HashSet, not `Vec::dedup()`: that only collapses *adjacent* duplicates,
            // but flat_map interleaves types battle by battle (e.g. Android from battle #1 then
            // AntiCraft from #2 then Android again from #4 - never adjacent), so `dedup()` left
            // real duplicate legend entries showing side by side.
            let unique_types: HashSet<RoomType> = recent_dps
                .iter()
                .flat_map(|entry| entry.dps_by_type.keys().copied())
                .collect();
            let mut legend_types: Vec<RoomType> = unique_types.into_iter().collect();
            legend_types.sort_by_key(|rt| (history_legend_rank(rt), room_type_history_label(rt)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for room_type in legend_types.iter().rev() {
                    ui.horizontal(|ui| {
                        ui.colored_label(room_type_history_color(room_type), "■");
                        ui.label(room_type_history_label(room_type));
                    });
                }
            });
        });
        // One group per battle (oldest to newest, left to right), one bar per weapon type that
        // battle used - unlike the previous single all-time-average bar per type, this shows how
        // DPS trends battle to battle, matching docs/Ui's grouped view.
        let groups: Vec<(String, Vec<(f32, egui::Color32)>)> = recent_dps
            .iter()
            .rev()
            .map(|entry| {
                let mut types: Vec<RoomType> = entry.dps_by_type.keys().copied().collect();
                types.sort_by_key(history_legend_rank);
                let series = types
                    .into_iter()
                    .map(|rt| (entry.dps_by_type[&rt] as f32, room_type_history_color(&rt)))
                    .collect();
                (format!("#{}", entry.battle_id), series)
            })
            .collect();
        draw_multi_bar_groups(ui, 220.0, &groups);

        ui.add_space(16.0);
        ui.columns(2, |cols| {
            cols[0].label(tr!("history.winrate"));
            for record in &self.win_rates {
                let total = record.wins + record.losses;
                let pct = if total > 0 {
                    record.wins as f64 / total as f64
                } else {
                    0.0
                };
                cols[0].label(format!("ship #{}", record.ship_id));
                cols[0]
                    .add(egui::ProgressBar::new(pct as f32).text(format!("{:.1} %", pct * 100.0)));
                cols[0].label(tr!(
                    "history.account_row",
                    n = record.appearances,
                    w = record.wins,
                    l = record.losses
                ));
                cols[0].add_space(6.0);
            }

            cols[1].label(tr!("history.damage_chart", n = BATTLES_PER_PAGE));
            let recent: Vec<_> = self
                .battle_damage
                .iter()
                .rev()
                .skip(skip)
                .take(BATTLES_PER_PAGE)
                .collect();
            let grouped: Vec<(String, f32, f32)> = recent
                .iter()
                .map(|e| {
                    (
                        format!("#{}", e.battle_id),
                        e.attacker_damage_dealt as f32,
                        e.attacker_hull_lost as f32,
                    )
                })
                .collect();
            draw_grouped_bar_chart(
                &mut cols[1],
                220.0,
                &grouped,
                egui::Color32::from_rgb(135, 206, 250),
                egui::Color32::from_rgb(197, 48, 48),
            );
        });
    }

    fn render_designs_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(tr!("designs.title"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let never_synced = self.design_counts.room_designs == 0
                    && self.design_counts.item_designs == 0
                    && self.design_counts.character_designs == 0
                    && self.design_counts.missile_designs == 0
                    && self.design_counts.craft_designs == 0;
                let label = if never_synced {
                    tr!("designs.sync_all")
                } else {
                    tr!("designs.resync_all")
                };
                if ui.button(label).clicked() {
                    self.start_designs_sync();
                }
            });
        });
        ui.add_space(8.0);

        ui.group(|ui| {
            ui.label(tr!("designs.cache"));
            let guard = self.sync_state.lock().unwrap();
            let (room_phase, item_phase, character_phase, missile_phase, craft_phase) = (
                guard.room,
                guard.item,
                guard.character,
                guard.missile,
                guard.craft,
            );
            drop(guard);
            render_sync_row(
                ui,
                "RoomDesign",
                self.design_counts.room_designs,
                room_phase,
            );
            render_sync_row(
                ui,
                "ItemDesign",
                self.design_counts.item_designs,
                item_phase,
            );
            render_sync_row(
                ui,
                "CharacterDesign",
                self.design_counts.character_designs,
                character_phase,
            );
            render_sync_row(
                ui,
                "MissileDesign",
                self.design_counts.missile_designs,
                missile_phase,
            );
            render_sync_row(
                ui,
                "CraftDesign",
                self.design_counts.craft_designs,
                craft_phase,
            );
            ui.label(tr!(
                "designs.file",
                path = app_paths().designs_db().display().to_string()
            ));
        });

        if !self.calibrations.is_empty() {
            ui.add_space(8.0);
            ui.group(|ui| {
                egui::Grid::new("calibration_grid")
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong(tr!("designs.weapon"));
                        ui.strong(tr!("designs.samples"));
                        ui.strong(tr!("designs.hull_shot"));
                        ui.strong(tr!("designs.system_shot"));
                        ui.strong(tr!("designs.shield_shot"));
                        ui.strong(tr!("designs.crew_shot"));
                        ui.strong(tr!("designs.status"));
                        ui.end_row();

                        for (record, name) in &self.calibrations {
                            ui.label(format!("{name} (item {})", record.item_design_id));
                            ui.label(format!("{}", record.calibrated_from_shots));
                            ui.label(format!("{:.2}", record.hull_damage));
                            ui.label(format!("{:.2}", record.system_damage));
                            ui.label(format!("{:.2}", record.shield_damage));
                            ui.label(format!("{:.2}", record.character_damage));
                            let (status_text, status_color) = if record.calibrated_from_shots < 5 {
                                (tr!("designs.low_sample"), palette::WARN)
                            } else {
                                (tr!("designs.calibrated"), palette::GOOD)
                            };
                            ui.colored_label(status_color, status_text);
                            ui.end_row();
                        }
                    });
            });
        }

        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(tr!("designs.constants"));
            ui.horizontal(|ui| {
                ui.label("FRAMES_PER_SECOND");
                // Not a free-form value - the PSS client itself only ever runs its simulation at
                // one of these two rates (see the game's own "Target FPS" setting), so a
                // continuous slider offered a false sense of precision.
                ui.selectable_value(&mut self.fps, 30.0, "30");
                ui.selectable_value(&mut self.fps, 60.0, "60");
                ui.label(tr!("designs.source"));
            });

            let mut always_on = true;
            ui.add_enabled(
                false,
                egui::Checkbox::new(&mut always_on, tr!("designs.div100")),
            );

            let mut capturing = self.capture_handle.is_some();
            ui.add_enabled_ui(self.ca_setup == CaSetupState::Ready, |ui| {
                if ui
                    .checkbox(
                        &mut capturing,
                        tr!("designs.capture", addr = CAPTURE_LISTEN),
                    )
                    .changed()
                {
                    self.toggle_capture();
                }
            });
            if self.ca_setup != CaSetupState::Ready {
                ui.small(tr!("designs.capture_hint"));
            }
        });
    }

    fn render_patch_notes_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(tr!("patch.title"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(tr!("patch.check_blog")).clicked() {
                    self.refresh_patch_notes();
                }
                if let Some(fetched_at) = &self.patch_notes_last_fetched_at {
                    ui.weak(tr!("patch.last_fetch", at = fetched_at.as_str()));
                }
            });
        });
        ui.add_space(4.0);

        if self.patch_notes.is_empty() {
            ui.label(tr!("patch.empty"));
            return;
        }

        let mut newly_selected: Option<String> = None;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(220.0, ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.strong(tr!("patch.versions"));
                    egui::ScrollArea::vertical()
                        .id_salt("patch_notes_versions")
                        .show(ui, |ui| {
                            for (i, version) in self.patch_notes.iter().enumerate() {
                                let selected = self.selected_patch_note_version.as_deref()
                                    == Some(version.version.as_str());
                                let label = tr!(
                                    "patch.version_line",
                                    label = version.version.as_str(),
                                    current = if i == 0 {
                                        tr!("patch.current")
                                    } else {
                                        String::new()
                                    },
                                    date = version.published_at.as_str(),
                                    n = version.entries.len()
                                );
                                if ui.selectable_label(selected, label).clicked() {
                                    newly_selected = Some(version.version.clone());
                                }
                            }
                        });
                },
            );

            ui.separator();

            ui.vertical(|ui| {
                let selected_version = self.selected_patch_note_version.clone();
                match self
                    .patch_notes
                    .iter()
                    .find(|v| Some(v.version.as_str()) == selected_version.as_deref())
                {
                    Some(version) => render_patch_note_detail(ui, version, &self.my_current_rooms),
                    None => {
                        ui.label(tr!("patch.select_version"));
                    }
                }
            });
        });

        if let Some(v) = newly_selected {
            self.selected_patch_note_version = Some(v);
        }
    }

    fn render_journal_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(tr!("journal.title"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut self.log_min_level, tracing::Level::DEBUG, "DEBUG");
                ui.selectable_value(&mut self.log_min_level, tracing::Level::WARN, "WARN");
                ui.selectable_value(&mut self.log_min_level, tracing::Level::INFO, "INFO");
            });
        });

        let buffer = self.log_buffer.lock().unwrap();
        if buffer.is_empty() {
            ui.label(tr!("journal.empty"));
        }
        for entry in buffer.iter().rev() {
            if entry.level > self.log_min_level {
                continue;
            }
            let color = match entry.level {
                tracing::Level::ERROR => palette::BAD,
                tracing::Level::WARN => palette::WARN,
                tracing::Level::INFO => palette::GOOD,
                _ => palette::NEUTRAL,
            };
            ui.horizontal(|ui| {
                ui.label(&entry.time);
                ui.colored_label(color, format!("{}", entry.level));
                ui.label(&entry.message);
            });
        }
    }
}

fn patch_note_category_badge(ui: &mut egui::Ui, label: &str, count: usize, color: egui::Color32) {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &format!("{label} +{count}"),
        0.0,
        egui::TextFormat {
            color,
            ..Default::default()
        },
    );
    draw_chip(ui, job);
}

/// Ship-wide count of your currently active rooms whose cached name fuzzy-matches (either string
/// contains the other, case-insensitive) a `BalanceChange`'s free-text `subject` - there's no
/// clean id-based join between blog prose and `room_designs`, so this is a best-effort heuristic,
/// not a guarantee (a short/generic room name could over-match; acceptable for a "did I even have
/// this room" indicator, not used for anything that needs to be exact).
/// `"Bolter Lv10"` -> `"Bolter"` - `room_designs.room_name` always carries this suffix (verified
/// against the real cache: every sampled name ends in `" Lv{N}"`), but patch note prose never
/// mentions a level (a balance change applies to every tier of a room), so the level has to be
/// stripped before any substring comparison can succeed at all.
fn strip_level_suffix(name: &str) -> &str {
    match name.rfind(" Lv") {
        Some(idx)
            if name[idx + 3..].chars().all(|c| c.is_ascii_digit())
                && !name[idx + 3..].is_empty() =>
        {
            &name[..idx]
        }
        _ => name,
    }
}

fn parse_level_suffix(name: &str) -> Option<u32> {
    let idx = name.rfind(" Lv")?;
    let digits = &name[idx + 3..];
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .then(|| digits.parse().ok())?
}

/// "vos salles" cell text, e.g. `"L6 ×2"`, `"L7 ×3, L3 ×1"` when more than one level of a
/// fuzzy-matching room is active, or `None` when nothing matches (rendered as "—").
fn matching_rooms_label(subject: &str, my_current_rooms: &[(String, usize)]) -> Option<String> {
    let subject_lower = subject.to_lowercase();
    let mut by_level: Vec<(Option<u32>, usize)> = Vec::new();
    for (name, count) in my_current_rooms {
        let base_lower = strip_level_suffix(name).to_lowercase();
        if subject_lower.contains(&base_lower) || base_lower.contains(&subject_lower) {
            let level = parse_level_suffix(name);
            match by_level.iter_mut().find(|(l, _)| *l == level) {
                Some(entry) => entry.1 += count,
                None => by_level.push((level, *count)),
            }
        }
    }
    if by_level.is_empty() {
        return None;
    }
    by_level.sort_by(|a, b| b.0.cmp(&a.0));
    Some(
        by_level
            .into_iter()
            .map(|(level, count)| match level {
                Some(l) => format!("L{l} ×{count}"),
                None => format!("×{count}"),
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn format_balance_value(value: f64, unit: Option<&str>) -> String {
    match unit {
        Some("%") => format!("{value:.1} %"),
        Some(u) => format!("{value:.2} {u}"),
        None => format!("{value:.2}"),
    }
}

fn render_balance_table(
    ui: &mut egui::Ui,
    entries: &[&pss_designs::patch_notes::ChangelogEntry],
    my_current_rooms: &[(String, usize)],
) {
    let structured: Vec<&&pss_designs::patch_notes::ChangelogEntry> =
        entries.iter().filter(|e| e.balance.is_some()).collect();
    let plain: Vec<&&pss_designs::patch_notes::ChangelogEntry> =
        entries.iter().filter(|e| e.balance.is_none()).collect();

    if !structured.is_empty() {
        egui::Grid::new("patch_notes_balance_grid")
            .striped(true)
            .num_columns(5)
            .show(ui, |ui| {
                ui.strong(tr!("patch.th_subject"));
                ui.strong(tr!("patch.th_before"));
                ui.strong(tr!("patch.th_after"));
                ui.strong("Δ");
                ui.strong(tr!("patch.th_rooms"));
                ui.end_row();

                for entry in &structured {
                    let Some(b) = &entry.balance else { continue };
                    ui.label(&b.subject);
                    match b.before {
                        Some(before) => ui.label(format_balance_value(before, b.unit.as_deref())),
                        None => ui.weak("?"),
                    };
                    ui.label(format_balance_value(b.after, b.unit.as_deref()));
                    if b.is_new() {
                        ui.colored_label(palette::WARN, tr!("patch.new"));
                    } else {
                        match b.delta_percent() {
                            Some(delta) => {
                                let color = if delta > 0.0 {
                                    palette::GOOD
                                } else {
                                    palette::BAD
                                };
                                ui.colored_label(color, format!("{delta:+.1} %"));
                            }
                            None => {
                                ui.weak("—");
                            }
                        }
                    }
                    match matching_rooms_label(&b.subject, my_current_rooms) {
                        Some(label) => {
                            ui.label(label);
                        }
                        None => {
                            ui.weak("—");
                        }
                    }
                    ui.end_row();
                }
            });
    }

    if !plain.is_empty() {
        if !structured.is_empty() {
            ui.add_space(4.0);
        }
        for entry in plain {
            ui.label(format!("• {}", entry.text));
        }
    }
}

fn render_patch_note_detail(
    ui: &mut egui::Ui,
    version: &pss_designs::patch_notes::PatchNoteVersion,
    my_current_rooms: &[(String, usize)],
) {
    use pss_designs::patch_notes::{ChangelogCategory, UgcKind};

    ui.heading(&version.post_title);
    ui.weak(format!("{} · {}", version.published_at, version.author));
    ui.add_space(4.0);

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        patch_note_category_badge(
            ui,
            &tr!("patch.balance"),
            version.count(ChangelogCategory::Balance),
            palette::INFO_BLUE,
        );
        patch_note_category_badge(
            ui,
            &tr!("patch.comfort"),
            version.count(ChangelogCategory::Other),
            palette::NEUTRAL,
        );
        patch_note_category_badge(
            ui,
            &tr!("patch.fixes"),
            version.count(ChangelogCategory::Fix),
            palette::GOOD,
        );
        patch_note_category_badge(ui, "UGC", version.ugc_designs.len(), palette::WARN);
    });
    ui.add_space(8.0);

    if !version.ugc_designs.is_empty() {
        ui.separator();
        ui.colored_label(
            palette::WARN,
            egui::RichText::new(tr!(
                "patch.ugc_title",
                skins = version
                    .ugc_designs
                    .iter()
                    .filter(|d| d.kind == UgcKind::Skin)
                    .count(),
                stickers = version
                    .ugc_designs
                    .iter()
                    .filter(|d| d.kind == UgcKind::Sticker)
                    .count()
            ))
            .strong(),
        );
        ui.add_space(4.0);
        let enlarge_id = egui::Id::new("patch_notes_enlarged_ugc");
        egui::ScrollArea::horizontal()
            .id_salt("patch_notes_ugc_scroll")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for design in &version.ugc_designs {
                        ui.vertical(|ui| {
                            ui.set_max_width(90.0);
                            if let Some(url) = &design.image_url {
                                let response = ui.add(
                                    egui::Image::new(url.as_str())
                                        .max_width(64.0)
                                        .max_height(64.0)
                                        .maintain_aspect_ratio(true)
                                        .sense(egui::Sense::click()),
                                );
                                if response.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if response.clicked() {
                                    ui.memory_mut(|mem| {
                                        mem.data.insert_temp(enlarge_id, url.clone())
                                    });
                                }
                            }
                            ui.label(egui::RichText::new(&design.name).small());
                            ui.weak(egui::RichText::new(&design.creator).small());
                        });
                        ui.add_space(8.0);
                    }
                });
            });
        ui.add_space(8.0);

        let enlarged_url: Option<String> = ui.memory(|mem| mem.data.get_temp(enlarge_id));
        if let Some(url) = enlarged_url {
            let modal = egui::Modal::new(enlarge_id.with("popup")).show(ui.ctx(), |ui| {
                ui.set_max_width(512.0);
                ui.add(
                    egui::Image::new(url.as_str())
                        .max_width(512.0)
                        .max_height(512.0)
                        .maintain_aspect_ratio(true),
                );
                ui.add_space(8.0);
                if ui.button(tr!("patch.close")).clicked() {
                    ui.memory_mut(|mem| mem.data.remove::<String>(enlarge_id));
                }
            });
            if modal.should_close() {
                ui.memory_mut(|mem| mem.data.remove::<String>(enlarge_id));
            }
        }
    }

    let balance_entries: Vec<_> = version
        .entries
        .iter()
        .filter(|e| e.category == ChangelogCategory::Balance)
        .collect();
    if !balance_entries.is_empty() {
        ui.separator();
        ui.colored_label(
            palette::INFO_BLUE,
            egui::RichText::new(tr!("patch.balance_n", n = balance_entries.len())).strong(),
        );
        ui.add_space(4.0);
        render_balance_table(ui, &balance_entries, my_current_rooms);
        ui.add_space(8.0);
    }

    let category_section =
        |ui: &mut egui::Ui, title: &str, category: ChangelogCategory, color: egui::Color32| {
            let entries: Vec<&str> = version
                .entries
                .iter()
                .filter(|e| e.category == category)
                .map(|e| e.text.as_str())
                .collect();
            if entries.is_empty() {
                return;
            }
            ui.separator();
            ui.colored_label(
                color,
                egui::RichText::new(format!("{title} ({})", entries.len())).strong(),
            );
            ui.add_space(4.0);
            for text in entries {
                ui.label(format!("• {text}"));
            }
            ui.add_space(8.0);
        };

    category_section(
        ui,
        &tr!("patch.comfort"),
        ChangelogCategory::Other,
        palette::NEUTRAL,
    );
    category_section(
        ui,
        &tr!("patch.fixes"),
        ChangelogCategory::Fix,
        palette::GOOD,
    );

    ui.separator();
    ui.weak(tr!("patch.source", src = version.post_url.as_str()));
}

/// Fixed display order for the Historique weapon-type legend: Laser/Missile/Craft first (matching
/// docs/Ui), anything else (Android, AntiCraft...) after, in whatever order they're encountered.
fn history_legend_rank(room_type: &RoomType) -> u8 {
    match room_type {
        RoomType::Laser => 0,
        RoomType::Missile => 1,
        RoomType::Carrier => 2,
        RoomType::AntiCraft => 3,
        RoomType::Android => 4,
        _ => 5,
    }
}

/// The battleReport's `Carrier` room type is what launches Crafts - the mockup and the rest of
/// the GUI call it "Craft", so the history legend/bars follow that instead of the raw enum name.
fn room_type_history_label(room_type: &RoomType) -> String {
    match room_type {
        RoomType::Carrier => "Craft".to_string(),
        other => format!("{other:?}"),
    }
}

/// Every weapon-ish room type that can actually appear here gets its own distinct hue (not a
/// shared fallback grey) - `estimate_room_damage` only ever produces stats for Laser, Missile,
/// Carrier, AntiCraft or Android (the room types with a shot/activation counter, see
/// `RoomReport::shots_for` in pss-model), so five colors covers every real case; only a genuinely
/// unrecognized type falls back to neutral grey.
fn room_type_history_color(room_type: &RoomType) -> egui::Color32 {
    match room_type {
        RoomType::Laser => egui::Color32::from_rgb(135, 206, 250),
        RoomType::Missile => egui::Color32::from_rgb(218, 165, 32),
        RoomType::Carrier => palette::GOOD,
        RoomType::AntiCraft => egui::Color32::from_rgb(159, 122, 234),
        RoomType::Android => egui::Color32::from_rgb(237, 100, 166),
        _ => palette::NEUTRAL,
    }
}

/// A clickable column header for a sortable table: bold text, plus a ▲/▼ arrow when this column
/// is the active sort key. Clicking an inactive column switches to it (descending by default -
/// that's the more useful first view for damage/HP-like columns); clicking the active column
/// flips direction.
/// Paints one "chip": a rounded, bordered box sized to fit `job`'s text - a single
/// `allocate_exact_size` + direct painter calls, no `Frame`/nested child `Ui`. That distinction
/// matters specifically inside `horizontal_wrapped` (used for the Consommés and Butin rows):
/// `horizontal_wrapped` decides row breaks from each child's precomputed desired size, and a
/// `Frame::group` with multiple widgets inside runs its own nested layout pass to size itself,
/// which doesn't feed back to the outer wrap cursor reliably - that produced two different
/// broken-layout bugs here (text wrapping one character per line, then chips overlapping and
/// disconnecting) as soon as a row had enough chips to need wrapping. A single flat widget with a
/// size that's known before layout doesn't have that failure mode.
fn draw_chip(ui: &mut egui::Ui, job: egui::text::LayoutJob) {
    let galley = ui.painter().layout_job(job);
    let padding = egui::vec2(8.0, 4.0);
    let desired_size = galley.size() + padding * 2.0;
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        ui.painter()
            .rect_stroke(rect, 4.0, stroke, egui::StrokeKind::Outside);
        ui.painter()
            .galley(rect.min + padding, galley, ui.visuals().text_color());
    }
}

fn sortable_header<K: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    key: K,
    sort: &mut (K, SortDirection),
) {
    let is_active = sort.0 == key;
    let text = if is_active {
        format!("{label} {}", sort.1.arrow())
    } else {
        label.to_string()
    };
    let response =
        ui.add(egui::Label::new(egui::RichText::new(text).strong()).sense(egui::Sense::click()));
    if response.clicked() {
        if is_active {
            sort.1 = sort.1.toggled();
        } else {
            *sort = (key, SortDirection::Descending);
        }
    }
}

/// One design type's cache row: a status bar (green/full = up to date, blue/full = fetching right
/// now, grey/empty = never synced this cache) plus its real cached count. Not a fractional
/// progress bar - see `SyncPhase`'s doc comment on why there's no live item-by-item count to show.
fn render_sync_row(ui: &mut egui::Ui, label: &str, count: i64, phase: SyncPhase) {
    ui.horizontal(|ui| {
        ui.label(label);
        let (fraction, color, status_text) = match phase {
            SyncPhase::InProgress => (1.0, palette::INFO_BLUE, tr!("designs.syncing")),
            SyncPhase::Failed => (1.0, palette::BAD, tr!("designs.failed")),
            _ if count > 0 => (1.0, palette::GOOD, tr!("designs.synced")),
            _ => (0.0, palette::NEUTRAL, tr!("designs.never")),
        };
        ui.add(
            egui::ProgressBar::new(fraction)
                .fill(color)
                .desired_width(220.0),
        );
        ui.label(format!("{count} · {status_text}"));
    });
}

fn stat_card(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    subtitle: &str,
    subtitle_color: egui::Color32,
) {
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(title);
        ui.heading(value);
        ui.label(egui::RichText::new(subtitle).small().color(subtitle_color));
    });
}

/// Draws a grouped bar chart with hand-painted rectangles, any number of series per group (e.g.
/// one bar per weapon type used that battle - the set of series can differ group to group).
/// `egui_plot`'s axes are built for continuous data; for a handful of named categories, painting
/// directly gives reliable label/bar alignment without fighting its tick formatter.
/// Horizontal gridlines with value labels along the left edge - without this, a hand-painted bar
/// chart gives no way to read a bar's actual value from its height, only compare relative sizes.
fn draw_y_axis_gridlines(
    painter: &egui::Painter,
    plot_rect: egui::Rect,
    max_value: f32,
    text_color: egui::Color32,
) {
    const STEPS: i32 = 4;
    for i in 0..=STEPS {
        let frac = i as f32 / STEPS as f32;
        let y = plot_rect.bottom() - frac * plot_rect.height();
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), y),
                egui::pos2(plot_rect.right(), y),
            ],
            egui::Stroke::new(1.0, text_color.gamma_multiply(0.15)),
        );
        painter.text(
            egui::pos2(plot_rect.left() + 2.0, y - 1.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{:.0}", max_value * frac),
            egui::FontId::proportional(9.0),
            text_color.gamma_multiply(0.55),
        );
    }
}

fn draw_multi_bar_groups(
    ui: &mut egui::Ui,
    height: f32,
    groups: &[(String, Vec<(f32, egui::Color32)>)],
) {
    if groups.is_empty() {
        ui.label(tr!("table.no_data"));
        return;
    }
    let max_value = groups
        .iter()
        .flat_map(|(_, series)| series.iter().map(|(v, _)| *v))
        .fold(0.0_f32, f32::max)
        .max(1.0);
    let desired_size = egui::vec2(ui.available_width(), height);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let text_color = ui.visuals().text_color();

    // Top margin leaves room for each bar's value label without clipping the tallest bar;
    // bottom margin is the x-axis (group) labels.
    let plot_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + 14.0),
        egui::pos2(rect.right(), rect.bottom() - 18.0),
    );
    draw_y_axis_gridlines(&painter, plot_rect, max_value, text_color);

    let n = groups.len() as f32;
    let gap = 14.0;
    let group_width = ((rect.width() - gap * (n + 1.0)) / n).max(8.0);

    for (i, (label, series)) in groups.iter().enumerate() {
        let x0 = rect.left() + gap + i as f32 * (group_width + gap);
        let series_count = series.len().max(1) as f32;
        let bar_gap = 2.0;
        let bar_width = ((group_width - bar_gap * (series_count - 1.0)) / series_count).max(2.0);

        for (j, (value, color)) in series.iter().enumerate() {
            let bar_x0 = x0 + j as f32 * (bar_width + bar_gap);
            let bar_height = (value / max_value) * plot_rect.height();
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(bar_x0, plot_rect.bottom() - bar_height.max(0.0)),
                    egui::pos2(bar_x0 + bar_width, plot_rect.bottom()),
                ),
                2.0,
                *color,
            );
            painter.text(
                egui::pos2(
                    bar_x0 + bar_width / 2.0,
                    plot_rect.bottom() - bar_height.max(0.0) - 1.0,
                ),
                egui::Align2::CENTER_BOTTOM,
                format!("{value:.0}"),
                egui::FontId::proportional(8.0),
                text_color,
            );
        }

        painter.text(
            egui::pos2(x0 + group_width / 2.0, plot_rect.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(11.0),
            text_color,
        );
    }
}

/// Same idea as `draw_multi_bar_groups`, but fixed at exactly two bars per category (e.g. dealt
/// vs. taken) - kept separate since its two-color call sites are simpler without a `Vec` per group.
fn draw_grouped_bar_chart(
    ui: &mut egui::Ui,
    height: f32,
    groups: &[(String, f32, f32)],
    color_a: egui::Color32,
    color_b: egui::Color32,
) {
    if groups.is_empty() {
        ui.label(tr!("table.no_data"));
        return;
    }
    let max_value = groups
        .iter()
        .flat_map(|(_, a, b)| [*a, *b])
        .fold(0.0_f32, f32::max)
        .max(1.0);
    let desired_size = egui::vec2(ui.available_width(), height);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let text_color = ui.visuals().text_color();

    let plot_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + 14.0),
        egui::pos2(rect.right(), rect.bottom() - 18.0),
    );
    draw_y_axis_gridlines(&painter, plot_rect, max_value, text_color);

    let n = groups.len() as f32;
    let gap = 14.0;
    let group_width = ((rect.width() - gap * (n + 1.0)) / n).max(8.0);
    let bar_width = (group_width - 4.0) / 2.0;

    for (i, (label, a, b)) in groups.iter().enumerate() {
        let x0 = rect.left() + gap + i as f32 * (group_width + gap);

        let ah = (a / max_value) * plot_rect.height();
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, plot_rect.bottom() - ah.max(0.0)),
                egui::pos2(x0 + bar_width, plot_rect.bottom()),
            ),
            2.0,
            color_a,
        );
        painter.text(
            egui::pos2(x0 + bar_width / 2.0, plot_rect.bottom() - ah.max(0.0) - 1.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{a:.0}"),
            egui::FontId::proportional(8.0),
            text_color,
        );

        let bh = (b / max_value) * plot_rect.height();
        let x1 = x0 + bar_width + 4.0;
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x1, plot_rect.bottom() - bh.max(0.0)),
                egui::pos2(x1 + bar_width, plot_rect.bottom()),
            ),
            2.0,
            color_b,
        );
        painter.text(
            egui::pos2(x1 + bar_width / 2.0, plot_rect.bottom() - bh.max(0.0) - 1.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{b:.0}"),
            egui::FontId::proportional(8.0),
            text_color,
        );

        painter.text(
            egui::pos2(x0 + group_width / 2.0, plot_rect.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(11.0),
            text_color,
        );
    }
}

/// Weapon rooms (from `side.stats.damage.rooms`) plus any room that repaired HP (Repair Bay and
/// similar), which don't fire and so aren't in the weapon list at all. `duration_secs` is the
/// whole battle's length - the report has no per-room "how long was this room actively used"
/// signal, so per-room DPS necessarily divides by the same battle-wide duration used everywhere
/// else in this tab, not a room-specific active-fire window.
struct RoomRow {
    label: String,
    shots: Option<i32>,
    hull: Option<f64>,
    system: Option<f64>,
    shield: Option<f64>,
    character: Option<f64>,
    ap: Option<f64>,
    dps: Option<f64>,
    repair_share: Option<f64>,
}

fn render_room_table(
    ui: &mut egui::Ui,
    side: &SideDetail,
    duration_secs: f64,
    sort: &mut (RoomSortKey, SortDirection),
    cache: &DesignsCache,
    id: &str,
) {
    let repair_shares = pss_stats::room_repair_shares(&side.ship);
    if side.stats.damage.rooms.is_empty() && repair_shares.is_empty() {
        ui.label(tr!("table.no_rooms"));
        return;
    }

    let mut rows: Vec<RoomRow> = side
        .stats
        .damage
        .rooms
        .iter()
        .map(|room| {
            let total = room.estimated_hull_damage
                + room.estimated_system_damage
                + room.estimated_character_damage
                + room.estimated_shield_damage
                + room.estimated_ap_damage
                + room.estimated_craft_damage
                + room.estimated_fire_ap_damage
                + room.estimated_fire_character_damage;
            let dps = if duration_secs > 0.0 {
                total / duration_secs
            } else {
                0.0
            };
            RoomRow {
                label: format!("{} (#{})", room.room_name, room.room_id),
                shots: Some(room.shots_fired),
                hull: Some(room.estimated_hull_damage),
                system: Some(room.estimated_system_damage),
                shield: Some(room.estimated_shield_damage),
                character: Some(room.estimated_character_damage),
                ap: Some(room.estimated_ap_damage),
                dps: Some(dps),
                repair_share: repair_shares.get(&room.room_id).copied(),
            }
        })
        .collect();

    let weapon_room_ids: HashSet<i32> = side.stats.damage.rooms.iter().map(|r| r.room_id).collect();
    for (&room_id, &share) in &repair_shares {
        if weapon_room_ids.contains(&room_id) {
            continue; // already listed above with its weapon stats
        }
        let report_room = side.ship.room_reports.get(&room_id);
        let label = report_room
            .and_then(|r| cache.get_room_design(r.room_design_id).ok().flatten())
            .map(|d| format!("{} (#{room_id})", d.room_name))
            .unwrap_or_else(|| {
                let room_type = report_room
                    .map(|r| r.room_type)
                    .unwrap_or(RoomType::Unknown(-1));
                tr!(
                    "table.room_n",
                    kind = format!("{room_type:?}"),
                    id = room_id
                )
            });
        rows.push(RoomRow {
            label,
            shots: None,
            hull: None,
            system: None,
            shield: None,
            character: None,
            ap: None,
            dps: None,
            repair_share: Some(share),
        });
    }

    rows.sort_by(|a, b| {
        let ordering = match sort.0 {
            RoomSortKey::Room => a.label.cmp(&b.label),
            RoomSortKey::Shots => a.shots.unwrap_or(-1).cmp(&b.shots.unwrap_or(-1)),
            RoomSortKey::Hull => a.hull.unwrap_or(-1.0).total_cmp(&b.hull.unwrap_or(-1.0)),
            RoomSortKey::System => a
                .system
                .unwrap_or(-1.0)
                .total_cmp(&b.system.unwrap_or(-1.0)),
            RoomSortKey::Shield => a
                .shield
                .unwrap_or(-1.0)
                .total_cmp(&b.shield.unwrap_or(-1.0)),
            RoomSortKey::Character => a
                .character
                .unwrap_or(-1.0)
                .total_cmp(&b.character.unwrap_or(-1.0)),
            RoomSortKey::Ap => a.ap.unwrap_or(-1.0).total_cmp(&b.ap.unwrap_or(-1.0)),
            RoomSortKey::Dps => a.dps.unwrap_or(-1.0).total_cmp(&b.dps.unwrap_or(-1.0)),
            RoomSortKey::Repair => a
                .repair_share
                .unwrap_or(-1.0)
                .total_cmp(&b.repair_share.unwrap_or(-1.0)),
        };
        match sort.1 {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });

    egui::Grid::new(id).striped(true).show(ui, |ui| {
        sortable_header(ui, &tr!("table.room"), RoomSortKey::Room, sort);
        sortable_header(ui, &tr!("table.shots"), RoomSortKey::Shots, sort);
        sortable_header(ui, &tr!("table.hull"), RoomSortKey::Hull, sort);
        sortable_header(ui, &tr!("table.system"), RoomSortKey::System, sort);
        sortable_header(ui, &tr!("table.shield"), RoomSortKey::Shield, sort);
        sortable_header(ui, &tr!("table.crew"), RoomSortKey::Character, sort);
        sortable_header(ui, &tr!("table.ap"), RoomSortKey::Ap, sort);
        sortable_header(ui, &tr!("table.dps"), RoomSortKey::Dps, sort);
        sortable_header(ui, &tr!("table.repair"), RoomSortKey::Repair, sort);
        ui.end_row();

        for row in &rows {
            ui.label(&row.label);
            match row.shots {
                Some(shots) => ui.label(format!("{shots}")),
                None => ui.label("–"),
            };
            match row.hull {
                Some(v) => ui.label(format!("{v:.0}")),
                None => ui.label("–"),
            };
            match row.system {
                Some(v) => ui.label(format!("{v:.0}")),
                None => ui.label("–"),
            };
            match row.shield {
                Some(v) => ui.label(format!("{v:.0}")),
                None => ui.label("–"),
            };
            match row.character {
                Some(v) => ui.label(format!("{v:.0}")),
                None => ui.label("–"),
            };
            match row.ap {
                Some(v) => ui.label(format!("{v:.0}")),
                None => ui.label("–"),
            };
            match row.dps {
                Some(dps) => ui.label(format!("{dps:.1}")),
                None => ui.label("–"),
            };
            match row.repair_share {
                Some(share) => ui.label(format!("{share:.2}")),
                None => ui.label("–"),
            };
            ui.end_row();
        }
    });
}

fn render_crew_table(
    ui: &mut egui::Ui,
    side: &SideDetail,
    sort: &mut (CrewSortKey, SortDirection),
    id: &str,
) {
    if side.stats.named_crew.is_empty() {
        ui.label(tr!("table.no_crew"));
        return;
    }

    let mut sorted_crew: Vec<&NamedCrewStat> = side.stats.named_crew.iter().collect();
    sorted_crew.sort_by(|a, b| {
        let ordering = match sort.0 {
            CrewSortKey::Name => a.name.cmp(&b.name),
            CrewSortKey::State => a.is_dead.cmp(&b.is_dead),
            CrewSortKey::DamageTaken => a.damage_taken.total_cmp(&b.damage_taken),
            CrewSortKey::Healed => a.healed.total_cmp(&b.healed),
            CrewSortKey::Teleports => a.teleports.cmp(&b.teleports),
            CrewSortKey::Ability => special_ability_display_name(a.special_ability_type)
                .cmp(&special_ability_display_name(b.special_ability_type)),
        };
        match sort.1 {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });

    egui::Grid::new(id).striped(true).show(ui, |ui| {
        sortable_header(ui, &tr!("table.member"), CrewSortKey::Name, sort);
        sortable_header(ui, &tr!("table.state"), CrewSortKey::State, sort);
        sortable_header(ui, &tr!("table.dmg_taken"), CrewSortKey::DamageTaken, sort);
        sortable_header(ui, &tr!("table.healed"), CrewSortKey::Healed, sort);
        sortable_header(ui, &tr!("table.tp"), CrewSortKey::Teleports, sort);
        sortable_header(ui, &tr!("table.ability"), CrewSortKey::Ability, sort);
        ui.end_row();

        for crew in sorted_crew {
            ui.label(format!(
                "{} {}",
                crew.name,
                if crew.is_android {
                    tr!("table.android")
                } else {
                    String::new()
                }
            ));
            ui.label(if crew.is_dead {
                tr!("table.dead")
            } else {
                tr!("table.alive")
            });
            match crew.damage_taken_pct {
                Some(pct) => {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(format!("{:.1} HP", crew.damage_taken));
                        // Severity gradient on the % of max HP taken - matches the mockup, where
                        // an overkill hit (>=100%) is red, a heavy but survivable hit is orange,
                        // and anything lighter stays neutral grey. Thresholds are a reasonable
                        // read of the mockup's sample values (106% red, 79.6% orange, 38.7% grey),
                        // not a value pulled from game data - tune if that reading turns out wrong.
                        let pct_color = if pct >= 100.0 {
                            palette::BAD
                        } else if pct >= 50.0 {
                            palette::WARN
                        } else {
                            palette::NEUTRAL
                        };
                        ui.colored_label(pct_color, format!("{pct:.0} %"));
                    });
                }
                None if crew.damage_taken > 0.0 => {
                    ui.label(format!("{:.1} HP", crew.damage_taken));
                }
                None => {
                    ui.label("—");
                }
            };
            if crew.healed > 0.0 {
                ui.label(format!("{:.1} HP", crew.healed));
            } else {
                ui.label("—");
            }
            ui.label(format!("{}", crew.teleports));
            if crew.special_ability_type == SpecialAbilityType::None {
                ui.label("—");
            } else {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(special_ability_display_name(crew.special_ability_type));
                    // No "✓"/"✗" - same missing-glyph issue as the status dot and sort arrows.
                    // The word alone plus color already carries the distinction.
                    if crew.ability_triggered {
                        ui.colored_label(palette::GOOD, tr!("table.used"));
                    } else {
                        ui.colored_label(palette::NEUTRAL, tr!("table.unused"));
                    }
                });
            }
            ui.end_row();
        }
    });
}

/// Sum of every damage component of a `WeaponDamage` (same eight components as
/// `ShipDamageEstimate::total_damage`).
fn total_of(d: &pss_stats::WeaponDamage) -> f64 {
    d.hull + d.system + d.character + d.shield + d.ap + d.craft + d.fire_ap + d.fire_character
}

fn render_drone_table(
    ui: &mut egui::Ui,
    side: &SideDetail,
    item_type: ItemType,
    duration_secs: f64,
    sort: &mut (DroneSortKey, SortDirection),
    id: &str,
) {
    let mut sorted: Vec<&pss_stats::ItemWeaponStats> = side
        .drones
        .iter()
        .filter(|item| item.item_type == item_type)
        .collect();
    if sorted.is_empty() {
        let label = match item_type {
            ItemType::Missile => tr!("table.no_missile"),
            ItemType::Craft => tr!("table.no_drone"),
            _ => tr!("table.none"),
        };
        ui.label(label);
        return;
    }

    sorted.sort_by(|a, b| {
        let fire = |item: &pss_stats::ItemWeaponStats| {
            let t = item.total();
            t.fire_ap + t.fire_character
        };
        let dps = |item: &pss_stats::ItemWeaponStats| -> f64 {
            if duration_secs > 0.0 {
                total_of(&item.total()) / duration_secs
            } else {
                0.0
            }
        };
        let ordering = match sort.0 {
            DroneSortKey::Name => a.item_name.cmp(&b.item_name),
            DroneSortKey::Quantity => a.quantity_deployed.cmp(&b.quantity_deployed),
            DroneSortKey::Hull => a.total().hull.total_cmp(&b.total().hull),
            DroneSortKey::System => a.total().system.total_cmp(&b.total().system),
            DroneSortKey::Shield => a.total().shield.total_cmp(&b.total().shield),
            DroneSortKey::Character => a.total().character.total_cmp(&b.total().character),
            DroneSortKey::Ap => a.total().ap.total_cmp(&b.total().ap),
            DroneSortKey::Craft => a.total().craft.total_cmp(&b.total().craft),
            DroneSortKey::Fire => fire(a).total_cmp(&fire(b)),
            DroneSortKey::Dps => dps(a).total_cmp(&dps(b)),
        };
        match sort.1 {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });

    egui::Grid::new(id).striped(true).show(ui, |ui| {
        sortable_header(ui, &tr!("table.name"), DroneSortKey::Name, sort);
        sortable_header(ui, &tr!("table.qty"), DroneSortKey::Quantity, sort);
        sortable_header(ui, &tr!("table.hull"), DroneSortKey::Hull, sort);
        sortable_header(ui, &tr!("table.system"), DroneSortKey::System, sort);
        sortable_header(ui, &tr!("table.shield"), DroneSortKey::Shield, sort);
        sortable_header(ui, &tr!("table.crew"), DroneSortKey::Character, sort);
        sortable_header(ui, &tr!("table.ap"), DroneSortKey::Ap, sort);
        sortable_header(ui, &tr!("table.craft"), DroneSortKey::Craft, sort);
        sortable_header(ui, &tr!("table.fire"), DroneSortKey::Fire, sort);
        ui.label(tr!("table.aoe"));
        sortable_header(ui, &tr!("table.dps"), DroneSortKey::Dps, sort);
        ui.end_row();

        for item in sorted {
            let t = item.total();
            let fire = t.fire_ap + t.fire_character;
            ui.label(&item.item_name);
            ui.label(format!("{}", item.quantity_deployed))
                .on_hover_text(tr!(
                    "table.projectiles_hover",
                    n = format!("{:.0}", item.projectiles),
                    volley = format!("{:.0}", item.volley)
                ));
            ui.label(format!("{:.1}", t.hull));
            ui.label(format!("{:.1}", t.system));
            ui.label(format!("{:.1}", t.shield));
            ui.label(format!("{:.1}", t.character));
            ui.label(format!("{:.1}", t.ap));
            ui.label(format!("{:.1}", t.craft));
            if fire > 0.0 {
                ui.colored_label(
                    palette::WARN,
                    tr!(
                        "table.drone_detail",
                        total = format!("{:.1}", fire),
                        ap = format!("{:.1}", t.fire_ap),
                        crew = format!("{:.1}", t.fire_character)
                    ),
                )
                .on_hover_text(tr!("table.fire_hover"));
            } else {
                ui.weak("—");
            }
            if item.explosion_type == "Single" {
                ui.weak("—");
            } else {
                ui.colored_label(
                    palette::WARN,
                    format!("{} (r={:.0})", item.explosion_type, item.explosion_radius),
                );
            }
            let dps = if duration_secs > 0.0 {
                total_of(&t) / duration_secs
            } else {
                0.0
            };
            ui.label(format!("{dps:.2}"));
            ui.end_row();
        }
    });
}

/// Real in-game ability names, keyed by the wire enum's variant - `SpecialAbilityType`'s Rust
/// names are the game's internal identifiers (e.g. `AddReload`), not what players see (`Rush`).
/// Sourced from PixyShip's `ABILITY_NAME_MAP` (github.com/solevis/pixyship,
/// backend/app/constants.py), the same community reference already used for `RoomDesign.Capacity`
/// (see CLAUDE.md). That map only covers the abilities it needed to rename; variants not listed
/// there either already read as a real name as-is (Teleport, Freeze, Clone, Polymorph, Mimic,
/// Summon, FireWalk, Bloodlust) or aren't confirmed (PoisonRoom, RemoveDebuffsFromSameRoomCharacters,
/// SelfDestruct, XmasExplosion) - those fall back to the technical enum name rather than guessing.
fn special_ability_display_name(ability: SpecialAbilityType) -> String {
    match ability {
        SpecialAbilityType::None => String::new(),
        SpecialAbilityType::HealSelfHp => "First Aid".to_string(),
        SpecialAbilityType::HealSameRoomCharacters => "Healing Rain".to_string(),
        SpecialAbilityType::AddReload => "Rush".to_string(),
        SpecialAbilityType::DeductReload => "System Hack".to_string(),
        SpecialAbilityType::HealRoomHp => "Urgent Repair".to_string(),
        SpecialAbilityType::DamageToRoom => "Ultra Dismantle".to_string(),
        SpecialAbilityType::DamageToSameRoomCharacters => "Gas".to_string(),
        SpecialAbilityType::DamageToCurrentEnemy => "Critical Strike".to_string(),
        SpecialAbilityType::SetFire => "Arson".to_string(),
        SpecialAbilityType::Invulnerability => "Phase Shift".to_string(),
        SpecialAbilityType::ProtectRoom => "Stasis Shield".to_string(),
        SpecialAbilityType::PoisonCrew => "Poison Blast".to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_saved_before_the_language_existed_still_loads() {
        let old = r#"{"ca_cert_path":"/a.pem","ca_key_path":"/a-key.pem"}"#;
        let config: GuiConfig = serde_json::from_str(old).expect("old config should load");
        assert_eq!(config.lang, i18n::DEFAULT_LANG);
        assert_eq!(config.ca_cert_path, Some(PathBuf::from("/a.pem")));
        assert_eq!(GuiConfig::default().lang, i18n::DEFAULT_LANG);
    }

    fn sample_battle(
        battle_outcome: OutcomeType,
        attacking_ship_surrendered: bool,
        defending_ship_surrendered: bool,
    ) -> BattleSummary {
        BattleSummary {
            id: 1,
            checksum: "c".to_string(),
            imported_at: "2026-01-01T00:00:00Z".to_string(),
            battle_outcome,
            last_frame: 1000,
            attacking_ship_id: 1,
            defending_ship_id: 2,
            attacking_ship_surrendered,
            defending_ship_surrendered,
        }
    }

    #[test]
    fn outcome_label_distinguishes_clean_wins_losses_from_surrenders() {
        assert_eq!(
            outcome_label(&sample_battle(OutcomeType::AttackerWon, false, false)),
            OutcomeKind::Win
        );
        assert_eq!(
            outcome_label(&sample_battle(OutcomeType::AttackerWon, false, true)),
            OutcomeKind::SurrenderAdverse
        );
        assert_eq!(
            outcome_label(&sample_battle(OutcomeType::DefenderWon, false, false)),
            OutcomeKind::Loss
        );
        assert_eq!(
            outcome_label(&sample_battle(OutcomeType::DefenderWon, true, false)),
            OutcomeKind::Surrender
        );
    }

    #[test]
    fn format_thousands_groups_by_three_digits() {
        assert_eq!(format_thousands(7441203), "7 441 203");
        assert_eq!(format_thousands(203), "203");
        assert_eq!(format_thousands(1000), "1 000");
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(-9397907), "-9 397 907");
    }

    #[test]
    fn strips_and_parses_room_design_level_suffix() {
        assert_eq!(strip_level_suffix("Bolter Lv10"), "Bolter");
        assert_eq!(strip_level_suffix("Small Reactor Lv2"), "Small Reactor");
        // " Lv" not followed by digits isn't a level suffix - left untouched, not truncated.
        assert_eq!(strip_level_suffix("Object Lvl5"), "Object Lvl5");
        assert_eq!(strip_level_suffix("No Suffix Here"), "No Suffix Here");

        assert_eq!(parse_level_suffix("Bolter Lv10"), Some(10));
        assert_eq!(parse_level_suffix("Small Reactor Lv2"), Some(2));
        assert_eq!(parse_level_suffix("No Suffix Here"), None);
    }

    #[test]
    fn matches_rooms_ignoring_level_and_reports_it_in_the_label() {
        let rooms = vec![
            ("Laser Blaster Lv1".to_string(), 1),
            ("Bolter Lv10".to_string(), 2),
        ];
        assert_eq!(
            matching_rooms_label("Laser Blaster Room now has 1 sec Cooldown Time", &rooms),
            Some("L1 ×1".to_string())
        );
        assert_eq!(
            matching_rooms_label(
                "Ion Cannon Room Reload Time increased from 13.25 to 13.5 sec",
                &rooms
            ),
            None
        );
    }

    #[test]
    fn merges_matching_rooms_across_multiple_levels() {
        let rooms = vec![
            ("Bolter Lv10".to_string(), 2),
            ("Bolter Lv5".to_string(), 1),
        ];
        let label = matching_rooms_label("Bolter Reload Time increased from 1 to 2 sec", &rooms)
            .expect("should match");
        assert!(label.contains("L10 ×2"));
        assert!(label.contains("L5 ×1"));
    }

    #[test]
    fn highlights_known_field_names_with_their_operator() {
        assert_eq!(find_field_highlight_spans("player_id:"), vec![(0, 10)]);
        assert_eq!(find_field_highlight_spans("player_id: 123"), vec![(0, 10)]);
        assert_eq!(
            find_field_highlight_spans("def_trophy > 2000"),
            vec![(0, 12)]
        );
        assert_eq!(
            find_field_highlight_spans("player_name != Foo"),
            vec![(0, 14)]
        );
        assert_eq!(
            find_field_highlight_spans("def_trophy >= 2000"),
            vec![(0, 13)]
        );
    }

    #[test]
    fn does_not_highlight_unknown_or_bare_identifiers() {
        assert_eq!(find_field_highlight_spans("unknown_field: 5"), vec![]);
        assert_eq!(find_field_highlight_spans("just some text"), vec![]);
        assert_eq!(find_field_highlight_spans("player_id"), vec![]); // no trailing operator yet
    }

    #[test]
    fn highlights_multiple_fields_in_one_query() {
        let spans = find_field_highlight_spans("player_name: Foo && def_level > 10");
        assert_eq!(spans.len(), 2);
        assert_eq!(
            &"player_name: Foo && def_level > 10"[spans[0].0..spans[0].1],
            "player_name:"
        );
        assert_eq!(
            &"player_name: Foo && def_level > 10"[spans[1].0..spans[1].1],
            "def_level >"
        );
    }
}
