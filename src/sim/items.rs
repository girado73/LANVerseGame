//! A static, respawning world pickup. Behaviour is driven by simulation ticks
//! so it never depends on the wall clock.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Heal,
    Speed,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heal => "heal",
            Self::Speed => "speed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemState {
    pub id: String,
    pub kind: ItemKind,
    pub x: f64,
    pub y: f64,
    pub available: bool,
    pub respawn_at_tick: i64,
}

impl ItemState {
    pub fn new(id: impl Into<String>, kind: ItemKind, x: f64, y: f64) -> Self {
        Self {
            id: id.into(),
            kind,
            x,
            y,
            available: true,
            respawn_at_tick: 0,
        }
    }
}
