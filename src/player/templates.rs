use askama::Template;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use hyper::StatusCode;

#[derive(Template)]
#[template(path = "player.html")]
pub struct PlayerTemplate {
    pub current_song: String,
    pub current_index: usize,
    pub total_songs: usize,
    pub session_id: String,
}

#[derive(Template)]
#[template(path = "player_controls.html")]
pub struct PlayerControlsTemplate {
    pub current_song: String,
    pub current_index: usize,
    pub total_songs: usize,
}

/// Makes returning PlayerTemplate returnable as an axum response
impl IntoResponse for PlayerTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(
                    header::SET_COOKIE,
                    format!(
                        "player_session={}; Path=/player; HttpOnly; SameSite=Strict; Max-Age=3600",
                        self.session_id
                    ),
                )
                .body(axum::body::Body::from(html))
                .unwrap(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", e),
            )
                .into_response(),
        }
    }
}

/// Makes returning PlayerControlsTemplate returnable as an axum response
impl IntoResponse for PlayerControlsTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", e),
            )
                .into_response(),
        }
    }
}