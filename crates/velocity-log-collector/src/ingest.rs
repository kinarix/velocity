//! The top-level scan-and-tail loop.
//!
//! Every `scan_secs`:
//!   1. List `<log_root>/*/`. For each directory matching
//!      `{namespace}_{pod}_{uid}`, list its container subdirectories,
//!      and within each, register the `0.log` (or rotated equivalent)
//!      as a watched file.
//!   2. For each watched file, read any new lines and enqueue to the
//!      shipper.
//!   3. Flush the shipper if size or age thresholds say so.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::parser::{parse_line, parse_pod_dir, PodMeta};
use crate::shipper::{Shipper, ShipperHandle};
use crate::tail::TailState;

#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub log_root: PathBuf,
    pub processor_endpoint: String,
    pub ingest_token: String,
    pub scan_interval: Duration,
    pub flush_interval: Duration,
    pub max_batch_records: usize,
    pub max_batch_age: Duration,
}

impl CollectorConfig {
    pub fn from_env() -> Result<Self> {
        let log_root: PathBuf =
            std::env::var("VELOCITY_LC_LOG_ROOT").unwrap_or_else(|_| "/var/log/pods".into()).into();
        let processor_endpoint = std::env::var("VELOCITY_LC_PROCESSOR_ENDPOINT")
            .context("VELOCITY_LC_PROCESSOR_ENDPOINT required (e.g. http://velocity-log-processor:9090/v1/logs)")?;
        let ingest_token = std::env::var("VELOCITY_LC_INGEST_TOKEN")
            .context("VELOCITY_LC_INGEST_TOKEN required")?;
        let scan_interval = secs_env("VELOCITY_LC_SCAN_SECS", 5);
        let flush_interval = secs_env("VELOCITY_LC_FLUSH_SECS", 1);
        let max_batch_records =
            std::env::var("VELOCITY_LC_MAX_BATCH").ok().and_then(|s| s.parse().ok()).unwrap_or(500);
        let max_batch_age = secs_env("VELOCITY_LC_MAX_BATCH_AGE_SECS", 2);
        Ok(Self {
            log_root,
            processor_endpoint,
            ingest_token,
            scan_interval,
            flush_interval,
            max_batch_records,
            max_batch_age,
        })
    }
}

fn secs_env(key: &str, default: u64) -> Duration {
    Duration::from_secs(std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default))
}

/// Coordinator: owns the file table + shipper.
#[derive(Debug)]
pub struct Collector {
    cfg: CollectorConfig,
    shipper: ShipperHandle,
    files: HashMap<PathBuf, WatchedFile>,
}

#[derive(Debug)]
struct WatchedFile {
    meta: PodMeta,
    container: String,
    tail: TailState,
}

impl Collector {
    pub fn new(cfg: CollectorConfig) -> Result<Self> {
        let shipper = Shipper {
            endpoint: cfg.processor_endpoint.clone(),
            token: cfg.ingest_token.clone(),
            max_records: cfg.max_batch_records,
            max_age: cfg.max_batch_age,
        }
        .handle()?;
        Ok(Self { cfg, shipper, files: HashMap::new() })
    }

    /// Run forever. Picks the shorter of `scan_interval` and
    /// `flush_interval` as the tick period — scans on every Nth tick,
    /// flushes whenever the shipper says it should.
    pub async fn run(mut self) -> ! {
        let mut last_scan = std::time::Instant::now() - self.cfg.scan_interval;
        let tick = self.cfg.flush_interval.min(self.cfg.scan_interval);
        loop {
            tokio::time::sleep(tick).await;
            if last_scan.elapsed() >= self.cfg.scan_interval {
                if let Err(e) = self.scan_root().await {
                    tracing::warn!(error = %e, "log-collector scan failed");
                }
                last_scan = std::time::Instant::now();
            }
            if let Err(e) = self.read_all().await {
                tracing::warn!(error = %e, "log-collector read failed");
            }
            if self.shipper.should_flush() {
                let _ = self.shipper.flush().await;
            }
        }
    }

