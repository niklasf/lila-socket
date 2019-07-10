use std::mem;

use serde::{Deserialize, Serialize, Deserializer, de};

use shakmaty::{Square, PositionError, Position, MoveList, Role};
use shakmaty::variants::{Chess, Giveaway, KingOfTheHill, ThreeCheck, Atomic, Horde, RacingKings, Crazyhouse};
use shakmaty::fen::{Fen, FenOpts};

use crate::opening_db::{Opening, FULL_OPENING_DB};
use crate::util;

fn lookup_opening(mut fen: Fen) -> Option<&'static Opening> {
    fen.pockets = None;
    fen.remaining_checks = None;
    FULL_OPENING_DB.get(FenOpts::new().epd(&fen).as_str())
}

fn piotr(sq: Square) -> char {
    if sq < Square::C4 {
        (b'a' + u8::from(sq)) as char
    } else if sq < Square::E7 {
        (b'A' + (sq - Square::C4) as u8) as char
    } else if sq < Square::G8 {
        (b'0' + (sq - Square::E7) as u8) as char
    } else if sq == Square::G8 {
        '!'
    } else {
        '?'
    }
}

#[derive(Deserialize, Copy, Clone)]
enum VariantKey {
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "fromPosition")]
    FromPosition,
    #[serde(rename = "chess960")]
    Chess960,
    #[serde(rename = "antichess")]
    Antichess,
    #[serde(rename = "kingOfTheHill")]
    KingOfTheHill,
    #[serde(rename = "threeCheck")]
    ThreeCheck,
    #[serde(rename = "atomic")]
    Atomic,
    #[serde(rename = "horde")]
    Horde,
    #[serde(rename = "racingKings")]
    RacingKings,
    #[serde(rename = "crazyhouse")]
    Crazyhouse,
}

#[derive(Copy, Clone)]
enum EffectiveVariantKey {
    Standard,
    Antichess,
    KingOfTheHill,
    ThreeCheck,
    Atomic,
    Horde,
    RacingKings,
    Crazyhouse,
}

impl EffectiveVariantKey {
    fn is_opening_sensible(self) -> bool {
        match self {
            EffectiveVariantKey::Standard |
            EffectiveVariantKey::Crazyhouse |
            EffectiveVariantKey::ThreeCheck |
            EffectiveVariantKey::KingOfTheHill => true,
            _ => false,
        }
    }

    fn position(self, fen: &Fen) -> Result<VariantPosition, PositionError> {
        match self {
            EffectiveVariantKey::Standard => fen.position().map(VariantPosition::Standard),
            EffectiveVariantKey::Antichess => fen.position().map(VariantPosition::Antichess),
            EffectiveVariantKey::KingOfTheHill => fen.position().map(VariantPosition::KingOfTheHill),
            EffectiveVariantKey::ThreeCheck => fen.position().map(VariantPosition::ThreeCheck),
            EffectiveVariantKey::Atomic => fen.position().map(VariantPosition::Atomic),
            EffectiveVariantKey::Horde => fen.position().map(VariantPosition::Horde),
            EffectiveVariantKey::RacingKings => fen.position().map(VariantPosition::RacingKings),
            EffectiveVariantKey::Crazyhouse => fen.position().map(VariantPosition::Crazyhouse),
        }
    }
}

impl From<VariantKey> for EffectiveVariantKey {
    fn from(variant: VariantKey) -> EffectiveVariantKey {
        match variant {
            VariantKey::Standard | VariantKey::FromPosition | VariantKey::Chess960 =>
                EffectiveVariantKey::Standard,
            VariantKey::Antichess => EffectiveVariantKey::Antichess,
            VariantKey::KingOfTheHill => EffectiveVariantKey::KingOfTheHill,
            VariantKey::ThreeCheck => EffectiveVariantKey::ThreeCheck,
            VariantKey::Atomic => EffectiveVariantKey::Atomic,
            VariantKey::Horde => EffectiveVariantKey::Horde,
            VariantKey::RacingKings => EffectiveVariantKey::RacingKings,
            VariantKey::Crazyhouse => EffectiveVariantKey::Crazyhouse,
        }
    }
}

enum VariantPosition {
    Standard(Chess),
    Antichess(Giveaway),
    KingOfTheHill(KingOfTheHill),
    ThreeCheck(ThreeCheck),
    Atomic(Atomic),
    Horde(Horde),
    RacingKings(RacingKings),
    Crazyhouse(Crazyhouse),
}

