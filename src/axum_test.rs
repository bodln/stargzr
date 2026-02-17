use crate::primitives::{ArcToo, MutexToo};
use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use hyper::StatusCode;

// App state holds the counter value
// Arc = Atomic Reference Counted (thread-safe sharing)
// Mutex = Mutual Exclusion (only one thread can modify at a time)
struct AppStateTest {
    count: MutexToo<u32>,
}

// Full page template - used for initial page load
#[derive(Template)]
#[template(path = "counter.html")]
struct GreetingTemplate {
    message: String,
    count: u32,
}

// Partial template - used for HTMX updates
#[derive(Template)]
#[template(path = "counter_partial.html")]
struct CounterPartial {
    count: u32,
}

// Implement IntoResponse for full page
impl IntoResponse for GreetingTemplate {
    fn into_response(self) -> axum::response::Response {
        match self.render() {
            // render() generates the HTML string from the template
            Ok(html) => Html(html).into_response(),
            // Wrap in Html() to set Content-Type: text/html header
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template: {}", err),
            )
                .into_response(),
        }
    }
}

// Implement IntoResponse for partial
impl IntoResponse for CounterPartial {
    fn into_response(self) -> axum::response::Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template: {}", err),
            )
                .into_response(),
        }
    }
}

// Handler for GET / - Initial page load
async fn handler(State(state): State<ArcToo<AppStateTest>>) -> GreetingTemplate {
    // State(state) extracts the Arc<AppState> from the router

    let count = *state.count.lock();
    // .lock() acquires the mutex (waits if another thread is using it)
    // .unwrap() panics if the mutex is poisoned (thread panicked while holding it)
    // * dereferences to get the u32 value

    GreetingTemplate {
        message: "Hello from Rust!".to_string(),
        count,
    }
    // Returns the full page template
    // Axum automatically calls .into_response() on this
}

// Handler for POST /increment - HTMX calls this
async fn increment(State(state): State<ArcToo<AppStateTest>>) -> CounterPartial {
    let mut count = state.count.lock();
    // mut because we're modifying the value

    *count += 1;
    // Increment the counter
    // The * dereferences the MutexGuard to access the u32

    CounterPartial { count: *count }
    // Return ONLY the partial template
    // This becomes: "<p>Count: 6</p>"
    // HTMX receives this and updates the page
}

async fn _hello() -> Html<&'static str> {
    Html(include_str!("player/templates/index.html"))
}

pub async fn initilaze() {
    // Create shared state
    let app_state = ArcToo::new(AppStateTest {
        count: MutexToo::new(0), // Start at 0
    });

    let app = Router::new()
        .route("/", get(handler))
        // GET / returns the full page
        .route("/increment", post(increment))
        // POST /increment returns just the counter partial
        .with_state(app_state);
    // Share the app_state with all handlers

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8081")
        .await
        .unwrap();

    println!(
        "✓ HTML server listening on {}",
        listener.local_addr().unwrap()
    );
    axum::serve(listener, app).await.unwrap();
}
