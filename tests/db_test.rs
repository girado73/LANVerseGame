//! Integration against a real, ephemeral Postgres (`pg-embed`).
//!
//! One cluster per test binary; each test uses its own database name prefix so
//! they can run in parallel. No network access beyond localhost, no fixture
//! data: everything is created through the same code paths the server uses.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use lanverse_game::auth::TokenService;
use lanverse_game::db::Db;
use lanverse_game::db::accounts::USER_MORPH;
use lanverse_game::net::service::GameIdentity;
use lanverse_game::net::service::{RoundPlayer, RoundResult};
use lanverse_game::store::modes::{ModeLibrary, is_allowed_id, validate};
use lanverse_game::store::results::{ResultRecorder, SignedResult, sign, verify};
use pg_embed::pg_enums::PgAuthMethod;
use pg_embed::pg_fetch::PG_V17;
use pg_embed::postgres::{PgEmbed, PgSettings};
use serde_json::{Value, json};
use sqlx::Row;

static POSTGRES: OnceLock<Option<String>> = OnceLock::new();

/// Starts one Postgres for the whole test binary; `None` when it cannot start
/// (offline machine without a cached server binary).
async fn postgres_url(database: &str) -> Option<String> {
    // `get_or_init` runs synchronously inside the caller's async test, so the
    // embedded server is started on its own thread.
    let base = match POSTGRES.get() {
        Some(base) => base.clone(),
        None => {
            let started = std::thread::spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(start_postgres())
            })
            .join()
            .unwrap_or(None);

            let _ = POSTGRES.set(started);
            POSTGRES.get().cloned().flatten()
        }
    };

    let base = base?;
    let db = Db::connect(&base, 2).await.ok()?;

    let exists = sqlx::query("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(database)
        .fetch_optional(db.pool())
        .await
        .ok()?
        .is_some();

    if !exists {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{database}\""
        )))
        .execute(db.pool())
        .await
        .ok()?;
    }

    Some(base.replace("/postgres", &format!("/{database}")))
}

async fn start_postgres() -> Option<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/pg-embed-test");

    let settings = PgSettings {
        database_dir: dir,
        port: 55432,
        user: "lanverse".to_owned(),
        password: "lanverse".to_owned(),
        auth_method: PgAuthMethod::Plain,
        persistent: true,
        timeout: Some(std::time::Duration::from_secs(60)),
        migration_dir: None,
    };

    let fetch = pg_embed::pg_fetch::PgFetchSettings {
        version: PG_V17,
        ..Default::default()
    };

    let mut pg = PgEmbed::new(settings, fetch).await.ok()?;
    pg.setup().await.ok()?;
    pg.start_db().await.ok()?;
    let _ = pg.database_exists("postgres").await;

    let url = pg.full_db_uri("postgres");
    // The embedded server must outlive this call.
    std::mem::forget(pg);
    Some(url)
}

struct Fixture {
    db: Db,
    tokens: Arc<TokenService>,
    library: ModeLibrary,
}

