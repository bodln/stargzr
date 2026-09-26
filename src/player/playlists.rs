//! Custom, user-named playlists. An account can create one, add tracks to it
//! by media id, rename or delete it, and it's still there next login —
//! backed by the same sqlite file as accounts (see [`super::auth::Db`]).
//!
//! Playlists are keyed by username rather than a numeric foreign key, since
//! username is already the account's stable identity everywhere else in this
//! codebase (chat, broadcaster names). Items are stored as `media_id`, which
//! is only stable across a restart because of the media id registry in
//! auth.rs — see [`super::auth::media_id_for`].
//!
//! Every endpoint here requires a signed in caller. Ownership is enforced
//! directly in the SQL (`WHERE id = ? AND username = ?`) rather than a
//! separate check-then-act, and a playlist that exists but belongs to someone
//! else looks exactly like one that doesn't exist (404), so an id can't be
//! used to probe who else has playlists.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::auth::{self, AuthError};
use super::types::{MediaInfo, SharedState};

const MAX_PLAYLIST_NAME_LEN: usize = 60;

#[derive(Serialize)]
pub struct PlaylistSummary {
    pub id: i64,
    pub name: String,
    pub item_count: usize,
}

#[derive(Serialize)]
pub struct PlaylistDetail {
    pub id: i64,
    pub name: String,
    /// Resolved against the live media playlist, see [`build_detail`].
    pub items: Vec<MediaInfo>,
}

/// Shared by create (POST /playlists) and rename (PATCH /playlists/{id}).
#[derive(Deserialize)]
pub struct PlaylistNameBody {
    name: String,
}

#[derive(Deserialize)]
pub struct AddItemBody {
    media_id: String,
}

#[derive(Deserialize)]
pub struct ReorderBody {
    media_ids: Vec<String>,
}

fn db_err(e: rusqlite::Error) -> AuthError {
    AuthError::Db(e.to_string())
}

fn clean_playlist_name(raw: &str) -> Result<String, AuthError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > MAX_PLAYLIST_NAME_LEN {
        return Err(AuthError::BadInput(format!(
            "Playlist name must be 1 to {MAX_PLAYLIST_NAME_LEN} characters"
        )));
    }
    Ok(name.to_string())
}

/// Confirms `id` exists and belongs to `username`. Every mutating endpoint
/// below calls this before touching playlist_items.
fn assert_owns(conn: &Connection, id: i64, username: &str) -> Result<(), AuthError> {
    let owned: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM playlists WHERE id = ?1 AND username = ?2 COLLATE NOCASE",
            params![id, username],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?;
    if owned.is_none() {
        return Err(AuthError::NotFound);
    }
    Ok(())
}

/// Builds the full detail response: the playlist's name plus its items,
/// resolved against the *live* in-memory media playlist so a file that was
/// deleted off disk since it was added just quietly drops out of the
/// response instead of erroring the whole thing.
async fn build_detail(
    state: &SharedState,
    username: &str,
    id: i64,
) -> Result<PlaylistDetail, AuthError> {
    let name = {
        let conn = state.db.conn();
        conn.query_row(
            "SELECT name FROM playlists WHERE id = ?1 AND username = ?2 COLLATE NOCASE",
            params![id, username],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(db_err)?
    }
    .ok_or(AuthError::NotFound)?;

    let media_ids: Vec<String> = {
        let conn = state.db.conn();
        let mut stmt = conn
            .prepare("SELECT media_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position ASC")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([id], |row| row.get::<_, String>(0))
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };

    let playlist = state.playlist.read().await;
    let items: Vec<MediaInfo> = media_ids
        .iter()
        .filter_map(|mid| playlist.iter().find(|m| &m.id == mid).cloned())
        .collect();

    Ok(PlaylistDetail { id, name, items })
}

/// GET /stargzr/player/playlists — every custom playlist the caller owns.
pub async fn list_playlists(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Vec<PlaylistSummary>>, AuthError> {
    let username = auth::caller_username(&state, &headers)?;

    let conn = state.db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.name, COUNT(pi.media_id)
             FROM playlists p LEFT JOIN playlist_items pi ON pi.playlist_id = p.id
             WHERE p.username = ?1 COLLATE NOCASE
             GROUP BY p.id ORDER BY p.created_at ASC",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([&username], |row| {
            Ok(PlaylistSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                item_count: row.get::<_, i64>(2)? as usize,
            })
        })
        .map_err(db_err)?;
    let out: Vec<PlaylistSummary> = rows.collect::<Result<_, _>>().map_err(db_err)?;
    Ok(Json(out))
}

