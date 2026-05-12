use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant, sleep_until};

/// A thread-safe rate limiter that enforces strict request spacing (QPS).
///
/// This implementation uses the "Next Available Time" algorithm (Leaky Bucket variant).
/// When a thread requests a permit:
/// 1. We calculate the target time: `max(now, next_allowed)`.
/// 2. We increment `next_allowed` by the interval (1/QPS).
/// 3. The thread sleeps until `target_time`.
///
/// This guarantees that requests never exceed the QPS, and bursts are smoothed out
/// into a steady stream.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    next_allowed: Arc<Mutex<Instant>>,
    interval: Duration,
}

impl RateLimiter {
    /// Creates a new RateLimiter for the given Queries Per Second (QPS).
    pub fn new(qps: f64) -> Self {
        // Ensure QPS is valid to avoid division by zero or negative sleep
        let safe_qps = if qps <= 0.0 { 1.0 } else { qps };
        let interval_micros = (1_000_000.0 / safe_qps) as u64;

        Self {
            next_allowed: Arc::new(Mutex::new(Instant::now())),
            interval: Duration::from_micros(interval_micros),
        }
    }

    /// Acquires a permit, sleeping if necessary to maintain the rate limit.
    pub async fn acquire(&self) {
        let target_time = {
            let mut next = self.next_allowed.lock().unwrap();
            let now = Instant::now();

            // If we've been idle, we can start immediately (reset bucket).
            // Without this, if we idle for 10s with QPS=1, the next 10 requests
            // would be instant (bursting), which we might want to avoid if strict spacing is needed.
            // However, typically "Leaky Bucket" allows bursting up to capacity.
            // This implementation enforces STRICT spacing (no bursting) to be safe for 429s.
            if *next < now {
                *next = now;
            }

            let target = *next;
            *next += self.interval;
            target
        };

        sleep_until(target_time).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Instant;

    #[tokio::test]
    async fn test_rate_limiter_spacing() {
        // 10 QPS = 100ms interval
        let limiter = RateLimiter::new(10.0);

        let start = Instant::now();

        // Make 3 requests.
        // 1st: T+0ms
        // 2nd: T+100ms
        // 3rd: T+200ms
        limiter.acquire().await;
        limiter.acquire().await;
        limiter.acquire().await;

        let elapsed = start.elapsed();

        // Should take at least 200ms (spacing between 3 requests)
        assert!(
            elapsed.as_millis() >= 190,
            "Elapsed: {}ms",
            elapsed.as_millis()
        );
    }
}
