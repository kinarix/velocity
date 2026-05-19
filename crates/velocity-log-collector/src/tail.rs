//! Per-file tail reader.
//!
//! Keeps `(path, offset)` in memory. `read_new_lines` opens the file,
//! seeks to `offset`, reads everything between there and EOF, returns
//! the completed lines + advances `offset` past them. Any partial
//! line at EOF stays buffered for next tick.

use std::io::SeekFrom;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Debug)]
pub struct TailState {
    pub path: PathBuf,
    pub offset: u64,
    pub remainder: Vec<u8>,
}

impl TailState {
    /// Start tailing from the file's current size, so a freshly-opened
    /// file doesn't dump backlog into the processor. Pass `from_zero:
    /// true` only for newly-rotated files where the previous offset
    /// would be past the new EOF.
    pub async fn new(path: PathBuf, from_zero: bool) -> Result<Self> {
        let offset = if from_zero {
            0
        } else {
            tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .with_context(|| format!("stat {}", path.display()))?
        };
        Ok(Self { path, offset, remainder: Vec::new() })
    }

    /// Read everything appended since `offset`. Returns the completed
    /// lines (without trailing `\n`). Detects truncation (file shorter
    /// than `offset`) and resets to read from the new start.
    pub async fn read_new_lines(&mut self) -> Result<Vec<String>> {
        let meta = tokio::fs::metadata(&self.path)
            .await
            .with_context(|| format!("stat {}", self.path.display()))?;
        let len = meta.len();
        if len < self.offset {
            // Truncation (rare for kubelet, but possible for in-place
            // rotation): reset to 0 and read from the new start.
            tracing::debug!(
                path = %self.path.display(),
                old_offset = self.offset,
                new_len = len,
                "file truncated; restarting tail at 0"
            );
            self.offset = 0;
            self.remainder.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        let mut f = File::open(&self.path)
            .await
            .with_context(|| format!("open {}", self.path.display()))?;
        f.seek(SeekFrom::Start(self.offset)).await?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        f.read_to_end(&mut buf).await?;
        self.offset = len;

        // Splice the remainder from the previous tick onto the front so
        // a line that straddled the read boundary is reassembled.
        let mut combined = std::mem::take(&mut self.remainder);
        combined.extend_from_slice(&buf);

        let mut lines = Vec::new();
        let mut start = 0usize;
        for (i, b) in combined.iter().enumerate() {
            if *b == b'\n' {
                let line = &combined[start..i];
                // Trim a trailing \r so CRLF files behave.
                let trimmed =
                    if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
                lines.push(String::from_utf8_lossy(trimmed).into_owned());
                start = i + 1;
            }
        }
        if start < combined.len() {
            self.remainder = combined[start..].to_vec();
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn write(path: &std::path::Path, content: &str) {
        let mut f =
            tokio::fs::OpenOptions::new().create(true).append(true).open(path).await.unwrap();
        f.write_all(content.as_bytes()).await.unwrap();
        // tokio's File::drop is async-finalised — without an explicit
        // flush+sync the kernel hasn't necessarily persisted the bytes
        // by the time the next stat() runs (especially noticeable on
        // macOS APFS, where stat may report stale len).
        f.flush().await.unwrap();
        f.sync_data().await.unwrap();
    }

    /// Each test gets a unique log file inside a private tempdir.
    /// `tempfile::NamedTempFile` keeps an open `File` handle on the
    /// path which races with `OpenOptions::append(true).open(...)` on
    /// macOS — using an explicit tempdir+path sidesteps that entirely.
    async fn fresh_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tail.log");
        tokio::fs::write(&path, b"").await.unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn new_then_append_yields_appended_lines() {
        let (_dir, path) = fresh_path().await;
        // Start tailing from EOF — pre-existing content is ignored.
        write(&path, "pre-existing\n").await;
        let mut state = TailState::new(path.clone(), false).await.unwrap();
        assert_eq!(state.read_new_lines().await.unwrap(), Vec::<String>::new());

        write(&path, "new-line-1\nnew-line-2\n").await;
        let lines = state.read_new_lines().await.unwrap();
        assert_eq!(lines, vec!["new-line-1".to_string(), "new-line-2".to_string()]);
    }

    #[tokio::test]
    async fn straddled_line_reassembled_across_ticks() {
        let (_dir, path) = fresh_path().await;
        let mut state = TailState::new(path.clone(), false).await.unwrap();

        // First half of a line — no newline yet, so nothing emitted.
        write(&path, "partial").await;
        assert_eq!(state.read_new_lines().await.unwrap(), Vec::<String>::new());

        // Second half completes the line.
        write(&path, "-completed\n").await;
        let lines = state.read_new_lines().await.unwrap();
        assert_eq!(lines, vec!["partial-completed".to_string()]);
    }

    #[tokio::test]
    async fn truncation_resets_offset() {
        let (_dir, path) = fresh_path().await;
        write(&path, "long-content-that-fills-the-file\n").await;
        let mut state = TailState::new(path.clone(), true).await.unwrap();
        let _ = state.read_new_lines().await.unwrap();
        let offset_before = state.offset;

        // Replace the file with shorter content. tokio::fs::write
        // creates+truncates+writes atomically, so after this returns
        // the file is exactly the new bytes (no append-mode race).
        tokio::fs::write(&path, b"new-line-after-truncate\n").await.unwrap();
        let new_len = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(
            new_len < offset_before,
            "test precondition: new len {new_len} must be < prior offset {offset_before}"
        );
        let lines = state.read_new_lines().await.unwrap();
        assert_eq!(lines, vec!["new-line-after-truncate".to_string()]);
    }

    #[tokio::test]
    async fn from_zero_reads_pre_existing_content() {
        let (_dir, path) = fresh_path().await;
        write(&path, "line-a\nline-b\n").await;
        let mut state = TailState::new(path.clone(), true).await.unwrap();
        let lines = state.read_new_lines().await.unwrap();
        assert_eq!(lines, vec!["line-a".to_string(), "line-b".to_string()]);
    }
}
