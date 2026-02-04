use thiserror::Error;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Error, Debug)]
pub enum PlayerError {
    #[error("Invalid session ID format: {0}")]
    InvalidSessionId(String),
    
    #[error("Song index {0} out of bounds (playlist size: {1})")]
    InvalidSongIndex(usize, usize),
    
    #[error("Broadcaster {0} not found")]
    BroadcasterNotFound(String),
    
    #[error("WebSocket connection failed: {0}")]
    WebSocketError(String),
    
    #[error("File not found: {0}")]
    FileNotFound(String),
    
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    
    #[error("Template rendering failed: {0}")]
    TemplateError(#[from] askama::Error),
    
    #[error("JSON serialization failed: {0}")]
    SerializationError(#[from] serde_json::Error),
    
    #[error("Broadcast channel send failed")]
    BroadcastSendError,
    
    #[error("Rate limit exceeded for session {0}")]
    RateLimitExceeded(String),
}

// Tells Axum how to convert our errors into HTTP responses
impl IntoResponse for PlayerError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            PlayerError::InvalidSessionId(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            PlayerError::InvalidSongIndex(_, _) => (StatusCode::NOT_FOUND, self.to_string()),
            PlayerError::BroadcasterNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            PlayerError::FileNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            PlayerError::RateLimitExceeded(_) => (StatusCode::TOO_MANY_REQUESTS, self.to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string()),
        };

        tracing::error!("Request failed: {}", self);
        
        (status, message).into_response()
    }
}

pub type PlayerResult<T> = Result<T, PlayerError>;