impl VariantPosition {
    fn borrow(&self) -> &dyn Position {
        match *self {
            VariantPosition::Standard(ref pos) => pos,
            VariantPosition::Antichess(ref pos) => pos,
            VariantPosition::KingOfTheHill(ref pos) => pos,
            VariantPosition::ThreeCheck(ref pos) => pos,
            VariantPosition::Atomic(ref pos) => pos,
            VariantPosition::Horde(ref pos) => pos,
            VariantPosition::RacingKings(ref pos) => pos,
            VariantPosition::Crazyhouse(ref pos) => pos,
        }
    }
}

#[derive(Deserialize)]
pub struct GetOpening {
    variant: Option<VariantKey>,
    path: String,
    fen: String,
}

impl GetOpening {
    pub fn respond(self) -> Option<OpeningResponse> {
        let variant = EffectiveVariantKey::from(self.variant.unwrap_or(VariantKey::Standard));
        if variant.is_opening_sensible() {
            self.fen.parse().ok()
                .and_then(lookup_opening)
                .map(|opening| OpeningResponse {
                    path: self.path,
                    opening
                })
        } else {
            None
        }
    }
}

#[derive(Serialize)]
pub struct OpeningResponse {
    path: String,
    opening: &'static Opening,
}

#[derive(Deserialize)]
pub struct GetDests {
    variant: Option<VariantKey>,
    fen: String,
    path: String,
    #[serde(rename = "ch")]
    chapter_id: Option<String>,
}

impl GetDests {
    pub fn respond(self) -> Result<DestsResponse, DestsFailure> {
        let variant = EffectiveVariantKey::from(self.variant.unwrap_or(VariantKey::Standard));
        let fen: Fen = self.fen.parse().map_err(|_| DestsFailure)?;
        let pos = variant.position(&fen).map_err(|_| DestsFailure)?;

        let mut legals = MoveList::new();
        pos.borrow().legal_moves(&mut legals);

        let mut dests = String::with_capacity(80);
        let mut first = true;
        for from_sq in pos.borrow().us() {
            let mut from_here = legals.iter().filter(|m| m.from() == Some(from_sq)).peekable();
            if from_here.peek().is_some() {
                if mem::replace(&mut first, false) {
                    dests.push(' ');
                }
                dests.push(piotr(from_sq));
                for m in from_here {
                    dests.push(piotr(m.to()));
                }
            }
        }

        Ok(DestsResponse {
            path: self.path,
            opening: lookup_opening(fen),
            chapter_id: self.chapter_id,
            dests,
        })
    }
}

#[derive(Serialize)]
pub struct DestsResponse {
    path: String,
    dests: String,
    #[serde(flatten)]
    opening: Option<&'static Opening>,
    #[serde(rename = "ch", flatten)]
    chapter_id: Option<String>,
}

#[derive(Debug)]
pub struct DestsFailure;

#[derive(Deserialize)]
pub struct PlayMove {
    #[serde(deserialize_with = "util::parsable")]
    orig: Square,
    #[serde(deserialize_with = "util::parsable")]
    dest: Square,
    variant: Option<VariantKey>,
    fen: String,
    path: String,
    promotion: Option<Role>,
    #[serde(rename = "ch")]
    chapter_id: Option<String>,
}

impl PlayMove {
    pub fn respond(self) -> Result<Node, StepFailure> {
        unimplemented!()
    }
}

#[derive(Deserialize)]
pub struct PlayDrop {
    //role: Role,
    //pos: Square,
    variant: Option<VariantKey>,
    fen: String,
    path: String,
    chapter_id: Option<String>,
}

impl PlayDrop {
    pub fn respond(self) -> Result<Node, StepFailure> {
        unimplemented!()
    }
}

#[derive(Serialize)]
pub struct Node {
    node: Branch,
    path: String,
    #[serde(rename = "ch", flatten)]
    chapter_id: Option<String>,
}

#[derive(Serialize)]
pub struct Branch {
    id: String, // uci chair pair
    ply: u32, // game.turns
    fen: String,
    check: bool, // situation.check
    dests: String, // dests in the current position
    opening: Option<&'static Opening>,
    drops: String, // ???
    crazy_data: String, // ???
}

#[derive(Debug)]
pub struct StepFailure;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piotr() {
        assert_eq!(piotr(Square::A1), b'a');
        assert_eq!(piotr(Square::B4), b'z');
        assert_eq!(piotr(Square::C4), b'A');
        assert_eq!(piotr(Square::D7), b'Z');
        assert_eq!(piotr(Square::E7), b'0');
        assert_eq!(piotr(Square::F8), b'9');
        assert_eq!(piotr(Square::G8), b'!');
        assert_eq!(piotr(Square::H8), b'?');
    }
}
