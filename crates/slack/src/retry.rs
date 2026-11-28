use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Error;

const CAP: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
#[error("retry in {0:?}")]
pub(crate) struct RetryAfter(pub Duration);

#[derive(Default)]
pub(crate) struct Backoff(u32);

impl Backoff {
    pub fn delay(&mut self, err: Option<&Error>) -> Duration {
        let ceiling = Duration::from_secs(2_u64.pow(self.0.min(7))).min(CAP);
        self.0 = self.0.saturating_add(1);
        let jitter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let delay = ceiling.mul_f64(0.5 + f64::from(jitter) / 2_000_000_000.0);
        err.and_then(|err| err.downcast_ref::<RetryAfter>())
            .map_or(delay, |RetryAfter(after)| delay.max(*after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_disconnects_back_off_and_healthy_sessions_reset() {
        let mut retries = Backoff::default();
        let mut previous = Duration::ZERO;
        for seconds in [1, 2, 4, 8, 16, 32, 64, 120] {
            let ceiling = Duration::from_secs(seconds);
            let delay = retries.delay(None);
            assert!(delay >= previous || seconds == 120);
            assert!((ceiling / 2..=ceiling).contains(&delay));
            previous = delay;
        }
        assert!(retries.delay(None) <= CAP);
        let after = Duration::from_secs(180);
        assert_eq!(retries.delay(Some(&RetryAfter(after).into())), after);
        assert!(Backoff::default().delay(None) <= Duration::from_secs(1));
    }
}
