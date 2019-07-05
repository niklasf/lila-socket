use arrayvec::ArrayString;

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An 8 charatcer game id.
#[derive(Deserialize, Serialize, Eq, PartialEq, Hash, Clone, Debug)]
#[serde(transparent)]
pub struct GameId(pub ArrayString<[u8; 8]>);

#[derive(Debug)]
pub struct InvalidGameId;

impl FromStr for GameId {
    type Err = InvalidGameId;

    fn from_str(s: &str) -> Result<GameId, InvalidGameId> {
        Ok(GameId(ArrayString::from(s).map_err(|_| InvalidGameId)?))
    }
}

/// Channels for server sent updates.
#[derive(Deserialize, Debug, Copy, Clone)]
pub enum Flag {
    #[serde(rename = "simul")]
    Simul = 0,
    #[serde(rename = "tournament")]
    Tournament = 1,
}
