//! Friends and one to one messages.
//!
//! Friending here is one directional and needs no approval: adding someone is
//! a statement about you, not a request to them, so there is no pending state
//! to manage and nothing to accept. What the two directions *combine* into is
//! what matters — once both people have added each other they are mutual, and
//! mutual is the only thing that unlocks direct messages. That keeps "who can
//! write to me" entirely in the hands of the person being written to without
//! ever putting a request in front of them.
//!
//! Like [`super::playlists`] everything is keyed by username, which is already
//! the stable account identity everywhere else in this codebase, and every
//! endpoint requires a signed in caller.
//!
//! Messages are stored, so a conversation survives both sides going offline,
//! and are *also* pushed live at the recipient's open sockets (see
//! [`push_to_user`]). The two are not alternatives: the database is the record
//! and the push is only there so an open app doesn't have to poll. A client
//! that misses a push still sees the message next time it loads the thread.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Json;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::auth::{self, AuthError};
use super::session::now_ms;
use super::types::{RadioMessage, SharedState};

/// Longest direct message we keep. Roomier than a chat line since this is a
/// conversation rather than a room everyone else has to scroll past.
const MAX_DM_LEN: usize = 1000;

/// How much of a thread one fetch returns, newest last. Long enough that
/// nobody hits it in normal use, short enough to bound the response.
const THREAD_LIMIT: i64 = 300;

// ─── Response shapes ─────────────────────────────────────────────────────────

/// One other account, as seen by the caller. Carries both directions of the
/// friendship separately rather than a single "are we friends" flag, because
/// the whole point of the model is that the two can disagree — the Account tab
/// shows exactly that split.
#[derive(Serialize)]
pub struct UserSummary {
    pub username: String,
    /// Signed in with a live socket open right now.
    pub online: bool,
    /// The caller has added this person.
    pub i_added: bool,
    /// This person has added the caller.
    pub added_me: bool,
    /// Both directions. The requirement for direct messages.
    pub mutual: bool,
    /// Unread messages waiting from this person.
    pub unread: i64,
    /// Profile picture version, null when they have none. See avatars.rs.
    pub avatar: Option<i64>,
}

