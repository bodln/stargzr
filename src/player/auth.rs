//! Small account system: a local SQLite file for usernames and password
//! hashes, PBKDF2 for the hashing, and hand rolled HS256 JWTs for the login
//! token. Kept deliberately plain, no ORM, no connection pool, no crypto
//! framework, just enough to know who is typing in the chat.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use rand::RngCore;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::player::PeerAddr;

use super::types::SharedState;

type HmacSha256 = Hmac<Sha256>;

/// How long a login stays good for.
const TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;

/// Name of the cookie the login token rides in. HttpOnly, so script on the page
/// can't read it or have it stolen by an injection, and the browser sends it
/// back on every request under /stargzr on its own.
const AUTH_COOKIE: &str = "stargzr_auth";

/// PBKDF2 rounds for new hashes. 600k is the current OWASP figure for
/// PBKDF2-HMAC-SHA256. A debug build uses far fewer so local logins don't crawl,
/// the round count is stored inside each hash so old rows still verify either way.
#[cfg(not(debug_assertions))]
const PBKDF2_ROUNDS: u32 = 600_000;
#[cfg(debug_assertions)]
const PBKDF2_ROUNDS: u32 = 50_000;

const MIN_USERNAME_LEN: usize = 3;
const MAX_USERNAME_LEN: usize = 20;
const MIN_PASSWORD_LEN: usize = 6;
const MAX_PASSWORD_LEN: usize = 128;

// ─── Database ────────────────────────────────────────────────────────────────

/// The accounts database. One SQLite file behind a Mutex. Every call here is a
/// sub millisecond local read or write, so holding the lock across one is fine
/// and it saves pulling in a pool.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// Opens the database at `path`, creating the file and the users table if
    /// they are not there yet.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS users (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
                 password_hash TEXT NOT NULL,
                 created_at    INTEGER NOT NULL
             );",
        )?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    fn insert_user(&self, username: &str, password_hash: &str) -> Result<(), AuthError> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![username, password_hash, now_secs() as i64],
        )
        .map_err(|e| match e {
            // The UNIQUE index is COLLATE NOCASE, so this also catches a name
            // that only differs by case from one already taken.
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                AuthError::UsernameTaken
            }
            other => AuthError::Db(other.to_string()),
        })?;
        Ok(())
    }

    /// Looks a user up case insensitively and returns their stored name (with
    /// its original casing) and password hash.
    fn lookup(&self, username: &str) -> Result<Option<(String, String)>, AuthError> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT username, password_hash FROM users WHERE username = ?1 COLLATE NOCASE",
            [username],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(AuthError::Db(other.to_string())),
        })
    }
}

// ─── Password hashing ────────────────────────────────────────────────────────

/// PBKDF2-HMAC-SHA256 with a fresh 16 byte salt, packed into one string as
/// `pbkdf2$<rounds>$<salt_b64>$<hash_b64>` so a row carries everything a later
/// verify needs.
fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);

    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, PBKDF2_ROUNDS, &mut out);

    format!(
        "pbkdf2${}${}${}",
        PBKDF2_ROUNDS,
        B64.encode(salt),
        B64.encode(out)
    )
}

/// Recomputes the hash with the salt and rounds pulled from `stored` and
/// compares it in constant time. Any malformed field just means "no match".
fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    if parts.next() != Some("pbkdf2") {
        return false;
    }
    let Some(rounds) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
        return false;
    };
    let Some(salt) = parts.next().and_then(|s| B64.decode(s).ok()) else {
        return false;
    };
    let Some(expected) = parts.next().and_then(|s| B64.decode(s).ok()) else {
        return false;
    };

    let mut out = vec![0u8; expected.len()];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, rounds, &mut out);
    constant_time_eq(&out, &expected)
}

/// Byte compare that doesn't bail early, so a caller can't time their way to
/// the right answer one byte at a time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── JWT (HS256) ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct Claims {
    /// Username the token was issued for.
    sub: String,
    /// Expiry, seconds since the epoch.
    exp: u64,
}

/// Builds a signed HS256 token for `username`.
pub fn make_token(secret: &[u8], username: &str) -> String {
    // Fixed header, so its base64 is a constant.
    const HEADER_B64: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

    let claims = Claims {
        sub: username.to_string(),
        exp: now_secs() + TOKEN_TTL_SECS,
    };
    let payload_b64 = B64.encode(
        serde_json::to_vec(&claims).expect("Claims serialization is infallible"),
    );

    let signing_input = format!("{HEADER_B64}.{payload_b64}");
    let sig = sign(secret, signing_input.as_bytes());
    format!("{signing_input}.{}", B64.encode(sig))
}

