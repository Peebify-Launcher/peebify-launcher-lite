// ------------ Speed Limit ------------
// One token bucket every transfer draws from when the user sets a download speed limit.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const MIN_BURST: u64 = 64 << 10;
const BURST_WINDOW: Duration = Duration::from_millis(100);
const WAIT_SLICE: Duration = Duration::from_millis(100);

pub struct RateLimiter {
    limit: AtomicU64,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            limit: AtomicU64::new(0),
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    pub fn set_limit(&self, bytes_per_sec: u64) {
        let before = self.limit.swap(bytes_per_sec, Ordering::SeqCst);
        if before != bytes_per_sec {
            let mut bucket = self.bucket.lock();
            bucket.tokens = 0.0;
            bucket.last = Instant::now();
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    fn burst(limit: u64) -> f64 {
        ((limit as f64) * BURST_WINDOW.as_secs_f64()).max(MIN_BURST as f64)
    }

    pub async fn consume(&self, bytes: u64) {
        let limit = self.limit();
        if limit == 0 {
            return;
        }
        let deficit = {
            let mut bucket = self.bucket.lock();
            let now = Instant::now();
            let refill = now.duration_since(bucket.last).as_secs_f64() * limit as f64;
            bucket.tokens = (bucket.tokens + refill).min(Self::burst(limit));
            bucket.last = now;
            bucket.tokens -= bytes as f64;
            -bucket.tokens
        };
        if deficit <= 0.0 {
            return;
        }
        let mut wait = Duration::from_secs_f64(deficit / limit as f64);
        while !wait.is_zero() {
            let slice = wait.min(WAIT_SLICE);
            tokio::time::sleep(slice).await;
            wait -= slice;
            if self.limit() == 0 {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_limit_costs_nothing() {
        let limiter = RateLimiter::new();
        let started = Instant::now();
        for _ in 0..1000 {
            limiter.consume(1 << 20).await;
        }
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn a_limit_spreads_reads_over_time() {
        let limiter = std::sync::Arc::new(RateLimiter::new());
        limiter.set_limit(4 << 20);
        let started = Instant::now();
        let mut streams = Vec::new();
        for _ in 0..4 {
            let limiter = std::sync::Arc::clone(&limiter);
            streams.push(tokio::spawn(async move {
                for _ in 0..16 {
                    limiter.consume(64 << 10).await;
                }
            }));
        }
        for s in streams {
            s.await.unwrap();
        }
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(800) && took < Duration::from_millis(2000),
            "4 MiB at 4 MiB/s took {took:?}"
        );
    }

    #[tokio::test]
    async fn lifting_the_limit_releases_a_waiting_stream() {
        let limiter = std::sync::Arc::new(RateLimiter::new());
        limiter.set_limit(64 << 10);
        let waiting = {
            let limiter = std::sync::Arc::clone(&limiter);
            tokio::spawn(async move { limiter.consume(64 << 20).await })
        };
        tokio::time::sleep(Duration::from_millis(150)).await;
        limiter.set_limit(0);
        tokio::time::timeout(Duration::from_millis(500), waiting)
            .await
            .expect("the wait ends once the limit is gone")
            .unwrap();
    }
}
