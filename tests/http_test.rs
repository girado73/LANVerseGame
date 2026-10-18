//! REST API over real HTTP against a real Postgres (skipped when `pg-embed`
//! cannot start a server, e.g. offline without a cached binary).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use lanverse_game::auth::TokenService;
use lanverse_game::db::Db;
use lanverse_game::net::http::{AppParts, build};
use lanverse_game::net::loop_driver::StatsHandle;
use lanverse_game::net::router::InProcessRouter;
use lanverse_game::settings::Settings;
use lanverse_game::store::modes::ModeLibrary;
use pg_embed::pg_enums::PgAuthMethod;
use pg_embed::pg_fetch::PG_V17;
use pg_embed::postgres::{PgEmbed, PgSettings};
use serde_json::{Value, json};

static POSTGRES: OnceLock<Option<String>> = OnceLock::new();

fn base_url(database: &str) -> Option<String> {
    match POSTGRES.get() {
        Some(url) => url.clone(),
        None => {
            let started = std::thread::spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let dir =
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/pg-embed-http");
                    let settings = PgSettings {
                        database_dir: dir,
                        port: 55433,
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
                    let url = pg.full_db_uri("postgres");
                    std::mem::forget(pg);
                    Some(url)
                })
            })
            .join()
            .unwrap_or(None);

            let _ = POSTGRES.set(started);
            POSTGRES.get().cloned().flatten()
        }
    }
    .map(|url| url.replace("/postgres", &format!("/{database}")))
}