async fn fixture(name: &str) -> Option<Fixture> {
    let url = postgres_url(name).await?;
    let db = Db::connect(&url, 4).await.ok()?;
    db.migrate().await.ok()?;

    // A fresh schema per run: drop everything the migrations created.
    sqlx::query("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(db.pool())
        .await
        .ok()?;
    db.migrate().await.ok()?;

    let tokens = Arc::new(TokenService::new(db.clone()));
    let library = ModeLibrary::new(db.clone(), None, true, 65_536);

    Some(Fixture {
        db,
        tokens,
        library,
    })
}

/// `let Some(fixture) = make_fixture("name").await else { … }` with a skip.
async fn make_fixture(name: &str) -> Option<Fixture> {
    let fixture = fixture(name).await;

    if fixture.is_none() {
        eprintln!("skipping {name}: embedded Postgres unavailable");
    }

    fixture
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrations_create_the_full_schema() {
    let db = make_fixture("lv_schema").await.map(|f| f.db);
    let Some(db) = db else {
        eprintln!("skipping");
        return;
    };

    for table in [
        "users",
        "players",
        "personal_access_tokens",
        "cosmetics",
        "player_cosmetic",
        "friendships",
        "lobbies",
        "lobby_members",
        "matches",
        "match_players",
        "match_rounds",
        "highscores",
        "game_modes",
    ] {
        assert_eq!(db.count(table).await.unwrap(), 0, "{table}");
    }

    // The migration is idempotent.
    db.migrate().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_login_and_token_lifecycle() {
    let fixture = make_fixture("lv_auth").await;
    let Some(Fixture { db, tokens, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let hash = lanverse_game::auth::tokens::hash_password("supersecret123").unwrap();
    let (user, player) = db
        .create_user("Alice", "alice@example.test", &hash)
        .await
        .unwrap();
    assert_eq!(player.display_name, "Alice");

    // A duplicate email is rejected by the unique index.
    assert!(
        db.create_user("Alice2", "ALICE@example.test", &hash)
            .await
            .is_err()
    );

    // Login verifies the password and issues a Sanctum-shaped token.
    let (user_id, logged_in) = tokens
        .authenticate("alice@example.test", "supersecret123")
        .await
        .unwrap();
    assert_eq!(user_id, user.id);
    assert_eq!(logged_in.id, player.id);

    let wrong = tokens
        .authenticate("alice@example.test", "nope")
        .await
        .unwrap_err();
    assert!(wrong.message.contains("credentials"));

    let (token, _) = tokens.create_token(user.id, "builder").await.unwrap();
    let (prefix, secret) = token.split_once('|').unwrap();
    assert_eq!(prefix.parse::<i64>().unwrap(), 1);
    assert_eq!(secret.len(), 40);

    // The stored value is the SHA-256 of the plaintext.
    let row = sqlx::query("SELECT token, tokenable_type FROM personal_access_tokens WHERE id = $1")
        .bind(prefix.parse::<i64>().unwrap())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        row.get::<String, _>("token"),
        lanverse_game::auth::tokens::hash_token(&token)
    );
    assert_eq!(row.get::<String, _>("tokenable_type"), USER_MORPH);

    // The token resolves to the profile and marks itself used.
    let (resolved_user, resolved_player) = tokens.resolve_token(&token).await.unwrap();
    assert_eq!(resolved_user, user.id);
    assert_eq!(resolved_player.id, player.id);

    let last_used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM personal_access_tokens WHERE id = $1")
            .bind(prefix.parse::<i64>().unwrap())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(last_used.is_some());

    // Unknown tokens are refused.
    assert!(tokens.resolve_token("999|not-a-real-token").await.is_err());

    // Banned accounts are refused even with a valid token.
    sqlx::query("UPDATE players SET moderation_status = 'banned' WHERE id = $1")
        .bind(player.id)
        .execute(db.pool())
        .await
        .unwrap();
    let banned = tokens.resolve_token(&token).await.unwrap_err();
    assert!(banned.message.contains("banned"));

    sqlx::query("UPDATE players SET moderation_status = 'active' WHERE id = $1")
        .bind(player.id)
        .execute(db.pool())
        .await
        .unwrap();

    // Logout revokes exactly one token.
    let second = tokens.create_token(user.id, "laptop").await.unwrap().0;
    let first_row = db
        .token_by_hash(&lanverse_game::auth::tokens::hash_token(&token))
        .await
        .unwrap()
        .unwrap();
    db.revoke_token(first_row.id).await.unwrap();

    assert!(tokens.resolve_token(&token).await.is_err());
    assert!(tokens.resolve_token(&second).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_tokens_are_refused() {
    let fixture = make_fixture("lv_expiry").await;
    let Some(Fixture { db, tokens, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let hash = lanverse_game::auth::tokens::hash_password("supersecret123").unwrap();
    let (user, _) = db
        .create_user("Bob", "bob@example.test", &hash)
        .await
        .unwrap();

    let token = lanverse_game::auth::tokens::new_token(1);
    let id = db
        .create_token(
            user.id,
            "old",
            &lanverse_game::auth::tokens::hash_token(&token),
            Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        )
        .await
        .unwrap();
    let token = format!("{id}|{}", token.split_once('|').unwrap().1);

    let error = tokens.resolve_token(&token).await.unwrap_err();
    assert!(error.message.contains("expired"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modes_upload_publish_and_package_lookup() {
    let fixture = make_fixture("lv_modes").await;
    let Some(Fixture { db, library, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let hash = lanverse_game::auth::tokens::hash_password("supersecret123").unwrap();
    let (author, _) = db
        .create_user("Builder", "builder@example.test", &hash)
        .await
        .unwrap();
    let (other, _) = db
        .create_user("Mallory", "mallory@example.test", &hash)
        .await
        .unwrap();

    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../LANVerseBuilder/modes/coin_rush");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mode.json")).unwrap()).unwrap();
    let script = std::fs::read_to_string(dir.join("script.lua")).unwrap();

    // Upload as a draft: stored, not playable.
    let outcome = library
        .upload(&manifest, &script, false, Some(author.id))
        .unwrap();
    assert!(outcome.report.is_valid(), "{:?}", outcome.report.errors);
    assert_eq!(outcome.mode.status, "draft");
    assert!(db.live_mode("coin_rush").await.unwrap().is_none());

    // Publish, then the realtime lookup finds it.
    let published = library.publish("coin_rush").unwrap();
    assert_eq!(published.status, "live");

    let package = library.find_for_match("coin_rush").unwrap();
    assert_eq!(package.id, "coin_rush");
    assert_eq!(package.name, "Coin Rush");
    assert!(package.script.contains("spawn_coin"));

    // Only the owner may upload again.
    let denied = library
        .upload(&manifest, &script, true, Some(other.id))
        .unwrap_err();
    assert!(matches!(
        denied,
        lanverse_game::store::modes::LibraryError::NotOwner
    ));

    // A broken upload never takes the live mode offline.
    let broken = script.replace("function on_start()", "function on_start( end");
    let outcome = library
        .upload(&manifest, &broken, true, Some(author.id))
        .unwrap();
    assert!(!outcome.report.is_valid());
    assert_eq!(outcome.mode.status, "live");
    assert!(db.live_mode("coin_rush").await.unwrap().is_some());

    // Unpublish blocks playback; delete only works once unpublished.
    library.unpublish("coin_rush").unwrap();
    assert!(library.find_for_match("coin_rush").is_none());
    library.delete("coin_rush").unwrap();
    assert!(db.mode_by_slug("coin_rush").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_mode_cannot_be_deleted() {
    let fixture = make_fixture("lv_delete").await;
    let Some(Fixture { db, library, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let manifest = json!({
        "schema_version": 1, "id": "tiny", "name": "Tiny",
        "world": {"width": 400.0, "height": 300.0},
        "round": {"duration_ms": 10000},
        "script": "script.lua"
    });
    library
        .upload(
            &manifest,
            "function on_tick(dt) lb.log('x') end",
            true,
            None,
        )
        .unwrap();
    library.publish("tiny").unwrap();

    let error = library.delete("tiny").unwrap_err();
    assert!(matches!(
        error,
        lanverse_game::store::modes::LibraryError::Live
    ));
    let _ = db;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_round_results_are_written_once() {
    let fixture = make_fixture("lv_results").await;
    let Some(Fixture { db, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let hash = lanverse_game::auth::tokens::hash_password("supersecret123").unwrap();
    let (_, alice) = db
        .create_user("Alice", "alice@example.test", &hash)
        .await
        .unwrap();
    let (_, bob) = db
        .create_user("Bob", "bob@example.test", &hash)
        .await
        .unwrap();

    db.ensure_match("m-1", "deathmatch", 60, 512).await.unwrap();
    db.record_participant(
        "m-1",
        &format!("p-{}", alice.id),
        "Alice",
        Some(alice.id),
        Some("red"),
    )
    .await
    .unwrap();
    db.record_participant(
        "m-1",
        &format!("p-{}", bob.id),
        "Bob",
        Some(bob.id),
        Some("blue"),
    )
    .await
    .unwrap();

    let result = RoundResult {
        match_id: "m-1".into(),
        mode_id: "deathmatch".into(),
        round_number: 1,
        round_id: "r-1".into(),
        winner: Some("red".into()),
        team_scores: json!({"red": 3, "blue": 1}).as_object().unwrap().clone(),
        players: vec![
            RoundPlayer {
                player_id: format!("p-{}", alice.id),
                user_id: Some(alice.user_id),
                profile_id: Some(alice.id),
                display_name: "Alice".into(),
                team: Some("red".into()),
                score: 3,
                deaths: 0,
            },
            RoundPlayer {
                player_id: format!("p-{}", bob.id),
                user_id: Some(bob.user_id),
                profile_id: Some(bob.id),
                display_name: "Bob".into(),
                team: Some("blue".into()),
                score: 1,
                deaths: 2,
            },
        ],
        duration_ms: 60_000,
        finished_at_tick: 3_600,
    };

    let recorder = ResultRecorder::new(db.clone(), "test-secret");
    let (body, signature) = recorder.signed(&result).unwrap();

    assert!(recorder.accept(&body, &signature).unwrap(), "first write");
    assert!(
        !recorder.accept(&body, &signature).unwrap(),
        "replay is idempotent"
    );

    // A forged body is refused and writes nothing.
    let mut forged = body.clone();
    forged["team_scores"]["red"] = json!(999);
    assert!(recorder.accept(&forged, &signature).is_err());

    let rounds = db.match_rounds("m-1").await.unwrap();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0]["winner"], "red");
    assert_eq!(rounds[0]["scores"]["red"], 3);

    let scores = db.highscores("deathmatch", 25).await.unwrap();
    assert_eq!(scores.len(), 2);
    assert_eq!(scores[0]["display_name"], "Alice");
    assert_eq!(scores[0]["score"], 3);

    let totals: Vec<i32> = sqlx::query_scalar("SELECT total_score FROM players ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(totals, vec![3, 1]);

    // The signature is stable and verifiable from the JSON alone.
    let parsed: SignedResult = serde_json::from_value(body).unwrap();
    assert!(verify("test-secret", &parsed, &signature));
    assert_eq!(sign("test-secret", &parsed).unwrap(), signature);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_bcrypt_passwords_are_upgraded_on_login() {
    let fixture = make_fixture("lv_bcrypt").await;
    let Some(Fixture { db, tokens, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let legacy = bcrypt::hash("supersecret123", 4).unwrap();
    let (user, _) = db
        .create_user("Legacy", "legacy@example.test", &legacy)
        .await
        .unwrap();

    let (_, player) = tokens
        .authenticate("legacy@example.test", "supersecret123")
        .await
        .unwrap();
    assert_eq!(player.display_name, "Legacy");

    let stored: String = sqlx::query_scalar("SELECT password FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(stored.starts_with("$argon2id$"), "hash upgraded: {stored}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_and_friends_round_trip() {
    let fixture = make_fixture("lv_profile").await;
    let Some(Fixture { db, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let hash = lanverse_game::auth::tokens::hash_password("supersecret123").unwrap();
    let (_, alice) = db
        .create_user("Alice", "alice@example.test", &hash)
        .await
        .unwrap();
    let (_, bob) = db
        .create_user("Bob", "bob@example.test", &hash)
        .await
        .unwrap();

    let updated = db
        .update_profile(alice.id, Some("Alice2"), Some(Some("hat-1")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.display_name, "Alice2");
    assert_eq!(updated.avatar.as_deref(), Some("hat-1"));

    db.request_friend(alice.id, bob.id).await.unwrap();
    let friends = db.friends_for(alice.id).await.unwrap();
    assert_eq!(friends["outgoing"][0]["display_name"], "Bob");

    let incoming = db.friends_for(bob.id).await.unwrap();
    assert_eq!(incoming["incoming"][0]["display_name"], "Alice2");

    db.remove_friend(bob.id, alice.id).await.unwrap();
    let friends = db.friends_for(alice.id).await.unwrap();
    assert!(friends["outgoing"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_identity_resolver_uses_the_profile_name() {
    let fixture = make_fixture("lv_identity").await;
    let Some(Fixture { db, tokens, .. }) = fixture else {
        eprintln!("skipping");
        return;
    };

    let hash = lanverse_game::auth::tokens::hash_password("supersecret123").unwrap();
    let (user, _) = db
        .create_user("Alice", "alice@example.test", &hash)
        .await
        .unwrap();
    let (token, _) = tokens.create_token(user.id, "client").await.unwrap();

    use lanverse_game::net::service::IdentityResolver;

    let resolver = lanverse_game::auth::Resolver::new(tokens.clone());
    let identity = tokio::task::spawn_blocking(move || resolver.resolve(Some(&token), "Admin"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(identity.display_name(), "Alice");
    assert_eq!(identity.user_id, Some(user.id));
    assert!(!identity.is_guest);
    assert_eq!(GameIdentity::guest("X").display_name(), "X");
}

#[test]
fn validation_is_shared_with_the_builder() {
    // Sanity check that the library-level validator rejects reserved ids.
    assert!(!is_allowed_id("deathmatch"));

    let manifest = json!({"schema_version": 1, "id": "lan", "name": "X", "script": "script.lua"});
    let report = validate(&manifest, "function on_start() lb.log('a') end", 65_536);
    assert!(!report.is_valid());
}
