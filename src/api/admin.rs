//! The admin page: modes, uploads, tokens, health.
//!
//! Server-rendered HTML, no build step, no JavaScript beyond a `<details>`
//! toggle. It listens on `LANBASH_ADMIN_BIND` (127.0.0.1:8081 by default) and is
//! never routed from the LAN port.
//!
//! Security model: the admin key (`LANBASH_ADMIN_KEY`) is exchanged once for a
//! session cookie (`HttpOnly`, `SameSite=Strict`) and every state-changing form
//! carries a CSRF token bound to that session. Without a configured key the
//! page only reports health and refuses every action.

use axum::Json;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::net::state::AppState;

const SESSION_COOKIE: &str = "lanverse_admin";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoginForm {
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UploadForm {
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub manifest: String,
    #[serde(default)]
    pub script: String,
    #[serde(default)]
    pub publish: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ActionForm {
    #[serde(default)]
    pub csrf: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OwnerForm {
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub owner: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TokenForm {
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub revoke: Option<String>,
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// The admin session token derived from the admin key (no extra state needed).
pub fn session_for(key: &str) -> String {
    crate::auth::tokens::hash_token(&format!("lanverse-admin|{key}"))
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;

    cookie.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;

        if name == SESSION_COOKIE {
            Some(value.to_owned())
        } else {
            None
        }
    })
}

fn is_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(key) = state
        .control
        .admin_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return false;
    };

    cookie_token(headers).is_some_and(|token| token == session_for(key))
}

/// CSRF token for forms: bound to the session, so a foreign page cannot use it.
fn csrf_for(session: &str) -> String {
    crate::auth::tokens::hash_token(&format!("csrf|{session}"))[..32].to_owned()
}

fn csrf_ok(state: &AppState, _headers: &HeaderMap, given: &str) -> bool {
    let Some(key) = state.control.admin_key.as_deref() else {
        return false;
    };
    let session = session_for(key);
    let expected = csrf_for(&session);

    given.len() == expected.len()
        && subtle::ConstantTimeEq::ct_eq(expected.as_bytes(), given.as_bytes()).into()
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let stats = state.stats.get();
    let authenticated = is_authenticated(&state, &headers);

    let mut body = format!(
        "<section class=\"stats\">\
           <div><strong>{}</strong><span>worlds</span></div>\
           <div><strong>{}</strong><span>players</span></div>\
           <div><strong>{}</strong><span>tick rate</span></div>\
           <div><strong>{}</strong><span>protocol</span></div>\
         </section>",
        stats.active_worlds,
        stats.connected_players,
        state.settings.simulation.tick_rate,
        state.settings.realtime.protocol_version
    );

    if authenticated {
        let session = state
            .control
            .admin_key
            .as_deref()
            .map(session_for)
            .unwrap_or_default();
        let csrf = csrf_for(&session);
        body.push_str(&modes_section(&state, &csrf));
        body.push_str(&upload_section(&csrf));
        body.push_str(&tokens_section(&csrf));
        body.push_str(&health_section(&state, &csrf));
        body.push_str(
            "<form method=\"post\" action=\"/admin/logout\" class=\"logout\">\
               <button type=\"submit\">Log out</button>\
             </form>",
        );
    } else if state
        .control
        .admin_key
        .as_deref()
        .is_some_and(|key| !key.is_empty())
    {
        body.push_str(
            "<form method=\"post\" action=\"/admin/login\" class=\"login\">\
               <label>Admin key <input type=\"password\" name=\"key\" autofocus required></label>\
               <button type=\"submit\">Sign in</button>\
             </form>",
        );
    } else {
        body.push_str("<p class=\"warn\">No admin key is configured (LANBASH_ADMIN_KEY). Administration is disabled.</p>");
    }

    page("LAN Verse admin", &body).into_response()
}

fn modes_section(state: &AppState, csrf: &str) -> String {
    let Some(library) = state.control.modes.as_ref() else {
        return "<p class=\"warn\">Modes are unavailable: no database.</p>".to_owned();
    };

    let modes = match library.list(true) {
        Ok(modes) => modes,
        Err(error) => {
            return format!(
                "<p class=\"warn\">Could not list modes: {}</p>",
                escape(&error)
            );
        }
    };

    let mut rows = String::new();

    for mode in &modes {
        let id = mode["id"].as_str().unwrap_or("");
        let status = mode["status"].as_str().unwrap_or("draft");
        let publish = if status == "live" {
            button(&format!("/admin/modes/{id}/unpublish"), csrf, "Unpublish")
        } else {
            button(&format!("/admin/modes/{id}/publish"), csrf, "Publish")
        };
        let delete = if status == "live" {
            String::new()
        } else {
            button(&format!("/admin/modes/{id}/delete"), csrf, "Delete")
        };

        let owner = mode["owner"].as_str().unwrap_or("");

        rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td><span class=\"badge {}\">{}</span></td>\
             <td>{}</td><td><code class=\"checksum\">{}</code></td><td>{}{}{}</td></tr>",
            escape(id),
            escape(mode["name"].as_str().unwrap_or("")),
            status,
            status,
            escape(owner),
            escape(
                &mode["checksum"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(12)
                    .collect::<String>()
            ),
            publish,
            owner_form(id, csrf),
            delete
        ));
    }

    if rows.is_empty() {
        rows.push_str("<tr><td colspan=\"5\">No modes uploaded yet.</td></tr>");
    }

    format!(
        "<section><h2>Modes</h2><table><thead><tr><th>id</th><th>name</th><th>status</th>\
         <th>owner</th><th>checksum</th><th>actions</th></tr></thead><tbody>{rows}</tbody></table></section>"
    )
}

