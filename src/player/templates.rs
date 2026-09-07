use askama::Template;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use hyper::StatusCode;

#[derive(Template)]
#[template(path = "player.html")]
pub struct PlayerTemplate {
    pub current_media: String,
    pub current_index: usize,
    pub total_medias: usize,
    pub session_id: String,
    pub is_video: bool,
    /// Account name for whoever is signed in on this browser, empty if nobody is.
    /// Lets the page render the right auth state with no flash of the login form.
    pub current_username: String,
    /// Content hash of the JS and CSS, appended to every asset URL as ?v=...
    pub asset_version: String,
}

#[derive(Template)]
#[template(path = "player_controls.html")]
pub struct PlayerControlsTemplate {
    pub current_media: String,
    pub current_index: usize,
    pub total_medias: usize,
}

/// Makes returning PlayerTemplate returnable as an axum response
impl IntoResponse for PlayerTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                // Never let a browser reuse the page without checking back. The
                // page is what points at the versioned asset URLs, so a stale
                // page is the one thing that could still pin someone to old JS.
                .header(header::CACHE_CONTROL, "no-cache")
                .header(
                    header::SET_COOKIE,
                    format!(
                        // Path is /stargzr, not /stargzr/player, so the browser
                        // also sends it to /stargzr/auth/* . That is what lets
                        // register and login tie this session to an account.
                        "player_session={}; Path=/stargzr; HttpOnly; SameSite=Strict; Max-Age={}",
                        self.session_id,
                        1 * 24 * 3600
                    )
                )
                // Clear the old narrower cookie some browsers still hold from
                // before the path widened, so it isn't sent alongside the new one.
                .header(
                    header::SET_COOKIE,
                    "player_session=; Path=/stargzr/player; HttpOnly; SameSite=Strict; Max-Age=0",
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
