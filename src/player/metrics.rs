use metrics::{counter, gauge};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;

// Global storage for the Prometheus exporter handle.
//
// - `OnceLock` guarantees the value can be set only once.
// - This prevents multiple metric recorders from being installed.
// - Acts as shared, read-only access point after initialization.
// - Typically set during application startup in `init_metrics()`.
// - Safe for concurrent access across threads.
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn init_metrics() {
    // Install Prometheus as the global metrics recorder.
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("Failed to install Prometheus recorder");
    HANDLE.set(handle).ok();
}

pub fn render() -> String {
    // Expose metrics in Prometheus text format.
    HANDLE.get().map(|h| h.render()).unwrap_or_default()
}

pub fn inc_ws_connections() {
    counter!("radio_ws_connections_total").increment(1);
}

pub fn inc_ws_rejected() {
    counter!("radio_ws_rejected_total").increment(1);
}

pub fn inc_messages(msg_type: &'static str) {
    // Counter with "type" label.
    counter!("radio_messages_total", "type" => msg_type).increment(1);
}

pub fn inc_rate_limit_hits(limiter: &'static str) {
    // Counter with "limiter" label.
    counter!("radio_rate_limit_hits_total", "limiter" => limiter).increment(1);
}

pub fn set_active(connections: usize, broadcasters: usize, listeners: usize) {
    // Gauges represent current (non-monotonic) values.
    gauge!("radio_active_connections").set(connections as f64);
    gauge!("radio_active_broadcasters").set(broadcasters as f64);
    gauge!("radio_active_listeners").set(listeners as f64);
}

pub fn inc_sessions_created() {
    counter!("player_sessions_created_total").increment(1);
}

pub fn inc_sessions_cleaned(count: usize) {
    counter!("player_sessions_cleaned_total").increment(count as u64);
}

// Fired when a client drops without sending TuneOut first (abrupt disconnect, tab close, etc).
// Lets us see in Grafana how often clients disconnect uncleanly vs tuning out properly.
pub fn inc_abrupt_disconnects() {
    counter!("radio_abrupt_disconnects_total").increment(1);
}