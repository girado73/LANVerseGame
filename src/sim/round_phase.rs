//! Round lifecycle, mirrored 1:1 by `round_state.phase` on the wire.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundPhase {
    Connecting,
    Lobby,
    Countdown,
    Running,
    Finished,
}

impl RoundPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Lobby => "lobby",
            Self::Countdown => "countdown",
            Self::Running => "running",
            Self::Finished => "finished",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "connecting" => Self::Connecting,
            "countdown" => Self::Countdown,
            "running" => Self::Running,
            "finished" => Self::Finished,
            // Anything unknown (including "lobby") means "no round yet", which
            // matches how the reference `RoundPhase::from()` behaved for
            // restored state with a missing key.
            _ => Self::Lobby,
        }
    }

    /// Only a running round accepts gameplay-affecting input.
    pub fn accepts_gameplay_input(self) -> bool {
        self == Self::Running
    }
}

impl std::fmt::Display for RoundPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_accepts_input() {
        assert!(RoundPhase::Running.accepts_gameplay_input());
        for phase in [
            RoundPhase::Connecting,
            RoundPhase::Lobby,
            RoundPhase::Countdown,
            RoundPhase::Finished,
        ] {
            assert!(!phase.accepts_gameplay_input());
        }
    }

    #[test]
    fn wire_names_round_trip() {
        for phase in [
            RoundPhase::Connecting,
            RoundPhase::Lobby,
            RoundPhase::Countdown,
            RoundPhase::Running,
            RoundPhase::Finished,
        ] {
            assert_eq!(RoundPhase::parse(phase.as_str()), phase);
        }
        assert_eq!(RoundPhase::parse("nonsense"), RoundPhase::Lobby);
    }
}
