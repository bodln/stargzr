use super::error::{PlayerError, PlayerResult};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: String) -> PlayerResult<Self> {
        // uuid::Uuid::parse_str is faster and drops the regex and once_cell dependencies
        uuid::Uuid::parse_str(&id)
            .map_err(|_| PlayerError::InvalidSessionId(format!(
                "Session ID must be a valid UUID, got: {}",
                id
            )))?;
        Ok(SessionId(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

// Implement Display so we can use it in format strings
impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn validate_song_index(index: usize, playlist_size: usize) -> PlayerResult<usize> {
    if index >= playlist_size {
        return Err(PlayerError::InvalidSongIndex(index, playlist_size));
    }
    Ok(index)
}