/// Checks the signature and the expiry, then hands back the username the token
/// was made for.
pub fn verify_token(secret: &[u8], token: &str) -> Result<String, AuthError> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AuthError::Token);
    };

    let want = sign(secret, format!("{header}.{payload}").as_bytes());
    let got = B64.decode(sig_b64).map_err(|_| AuthError::Token)?;
    if !constant_time_eq(&want, &got) {
        return Err(AuthError::Token);
    }

    let claims: Claims = serde_json::from_slice(
        &B64.decode(payload).map_err(|_| AuthError::Token)?,
    )
    .map_err(|_| AuthError::Token)?;

    if claims.exp < now_secs() {
        return Err(AuthError::Token);
    }
    Ok(claims.sub)
}

fn sign(secret: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AuthError {
    UsernameTaken,
    InvalidCredentials,
    BadInput(String),
    TooManyAttempts,
    Db(String),
    Token,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AuthError::UsernameTaken => (StatusCode::CONFLICT, "That username is taken".to_string()),
            AuthError::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, "Wrong username or password".to_string())
            }
            AuthError::BadInput(m) => (StatusCode::BAD_REQUEST, m),
            AuthError::TooManyAttempts => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts, wait a moment and try again".to_string(),
            ),
            AuthError::Token => (StatusCode::UNAUTHORIZED, "Not signed in".to_string()),
            AuthError::Db(e) => {
                tracing::error!("Accounts DB error: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

// ─── Validation ──────────────────────────────────────────────────────────────

/// Trims and checks a username, returning the cleaned form.
fn clean_username(raw: &str) -> Result<String, AuthError> {
    let name = raw.trim();
    if name.len() < MIN_USERNAME_LEN || name.len() > MAX_USERNAME_LEN {
        return Err(AuthError::BadInput(format!(
            "Username must be {MIN_USERNAME_LEN} to {MAX_USERNAME_LEN} characters"
        )));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AuthError::BadInput(
            "Username can only use letters, numbers and underscores".to_string(),
        ));
    }
    Ok(name.to_string())
}

fn check_password(pw: &str) -> Result<(), AuthError> {
    if pw.len() < MIN_PASSWORD_LEN || pw.len() > MAX_PASSWORD_LEN {
        return Err(AuthError::BadInput(format!(
            "Password must be {MIN_PASSWORD_LEN} to {MAX_PASSWORD_LEN} characters"
        )));
    }
    Ok(())
}

// ─── HTTP handlers ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct Credentials {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct AuthOk {
    /// The login token. The browser gets it as an HttpOnly cookie and ignores
    /// this copy, it's here so a non browser client (the mobile app later) can
    /// pick it up and send it back as a bearer token.
    token: String,
    username: String,
}

/// POST /stargzr/auth/register
pub async fn register(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, AuthError> {
    rate_limit(&state, &headers, &addr)?;

    let username = clean_username(&body.username)?;
    check_password(&body.password)?;

    state.db.insert_user(&username, &hash_password(&body.password))?;
    tracing::info!(username = %username, "New account registered");

    Ok(finish_login(&state, &headers, username))
}

/// POST /stargzr/auth/login
pub async fn login(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, AuthError> {
    rate_limit(&state, &headers, &addr)?;

    let record = state.db.lookup(body.username.trim())?;

    // Verify against a real hash either way. When the name doesn't exist we run
    // it against a throwaway hash so a login for a missing user costs the same
    // wall time as one for a real user, and the endpoint can't be used to probe
    // which names are registered.
    let hash = record
        .as_ref()
        .map(|(_, h)| h.as_str())
        .unwrap_or_else(|| DUMMY_HASH.as_str());
    let password_ok = verify_password(&body.password, hash);

    let Some((stored_name, _)) = record else {
        return Err(AuthError::InvalidCredentials);
    };
    if !password_ok {
        return Err(AuthError::InvalidCredentials);
    }

    tracing::info!(username = %stored_name, "Login");
    Ok(finish_login(&state, &headers, stored_name))
}

/// GET /stargzr/auth/me
///
/// Confirms the caller is signed in, from either the HttpOnly cookie (browser)
/// or an Authorization: Bearer header (other clients), and re links this browser
/// session to the account as a side effect.
pub async fn me(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<AuthOk>, AuthError> {
    let username = caller_username(&state, &headers)?;
    link_session(&state, &headers, &username);
    super::radio::broadcast_analytics(&state);
    Ok(Json(AuthOk {
        token: String::new(),
        username,
    }))
}

/// POST /stargzr/auth/logout
pub async fn logout(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Some(session_id) = session_cookie(&headers) {
        state.session_users.remove(&session_id);
    }
    super::radio::broadcast_analytics(&state);

    // Clear the token cookie by setting it empty with an immediate expiry.
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, auth_cookie_header("", is_https(&headers)))],
    )
        .into_response()
}

