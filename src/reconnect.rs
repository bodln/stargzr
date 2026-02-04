use std::time::Duration;
use tokio::time::sleep;
use rand::Rng;

#[derive(Debug, Clone)]
pub struct ReconnectionConfig {
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub max_attempts: Option<u32>,
    pub jitter_factor: f64,
}

impl Default for ReconnectionConfig {
    fn default() -> Self {
        Self {
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            max_attempts: Some(10),
            jitter_factor: 0.2,
        }
    }
}

pub struct ReconnectionStrategy {
    config: ReconnectionConfig,
    current_attempt: u32,
}

impl ReconnectionStrategy {
    pub fn new(config: ReconnectionConfig) -> Self {
        Self {
            config,
            current_attempt: 0,
        }
    }
    
    pub fn with_defaults() -> Self {
        Self::new(ReconnectionConfig::default())
    }
    
    // Calculate the next delay with exponential backoff and jitter
    pub fn next_delay(&mut self) -> Option<Duration> {
        if let Some(max) = self.config.max_attempts {
            if self.current_attempt >= max {
                tracing::warn!("Max reconnection attempts ({}) reached", max);
                return None;
            }
        }
        
        // Exponential backoff: base * 2^attempt
        let exponential_ms = self.config.base_delay_ms as f64 
            * 2_f64.powi(self.current_attempt as i32);
        
        // Add jitter to prevent thundering herd problem
        let mut rng = rand::rng();
        let jitter_range = exponential_ms * self.config.jitter_factor;
        let jitter = rng.random_range(-jitter_range..=jitter_range);
        
        let delay_ms = (exponential_ms + jitter)
            .max(self.config.base_delay_ms as f64)
            .min(self.config.max_delay_ms as f64);
        
        self.current_attempt += 1;
        
        tracing::debug!(
            "Reconnection attempt {}: waiting {}ms",
            self.current_attempt,
            delay_ms as u64
        );
        
        Some(Duration::from_millis(delay_ms as u64))
    }
    
    pub fn reset(&mut self) {
        tracing::info!("Reconnection successful, resetting attempt counter");
        self.current_attempt = 0;
    }
    
    pub fn current_attempt(&self) -> u32 {
        self.current_attempt
    }
    
    // Retry a given async operation with exponential backoff
    pub async fn retry<F, Fut, T, E>(&mut self, mut operation: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        loop {
            match operation().await {
                Ok(result) => {
                    self.reset();
                    return Ok(result);
                }
                Err(e) => {
                    tracing::warn!("Operation failed: {}. Retrying...", e);
                    
                    if let Some(delay) = self.next_delay() {
                        sleep(delay).await;
                    } else {
                        tracing::error!("All retry attempts exhausted");
                        return Err(e);
                    }
                }
            }
        }
    }
}