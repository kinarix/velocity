//! Velocity log collector.
//!
//! A DaemonSet that tails the kubelet-written log files at
//! `/var/log/pods/<ns>_<pod>_<uid>/<container>/0.log`, parses each
//! line as best-effort JSON, and ships batches over HTTP to a
//! `velocity-log-processor` endpoint.
//!
//! ## v1 scope
//!
//! - Scan-and-tail loop: every N seconds, list the configured log
//!   root, open any new files, resume any rotated files from offset 0.
//!   We track `(path, last_offset)` in memory. Restarts re-read from
//!   the file's current size, NOT from the last persisted offset —
//!   we explicitly accept some data loss on collector restart in
//!   exchange for zero persistence-layer complexity.
//! - Per-line shipping: each line goes into a buffer; the buffer is
//!   flushed when it reaches N records OR when M ms elapse since the
//!   first buffered line.
//! - Pod metadata: derived from the parent directory name
//!   (`{namespace}_{pod}_{uid}`). We don't talk to the kube API in v1
//!   — the processor's `enrich` step is the source of truth for
//!   `velocity.*` labels; the collector just hands over the namespace
//!   and pod name as `kubernetes.namespace` / `kubernetes.pod`.
//!
//! ## Not v1 (explicit non-goals)
//!
//! - No multi-line parsing (Java stack traces stay split across rows).
//! - No log-rotation reopen by inode — we re-scan and detect new
//!   files; in-place truncation is rare for kubelet-managed files.
//! - No buffering to disk on processor outage — buffer is dropped
//!   with a metric. v2 can add a bounded on-disk spillover.

pub mod ingest;
pub mod parser;
pub mod shipper;
pub mod tail;

pub use ingest::{CollectorConfig, Collector};
pub use parser::{parse_pod_dir, PodMeta};
pub use shipper::{Shipper, ShipperHandle};
