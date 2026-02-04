use crate::error::{PlayerError, PlayerResult};
use regex::Regex;
use once_cell::sync::Lazy;

// Compile the regex once at startup rather than on every validation
static SESSION_ID_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$").unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: String) -> PlayerResult<Self> {
        if !SESSION_ID_REGEX.is_match(&id) {
            return Err(PlayerError::InvalidSessionId(format!(
                "Session ID must be a valid UUID, got: {}",
                id
            )));
        }
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