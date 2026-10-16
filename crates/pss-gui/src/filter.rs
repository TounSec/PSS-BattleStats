//! Logical query language for the battle sidebar's search box: `field: value`/`field != value`/
//! `field > value` terms combined with `&&`/`||`/`!`/parens, plus a bare-word fallback that still
//! does the old plain substring search. See CLAUDE.md-adjacent design discussion in chat history -
//! no dedicated parser crate, the grammar is small enough to hand-roll.

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
                    Some(Token::Ge) | Some(Token::Le) | Some(Token::Gt) | Some(Token::Lt) => false,
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