/// A thread the caller has with one other account.
#[derive(Serialize)]
pub struct Conversation {
    pub username: String,
    pub online: bool,
    pub mutual: bool,
    pub unread: i64,
    /// Empty when they are mutual friends who have never written to each other.
    pub last_text: String,
    pub last_from_me: bool,
    /// 0 when there is no message yet.
    pub last_at_ms: i64,
    pub avatar: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct DirectMessage {
    pub id: i64,
    pub from: String,
    pub to: String,
    pub text: String,
    pub server_timestamp_ms: i64,
}

#[derive(Deserialize)]
pub struct SendMessageBody {
    text: String,
}

fn db_err(e: rusqlite::Error) -> AuthError {
    AuthError::Db(e.to_string())
}

// ─── Presence ────────────────────────────────────────────────────────────────

/// Every account with at least one live socket, lowercased for comparison
/// since usernames are matched case insensitively everywhere else.
///
/// `session_users` alone is not enough: it is written on login and outlives
/// the socket, so a session that has gone away entirely still appears there
/// until it is cleaned up. Intersecting it with `live_sessions` is what makes
/// this mean "right now" instead of "at some point".
fn online_usernames(state: &SharedState) -> HashSet<String> {
    state
        .session_users
        .iter()
        .filter(|entry| state.live_sessions.contains_key(entry.key()))
        .map(|entry| entry.value().to_lowercase())
        .collect()
}

/// Hands `msg` to every open socket belonging to `username`. Silent when they
/// have none — they are offline, and the database already holds whatever this
/// was announcing.
///
/// `try_send` rather than `send` so a client that has stopped draining its
/// queue can't stall the HTTP request that triggered this. Dropping the push
/// costs that client nothing but a refetch.
fn push_to_user(state: &SharedState, username: &str, msg: &RadioMessage) {
    let target = username.to_lowercase();
    for entry in state.session_users.iter() {
        if entry.value().to_lowercase() != target {
            continue;
        }
        if let Some(outbox) = state.session_outbox.get(entry.key()) {
            let _ = outbox.try_send(msg.clone());
        }
    }
}

// ─── Queries ─────────────────────────────────────────────────────────────────

/// Accounts `username` has added.
fn added_by(conn: &Connection, username: &str) -> Result<HashSet<String>, AuthError> {
    let mut stmt = conn
        .prepare("SELECT friend FROM friendships WHERE username = ?1 COLLATE NOCASE")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([username], |row| row.get::<_, String>(0))
        .map_err(db_err)?;
    Ok(rows.filter_map(Result::ok).map(|n| n.to_lowercase()).collect())
}

/// Accounts that have added `username`.
fn who_added(conn: &Connection, username: &str) -> Result<HashSet<String>, AuthError> {
    let mut stmt = conn
        .prepare("SELECT username FROM friendships WHERE friend = ?1 COLLATE NOCASE")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([username], |row| row.get::<_, String>(0))
        .map_err(db_err)?;
    Ok(rows.filter_map(Result::ok).map(|n| n.to_lowercase()).collect())
}

/// Unread count per sender, for messages addressed to `username`.
fn unread_by_sender(conn: &Connection, username: &str) -> Result<HashMap<String, i64>, AuthError> {
    let mut stmt = conn
        .prepare(
            "SELECT sender, COUNT(*) FROM direct_messages
             WHERE recipient = ?1 COLLATE NOCASE AND read_at_ms IS NULL
             GROUP BY sender COLLATE NOCASE",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([username], |row| {
            Ok((row.get::<_, String>(0)?.to_lowercase(), row.get::<_, i64>(1)?))
        })
        .map_err(db_err)?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// The stored name for `username` with its original casing, or None when no
/// such account exists.
fn resolve_user(conn: &Connection, username: &str) -> Result<Option<String>, AuthError> {
    conn.query_row(
        "SELECT username FROM users WHERE username = ?1 COLLATE NOCASE",
        [username],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(db_err)
}

/// Every account except the caller, with both friendship directions, presence
/// and unread counts filled in. Shared by the "who's around" listing and by
/// the two friend endpoints, which return the same refreshed view so a client
/// can replace its state outright instead of patching one row and hoping the
/// rest still matches.
fn user_summaries(state: &SharedState, me: &str) -> Result<Vec<UserSummary>, AuthError> {
    let online = online_usernames(state);
    let conn = state.db.conn();

    let i_added = added_by(&conn, me)?;
    let added_me = who_added(&conn, me)?;
    let unread = unread_by_sender(&conn, me)?;

    let mut stmt = conn
        .prepare("SELECT username FROM users WHERE username <> ?1 COLLATE NOCASE")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([me], |row| row.get::<_, String>(0))
        .map_err(db_err)?;

    let mut users: Vec<UserSummary> = rows
        .filter_map(Result::ok)
        .map(|username| {
            let key = username.to_lowercase();
            let i = i_added.contains(&key);
            let they = added_me.contains(&key);
            UserSummary {
                online: online.contains(&key),
                i_added: i,
                added_me: they,
                mutual: i && they,
                unread: unread.get(&key).copied().unwrap_or(0),
                avatar: super::avatars::version(state, &username),
                username,
            }
        })
        .collect();

    // Online first, then the people you already have a relationship with, then
    // alphabetically. Sorted here rather than in SQL because presence lives in
    // memory, not in the database.
    users.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then(b.mutual.cmp(&a.mutual))
            .then((b.i_added || b.added_me).cmp(&(a.i_added || a.added_me)))
            .then_with(|| a.username.to_lowercase().cmp(&b.username.to_lowercase()))
    });
    Ok(users)
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// GET /stargzr/social/users
pub async fn list_users(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Vec<UserSummary>>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    Ok(Json(user_summaries(&state, &me)?))
}

/// POST /stargzr/social/friends/{username}
///
/// Adding is idempotent — pressing it twice is the same as pressing it once,
/// which matters when the button is a tap away on a flaky connection.
pub async fn add_friend(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<Json<Vec<UserSummary>>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    let target = {
        let conn = state.db.conn();
        resolve_user(&conn, username.trim())?.ok_or(AuthError::NotFound)?
    };
    if target.eq_ignore_ascii_case(&me) {
        return Err(AuthError::BadInput("You can't add yourself".to_string()));
    }

    {
        let conn = state.db.conn();
        conn.execute(
            "INSERT OR IGNORE INTO friendships (username, friend, created_at) VALUES (?1, ?2, ?3)",
            params![me, target, auth::now_secs() as i64],
        )
        .map_err(db_err)?;
    }
    tracing::info!(user = %me, friend = %target, "Friend added");

    push_to_user(&state, &target, &RadioMessage::SocialUpdate { from: me.clone() });
    Ok(Json(user_summaries(&state, &me)?))
}

/// DELETE /stargzr/social/friends/{username}
///
/// Only drops the caller's own side. The other person's list is theirs.
pub async fn remove_friend(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<Json<Vec<UserSummary>>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    let target = username.trim().to_string();

    {
        let conn = state.db.conn();
        conn.execute(
            "DELETE FROM friendships WHERE username = ?1 COLLATE NOCASE AND friend = ?2 COLLATE NOCASE",
            params![me, target],
        )
        .map_err(db_err)?;
    }
    tracing::info!(user = %me, friend = %target, "Friend removed");

    push_to_user(&state, &target, &RadioMessage::SocialUpdate { from: me.clone() });
    Ok(Json(user_summaries(&state, &me)?))
}

/// GET /stargzr/social/messages
///
/// The inbox: one row per person the caller can talk to or has talked to.
/// Mutual friends appear even with an empty thread, since an empty thread with
/// someone you can write to is exactly what you want to tap on. Anyone with
/// history but no longer mutual stays listed too, marked `mutual: false`, so
/// unfriending hides the reply box rather than silently losing the record.
pub async fn list_conversations(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Conversation>>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    let online = online_usernames(&state);
    let conn = state.db.conn();

    let i_added = added_by(&conn, &me)?;
    let added_me = who_added(&conn, &me)?;
    let unread = unread_by_sender(&conn, &me)?;

    // Newest message per counterparty. Grouping on the other side of the
    // exchange (whichever of sender/recipient isn't the caller) collapses both
    // directions into the one thread they actually are.
    let mut stmt = conn
        .prepare(
            "SELECT other, body, sender, created_at_ms FROM (
                 SELECT CASE WHEN sender = ?1 COLLATE NOCASE THEN recipient ELSE sender END AS other,
                        body, sender, created_at_ms
                 FROM direct_messages
                 WHERE sender = ?1 COLLATE NOCASE OR recipient = ?1 COLLATE NOCASE
                 ORDER BY id DESC
             )
             GROUP BY other COLLATE NOCASE",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([&me], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(db_err)?;

    let mut threads: HashMap<String, Conversation> = HashMap::new();
    for (other, body, sender, at_ms) in rows.filter_map(Result::ok) {
        let key = other.to_lowercase();
        threads.insert(
            key.clone(),
            Conversation {
                online: online.contains(&key),
                mutual: i_added.contains(&key) && added_me.contains(&key),
                unread: unread.get(&key).copied().unwrap_or(0),
                last_text: body,
                last_from_me: sender.eq_ignore_ascii_case(&me),
                last_at_ms: at_ms,
                avatar: super::avatars::version(&state, &other),
                username: other,
            },
        );
    }

    // Mutual friends with nothing said yet. Resolved through the users table so
    // the name carries its registered casing rather than whatever was typed
    // into the friend button.
    for key in i_added.intersection(&added_me) {
        if threads.contains_key(key) {
            continue;
        }
        let Some(username) = resolve_user(&conn, key)? else {
            continue;
        };
        threads.insert(
            key.clone(),
            Conversation {
                online: online.contains(key),
                mutual: true,
                unread: 0,
                last_text: String::new(),
                last_from_me: false,
                last_at_ms: 0,
                avatar: super::avatars::version(&state, &username),
                username,
            },
        );
    }

    let mut conversations: Vec<Conversation> = threads.into_values().collect();
    // Most recently active first; the never-used mutual threads (last_at_ms 0)
    // fall to the bottom in name order.
    conversations.sort_by(|a, b| {
        b.last_at_ms
            .cmp(&a.last_at_ms)
            .then_with(|| a.username.to_lowercase().cmp(&b.username.to_lowercase()))
    });
    Ok(Json(conversations))
}

/// GET /stargzr/social/messages/{username}
///
/// The thread with one person, oldest first. Opening it is what marks their
/// messages read — there is no separate "mark read" call to get out of step
/// with what the reader actually saw.
pub async fn conversation(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<Json<Vec<DirectMessage>>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    let conn = state.db.conn();
    let other = resolve_user(&conn, username.trim())?.ok_or(AuthError::NotFound)?;

    conn.execute(
        "UPDATE direct_messages SET read_at_ms = ?1
         WHERE recipient = ?2 COLLATE NOCASE AND sender = ?3 COLLATE NOCASE AND read_at_ms IS NULL",
        params![now_ms() as i64, me, other],
    )
    .map_err(db_err)?;

    // Newest THREAD_LIMIT rows, then flipped, so a long thread keeps its recent
    // end rather than its beginning.
    let mut stmt = conn
        .prepare(
            "SELECT id, sender, recipient, body, created_at_ms FROM direct_messages
             WHERE (sender = ?1 COLLATE NOCASE AND recipient = ?2 COLLATE NOCASE)
                OR (sender = ?2 COLLATE NOCASE AND recipient = ?1 COLLATE NOCASE)
             ORDER BY id DESC LIMIT ?3",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![me, other, THREAD_LIMIT], |row| {
            Ok(DirectMessage {
                id: row.get(0)?,
                from: row.get(1)?,
                to: row.get(2)?,
                text: row.get(3)?,
                server_timestamp_ms: row.get(4)?,
            })
        })
        .map_err(db_err)?;

