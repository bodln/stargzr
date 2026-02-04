use crate::error::{PlayerError, PlayerResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

pub struct RateLimiter {
    buckets: Arc<RwLock<HashMap<String, TokenBucket>>>,
    capacity: f64,
    refill_rate: f64, // tokens per second
    cleanup_interval: Duration,
}

impl RateLimiter {
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            buckets: Arc::new(RwLock::new(HashMap::new())),
            capacity,
            refill_rate,
            cleanup_interval: Duration::from_secs(300), // Clean up every 5 minutes
        }
    }
    
    // For heartbeat messages - allow 1 per 2 seconds = 0.5 per second
    pub fn for_heartbeat() -> Self {
        Self::new(5.0, 0.5)
    }
    
    // For broadcast updates - allow bursts but limit sustained rate
    pub fn for_broadcast() -> Self {
        Self::new(10.0, 2.0)
    }
    
    pub async fn check_and_consume(&self, session_id: &str) -> PlayerResult<()> {
        let mut buckets = self.buckets.write().await;
        
        let bucket = buckets.entry(session_id.to_string())
            .or_insert_with(|| TokenBucket {
                tokens: self.capacity,
                last_refill: Instant::now(),
            });
        
        // Refill tokens based on elapsed time
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_rate).min(self.capacity);
        bucket.last_refill = now;
        
        // Check if we have enough tokens
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            tracing::warn!("Rate limit exceeded for session: {}", session_id);
            Err(PlayerError::RateLimitExceeded(session_id.to_string()))
        }
    }
    
    // Periodically clean up old buckets to prevent memory leaks
    pub async fn cleanup_old_buckets(&self) {
        let mut buckets = self.buckets.write().await;
        let now = Instant::now();
        
        buckets.retain(|_, bucket| {
            now.duration_since(bucket.last_refill) < Duration::from_secs(3600)
        });
        
        tracing::debug!("Cleaned up rate limiter, {} sessions remaining", buckets.len());
    }
    
    // Spawn a background task that periodically cleans up
    pub fn spawn_cleanup_task(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.cleanup_interval);
            loop {
                interval.tick().await;
                self.cleanup_old_buckets().await;
            }
        });
    }
}

// Quick rate limit test from browser console

// async function testRateLimit() {
//   const ws = new WebSocket('ws://localhost:8083/player/radio');
  
//   ws.onopen = () => {
//     console.log('Connected');
    
//     // Spam 20 heartbeats
//     for (let i = 0; i < 20; i++) {
//       setTimeout(() => {
//         ws.send(JSON.stringify({
//           type: 'Heartbeat',
//           broadcaster_id: '550e8400-e29b-41d4-a716-446655440000',
//           playback_time: 10.5
//         }));
//         console.log(`Sent ${i + 1}`);
//       }, i * 100); // 100ms between each
//     }
//   };
  
//   ws.onmessage = (event) => {
//     const msg = JSON.parse(event.data);
//     if (msg.type === 'Error') {
//       console.error('RATE LIMITED:', msg.message);
//     } else {
//       console.log('Success:', msg.type);
//     }
//   };
// }

// testRateLimit();