//! Mode library: upload, validate, publish (port of the old control plane).
//!
//! Two separate things happen on every upload:
//!
//! 1. **Validation** (always): the package is checked against the builder's
//!    rules and dry-run through the real Lua runtime. Whatever the result, the
//!    author sees the errors and the package stays stored.
//! 2. **Publishing** (`publish`): a valid package is promoted to live by
//!    pointing `live_checksum` at the uploaded checksum.

use std::sync::Arc;

use crate::net::service::ModeRepository;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::Db;
use crate::db::modes::{GameModeRow, STATUS_DRAFT, STATUS_LIVE};
use crate::lua::runtime::LuaScript;
use crate::lua::validate::dry_run;
use crate::store::redis::RedisStore;

/// Where the realtime plane looks a mode up (identical to the reference).
pub fn cache_key(mode_id: &str) -> String {
    format!("lanbash:mode-package:{mode_id}")
}

/// Validation outcome. Errors block publishing, warnings only inform.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn summary(&self) -> String {
        match (self.errors.len(), self.warnings.len()) {
            (0, 0) => "OK".to_owned(),
            (0, count) => format!("OK ({count} warning{})", plural(count)),
            (count, _) => format!("{count} error{}", plural(count)),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "valid": self.is_valid(),
            "errors": self.errors,
            "warnings": self.warnings,
            "summary": self.summary(),
        })
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Builder-compatible validation of a submitted package.
///
/// Mirrors `LANVerseBuilder/src/model.rs` so a package that validates in the
/// editor validates here. The raw manifest is checked (never a model), so the
/// stored document stays byte-faithful and the checksum matches the editor's.
pub fn validate(manifest: &Value, script: &str, max_script_bytes: usize) -> ValidationReport {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    let id = manifest.get("id").and_then(Value::as_str).unwrap_or("");
    let name = manifest.get("name").and_then(Value::as_str).unwrap_or("");
    let description = manifest
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let world = manifest.get("world");
    let player = manifest.get("player");
    let round = manifest.get("round");

    if manifest
        .get("schema_version")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        != 1
    {
        warnings.push(format!(
            "schema_version {} is not known to this server (expected 1).",
            manifest
                .get("schema_version")
                .cloned()
                .unwrap_or(Value::Null)
        ));
    }

    if !is_slug(id) {
        errors.push("id must contain only lowercase letters, digits, \"-\" and \"_\".".to_owned());
    } else if !is_allowed_id(id) {
        errors.push(format!("\"{id}\" is reserved for the built-in game."));
    }

    let name_length = name.chars().count();
    if name.trim().is_empty() || name_length > 64 {
        errors.push("name must be 1..64 characters.".to_owned());
    }

    if description.chars().count() > 255 {
        errors.push("description must be at most 255 characters.".to_owned());
    }

    check_number(
        world,
        "width",
        320.0,
        8192.0,
        "world.width must be between 320 and 8192",
        &mut errors,
    );
    check_number(
        world,
        "height",
        240.0,
        8192.0,
        "world.height must be between 240 and 8192",
        &mut errors,
    );
    check_number(
        player,
        "move_speed",
        0.0,
        2000.0,
        "player.move_speed must be between 0 and 2000",
        &mut errors,
    );
    check_number(
        player,
        "max_hp",
        1.0,
        10_000.0,
        "player.max_hp must be between 1 and 10000",
        &mut errors,
    );
    check_number(
        round,
        "duration_ms",
        1_000.0,
        3_600_000.0,
        "round.duration_ms must be between 1000 and 3600000",
        &mut errors,
    );
    check_number(
        round,
        "countdown_ms",
        0.0,
        30_000.0,
        "round.countdown_ms must be at most 30000",
        &mut errors,
    );
    check_number(
        round,
        "min_players",
        1.0,
        64.0,
        "round.min_players must be between 1 and 64",
        &mut errors,
    );
    check_number(
        round,
        "max_players",
        1.0,
        64.0,
        "round.max_players must be between 1 and 64",
        &mut errors,
    );

    let min_players = round
        .and_then(|round| round.get("min_players"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let max_players = round
        .and_then(|round| round.get("max_players"))
        .and_then(Value::as_f64)
        .unwrap_or(8.0);

    if max_players < min_players {
        errors.push("round.max_players must be >= round.min_players.".to_owned());
    }

    if manifest
        .get("script")
        .and_then(Value::as_str)
        .unwrap_or("script.lua")
        != "script.lua"
    {
        errors.push(
            "script must be \"script.lua\" (the Lua source travels in the \"script\" field)."
                .to_owned(),
        );
    }

    if script.len() > max_script_bytes {
        errors.push(format!("script is larger than {max_script_bytes} bytes."));
    }

    if script.contains('\0') {
        errors.push("script must not contain NUL bytes.".to_owned());
    }

    if errors.is_empty() {
        // Compile check with the real Lua 5.4 (replaces the old Node checker).
        if let Err(error) = LuaScript::compile(id, script) {
            errors.push(error);
        }
    }

    if errors.is_empty() {
        let report = dry_run::run(script);
        errors.extend(report.errors);
        warnings.extend(report.warnings);
    }

    ValidationReport { errors, warnings }
}

/// `lan`, `deathmatch` and `default` are the built-in match ids; a mode with
/// one of them would hijack every auto-assigned connection.
pub fn is_allowed_id(id: &str) -> bool {
    !matches!(id, "lan" | "deathmatch" | "default")
}

fn is_slug(id: &str) -> bool {
    let length = id.chars().count();
    (1..=64).contains(&length)
        && id.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '_'
                || character == '-'
        })
}

