use network_tests::validation::SessionId;
use network_tests::validation::validate_song_index;
use network_tests::rate_limit::RateLimiter;
use network_tests::reconnect::ReconnectionStrategy;

use std::time::Duration;

#[test]
fn test_session_id_validation() {
    let valid = SessionId::new(
        "550e8400-e29b-41d4-a716-446655440000".to_string()
    );
    assert!(valid.is_ok());

    assert!(SessionId::new("not-a-uuid".to_string()).is_err());
    assert!(SessionId::new("".to_string()).is_err());
    assert!(SessionId::new("550e8400".to_string()).is_err());
}

#[test]
fn test_song_index_validation() {
    assert!(validate_song_index(0, 10).is_ok());
    assert!(validate_song_index(9, 10).is_ok());
    assert!(validate_song_index(10, 10).is_err());
    assert!(validate_song_index(100, 10).is_err());
}

#[tokio::test]
async fn test_rate_limiter() {
    let limiter = RateLimiter::new(2.0, 1.0);
    let session_id = "test-session";

    assert!(limiter.check_and_consume(session_id).await.is_ok());
    assert!(limiter.check_and_consume(session_id).await.is_ok());
    assert!(limiter.check_and_consume(session_id).await.is_err());

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(limiter.check_and_consume(session_id).await.is_ok());
}

#[test]
fn test_reconnection_strategy() {
    let mut strategy = ReconnectionStrategy::with_defaults();

    let first = strategy.next_delay().unwrap();
    let second = strategy.next_delay().unwrap();

    assert!(second > first);

    strategy.reset();
    let after_reset = strategy.next_delay().unwrap();
    assert!(after_reset < second);
}
