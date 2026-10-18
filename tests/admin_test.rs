//! Admin page over HTTP: login, upload, publish, tokens, CSRF, signature.
//!
//! Uses the real `admin` router against a real Postgres (skipped when the
//! embedded server cannot start).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use lanverse_game::auth::TokenService;
use lanverse_game::db::Db;
use lanverse_game::net::http::{AppParts, admin};
use lanverse_game::net::loop_driver::StatsHandle;
use lanverse_game::net::router::InProcessRouter;
use lanverse_game::settings::Settings;
use lanverse_game::store::modes::ModeLibrary;
use lanverse_game::store::results::ResultRecorder;
use pg_embed::pg_enums::PgAuthMethod;
use pg_embed::pg_fetch::PG_V17;
use pg_embed::postgres::{PgEmbed, PgSettings};
use serde_json::{Value, json};

static POSTGRES: OnceLock<Option<String>> = OnceLock::new();

fn postgres(database: &str) -> Option<String> {
    let base = match POSTGRES.get() {
        Some(url) => url.clone(),
        None => {
            let started = std::thread::spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let settings = PgSettings {
                        database_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("target/pg-embed-admin"),
                        port: 55434,
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
    }?;

    Some(base.replace("/postgres", &format!("/{database}")))
}

struct Admin {
    base: String,
    db: Db,
}

async fn start(name: &str) -> Option<Admin> {
    let admin_base = postgres("postgres")?;
    let admin_db = Db::connect(&admin_base, 2).await.ok()?;
    let exists = sqlx::query("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(name)
        .fetch_optional(admin_db.pool())
        .await
        .ok()?
        .is_some();

    if !exists {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(admin_db.pool())
            .await
            .ok()?;
    }

    let db = Db::connect(&postgres(name)?, 4).await.ok()?;
    sqlx::query("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(db.pool())
        .await
        .ok()?;
    db.migrate().await.ok()?;

    let mut settings = Settings::from_env();
    settings.admin_key = Some("admin-secret".to_owned());
    settings.result_secret = Some("result-secret".to_owned());

    let recorder = Arc::new(ResultRecorder::new(db.clone(), "result-secret"));
    let app = admin(AppParts {
        stats: StatsHandle::default(),
        router: Arc::new(InProcessRouter::default()),
        settings: Arc::new(settings),
        control: Arc::new(lanverse_game::api::ControlPlane {
            db: Some(db.clone()),
            redis: None,
            tokens: Some(Arc::new(TokenService::new(db.clone()))),
            modes: Some(Arc::new(ModeLibrary::new(db.clone(), None, true, 65_536))),
            results: Some(recorder),
            secret: Some("result-secret".to_owned()),
            admin_key: Some("admin-secret".to_owned()),
        }),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let address = listener.local_addr().ok()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Some(Admin {
        base: format!("http://{address}"),
        db,
    })
}

/// (status, body, `Set-Cookie`)
async fn call(
    method: &str,
    url: &str,
    body: Option<&str>,
    cookie: Option<&str>,
) -> (u16, String, Option<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let without_scheme = url.strip_prefix("http://").unwrap();
    let (authority, path) = without_scheme.split_once('/').unwrap();
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();

    let payload = body.unwrap_or("");
    let mut head =
        format!("{method} /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");

    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    if let Some(cookie) = cookie {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(payload.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let cookie = head.lines().find_map(|line| {
        let value = line
            .strip_prefix("set-cookie: ")
            .or_else(|| line.strip_prefix("Set-Cookie: "))?;
        value.split(';').next().map(str::to_owned)
    });

    (status, body.to_owned(), cookie)
}

fn form_escape(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('+', "%2B")
}

async fn login(admin: &Admin) -> String {
    let (status, _, cookie) = call(
        "POST",
        &format!("{}/admin/login", admin.base),
        Some(&format!("key={}", form_escape("admin-secret"))),
        None,
    )
    .await;
    assert_eq!(status, 303);
    cookie.expect("session cookie")
}

fn csfr_from(html: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker).expect("csrf field") + marker.len();
    html[start..].split('"').next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admin_page_requires_the_key_and_guards_csrf() {
    let Some(admin) = start("lv_admin_auth").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };

    // Health is visible without a key, the mode list is not.
    let (status, body, _) = call("GET", &format!("{}/admin", admin.base), None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("Admin key"));
    assert!(!body.contains("Upload a mode"));

    // A wrong key is refused, without a cookie.
    let (status, _, cookie) = call(
        "POST",
        &format!("{}/admin/login", admin.base),
        Some("key=nope"),
        None,
    )
    .await;
    assert_eq!(status, 401);
    assert!(cookie.is_none());

    // The right key issues a session.
    let session = login(&admin).await;

    let (status, body, _) = call(
        "GET",
        &format!("{}/admin", admin.base),
        None,
        Some(&session),
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("Upload a mode"));

    // Actions without a CSRF token are bounced to the login page.
    let (status, _, _) = call(
        "POST",
        &format!("{}/admin/modes/coin_rush/publish", admin.base),
        Some("csrf=wrong"),
        Some(&session),
    )
    .await;
    assert_eq!(status, 303);

    // The CSRF token is bound to the session.
    let csrf = csfr_from(&body);
    let (status, _, _) = call(
        "POST",
        &format!("{}/admin/modes/coin_rush/publish", admin.base),
        Some(&format!("csrf={csrf}")),
        Some(&session),
    )
    .await;
    assert_eq!(status, 303);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploading_through_the_admin_page_publishes_a_mode() {
    let Some(admin) = start("lv_admin_upload").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };
    let session = login(&admin).await;
    let (_, body, _) = call(
        "GET",
        &format!("{}/admin", admin.base),
        None,
        Some(&session),
    )
    .await;
    let csrf = csfr_from(&body);

    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../LANVerseBuilder/modes/coin_rush");
    let manifest = std::fs::read_to_string(dir.join("mode.json")).unwrap();
    let script = std::fs::read_to_string(dir.join("script.lua")).unwrap();

    let payload = format!(
        "csrf={csrf}&manifest={}&script={}&publish=1",
        form_escape(&manifest),
        form_escape(&script)
    );

    let (status, body, _) = call(
        "POST",
        &format!("{}/admin/modes/upload", admin.base),
        Some(&payload),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("valid"), "{body}");
    assert!(body.contains("<strong>live</strong>"), "{body}");

    let mode = admin
        .db
        .live_mode("coin_rush")
        .await
        .unwrap()
        .expect("published");
    assert!(mode.script.contains("spawn_coin"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_uploads_report_their_errors() {
    let Some(admin) = start("lv_admin_invalid").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };
    let session = login(&admin).await;
    let (_, body, _) = call(
        "GET",
        &format!("{}/admin", admin.base),
        None,
        Some(&session),
    )
    .await;
    let csrf = csfr_from(&body);

    let payload = format!(
        "csrf={csrf}&manifest={}&script={}",
        form_escape(r#"{"schema_version":1,"id":"broken","name":"Broken","script":"script.lua"}"#),
        form_escape("function on_start( end")
    );

    let (status, body, _) = call(
        "POST",
        &format!("{}/admin/modes/upload", admin.base),
        Some(&payload),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("rejected"), "{body}");
    assert!(body.contains("Lua load error"), "{body}");
    assert!(admin.db.live_mode("broken").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_can_be_issued_and_revoked_from_the_page() {
    let Some(admin) = start("lv_admin_tokens").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };
    let session = login(&admin).await;
    let (_, body, _) = call(
        "GET",
        &format!("{}/admin", admin.base),
        None,
        Some(&session),
    )
    .await;
    let csrf = csfr_from(&body);

    let (status, body, _) = call(
        "POST",
        &format!("{}/admin/tokens", admin.base),
        Some(&format!("csrf={csrf}&email=builder%40example.test")),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200);

    // The token is shown once in a <pre> block.
    let start = body.find("<pre><code>").expect("token block") + "<pre><code>".len();
    let token = body[start..].split('<').next().unwrap().to_owned();
    assert!(token.contains('|'), "sanctum format: {token}");

    let tokens = TokenService::new(admin.db.clone());
    assert!(tokens.resolve_token(&token).await.is_ok());

    let (status, _, _) = call(
        "POST",
        &format!("{}/admin/tokens", admin.base),
        Some(&format!(
            "csrf={csrf}&email=builder%40example.test&revoke=1"
        )),
        Some(&session),
    )
    .await;
    assert_eq!(status, 200);
    assert!(tokens.resolve_token(&token).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_results_are_accepted_and_forgeries_refused() {
    let Some(admin) = start("lv_admin_results").await else {
        eprintln!("skipping: embedded Postgres unavailable");
        return;
    };

    let result = lanverse_game::net::service::RoundResult {
        match_id: "m-admin".into(),
        mode_id: "deathmatch".into(),
        round_number: 1,
        round_id: "r-1".into(),
        winner: Some("red".into()),
        team_scores: json!({"red": 2}).as_object().unwrap().clone(),
        players: vec![],
        duration_ms: 60_000,
        finished_at_tick: 3_600,
    };

    admin
        .db
        .ensure_match("m-admin", "deathmatch", 60, 512)
        .await
        .unwrap();
    let recorder = ResultRecorder::new(admin.db.clone(), "result-secret");
    let (_, signature) = recorder.signed(&result).unwrap();

    let body =
        serde_json::to_string(&lanverse_game::store::results::SignedResult::from(&result)).unwrap();

    let (status, _, _) = call(
        "POST",
        &format!("{}/internal/rounds", admin.base),
        Some(&body),
        None,
    )
    .await;
    assert_eq!(status, 422, "unsigned results are refused");

    // Unsigned is not possible through this client (custom header below).
    let response = signed_post(&admin.base, &body, &signature).await;
    assert_eq!(response, 201);

    let replay = signed_post(&admin.base, &body, &signature).await;
    assert_eq!(replay, 200, "idempotent replay");

    let forged = signed_post(
        &admin.base,
        &body.replace("\"red\":2", "\"red\":99"),
        &signature,
    )
    .await;
    assert_eq!(forged, 422, "tampered body is refused");

    assert_eq!(admin.db.match_rounds("m-admin").await.unwrap().len(), 1);
}

async fn signed_post(base: &str, body: &str, signature: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let authority = base.strip_prefix("http://").unwrap();
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    let head = format!(
        "POST /internal/rounds HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nX-LanVerse-Signature: {signature}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );

    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

#[allow(dead_code)]
fn unused(_: Value) {}