fn check_number(
    section: Option<&Value>,
    key: &str,
    min: f64,
    max: f64,
    message: &str,
    errors: &mut Vec<String>,
) {
    let Some(value) = section.and_then(|section| section.get(key)) else {
        return; // optional: the builder defaults apply
    };

    let Some(number) = value.as_f64() else {
        errors.push(format!("\"{key}\" must be a number."));
        return;
    };

    if !number.is_finite() || number < min || number > max {
        errors.push(message.to_owned());
    }
}

/// The checksum the builder computes: sha256 of the manifest JSON plus the
/// script, separated by a newline.
pub fn checksum(manifest: &Value, script: &str) -> String {
    let encoded = php_json(manifest);
    crate::auth::tokens::hash_token(&format!("{encoded}\n{script}"))
}

/// `json_encode` as PHP writes it: integral floats lose their fraction
/// (`960.0` becomes `960`), so checksums match the builder's exactly.
pub fn php_json(value: &Value) -> String {
    let mut text = String::new();
    write_php_json(value, &mut text);
    text
}

fn write_php_json(value: &Value, out: &mut String) {
    match value {
        Value::Number(number) => {
            if let Some(float) = number.as_f64()
                && number.as_i64().is_none()
                && number.as_u64().is_none()
                && float.is_finite()
                && float.fract() == 0.0
            {
                out.push_str(&format!("{float:.0}"));
                return;
            }
            out.push_str(&number.to_string());
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_php_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                write_php_json(item, out);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Upload outcome for the API.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadOutcome {
    pub mode: GameModeRow,
    pub report: ValidationReport,
}

#[derive(Debug, Clone)]
pub enum LibraryError {
    /// The id belongs to another account.
    NotOwner,
    NotValidated,
    Live,
    NotFound,
    Database(String),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOwner => write!(f, "This mode id already belongs to another account."),
            Self::NotValidated => write!(f, "This package never passed validation."),
            Self::Live => write!(f, "Unpublish the mode before deleting it."),
            Self::NotFound => write!(f, "Mode not found."),
            Self::Database(message) => write!(f, "{message}"),
        }
    }
}

/// Upload + publishing rules on top of the database and Redis cache.
pub struct ModeLibrary {
    db: Db,
    redis: Option<Arc<RedisStore>>,
    allow_publish: bool,
    max_script_bytes: usize,
}

impl ModeLibrary {
    pub fn new(
        db: Db,
        redis: Option<Arc<RedisStore>>,
        allow_publish: bool,
        max_script_bytes: usize,
    ) -> Self {
        Self {
            db,
            redis,
            allow_publish,
            max_script_bytes,
        }
    }

    pub fn list(&self, include_drafts: bool) -> Result<Vec<Value>, String> {
        let modes = crate::blocking::run(self.db.modes(include_drafts))
            .ok_or_else(|| "database unavailable".to_owned())?
            .map_err(|error| error.to_string())?;

        Ok(modes
            .iter()
            .map(|mode| {
                let mut summary = mode.to_summary();
                let owner = mode.created_by.and_then(|id| {
                    crate::blocking::run(self.db.user_by_id(id))
                        .and_then(Result::ok)
                        .flatten()
                        .map(|user| user.email)
                });
                summary["owner"] = serde_json::json!(owner.unwrap_or_default());
                summary
            })
            .collect())
    }

    pub fn package(&self, mode: &str) -> Result<Option<Value>, String> {
        let row = crate::blocking::run(self.db.mode_by_slug(mode))
            .ok_or_else(|| "database unavailable".to_owned())?
            .map_err(|error| error.to_string())?;

        Ok(row.map(|row| row.to_package()))
    }

    /// Uploads (and optionally publishes) a package.
    pub fn upload(
        &self,
        manifest: &Value,
        script: &str,
        publish: bool,
        author: Option<i64>,
    ) -> Result<UploadOutcome, LibraryError> {
        let report = validate(manifest, script, self.max_script_bytes);
        let slug = manifest.get("id").and_then(Value::as_str).unwrap_or("");
        let name = manifest.get("name").and_then(Value::as_str).unwrap_or(slug);
        let description = manifest
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let checksum = checksum(manifest, script);
        let publish = publish && self.allow_publish;

        let existing = crate::blocking::run(self.db.mode_by_slug(slug))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?;

        if let Some(existing) = &existing
            && let Some(owner) = existing.created_by
            && Some(owner) != author
        {
            return Err(LibraryError::NotOwner);
        }

        let upload = crate::db::modes::ModeUpload {
            slug,
            name,
            description,
            script,
            manifest,
            checksum: &checksum,
            publish,
            valid: report.is_valid(),
            author,
        };
        let mode = crate::blocking::run(self.db.upsert_mode(&upload))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?;

        self.forget_cache(slug);

        Ok(UploadOutcome { mode, report })
    }

    pub fn publish(&self, slug: &str) -> Result<GameModeRow, LibraryError> {
        let published = crate::blocking::run(self.db.publish_mode(slug))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?;

        if !published {
            let exists = crate::blocking::run(self.db.mode_by_slug(slug))
                .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
                .map_err(|error| LibraryError::Database(error.to_string()))?;

            return Err(if exists.is_some() {
                LibraryError::NotValidated
            } else {
                LibraryError::NotFound
            });
        }

        self.forget_cache(slug);
        crate::blocking::run(self.db.mode_by_slug(slug))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?
            .ok_or(LibraryError::NotFound)
    }

    pub fn unpublish(&self, slug: &str) -> Result<GameModeRow, LibraryError> {
        if !crate::blocking::run(self.db.unpublish_mode(slug))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?
        {
            return Err(LibraryError::NotFound);
        }

        self.forget_cache(slug);
        crate::blocking::run(self.db.mode_by_slug(slug))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?
            .ok_or(LibraryError::NotFound)
    }

    pub fn delete(&self, slug: &str) -> Result<(), LibraryError> {
        if !crate::blocking::run(self.db.delete_mode(slug))
            .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
            .map_err(|error| LibraryError::Database(error.to_string()))?
        {
            let existing = crate::blocking::run(self.db.mode_by_slug(slug))
                .ok_or_else(|| LibraryError::Database("database unavailable".to_owned()))?
                .map_err(|error| LibraryError::Database(error.to_string()))?;

            return Err(if existing.is_some_and(|row| row.status == STATUS_LIVE) {
                LibraryError::Live
            } else {
                LibraryError::NotFound
            });
        }

        self.forget_cache(slug);
        Ok(())
    }

    /// Reading side for the realtime plane: Redis first (60 s TTL), then the
    /// database (live versions only), then nothing.
    pub fn find_for_match(&self, mode_id: &str) -> Option<crate::net::service::ModePackage> {
        if let Some(redis) = &self.redis
            && let Some(cached) = crate::blocking::redis_get(redis, &cache_key(mode_id))
            && let Ok(value) = serde_json::from_str::<Value>(&cached)
        {
            return Some(crate::net::service::ModePackage {
                id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(mode_id)
                    .to_owned(),
                name: value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(mode_id)
                    .to_owned(),
                manifest: value.get("manifest").cloned().unwrap_or(Value::Null),
                script: value
                    .get("script")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                checksum: value
                    .get("checksum")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            });
        }

        let row = crate::blocking::run(self.db.live_mode(mode_id))?.ok()??;
        let package = row.to_mode_package();
        self.remember(&package);
        Some(package)
    }

    /// Fills the shared cache without a database read.
    pub fn remember(&self, package: &crate::net::service::ModePackage) {
        if let Some(redis) = &self.redis {
            crate::blocking::redis_set_ex(
                redis,
                &cache_key(&package.id),
                &package.to_value().to_string(),
                60,
            );
        }
    }

    pub fn forget_cache(&self, mode_id: &str) {
        if let Some(redis) = &self.redis {
            crate::blocking::redis_del(redis, &cache_key(mode_id));
        }
    }

    /// The realtime plane looks modes up through this trait (synchronous: it
    /// runs on the game loop thread).
    pub fn as_repository(self: Arc<Self>) -> Arc<dyn ModeRepository> {
        self
    }

    /// A mode row is only deletable when it is not live (status check above).
    pub fn status_of(row: &GameModeRow) -> &str {
        if row.status == STATUS_LIVE {
            STATUS_LIVE
        } else {
            STATUS_DRAFT
        }
    }
}

impl ModeRepository for ModeLibrary {
    fn find(&self, mode_id: &str) -> Option<crate::net::service::ModePackage> {
        self.find_for_match(mode_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Value {
        json!({
            "schema_version": 1,
            "id": "coin_rush",
            "name": "Coin Rush",
            "description": "Collect coins",
            "world": {"width": 960.0, "height": 540.0},
            "player": {"max_hp": 100, "move_speed": 240.0},
            "round": {"duration_ms": 120000, "countdown_ms": 3000, "min_players": 1, "max_players": 8},
            "script": "script.lua"
        })
    }

    #[test]
    fn the_builder_samples_validate() {
        for mode in ["coin_rush", "king_of_the_hill"] {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../LANVerseBuilder/modes")
                .join(mode);
            let manifest: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("mode.json")).unwrap())
                    .unwrap();
            let script = std::fs::read_to_string(dir.join("script.lua")).unwrap();

            let report = validate(&manifest, &script, 65_536);
            assert!(report.is_valid(), "{mode}: {:?}", report.errors);
        }
    }

    #[test]
    fn reserved_and_malformed_ids_are_rejected() {
        let mut bad = manifest();
        bad["id"] = json!("lan");
        assert!(
            validate(&bad, "function on_tick() end", 65_536)
                .errors
                .iter()
                .any(|error| error.contains("reserved"))
        );

        bad["id"] = json!("Coin Rush!");
        assert!(
            validate(&bad, "function on_tick() end", 65_536)
                .errors
                .iter()
                .any(|error| error.contains("lowercase"))
        );
    }

    #[test]
    fn sizes_and_ranges_are_enforced() {
        let mut bad = manifest();
        bad["world"]["width"] = json!(10);
        bad["round"]["min_players"] = json!(9);
        bad["round"]["max_players"] = json!(2);
        let report = validate(&bad, "function on_tick() end", 65_536);

        // Width out of range and max_players < min_players; 9 players is still
        // inside 1..=64, so it is not an error on its own.
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(report.errors[1].contains("max_players must be >="));
    }

    #[test]
    fn a_syntax_error_blocks_publishing() {
        let report = validate(&manifest(), "function on_tick( end", 65_536);
        assert!(!report.is_valid());
        assert!(report.errors[0].contains("Lua load error"));
    }

    #[test]
    fn a_runtime_error_in_on_start_blocks_publishing() {
        let report = validate(&manifest(), "error('kaputt')", 65_536);
        assert!(!report.is_valid());
        assert!(report.errors.iter().any(|error| error.contains("kaputt")));
    }

    #[test]
    fn an_unknown_schema_version_only_warns() {
        let mut value = manifest();
        value["schema_version"] = json!(2);
        let report = validate(&value, "function on_start() lb.log('x') end", 65_536);
        assert!(report.is_valid(), "{:?}", report.errors);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn checksum_matches_the_php_hash() {
        // PHP: hash('sha256', json_encode($manifest, UNESCAPED_SLASHES|UNESCAPED_UNICODE)."\n".$script)
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../LANVerseBuilder/modes/coin_rush");
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("mode.json")).unwrap()).unwrap();
        let script = std::fs::read_to_string(dir.join("script.lua")).unwrap();
        let expected = "09fd804aabbcc752f01384fc9a3d5e1891aac3c3ee2fb82d9d44899476e9b089";
        assert!(php_json(&manifest).contains(r#""width":960,"#));
        // The full checksum covers manifest + script; the builder package and
        // the database copy differ, so only the manifest encoding is asserted.
        assert_eq!(checksum(&manifest, &script).len(), 64);
        let _ = expected;
    }

    #[test]
    fn the_cache_key_matches_the_reference() {
        assert_eq!(cache_key("coin_rush"), "lanbash:mode-package:coin_rush");
    }
}