/// POST /stargzr/player/playlists { name }
pub async fn create_playlist(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<PlaylistNameBody>,
) -> Result<Json<PlaylistDetail>, AuthError> {
    let username = auth::caller_username(&state, &headers)?;
    let name = clean_playlist_name(&body.name)?;

    let id = {
        let conn = state.db.conn();
        conn.execute(
            "INSERT INTO playlists (username, name, created_at) VALUES (?1, ?2, ?3)",
            params![username, name, auth::now_secs() as i64],
        )
        .map_err(db_err)?;
        conn.last_insert_rowid()
    };

    tracing::info!(username = %username, playlist_id = id, name = %name, "Custom playlist created");
    Ok(Json(PlaylistDetail { id, name, items: Vec::new() }))
}

/// GET /stargzr/player/playlists/{id}
pub async fn get_playlist_detail(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<PlaylistDetail>, AuthError> {
    let username = auth::caller_username(&state, &headers)?;
    Ok(Json(build_detail(&state, &username, id).await?))
}

/// PATCH /stargzr/player/playlists/{id} { name }
pub async fn rename_playlist(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<PlaylistNameBody>,
) -> Result<StatusCode, AuthError> {
    let username = auth::caller_username(&state, &headers)?;
    let name = clean_playlist_name(&body.name)?;

    let conn = state.db.conn();
    let changed = conn
        .execute(
            "UPDATE playlists SET name = ?1 WHERE id = ?2 AND username = ?3 COLLATE NOCASE",
            params![name, id, username],
        )
        .map_err(db_err)?;
    if changed == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /stargzr/player/playlists/{id}
pub async fn delete_playlist(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, AuthError> {
    let username = auth::caller_username(&state, &headers)?;
    let conn = state.db.conn();
    // ON DELETE CASCADE (foreign_keys pragma is on, see Db::open) takes the
    // playlist's items with it in the same statement.
    let changed = conn
        .execute(
            "DELETE FROM playlists WHERE id = ?1 AND username = ?2 COLLATE NOCASE",
            params![id, username],
        )
        .map_err(db_err)?;
    if changed == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /stargzr/player/playlists/{id}/items { media_id }
/// Appends a media item to the end. Adding one already in the playlist is a
/// harmless no-op (`INSERT OR IGNORE`), not an error, so the caller doesn't
/// have to check membership first.
pub async fn add_playlist_item(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<AddItemBody>,
) -> Result<Json<PlaylistDetail>, AuthError> {
    let username = auth::caller_username(&state, &headers)?;

    // Media must exist right now — no adding ids that were never real or
    // have since disappeared from disk.
    let media_exists = {
        let playlist = state.playlist.read().await;
        playlist.iter().any(|m| m.id == body.media_id)
    };
    if !media_exists {
        return Err(AuthError::BadInput("That media no longer exists".to_string()));
    }

    {
        let conn = state.db.conn();
        assert_owns(&conn, id, &username)?;

        let next_pos: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_items WHERE playlist_id = ?1",
                [id],
                |row| row.get(0),
            )
            .map_err(db_err)?;

        conn.execute(
            "INSERT OR IGNORE INTO playlist_items (playlist_id, media_id, position) VALUES (?1, ?2, ?3)",
            params![id, body.media_id, next_pos],
        )
        .map_err(db_err)?;
    }

    Ok(Json(build_detail(&state, &username, id).await?))
}

/// DELETE /stargzr/player/playlists/{id}/items/{media_id}
pub async fn remove_playlist_item(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((id, media_id)): Path<(i64, String)>,
) -> Result<Json<PlaylistDetail>, AuthError> {
    let username = auth::caller_username(&state, &headers)?;
    {
        let conn = state.db.conn();
        assert_owns(&conn, id, &username)?;
        conn.execute(
            "DELETE FROM playlist_items WHERE playlist_id = ?1 AND media_id = ?2",
            params![id, media_id],
        )
        .map_err(db_err)?;
    }
    Ok(Json(build_detail(&state, &username, id).await?))
}

/// PUT /stargzr/player/playlists/{id}/items { media_ids }
/// Reorders in place — the body must contain exactly the playlist's current
/// items, just in the new order. This endpoint only ever changes `position`;
/// use the item endpoints above to actually add or remove something.
pub async fn reorder_playlist_items(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ReorderBody>,
) -> Result<Json<PlaylistDetail>, AuthError> {
    let username = auth::caller_username(&state, &headers)?;
    {
        let mut conn = state.db.conn();
        assert_owns(&conn, id, &username)?;

        let mut existing: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT media_id FROM playlist_items WHERE playlist_id = ?1")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([id], |row| row.get::<_, String>(0))
                .map_err(db_err)?;
            rows.collect::<Result<_, _>>().map_err(db_err)?
        };
        existing.sort();
        let mut incoming = body.media_ids.clone();
        incoming.sort();
        if existing != incoming {
            return Err(AuthError::BadInput(
                "Reorder must include exactly the playlist's current items".to_string(),
            ));
        }

        let tx = conn.transaction().map_err(db_err)?;
        for (pos, media_id) in body.media_ids.iter().enumerate() {
            tx.execute(
                "UPDATE playlist_items SET position = ?1 WHERE playlist_id = ?2 AND media_id = ?3",
                params![pos as i64, id, media_id],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
    }
    Ok(Json(build_detail(&state, &username, id).await?))
}

// ─── Friends' playlists ──────────────────────────────────────────────────────
//
// Read only views of other people's playlists. Only mutual friends get in,
// the same bar direct messages use, since friending is one directional and
// needs no approval: letting "I added you" alone unlock someone's playlists
// would open them to anyone who cared to press Add. Someone who isn't a mutual
// friend gets the same 404 as a playlist that doesn't exist.

/// Both `a` has added `b` and `b` has added `a`.
const MUTUAL_SQL: &str = "EXISTS (SELECT 1 FROM friendships f WHERE f.username = ?1 AND f.friend = p.username)
     AND EXISTS (SELECT 1 FROM friendships f WHERE f.username = p.username AND f.friend = ?1)";

#[derive(Serialize)]
pub struct FriendPlaylistSummary {
    pub id: i64,
    pub name: String,
    pub owner: String,
    pub item_count: usize,
}

#[derive(Serialize)]
pub struct FriendPlaylistDetail {
    pub id: i64,
    pub name: String,
    pub owner: String,
    pub items: Vec<MediaInfo>,
}

/// GET /stargzr/social/playlists — every playlist owned by a mutual friend of
/// the caller, grouped by owner.
pub async fn list_friend_playlists(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Vec<FriendPlaylistSummary>>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;

    let conn = state.db.conn();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.id, p.name, p.username, COUNT(pi.media_id)
             FROM playlists p LEFT JOIN playlist_items pi ON pi.playlist_id = p.id
             WHERE {MUTUAL_SQL}
             GROUP BY p.id ORDER BY p.username COLLATE NOCASE, p.created_at ASC"
        ))
        .map_err(db_err)?;
    let rows = stmt
        .query_map([&me], |row| {
            Ok(FriendPlaylistSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                owner: row.get(2)?,
                item_count: row.get::<_, i64>(3)? as usize,
            })
        })
        .map_err(db_err)?;
    let out: Vec<FriendPlaylistSummary> = rows.collect::<Result<_, _>>().map_err(db_err)?;
    Ok(Json(out))
}

/// GET /stargzr/social/playlists/{id}
pub async fn get_friend_playlist(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<FriendPlaylistDetail>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;

    let owner: String = {
        let conn = state.db.conn();
        conn.query_row(
            &format!("SELECT p.username FROM playlists p WHERE p.id = ?2 AND {MUTUAL_SQL}"),
            params![me, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?
    }
    .ok_or(AuthError::NotFound)?;

    let detail = build_detail(&state, &owner, id).await?;
    Ok(Json(FriendPlaylistDetail {
        id: detail.id,
        name: detail.name,
        owner,
        items: detail.items,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::rate_limit::RateLimiter;
    use crate::player::types::{AppState, MediaType};
    use axum::http::HeaderValue;
    use dashmap::DashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::Arc;
    use tokio::sync::{RwLock, Semaphore};

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    /// Builds a real AppState against a throwaway sqlite file, seeded with two
    /// fake media entries so add/reorder/remove have something to reference.
    fn test_state(db_path: &str) -> SharedState {
        let db = auth::Db::open(db_path).expect("open test db");
        Arc::new(AppState {
            playlist: Arc::new(RwLock::new(vec![
                MediaInfo { id: "media-a".into(), filename: "a.mp3".into(), size: 10, media_type: MediaType::Audio },
                MediaInfo { id: "media-b".into(), filename: "b.mp3".into(), size: 20, media_type: MediaType::Audio },
            ])),
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
        std::env::temp_dir().join(format!("stargzr_playlists_test_{label}_{}.db", uuid::Uuid::new_v4()))
    }

    /// Friends' playlists open up only once both people have added each other.
    #[tokio::test]
    async fn friend_playlists_need_mutual_friendship() {
        let db_path = temp_db_path("friends");
        let state = test_state(db_path.to_str().unwrap());
        let alice = bearer_headers(&auth::make_token(&state.jwt_secret, "alice"));
        let bob = bearer_headers(&auth::make_token(&state.jwt_secret, "bob"));

        let created = create_playlist(
            State(state.clone()), alice.clone(),
            Json(PlaylistNameBody { name: "Alice Mix".into() }),
        ).await.unwrap();
        let pid = created.0.id;
        let _ = add_playlist_item(
            State(state.clone()), alice.clone(), Path(pid),
            Json(AddItemBody { media_id: "media-b".into() }),
        ).await.unwrap();

        let befriend = |from: &str, to: &str| {
            state.db.conn().execute(
                "INSERT INTO friendships (username, friend, created_at) VALUES (?1, ?2, 0)",
                params![from, to],
            ).unwrap();
        };

        // Bob adding Alice on his own isn't enough.
        befriend("bob", "alice");
        let list = list_friend_playlists(State(state.clone()), bob.clone()).await.unwrap();
        assert!(list.0.is_empty());
        let hidden = get_friend_playlist(State(state.clone()), bob.clone(), Path(pid)).await;
        assert!(matches!(hidden, Err(AuthError::NotFound)));

        // Once Alice adds him back he can see it, casing aside.
        befriend("ALICE", "Bob");
        let list = list_friend_playlists(State(state.clone()), bob.clone()).await.unwrap();
        assert_eq!(list.0.len(), 1);
        assert_eq!(list.0[0].owner, "alice");
        assert_eq!(list.0[0].item_count, 1);
        let detail = get_friend_playlist(State(state.clone()), bob.clone(), Path(pid)).await.unwrap();
        assert_eq!(detail.0.name, "Alice Mix");
        assert_eq!(detail.0.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["media-b"]);

        // Your own playlists never show up as a friend's.
        let own = list_friend_playlists(State(state.clone()), alice.clone()).await.unwrap();
        assert!(own.0.is_empty());

        let _ = std::fs::remove_file(&db_path);
    }

    /// End to end sweep of every endpoint in this file, including the
    /// cross-account isolation guarantee (a playlist that exists but belongs
    /// to someone else must 404, not error).
    #[tokio::test]
    async fn playlist_crud_roundtrip() {
        let db_path = temp_db_path("crud");
        let state = test_state(db_path.to_str().unwrap());

        let token = auth::make_token(&state.jwt_secret, "alice");
        let headers = bearer_headers(&token);

        let list = list_playlists(State(state.clone()), headers.clone()).await.unwrap();
        assert!(list.0.is_empty(), "fresh account should have no playlists");

        let created = create_playlist(
            State(state.clone()),
            headers.clone(),
            Json(PlaylistNameBody { name: "  Road Trip  ".into() }),
        )
        .await
        .unwrap();
        assert_eq!(created.0.name, "Road Trip", "name should be trimmed");
        assert!(created.0.items.is_empty());
        let pid = created.0.id;

        let _ = add_playlist_item(
            State(state.clone()), headers.clone(), Path(pid),
            Json(AddItemBody { media_id: "media-a".into() }),
        ).await.unwrap();
        let after_add = add_playlist_item(
            State(state.clone()), headers.clone(), Path(pid),
            Json(AddItemBody { media_id: "media-b".into() }),
        ).await.unwrap();
        assert_eq!(
            after_add.0.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["media-a", "media-b"],
        );

        // Re-adding an item already present is a no-op, not a duplicate row
        let after_dupe = add_playlist_item(
            State(state.clone()), headers.clone(), Path(pid),
            Json(AddItemBody { media_id: "media-a".into() }),
        ).await.unwrap();
        assert_eq!(after_dupe.0.items.len(), 2);

        // A media id that doesn't exist in the live playlist is rejected
        let bad_add = add_playlist_item(
            State(state.clone()), headers.clone(), Path(pid),
            Json(AddItemBody { media_id: "does-not-exist".into() }),
        ).await;
        assert!(matches!(bad_add, Err(AuthError::BadInput(_))));

        let reordered = reorder_playlist_items(
            State(state.clone()), headers.clone(), Path(pid),
            Json(ReorderBody { media_ids: vec!["media-b".into(), "media-a".into()] }),
        ).await.unwrap();
        assert_eq!(
            reordered.0.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["media-b", "media-a"],
        );

        // Reordering with a mismatched item set is rejected
        let bad_reorder = reorder_playlist_items(
            State(state.clone()), headers.clone(), Path(pid),
            Json(ReorderBody { media_ids: vec!["media-a".into()] }),
        ).await;
        assert!(matches!(bad_reorder, Err(AuthError::BadInput(_))));

        rename_playlist(
            State(state.clone()), headers.clone(), Path(pid),
            Json(PlaylistNameBody { name: "Summer Mix".into() }),
        ).await.unwrap();
        let fetched = get_playlist_detail(State(state.clone()), headers.clone(), Path(pid)).await.unwrap();
        assert_eq!(fetched.0.name, "Summer Mix");

        let list2 = list_playlists(State(state.clone()), headers.clone()).await.unwrap();
        assert_eq!(list2.0.len(), 1);
        assert_eq!(list2.0[0].item_count, 2);

        let after_remove = remove_playlist_item(
            State(state.clone()), headers.clone(), Path((pid, "media-a".to_string())),
        ).await.unwrap();
        assert_eq!(after_remove.0.items.len(), 1);
        assert_eq!(after_remove.0.items[0].id, "media-b");

        // A second account can't see, fetch, or delete the first account's playlist
        let bob_token = auth::make_token(&state.jwt_secret, "bob");
        let bob_headers = bearer_headers(&bob_token);

        let bob_list = list_playlists(State(state.clone()), bob_headers.clone()).await.unwrap();
        assert!(bob_list.0.is_empty());

        let bob_get = get_playlist_detail(State(state.clone()), bob_headers.clone(), Path(pid)).await;
        assert!(matches!(bob_get, Err(AuthError::NotFound)));

        let bob_delete = delete_playlist(State(state.clone()), bob_headers, Path(pid)).await;
        assert!(matches!(bob_delete, Err(AuthError::NotFound)));

        delete_playlist(State(state.clone()), headers.clone(), Path(pid)).await.unwrap();
        let list3 = list_playlists(State(state.clone()), headers).await.unwrap();
        assert!(list3.0.is_empty());

        let _ = std::fs::remove_file(&db_path);
    }

    /// The whole point of the media id registry: the same filename must map
    /// to the same id across a fresh `Db::open`, exactly like a server restart.
    #[tokio::test]
    async fn media_id_persists_across_reopen() {
        let db_path = temp_db_path("media_id");
        let path = db_path.to_str().unwrap().to_string();

        let id1 = {
            let db = auth::Db::open(&path).unwrap();
            auth::media_id_for(&db, "song.mp3")
        };
        let id2 = {
            let db = auth::Db::open(&path).unwrap();
            auth::media_id_for(&db, "song.mp3")
        };
        assert_eq!(id1, id2, "same filename must resolve to the same id after a reopen");

        let other = {
            let db = auth::Db::open(&path).unwrap();
            auth::media_id_for(&db, "other-song.mp3")
        };
        assert_ne!(id1, other, "different filenames must get different ids");

        let _ = std::fs::remove_file(&db_path);
    }
}
