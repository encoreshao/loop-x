//! Per-user sliding-window rate limit.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(3600);

pub struct RateLimit {
    per_hour: u32,
    hits: HashMap<String, VecDeque<Instant>>,
}

impl RateLimit {
    pub fn new(per_hour: u32) -> Self {
        Self {
            per_hour,
            hits: HashMap::new(),
        }
    }

    /// Allow at most `per_hour` requests per user in the trailing hour.
    /// Denied attempts are not recorded.
    pub fn allow(&mut self, user: &str, now: Instant) -> bool {
        let window = self.hits.entry(user.to_string()).or_default();
        while window
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW)
        {
            window.pop_front();
        }
        if window.len() >= self.per_hour as usize {
            return false;
        }
        window.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eleventh_in_an_hour_is_denied() {
        let mut r = RateLimit::new(10);
        let t = Instant::now();
        for _ in 0..10 {
            assert!(r.allow("U1", t));
        }
        assert!(!r.allow("U1", t));
        assert!(r.allow("U2", t));
    }

    #[test]
    fn window_slides_after_an_hour() {
        let mut r = RateLimit::new(10);
        let t = Instant::now();
        for _ in 0..10 {
            assert!(r.allow("U1", t));
        }
        assert!(!r.allow("U1", t + Duration::from_secs(3599)));
        assert!(r.allow("U1", t + Duration::from_secs(3601)));
    }
}