fn owner_form(id: &str, csrf: &str) -> String {
    format!(
        "<details><summary>owner</summary>\
           <form method=\"post\" action=\"/admin/modes/{id}/owner\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
             <input type=\"email\" name=\"owner\" placeholder=\"new owner email\" required>\
             <button type=\"submit\">Transfer</button>\
           </form>\
         </details>"
    )
}

fn upload_section(csrf: &str) -> String {
    format!(
        "<section><h2>Upload a mode</h2>\
         <form method=\"post\" action=\"/admin/modes/upload\">\
           <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
           <label>mode.json<textarea name=\"manifest\" rows=\"10\" spellcheck=\"false\" placeholder=\"{{ \\\"id\\\": \\\"my_mode\\\", … }}\"></textarea></label>\
           <label>script.lua<textarea name=\"script\" rows=\"14\" spellcheck=\"false\"></textarea></label>\
           <label class=\"inline\"><input type=\"checkbox\" name=\"publish\" value=\"1\" checked> publish when valid</label>\
           <button type=\"submit\">Validate &amp; upload</button>\
         </form></section>"
    )
}

fn tokens_section(csrf: &str) -> String {
    format!(
        "<section><h2>Builder tokens</h2>\
         <form method=\"post\" action=\"/admin/tokens\">\
           <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
           <label>Email <input type=\"email\" name=\"email\" required></label>\
           <label>Display name (optional) <input type=\"text\" name=\"name\"></label>\
           <button type=\"submit\">Issue token</button>\
           <button type=\"submit\" name=\"revoke\" value=\"1\" class=\"danger\">Revoke all</button>\
         </form></section>"
    )
}

fn health_section(state: &AppState, csrf: &str) -> String {
    let database = state.control.db.is_some();
    let redis = state.control.redis.is_some();
    let modes = state
        .control
        .modes
        .as_ref()
        .and_then(|library| library.list(true).ok())
        .map(|modes| modes.len())
        .unwrap_or(0);
    let live = state
        .control
        .modes
        .as_ref()
        .and_then(|library| library.list(false).ok())
        .map(|modes| modes.len())
        .unwrap_or(0);

    let _ = csrf;

    format!(
        "<section><h2>Health</h2><table><tbody>\
           <tr><td>database</td><td>{}</td></tr>\
           <tr><td>redis</td><td>{}</td></tr>\
           <tr><td>modes (live / all)</td><td>{} / {}</td></tr>\
           <tr><td>tick rate</td><td>{} Hz</td></tr>\
         </tbody></table></section>",
        if database { "configured" } else { "missing" },
        if redis { "configured" } else { "missing" },
        live,
        modes,
        state.settings.simulation.tick_rate
    )
}

