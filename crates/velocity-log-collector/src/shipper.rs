//! Batch shipper.
//!
//! Owns the buffer + the HTTP client. Records are pushed via
//! `enqueue`. The shipper flushes whenever the buffer reaches
//! `max_records` OR `max_age` has elapsed since the first buffered
//! record — whichever comes first.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Shipper {
    pub endpoint: String,
    pub token: String,
    pub max_records: usize,
    pub max_age: Duration,
}

#[derive(Debug)]
pub struct ShipperHandle {
    cfg: Shipper,
    client: reqwest::Client,
    buf: Vec<Value>,
    first_at: Option<Instant>,
}

#[derive(Serialize, Debug)]
struct WireBatch<'a> {
    records: &'a [Value],
}

impl Shipper {
    pub fn handle(self) -> Result<ShipperHandle> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
        Ok(ShipperHandle { cfg: self, client, buf: Vec::new(), first_at: None })
    }
}

impl ShipperHandle {
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn enqueue(&mut self, record: Value) {
        if self.buf.is_empty() {
            self.first_at = Some(Instant::now());
        }
        self.buf.push(record);
    }

    /// True iff a flush should fire now based on size or age. Pure —
    /// no IO — so the tick loop can decide without `await`.
    pub fn should_flush(&self) -> bool {
        if self.buf.len() >= self.cfg.max_records {
            return true;
        }
        matches!(self.first_at, Some(t) if t.elapsed() >= self.cfg.max_age)
    }

    /// POST whatever's buffered to the processor. Returns the number
    /// of records sent on success; clears the buffer either way (drop-
    /// on-failure is the documented v1 behaviour — no retry queue, no
    /// disk spillover).
    pub async fn flush(&mut self) -> Result<usize> {
        if self.buf.is_empty() {
            return Ok(0);
        }
        let n = self.buf.len();
        let body = WireBatch { records: &self.buf };
        let res = self
            .client
            .post(&self.cfg.endpoint)
            .bearer_auth(&self.cfg.token)
            .json(&body)
            .send()
            .await;
        self.buf.clear();
        self.first_at = None;
        match res {
            Ok(r) if r.status().is_success() => Ok(n),
            Ok(r) => {
                tracing::warn!(
                    status = %r.status(),
                    dropped = n,
                    "log shipper: processor returned non-2xx; batch dropped"
                );
                Ok(0)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dropped = n,
                    "log shipper: HTTP send failed; batch dropped"
                );
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn cfg() -> Shipper {
        Shipper {
            endpoint: "http://127.0.0.1:0".into(),
            token: "tok".into(),
            max_records: 3,
            max_age: Duration::from_secs(60),
        }
    }

    #[test]
    fn should_flush_on_size() {
        let mut h = cfg().handle().unwrap();
        assert!(!h.should_flush());
        h.enqueue(json!({"a": 1}));
        h.enqueue(json!({"a": 2}));
        assert!(!h.should_flush(), "below max_records");
        h.enqueue(json!({"a": 3}));
        assert!(h.should_flush(), "at max_records");
    }

    #[test]
    fn should_flush_on_age() {
        let mut s = cfg();
        s.max_age = Duration::from_millis(1);
        let mut h = s.handle().unwrap();
        h.enqueue(json!({"a": 1}));
        std::thread::sleep(Duration::from_millis(5));
        assert!(h.should_flush(), "past max_age");
    }

    #[test]
    fn empty_handle_does_not_flush() {
        let h = cfg().handle().unwrap();
        assert!(!h.should_flush());
    }

    #[tokio::test]
    async fn flush_clears_buffer_even_on_failure() {
        // Endpoint will fail (port 0) — verify buffer still clears so
        // we don't leak memory under sustained processor outage.
        let mut h = cfg().handle().unwrap();
        h.enqueue(json!({"a": 1}));
        h.enqueue(json!({"a": 2}));
        let _ = h.flush().await.unwrap();
        assert_eq!(h.buffered(), 0);
        assert!(h.first_at.is_none());
    }
}
