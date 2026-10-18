//! Mode packages (Lua asset + manifest) a match can be created from.

use serde_json::Value;

/// A loaded mode package.
#[derive(Debug, Clone, PartialEq)]
pub struct ModePackage {
    pub id: String,
    pub name: String,
    pub manifest: Value,
    pub script: String,
    pub checksum: String,
}

impl ModePackage {
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "manifest": self.manifest,
            "script": self.script,
            "checksum": self.checksum,
        })
    }
}

/// Lookup order in the binary: Redis cache, then Postgres (live only).
pub trait ModeRepository: Send + Sync {
    fn find(&self, mode_id: &str) -> Option<ModePackage>;
}

/// No modes: every match is the built-in game.
pub struct EmptyModeRepository;

impl ModeRepository for EmptyModeRepository {
    fn find(&self, _mode_id: &str) -> Option<ModePackage> {
        None
    }
}