async fn database(name: &str) -> Option<Db> {
    let base = base_url("postgres")?;
    let admin = Db::connect(&base, 2).await.ok()?;

    let exists = sqlx::query("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(name)
        .fetch_optional(admin.pool())
        .await
        .ok()?
        .is_some();

    if !exists {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(admin.pool())
            .await
            .ok()?;
    }

    let url = base_url(name)?;
    let db = Db::connect(&url, 4).await.ok()?;
    sqlx::query("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(db.pool())
        .await
        .ok()?;
    db.migrate().await.ok()?;

    Some(db)
}

struct Api {
    base: String,
    db: Db,
}

async fn start(name: &str) -> Option<Api> {
    let db = database(name).await?;
    let mut settings = Settings::from_env();
    settings.result_secret = Some("http-test-secret".to_owned());
    settings.admin_key = Some("admin-test-key".to_owned());

    let tokens = Arc::new(TokenService::new(db.clone()));
    let library = Arc::new(ModeLibrary::new(db.clone(), None, true, 65_536));

    let app = build(AppParts {
        stats: StatsHandle::default(),
        router: Arc::new(InProcessRouter::default()),
        settings: Arc::new(settings),
        control: Arc::new(lanverse_game::api::ControlPlane {
            db: Some(db.clone()),
            redis: None,
            tokens: Some(tokens),
            modes: Some(library),
            results: None,
            secret: Some("http-test-secret".to_owned()),
            admin_key: Some("admin-test-key".to_owned()),
        }),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let address = listener.local_addr().ok()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Some(Api {
        base: format!("http://{address}"),
        db,
    })
}

/// Tiny HTTP client (the crate has no HTTP client dependency).
async fn request(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let without_scheme = url.strip_prefix("http://").unwrap();
    let (authority, path) = without_scheme.split_once('/').unwrap();
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();

    let payload = body.map(|body| body.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nAccept: application/json\r\n"
    );

    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(payload.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").trim();
    let json = serde_json::from_str(body).unwrap_or(Value::Null);

    (status, json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_login_me_and_logout() {
    let Some(api) = start("lv_http_auth").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };

    let (status, body) = request(
        "POST",
        &format!("{}/api/v1/auth/register", api.base),
        None,
        Some(&json!({
            "name": "Alice",
            "email": "alice@example.test",
            "password": "supersecret123",
            "password_confirmation": "supersecret123",
        })),
    )
    .await;

    assert_eq!(status, 201);
    assert_eq!(body["user"]["email"], "alice@example.test");
    assert_eq!(body["player"]["display_name"], "Alice");
    let token = body["token"].as_str().unwrap().to_owned();

    // Duplicate email.
    let (status, body) = request(
        "POST",
        &format!("{}/api/v1/auth/register", api.base),
        None,
        Some(&json!({
            "name": "Alice2",
            "email": "alice@example.test",
            "password": "supersecret123",
            "password_confirmation": "supersecret123",
        })),
    )
    .await;
    assert_eq!(status, 422);
    assert!(body["errors"]["email"].is_array());

    // Login.
    let (status, body) = request(
        "POST",
        &format!("{}/api/v1/auth/login", api.base),
        None,
        Some(&json!({"email": "alice@example.test", "password": "supersecret123", "device_name": "lan"})),
    )
    .await;
    assert_eq!(status, 200);
    let second = body["token"].as_str().unwrap().to_owned();
    assert_ne!(token, second);

    let (status, _) = request(
        "POST",
        &format!("{}/api/v1/auth/login", api.base),
        None,
        Some(&json!({"email": "alice@example.test", "password": "wrong"})),
    )
    .await;
    assert_eq!(status, 422);

    // Authenticated reads.
    let (status, body) = request(
        "GET",
        &format!("{}/api/v1/auth/me", api.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["player"]["display_name"], "Alice");

    let (status, body) = request(
        "GET",
        &format!("{}/api/v1/profile", api.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["player"]["level"], 1);

    let (status, body) = request(
        "PATCH",
        &format!("{}/api/v1/profile", api.base),
        Some(&token),
        Some(&json!({"display_name": "Alice2"})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["player"]["display_name"], "Alice2");

    let (status, _) = request(
        "PATCH",
        &format!("{}/api/v1/profile", api.base),
        Some(&token),
        Some(&json!({"display_name": "Evil\nName"})),
    )
    .await;
    assert_eq!(status, 422);

    // Without a token everything is 401.
    let (status, _) = request("GET", &format!("{}/api/v1/profile", api.base), None, None).await;
    assert_eq!(status, 401);

    // Logout revokes only that token.
    let (status, _) = request(
        "POST",
        &format!("{}/api/v1/auth/logout", api.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, 200);

    let (status, _) = request(
        "GET",
        &format!("{}/api/v1/auth/me", api.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, 401);

    let (status, _) = request(
        "GET",
        &format!("{}/api/v1/auth/me", api.base),
        Some(&second),
        None,
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modes_are_public_and_uploads_need_the_owner() {
    let Some(api) = start("lv_http_modes").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };

    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../LANVerseBuilder/modes/coin_rush");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mode.json")).unwrap()).unwrap();
    let script = std::fs::read_to_string(dir.join("script.lua")).unwrap();

    let tokens = TokenService::new(api.db.clone());
    let (_, alice) = api
        .db
        .create_user("Builder", "builder@example.test", "x")
        .await
        .unwrap();
    let (alice_token, _) = tokens.create_token(alice.user_id, "builder").await.unwrap();
    let (_, mallory) = api
        .db
        .create_user("Mallory", "mallory@example.test", "x")
        .await
        .unwrap();
    let (mallory_token, _) = tokens
        .create_token(mallory.user_id, "builder")
        .await
        .unwrap();

    // Upload without a token.
    let (status, _) = request(
        "POST",
        &format!("{}/api/v1/modes", api.base),
        None,
        Some(&json!({"manifest": manifest, "script": script})),
    )
    .await;
    assert_eq!(status, 401);

    // A valid upload is published.
    let (status, body) = request(
        "POST",
        &format!("{}/api/v1/modes", api.base),
        Some(&alice_token),
        Some(&json!({"manifest": manifest, "script": script, "publish": true})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["mode"]["status"], "live");
    assert_eq!(body["validation"]["valid"], true);

    // The list and the package are public.
    let (status, body) = request("GET", &format!("{}/api/v1/modes", api.base), None, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["modes"][0]["id"], "coin_rush");

    let (status, body) = request(
        "GET",
        &format!("{}/api/v1/modes/coin_rush", api.base),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body["mode"]["script"]
            .as_str()
            .unwrap()
            .contains("on_start")
    );

    // Another account cannot take it over.
    let (status, body) = request(
        "POST",
        &format!("{}/api/v1/modes", api.base),
        Some(&mallory_token),
        Some(&json!({"manifest": manifest, "script": script})),
    )
    .await;
    assert_eq!(status, 422);
    assert!(
        body["errors"]["manifest.id"][0]
            .as_str()
            .unwrap()
            .contains("another account")
    );

    // A broken upload is stored as a draft and reported, keeping the live one.
    let broken = json!({"manifest": manifest, "script": "function on_start( end"});
    let (status, body) = request(
        "POST",
        &format!("{}/api/v1/modes", api.base),
        Some(&alice_token),
        Some(&broken),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(body["validation"]["valid"], false);

    let (status, body) = request(
        "GET",
        &format!("{}/api/v1/modes/coin_rush", api.base),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["mode"]["status"], "live");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn highscores_are_grouped_by_player() {
    let Some(api) = start("lv_http_scores").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };

    let tokens = TokenService::new(api.db.clone());
    let (_, alice) = api
        .db
        .create_user("Alice", "alice@example.test", "x")
        .await
        .unwrap();
    let (token, _) = tokens.create_token(alice.user_id, "client").await.unwrap();

    for score in [3, 9, 5] {
        sqlx::query(
            "INSERT INTO highscores (player_id, game_mode, score, created_at, updated_at)
             VALUES ($1, 'deathmatch', $2, now(), now())",
        )
        .bind(alice.id)
        .bind(score)
        .execute(api.db.pool())
        .await
        .unwrap();
    }

    let (status, body) = request(
        "GET",
        &format!(
            "{}/api/v1/highscores?game_mode=deathmatch&limit=10",
            api.base
        ),
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["scores"][0]["score"], 9);
    assert_eq!(body["scores"][0]["entries"], 3);
    assert_eq!(body["scores"][0]["display_name"], "Alice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_reports_dependencies() {
    let Some(api) = start("lv_http_health").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };

    let (status, body) = request("GET", &format!("{}/api/v1/health", api.base), None, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["services"]["database"], "up");
    assert_eq!(body["protocol_version"], 1);
}
