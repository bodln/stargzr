//! Profile pictures.
//!
//! Each account has at most one, kept as a plain file in the avatar folder
//! (`AVATAR_PATH`, defaulting to an `avatars` folder beside the accounts
//! database, so a Docker volume on the data directory keeps them too). The
//! file is named after the lowercased username plus the image type's
//! extension. Nothing about it lives in the database: the file existing *is*
//! the record, and its modification time doubles as the version clients put
//! on the URL so a changed picture is never served back stale from a cache.
//!
//! Reading is public, the same as the usernames themselves in the people
//! list. Only the signed in owner can set or clear their own.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;

use super::auth::{self, AuthError};
use super::types::SharedState;

/// Upload cap. The app sends a downscaled square JPEG well under this, the
/// headroom is for anything else that posts an original photo.
pub const MAX_AVATAR_BYTES: usize = 5 * 1024 * 1024;

/// Accepted formats as (extension, content type). Checked by magic bytes,
/// never by whatever Content-Type the client claimed.
const FORMATS: [(&str, &str); 3] = [
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("webp", "image/webp"),
];

#[derive(Serialize)]
pub struct AvatarOk {
    /// The new version, or null once removed.
    avatar: Option<i64>,
}

/// Where avatars live when `AVATAR_PATH` isn't set: next to the database.
pub fn default_dir(db_path: &str) -> PathBuf {
    Path::new(db_path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("avatars")
}

/// The picture's version for `username` (its modification time in ms), or
/// None when the account has no picture.
pub fn version(state: &SharedState, username: &str) -> Option<i64> {
    let (path, _) = find(&state.avatar_dir, username)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// PUT /stargzr/social/avatar, body is the raw image bytes.
pub async fn upload_avatar(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AvatarOk>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    let ext = sniff(&body).ok_or_else(|| {
        AuthError::BadInput("Profile pictures must be JPEG, PNG or WebP".to_string())
    })?;
    let stem = file_stem(&me).ok_or(AuthError::Token)?;
    let dir = state.avatar_dir.as_path();

    tokio::fs::create_dir_all(dir).await.map_err(io_err)?;
    // Write beside the target and rename over it, so a reader never sees a
    // half written file.
    let target = dir.join(format!("{stem}.{ext}"));
    let staging = dir.join(format!("{stem}.{ext}.part"));
    tokio::fs::write(&staging, &body).await.map_err(io_err)?;
    tokio::fs::rename(&staging, &target).await.map_err(io_err)?;
    // A previous picture in a different format would otherwise shadow or
    // outlive this one.
    for (other, _) in FORMATS.iter().filter(|(e, _)| *e != ext) {
        let _ = tokio::fs::remove_file(dir.join(format!("{stem}.{other}"))).await;
    }

    tracing::info!(username = %me, bytes = body.len(), "Profile picture updated");
    Ok(Json(AvatarOk { avatar: version(&state, &me) }))
}

/// DELETE /stargzr/social/avatar
pub async fn delete_avatar(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<AvatarOk>, AuthError> {
    let me = auth::caller_username(&state, &headers)?;
    let stem = file_stem(&me).ok_or(AuthError::Token)?;
    for (ext, _) in FORMATS {
        let _ = tokio::fs::remove_file(state.avatar_dir.join(format!("{stem}.{ext}"))).await;
    }
    tracing::info!(username = %me, "Profile picture removed");
    Ok(Json(AvatarOk { avatar: None }))
}

/// GET /stargzr/avatars/{username}
///
/// Clients add `?v=<version>`, so the URL changes whenever the picture does
/// and a long cache lifetime is safe.
pub async fn get_avatar(
    State(state): State<SharedState>,
    UrlPath(username): UrlPath<String>,
) -> Response {
    let Some((path, mime)) = find(&state.avatar_dir, &username) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "public, max-age=604800"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Lowercased username, or None for anything a real username can't be.
/// Usernames are already restricted to letters, digits and underscores at
/// registration; checking again here is what keeps a crafted path parameter
/// from ever reaching the filesystem as `../` or similar.
fn file_stem(username: &str) -> Option<String> {
    let valid = !username.is_empty()
        && username.len() <= 64
        && username.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then(|| username.to_ascii_lowercase())
}

fn find(dir: &Path, username: &str) -> Option<(PathBuf, &'static str)> {
    let stem = file_stem(username)?;
    FORMATS
        .iter()
        .map(|(ext, mime)| (dir.join(format!("{stem}.{ext}")), *mime))
        .find(|(path, _)| path.is_file())
}

fn io_err(e: std::io::Error) -> AuthError {
    AuthError::Db(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_tricks() {
        assert_eq!(file_stem("Emin_1"), Some("emin_1".to_string()));
        assert_eq!(file_stem("../etc"), None);
        assert_eq!(file_stem("a/b"), None);
        assert_eq!(file_stem(""), None);
    }

    #[test]
    fn sniffs_by_magic_bytes() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(b"<svg></svg>"), None);
    }
}