fn button(action: &str, csrf: &str, label: &str) -> String {
    format!(
        "<form method=\"post\" action=\"{action}\" class=\"inline\">\
           <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
           <button type=\"submit\">{label}</button>\
         </form>"
    )
}

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>{title}</title><style>{}</style></head><body><header><h1>{title}</h1></header>\
         <main>{body}</main><footer>LAN Verse · <a href=\"/api/v1/health\">health</a></footer></body></html>",
        CSS
    ))
}

const CSS: &str = "
body{margin:0;font:15px/1.5 system-ui,sans-serif;background:#101418;color:#e6edf3}
header{padding:1rem 2rem;background:#171d23;border-bottom:1px solid #2a323c}
h1{margin:0;font-size:1.25rem}h2{font-size:1rem;margin:1.5rem 0 .75rem}
main{padding:1rem 2rem;max-width:1100px}
section{margin-bottom:2rem}
.stats{display:flex;gap:1rem;flex-wrap:wrap}
.stats div{background:#171d23;border:1px solid #2a323c;border-radius:8px;padding:.75rem 1rem;min-width:9rem}
.stats strong{font-size:1.5rem;display:block}.stats span{color:#8b98a5;font-size:.85rem}
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:.5rem;border-bottom:1px solid #2a323c}
th{color:#8b98a5;font-weight:500;font-size:.85rem}
.badge{font-size:.75rem;padding:.1rem .5rem;border-radius:999px;background:#39424d}
.badge.live{background:#1f6f43}
form.inline{display:inline}button{background:#2f6feb;border:0;color:#fff;padding:.45rem .9rem;border-radius:6px;cursor:pointer;font:inherit}
button.danger{background:#b62324}button:hover{filter:brightness(1.1)}
label{display:block;margin-bottom:.75rem}input,textarea{width:100%;box-sizing:border-box;background:#0b0f13;color:#e6edf3;border:1px solid #2a323c;border-radius:6px;padding:.5rem;font-family:ui-monospace,monospace}
label.inline{display:flex;align-items:center;gap:.5rem;width:auto}label.inline input{width:auto}
textarea{min-height:6rem}.checksum{color:#8b98a5}.warn{color:#e3b341}
.login{max-width:22rem}footer{padding:1rem 2rem;color:#8b98a5;font-size:.85rem}
a{color:#6ea8fe}
";

fn escape(text: &str) -> String {
    // Attributes and text alike: quote characters must never end an attribute.
    html_escape::encode_safe(text).to_string()
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

pub async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let Some(key) = state
        .control
        .admin_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no admin key configured").into_response();
    };

    let matches: bool =
        subtle::ConstantTimeEq::ct_eq(key.as_bytes(), form.key.trim().as_bytes()).into();

    if !matches {
        return (
            StatusCode::UNAUTHORIZED,
            Html(page("Denied", "<p class=\"warn\">Wrong admin key.</p>")),
        )
            .into_response();
    }

    let cookie = format!(
        "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
        session_for(key)
    );

    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/admin".to_owned()),
        ],
        (),
    )
        .into_response()
}

pub async fn logout() -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                format!("{SESSION_COOKIE}=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/"),
            ),
            (header::LOCATION, "/admin".to_owned()),
        ],
        (),
    )
        .into_response()
}

pub async fn modes(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&state, &headers) {
        return redirect_login();
    }

    index(State(state), headers).await
}

pub async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UploadForm>,
) -> Response {
    if !is_authenticated(&state, &headers) || !csrf_ok(&state, &headers, &form.csrf) {
        return redirect_login();
    }

    let Some(library) = state.control.modes.as_ref() else {
        return Html(page("Upload", "<p class=\"warn\">No database.</p>")).into_response();
    };

    let manifest: Value = match serde_json::from_str(&form.manifest) {
        Ok(value) => value,
        Err(error) => {
            return Html(page(
                "Upload",
                &format!(
                    "<p class=\"warn\">mode.json is not valid JSON: {}</p>",
                    escape(&error.to_string())
                ),
            ))
            .into_response();
        }
    };

    let publish = form.publish.is_some();
    // Uploads made here belong to the first account (the LAN host).
    let author = state
        .control
        .db
        .as_ref()
        .and_then(|db| db.block(db.user_by_id(1)).ok().flatten())
        .map(|user| user.id);

    match library.upload(&manifest, &form.script, publish, author) {
        Ok(outcome) => {
            let report = outcome.report;
            let mut list = String::new();

            for error in &report.errors {
                list.push_str(&format!("<li class=\"warn\">{}</li>", escape(error)));
            }
            for warning in &report.warnings {
                list.push_str(&format!("<li>{}</li>", escape(warning)));
            }

            let status = if report.is_valid() {
                "valid"
            } else {
                "rejected"
            };
            Html(page(
                "Upload",
                &format!(
                    "<p>{} — mode <code>{}</code> is now <strong>{}</strong>.</p><ul>{list}</ul>\
                     <p><a href=\"/admin\">Back to the mode list</a></p>",
                    status,
                    escape(&outcome.mode.slug),
                    escape(&outcome.mode.status)
                ),
            ))
            .into_response()
        }
        Err(error) => Html(page(
            "Upload",
            &format!("<p class=\"warn\">{}</p>", escape(&error.to_string())),
        ))
        .into_response(),
    }
}

pub async fn publish(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    moderate(&state, &headers, &form.csrf, &id, AdminAction::Publish)
}

pub async fn unpublish(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    moderate(&state, &headers, &form.csrf, &id, AdminAction::Unpublish)
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ActionForm>,
) -> Response {
    moderate(&state, &headers, &form.csrf, &id, AdminAction::Delete)
}

enum AdminAction {
    Publish,
    Unpublish,
    Delete,
}

fn moderate(
    state: &AppState,
    headers: &HeaderMap,
    csrf: &str,
    id: &str,
    action: AdminAction,
) -> Response {
    if !is_authenticated(state, headers) || !csrf_ok(state, headers, csrf) {
        return redirect_login();
    }

    let Some(library) = state.control.modes.as_ref() else {
        return Html(page("Modes", "<p class=\"warn\">No database.</p>")).into_response();
    };

    match action {
        AdminAction::Publish => {
            let _ = library.publish(id);
        }
        AdminAction::Unpublish => {
            let _ = library.unpublish(id);
        }
        AdminAction::Delete => {
            let _ = library.delete(id);
        }
    }

    Redirect::to("/admin").into_response()
}

/// Transfers a mode to another account (the old `game:mode:owner`).
pub async fn owner(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<OwnerForm>,
) -> Response {
    if !is_authenticated(&state, &headers) || !csrf_ok(&state, &headers, &form.csrf) {
        return redirect_login();
    }

    let Some(db) = state.control.db.as_ref() else {
        return Html(page("Ownership", "<p class=\"warn\">No database.</p>")).into_response();
    };

    let outcome = db.block(async {
        let Some(user) = db.user_by_email(form.owner.trim()).await? else {
            return Ok::<_, anyhow::Error>(format!("No account with email {}.", form.owner.trim()));
        };

        if db.transfer_mode_owner(&id, user.id).await? {
            Ok(format!("Mode {id} now belongs to {}.", user.email))
        } else {
            Ok(format!("Mode {id} does not exist."))
        }
    });

    match outcome {
        Ok(message) => Html(page(
            "Ownership",
            &format!(
                "<p>{}</p><p><a href=\"/admin\">Back</a></p>",
                escape(&message)
            ),
        ))
        .into_response(),
        Err(error) => Html(page(
            "Ownership",
            &format!("<p class=\"warn\">{}</p>", escape(&error.to_string())),
        ))
        .into_response(),
    }
}

pub async fn tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    if !is_authenticated(&state, &headers) || !csrf_ok(&state, &headers, &form.csrf) {
        return redirect_login();
    }

    let Some(tokens) = state.control.tokens.as_ref() else {
        return Html(page("Tokens", "<p class=\"warn\">No database.</p>")).into_response();
    };

    if form.email.trim().is_empty() {
        return Html(page(
            "Tokens",
            "<p class=\"warn\">An email address is required.</p>",
        ))
        .into_response();
    }

    if form.revoke.is_some() {
        let Some(db) = state.control.db.as_ref() else {
            return Html(page("Tokens", "<p class=\"warn\">No database.</p>")).into_response();
        };

        let outcome = db.block(async {
            match db.user_by_email(form.email.trim()).await? {
                Some(user) => db.revoke_user_tokens(user.id).await,
                None => Ok(0),
            }
        });

        return match outcome {
            Ok(count) => Html(page(
                "Tokens",
                &format!(
                    "<p>Revoked {count} token(s) for {}.</p><p><a href=\"/admin\">Back</a></p>",
                    escape(form.email.trim())
                ),
            ))
            .into_response(),
            Err(error) => Html(page(
                "Tokens",
                &format!("<p class=\"warn\">{}</p>", escape(&error.to_string())),
            ))
            .into_response(),
        };
    }

    let name = (!form.name.trim().is_empty()).then(|| form.name.trim().to_owned());

    let issued = tokens.block(tokens.issue_for_email(form.email.trim(), name.as_deref(), "admin"));

    match issued {
        Ok((token, _, display_name)) => Html(page(
            "Token issued",
            &format!(
                "<p>Token for <strong>{}</strong> ({}) — copy it now, it is not stored in clear text:</p>\
                 <pre><code>{}</code></pre><p><a href=\"/admin\">Back</a></p>",
                escape(form.email.trim()),
                escape(&display_name),
                escape(&token)
            ),
        ))
        .into_response(),
        Err(error) => Html(page("Tokens", &format!("<p class=\"warn\">{}</p>", escape(&error.to_string())))).into_response(),
    }
}

fn redirect_login() -> Response {
    Redirect::to("/admin").into_response()
}

// ---------------------------------------------------------------------------
// Internal result endpoint (same-process callers use `ResultRecorder` directly)
// ---------------------------------------------------------------------------

/// `POST /internal/rounds` with `X-LanVerse-Signature`.
///
/// Served on the admin listener only (never on the LAN port) and verified by
/// HMAC; the body is the canonical `SignedResult` JSON.
pub async fn internal_round(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(recorder) = state.control.results.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "Results are not configured." })),
        )
            .into_response();
    };

    let signature = headers
        .get(crate::store::results::SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Body is not JSON." })),
        )
            .into_response();
    };

    match recorder.accept(&value, signature) {
        Ok(true) => (StatusCode::CREATED, Json(json!({ "recorded": true }))).into_response(),
        Ok(false) => (
            StatusCode::OK,
            Json(json!({ "recorded": false, "reason": "duplicate" })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": error.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_is_escaped() {
        assert_eq!(escape("<script>&\""), "&lt;script&gt;&amp;&quot;");
        assert!(!escape("<x>").contains('<'));
    }

    #[test]
    fn sessions_are_derived_and_stable() {
        assert_eq!(session_for("k"), session_for("k"));
        assert_ne!(session_for("k"), session_for("other"));
        assert_eq!(csrf_for(&session_for("k")).len(), 32);
    }

    #[test]
    fn the_cookie_is_read_back() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "a=1; lanverse_admin=xyz; b=2".parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some("xyz"));

        headers.insert(header::COOKIE, "a=1".parse().unwrap());
        assert!(cookie_token(&headers).is_none());
    }

    #[test]
    fn the_page_has_a_strict_content_policy() {
        // No third-party assets: everything is inline.
        assert!(CSS.contains("system-ui"));
        assert!(!page("x", "y").0.contains("http://"));
    }
}