    /// Walk the root and register any new pod log files. Existing
    /// files keep their offsets; vanished files are dropped.
    pub async fn scan_root(&mut self) -> Result<()> {
        let mut entries = match tokio::fs::read_dir(&self.cfg.log_root).await {
            Ok(e) => e,
            // Missing root is non-fatal — kubelet creates it as pods
            // start landing on the node.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).context("read_dir log_root"),
        };

        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        while let Some(entry) = entries.next_entry().await? {
            let pod_dir = entry.path();
            let Some(name) = pod_dir.file_name().and_then(|n| n.to_str()) else { continue };
            let Some(meta) = parse_pod_dir(name) else { continue };

            let mut containers = match tokio::fs::read_dir(&pod_dir).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            while let Some(c_entry) = containers.next_entry().await? {
                let c_dir = c_entry.path();
                let Some(container) = c_dir.file_name().and_then(|n| n.to_str()) else { continue };
                let log_path = c_dir.join("0.log");
                if !log_path.exists() {
                    continue;
                }
                seen.insert(log_path.clone());
                if self.files.contains_key(&log_path) {
                    continue;
                }
                let tail = match TailState::new(log_path.clone(), false).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(path = %log_path.display(), error = %e, "open tail failed");
                        continue;
                    }
                };
                tracing::info!(path = %log_path.display(), "tailing new pod log");
                self.files.insert(
                    log_path,
                    WatchedFile { meta: meta.clone(), container: container.to_string(), tail },
                );
            }
        }

        // Drop watches for files that no longer exist (pod deleted).
        let to_drop: Vec<PathBuf> =
            self.files.keys().filter(|p| !seen.contains(*p)).cloned().collect();
        for p in to_drop {
            tracing::info!(path = %p.display(), "pod gone; dropping tail");
            self.files.remove(&p);
        }
        Ok(())
    }

    /// Read all watched files; enqueue each emitted line.
    pub async fn read_all(&mut self) -> Result<()> {
        for f in self.files.values_mut() {
            let lines = match f.tail.read_new_lines().await {
                Ok(l) => l,
                Err(e) => {
                    tracing::debug!(path = %f.tail.path.display(), error = %e, "tail read err");
                    continue;
                }
            };
            for line in lines {
                let record = parse_line(&line, &f.container, &f.meta);
                self.shipper.enqueue(record);
            }
        }
        Ok(())
    }

    /// Test accessor — returns the current shipper buffer count.
    #[cfg(test)]
    pub fn buffered(&self) -> usize {
        self.shipper.buffered()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::AsyncWriteExt;

    const UUID: &str = "12345678-1234-1234-1234-123456789abc";

    async fn write_line(path: &std::path::Path, line: &str) {
        let mut f =
            tokio::fs::OpenOptions::new().create(true).append(true).open(path).await.unwrap();
        f.write_all(line.as_bytes()).await.unwrap();
        f.write_all(b"\n").await.unwrap();
    }

    fn cfg(root: PathBuf) -> CollectorConfig {
        CollectorConfig {
            log_root: root,
            processor_endpoint: "http://127.0.0.1:0".into(),
            ingest_token: "tok".into(),
            scan_interval: Duration::from_millis(50),
            flush_interval: Duration::from_millis(50),
            max_batch_records: 1000,
            max_batch_age: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn scan_picks_up_pod_dirs_and_enqueues_appended_lines() {
        let root = tempfile::tempdir().unwrap();
        let pod_dir = root.path().join(format!("velocity_api_{UUID}"));
        let container_dir = pod_dir.join("main");
        tokio::fs::create_dir_all(&container_dir).await.unwrap();
        let log_path = container_dir.join("0.log");
        // Pre-existing content — TailState::new opens at EOF, so this
        // is intentionally NOT read.
        write_line(&log_path, "pre-existing").await;

        let mut c = Collector::new(cfg(root.path().to_path_buf())).unwrap();
        c.scan_root().await.unwrap();
        assert_eq!(c.files.len(), 1, "one container should be watched");

        write_line(&log_path, r#"{"level":"INFO","msg":"hello"}"#).await;
        c.read_all().await.unwrap();
        assert_eq!(c.buffered(), 1);
    }

    #[tokio::test]
    async fn non_pod_dir_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(root.path().join("not-a-pod")).await.unwrap();
        let mut c = Collector::new(cfg(root.path().to_path_buf())).unwrap();
        c.scan_root().await.unwrap();
        assert_eq!(c.files.len(), 0);
    }

    #[tokio::test]
    async fn vanished_pod_dir_drops_watch() {
        let root = tempfile::tempdir().unwrap();
        let pod_dir = root.path().join(format!("velocity_api_{UUID}"));
        let container_dir = pod_dir.join("main");
        tokio::fs::create_dir_all(&container_dir).await.unwrap();
        let log_path = container_dir.join("0.log");
        write_line(&log_path, "x").await;

        let mut c = Collector::new(cfg(root.path().to_path_buf())).unwrap();
        c.scan_root().await.unwrap();
        assert_eq!(c.files.len(), 1);

        tokio::fs::remove_dir_all(&pod_dir).await.unwrap();
        c.scan_root().await.unwrap();
        assert_eq!(c.files.len(), 0, "drop tail when pod dir disappears");
    }

    #[tokio::test]
    async fn missing_root_is_noop() {
        let mut c = Collector::new(cfg(PathBuf::from("/nonexistent-velocity-log-root"))).unwrap();
        // Must not error — kubelet creates the dir lazily on the node.
        c.scan_root().await.unwrap();
        assert_eq!(c.files.len(), 0);
    }
}
