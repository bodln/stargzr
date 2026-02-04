use tracing_subscriber::{
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};
use std::env;

pub fn init_logging() {
    let env = env::var("ENVIRONMENT").unwrap_or_else(|_| "development".to_string());

    let filter = match env.as_str() {
        "production" => EnvFilter::new("info,mp3_player=debug"),
        "development" => EnvFilter::new("debug"),
        _ => EnvFilter::new("info"),
    };

    let format: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> =
        match env.as_str() {
            "production" => Box::new(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_thread_ids(true)
                    .with_target(true),
            ),
            _ => Box::new(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_file(true)
                    .with_line_number(true)
                    .with_thread_ids(true),
            ),
        };

    tracing_subscriber::registry()
        .with(filter)
        .with(format)
        .init();
}