    let mut messages: Vec<DirectMessage> = rows.filter_map(Result::ok).collect();
    messages.reverse();
    Ok(Json(messages))
}

/// POST /stargzr/social/messages/{username}
pub async fn send_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<Json<DirectMessage>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;

    let text = body.text.trim();
    if text.is_empty() {
        return Err(AuthError::BadInput("Message is empty".to_string()));
    }
    // Clamp at a char boundary so a long message is shortened rather than cut
    // through the middle of a multibyte character.
    let text = if text.len() > MAX_DM_LEN {
        let mut end = MAX_DM_LEN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };

    let message = {
        let conn = state.db.conn();
        let other = resolve_user(&conn, username.trim())?.ok_or(AuthError::NotFound)?;

        // Mutual is the gate. Checked on send rather than only in the UI, since
        // the UI hiding a box is a convenience and this is the actual rule.
        let mutual: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM friendships a
                 JOIN friendships b ON b.username = a.friend AND b.friend = a.username
                 WHERE a.username = ?1 COLLATE NOCASE AND a.friend = ?2 COLLATE NOCASE",
                params![me, other],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        if mutual.is_none() {
            return Err(AuthError::BadInput(
                "You can only message people who have added you back".to_string(),
            ));
        }

        let at_ms = now_ms() as i64;
        conn.execute(
            "INSERT INTO direct_messages (sender, recipient, body, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![me, other, text, at_ms],
        )
        .map_err(db_err)?;

        DirectMessage {
            id: conn.last_insert_rowid(),
            from: me.clone(),
            to: other,
            text: text.to_string(),
            server_timestamp_ms: at_ms,
        }
    };

    push_to_user(
        &state,
        &message.to,
        &RadioMessage::DirectMessage {
            id: message.id,
            from: message.from.clone(),
            to: message.to.clone(),
            text: message.text.clone(),
            server_timestamp_ms: message.server_timestamp_ms as u128,
        },
    );

    Ok(Json(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::rate_limit::RateLimiter;
    use crate::player::types::AppState;
    use axum::http::HeaderValue;
    use dashmap::DashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use tokio::sync::{RwLock, Semaphore};

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    fn test_state(db_path: &str) -> SharedState {
        let db = auth::Db::open(db_path).expect("open test db");
        {
            let conn = db.conn();
            for name in ["alice", "bob", "carol"] {
                conn.execute(
                    "INSERT INTO users (username, password_hash, created_at) VALUES (?1, 'x', 0)",
                    [name],
                )
                .unwrap();
            }
        }
        Arc::new(AppState {
            playlist: Arc::new(RwLock::new(Vec::new())),
            media_folder: Arc::new(PathBuf::from(".")),
            sessions: DashMap::new(),
            broadcast_states: DashMap::new(),
            broadcast_channels: DashMap::new(),
            broadcaster_listeners: DashMap::new(),
            session_tuned_to: DashMap::new(),
            session_latency_ms: DashMap::new(),
            live_sessions: DashMap::new(),
            session_outbox: DashMap::new(),
            global_broadcast_tx: tokio::sync::broadcast::channel(10).0,
            active_connections: AtomicUsize::new(0),
            last_analytics_ms: AtomicU64::new(0),
            ws_rate_limiter: RateLimiter::for_websocket(),
            auth_rate_limiter: RateLimiter::for_auth(),
            upload_quotas: DashMap::new(),
            conversion_semaphore: Arc::new(Semaphore::new(1)),
            db,
            jwt_secret: b"test-secret".to_vec(),
            session_users: DashMap::new(),
            asset_version: "test".to_string(),
            avatar_dir: std::env::temp_dir().join("stargzr-test-avatars"),
        })
    }

    fn temp_db_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("stargzr_social_test_{label}_{}.db", uuid::Uuid::new_v4()))
    }

    fn headers_for(state: &SharedState, user: &str) -> HeaderMap {
        bearer_headers(&auth::make_token(&state.jwt_secret, user))
    }

    fn find<'a>(users: &'a [UserSummary], name: &str) -> &'a UserSummary {
        users.iter().find(|u| u.username == name).expect("user in list")
    }

    /// One pass over the whole feature: the two friendship directions stay
    /// independent, messaging unlocks only once they agree, and reading a
    /// thread is what clears its unread count.
    #[tokio::test]
    async fn friending_gates_messaging() {
        let db_path = temp_db_path("friends");
        let state = test_state(db_path.to_str().unwrap());
        let alice = headers_for(&state, "alice");
        let bob = headers_for(&state, "bob");

        let users = list_users(State(state.clone()), alice.clone()).await.unwrap().0;
        assert_eq!(users.len(), 2, "everyone but the caller");
        assert!(!find(&users, "bob").i_added && !find(&users, "bob").added_me);

        // One direction only — no approval needed, but no messaging yet either.
        let users = add_friend(State(state.clone()), alice.clone(), Path("bob".into()))
            .await
            .unwrap()
            .0;
        assert!(find(&users, "bob").i_added);
        assert!(!find(&users, "bob").mutual, "bob hasn't added alice back");

        let blocked = send_message(
            State(state.clone()),
            alice.clone(),
            Path("bob".into()),
            Json(SendMessageBody { text: "hi".into() }),
        )
        .await;
        assert!(blocked.is_err(), "one sided friendship must not allow messages");

        // Bob's own list shows the incoming side without him having done anything.
        let bobs_view = list_users(State(state.clone()), bob.clone()).await.unwrap().0;
        assert!(find(&bobs_view, "alice").added_me && !find(&bobs_view, "alice").i_added);

        let bobs_view = add_friend(State(state.clone()), bob.clone(), Path("ALICE".into()))
            .await
            .unwrap()
            .0;
        assert!(find(&bobs_view, "alice").mutual, "case insensitive match");

        let _ = send_message(
            State(state.clone()),
            alice.clone(),
            Path("bob".into()),
            Json(SendMessageBody { text: "  hey bob  ".into() }),
        )
        .await
        .unwrap();

        let inbox = list_conversations(State(state.clone()), bob.clone()).await.unwrap().0;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].last_text, "hey bob", "trimmed on the way in");
        assert_eq!(inbox[0].unread, 1);
        assert!(!inbox[0].last_from_me);

        let thread = conversation(State(state.clone()), bob.clone(), Path("alice".into()))
            .await
            .unwrap()
            .0;
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].from, "alice");

        let inbox = list_conversations(State(state.clone()), bob.clone()).await.unwrap().0;
        assert_eq!(inbox[0].unread, 0, "opening the thread marks it read");

        // Unfriending is one sided: alice drops bob, so she can no longer write,
        // and the thread survives marked as no longer mutual.
        let _ = remove_friend(State(state.clone()), alice.clone(), Path("bob".into()))
            .await
            .unwrap();
        assert!(
            send_message(
                State(state.clone()),
                alice.clone(),
                Path("bob".into()),
                Json(SendMessageBody { text: "still there?".into() }),
            )
            .await
            .is_err()
        );
        let inbox = list_conversations(State(state.clone()), alice.clone()).await.unwrap().0;
        assert_eq!(inbox.len(), 1, "history is kept");
        assert!(!inbox[0].mutual);

        // A mutual friend with nothing said yet still gets a row to tap on.
        let carol = headers_for(&state, "carol");
        let _ = add_friend(State(state.clone()), alice.clone(), Path("carol".into())).await.unwrap();
        let _ = add_friend(State(state.clone()), carol.clone(), Path("alice".into())).await.unwrap();
        let inbox = list_conversations(State(state.clone()), alice.clone()).await.unwrap().0;
        let empty = inbox.iter().find(|c| c.username == "carol").expect("carol listed");
        assert!(empty.mutual && empty.last_text.is_empty() && empty.last_at_ms == 0);

        let _ = std::fs::remove_file(&db_path);
    }

    /// Adding is idempotent, self-friending is refused, and an unknown name 404s.
    #[tokio::test]
    async fn add_friend_edge_cases() {
        let db_path = temp_db_path("edges");
        let state = test_state(db_path.to_str().unwrap());
        let alice = headers_for(&state, "alice");

        let _ = add_friend(State(state.clone()), alice.clone(), Path("bob".into())).await.unwrap();
        let twice = add_friend(State(state.clone()), alice.clone(), Path("bob".into()))
            .await
            .unwrap()
            .0;
        assert!(find(&twice, "bob").i_added);

        assert!(
            add_friend(State(state.clone()), alice.clone(), Path("alice".into()))
                .await
                .is_err()
        );
        assert!(
            add_friend(State(state.clone()), alice.clone(), Path("nobody".into()))
                .await
                .is_err()
        );

        let _ = std::fs::remove_file(&db_path);
    }
}
