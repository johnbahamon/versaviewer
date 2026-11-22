//! Device ID / one-time password generation and brute-force protection.

use anyhow::{Result, anyhow};
use std::time::{Duration, Instant};

/// Alphabet without look-alike characters (no 0/O, 1/l/I).
const PASSWORD_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
pub const PASSWORD_LEN: usize = 8;

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("OS RNG failure: {e}"))?;
    Ok(buf)
}

/// 9-digit numeric device ID (first digit non-zero), e.g. `"482913775"`.
pub fn generate_device_id() -> Result<String> {
    let bytes = random_bytes::<8>()?;
    let n = u64::from_le_bytes(bytes) % 900_000_000 + 100_000_000;
    Ok(n.to_string())
}

/// Formats `"482913775"` as `"482 913 775"` for display.
pub fn format_device_id(id: &str) -> String {
    id.as_bytes()
        .chunks(3)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Random one-time password from a CSPRNG, using rejection sampling to
/// avoid modulo bias.
pub fn generate_password() -> Result<String> {
    let n = PASSWORD_ALPHABET.len();
    let limit = 256 - (256 % n);
    let mut out = String::with_capacity(PASSWORD_LEN);
    while out.len() < PASSWORD_LEN {
        for b in random_bytes::<16>()? {
            if (b as usize) < limit && out.len() < PASSWORD_LEN {
                out.push(PASSWORD_ALPHABET[b as usize % n] as char);
            }
        }
    }
    Ok(out)
}

/// Constant-time comparison so the response time leaks nothing about how
/// many leading characters matched.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Locks out password attempts after repeated failures (exponential backoff).
pub struct AttemptLimiter {
    failures: u32,
    locked_until: Option<Instant>,
    max_failures: u32,
    base_lockout: Duration,
}

impl Default for AttemptLimiter {
    fn default() -> Self {
        Self { failures: 0, locked_until: None, max_failures: 5, base_lockout: Duration::from_secs(30) }
    }
}

impl AttemptLimiter {
    /// `Some(remaining)` while attempts are locked out.
    pub fn locked(&self, now: Instant) -> Option<Duration> {
        self.locked_until.and_then(|t| t.checked_duration_since(now)).filter(|d| !d.is_zero())
    }

    pub fn record_failure(&mut self, now: Instant) {
        self.failures += 1;
        if self.failures >= self.max_failures {
            let exp = (self.failures - self.max_failures).min(6);
            self.locked_until = Some(now + self.base_lockout * 2u32.pow(exp));
        }
    }

    pub fn record_success(&mut self) {
        self.failures = 0;
        self.locked_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_nine_digits() {
        for _ in 0..100 {
            let id = generate_device_id().unwrap();
            assert_eq!(id.len(), 9);
            assert!(id.bytes().all(|b| b.is_ascii_digit()));
        }
        assert_eq!(format_device_id("482913775"), "482 913 775");
    }

    #[test]
    fn password_uses_alphabet() {
        let p = generate_password().unwrap();
        assert_eq!(p.len(), PASSWORD_LEN);
        assert!(p.bytes().all(|b| PASSWORD_ALPHABET.contains(&b)));
        assert_ne!(p, generate_password().unwrap());
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn limiter_locks_after_max_failures() {
        let mut l = AttemptLimiter::default();
        let now = Instant::now();
        for _ in 0..4 {
            l.record_failure(now);
            assert!(l.locked(now).is_none());
        }
        l.record_failure(now);
        assert!(l.locked(now).is_some());
        assert!(l.locked(now + Duration::from_secs(31)).is_none());
        l.record_failure(now);
        assert!(l.locked(now + Duration::from_secs(31)).is_some()); // backoff doubled
        l.record_success();
        assert!(l.locked(now).is_none());
    }
}