/// Forces the dummy hash to compute now, at startup, rather than on the first
/// login for a missing user, so that request isn't the odd slow one out.
pub fn warm_up() {
    LazyLock::force(&DUMMY_HASH);
}

// ─── Used by the page handler ────────────────────────────────────────────────

/// Reads the token cookie, and if it checks out links this browser session to
/// the account and returns the name. Called from player_page so a signed in
/// user stays signed in across reloads and server restarts with nothing for the
/// frontend to do.
pub fn session_username(state: &SharedState, headers: &HeaderMap) -> Option<String> {
    let token = auth_cookie(headers)?;
    let username = verify_token(&state.jwt_secret, &token).ok()?;
    link_session(state, headers, &username);
    Some(username)
}

// ─── Handler helpers ─────────────────────────────────────────────────────────

/// Signs a token, sets it as an HttpOnly cookie, ties the browser session to
/// the account, and pushes a fresh analytics frame so the broadcaster list
/// shows the name straight away.
fn finish_login(state: &SharedState, headers: &HeaderMap, username: String) -> Response {
    let token = make_token(&state.jwt_secret, &username);
    link_session(state, headers, &username);
    super::radio::broadcast_analytics(state);

    (
        [(header::SET_COOKIE, auth_cookie_header(&token, is_https(headers)))],
        Json(AuthOk { token, username }),
    )
        .into_response()
}

fn rate_limit(state: &SharedState, headers: &HeaderMap, addr: &PeerAddr) -> Result<(), AuthError> {
    let ip = client_ip(headers, addr);
    state
        .auth_rate_limiter
        .check_and_consume(&ip)
        .map_err(|_| {
            crate::player::metrics::inc_rate_limit_hits("auth");
            tracing::warn!(ip = %ip, "Auth rate limit hit");
            AuthError::TooManyAttempts
        })
}

fn link_session(state: &SharedState, headers: &HeaderMap, username: &str) {
    if let Some(session_id) = session_cookie(headers) {
        state.session_users.insert(session_id, username.to_string());
    }
}

/// Signed in name from the token cookie first, then a bearer header.
fn caller_username(state: &SharedState, headers: &HeaderMap) -> Result<String, AuthError> {
    if let Some(token) = auth_cookie(headers) {
        if let Ok(name) = verify_token(&state.jwt_secret, &token) {
            return Ok(name);
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AuthError::Token)?;
    verify_token(&state.jwt_secret, bearer)
}

/// Builds the token cookie. An empty `token` with the Max-Age still positive is
/// fine, the browser treats an empty value as "no login", but we set Max-Age 0
/// on the clear path so it's removed outright. `secure` adds the Secure flag so
/// the cookie only travels over HTTPS, which we can only ask for when the
/// request actually came in over HTTPS.
fn auth_cookie_header(token: &str, secure: bool) -> String {
    let max_age = if token.is_empty() { 0 } else { TOKEN_TTL_SECS };
    let secure_flag = if secure { "; Secure" } else { "" };
    format!(
        "{AUTH_COOKIE}={token}; Path=/stargzr; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure_flag}"
    )
}

/// True when the original client request reached the proxy over HTTPS. Caddy
/// sets X-Forwarded-Proto on the way through.
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// Real client IP. Behind Caddy the socket peer is always localhost, so the
/// X-Real-IP header it sets is the one that matters, with the peer address as a
/// fallback for a direct local run.
fn client_ip(headers: &HeaderMap, addr: &PeerAddr) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| addr.0.ip().to_string())
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
            })
        })
        .map(|s| s.to_string())
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, "player_session")
}

fn auth_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, AUTH_COOKIE)
}

/// A valid hash to check a password against when the username was not found, so
/// login timing gives nothing away. Content doesn't matter, only that it's the
/// real format and takes the real time to verify.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("not a real password, only here for timing"));

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
