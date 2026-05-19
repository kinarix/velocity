//! Warm-tier event reader on top of DataFusion.
//!
//! The query shape today is narrow (range scan on `occurred_at`,
//! equality on `(schema_org, entity_id)`, project a handful of
//! columns), but using DataFusion now means:
//!
//!   - Predicate pushdown via Parquet row-group statistics is
//!     automatic — no hand-rolled `cmp::eq` + `filter_record_batch`
//!     dance.
//!   - Multi-file scans across candidate months go through one
//!     planner, not a per-file loop we maintain.
//!   - A future SQL-over-warm endpoint is one `ctx.sql(...)` call
//!     away; we don't have to rewrite the reader to expose it (see
//!     ADR-004 revision 2026-05-18).
//!
//! Per-request file-existence is checked via `object_store::head` so
//! DataFusion never sees a path that doesn't exist. We could let
//! DataFusion list a prefix instead, but explicit fan-out keeps the
//! per-request cost bounded by `max_months` and surfaces "no warm
//! coverage for this month yet" cleanly as `Vec::new()`.

use std::sync::Arc;

use arrow::array::{Array, StringArray, TimestampMicrosecondArray};
use chrono::{DateTime, TimeZone, Utc};
use datafusion::common::ScalarValue;
use datafusion::execution::context::SessionContext;
use datafusion::logical_expr::{col, lit};
use datafusion::prelude::ParquetReadOptions;
use object_store::{path::Path as ObjPath, ObjectStore, ObjectStoreExt};
use uuid::Uuid;

use crate::error::WarmReaderError;
use crate::object_layout;
use crate::types::EventRow;

/// All the per-request state needed to answer one
/// `POST /v1/warm/events` call.
#[allow(missing_debug_implementations)]
pub struct ReadParams<'a> {
    pub session: &'a SessionContext,
    pub store: Arc<dyn ObjectStore>,
    /// Full URL prefix matching what's registered with the
    /// `SessionContext`'s runtime ObjectStore. Used to construct
    /// per-file URLs for `read_parquet`.
    pub base_url: &'a str,
    pub path: &'a str,
    pub entity_id: Uuid,
    pub until: DateTime<Utc>,
    pub limit: u32,
    pub max_months: u32,
}

#[derive(Debug)]
pub struct ReadOutput {
    pub events: Vec<EventRow>,
    pub objects_scanned: u32,
}

pub async fn read_events(p: ReadParams<'_>) -> Result<ReadOutput, WarmReaderError> {
    // Walk candidate months newest-first, collecting only the ones
    // whose Parquet object already exists. DataFusion's
    // `read_parquet` errors on a missing path, so we filter first.
    let months = object_layout::candidate_months(p.until, p.max_months)
        .map_err(|e| WarmReaderError::BadRequest(format!("{e}")))?;

    let mut existing_urls: Vec<String> = Vec::with_capacity(months.len());
    let mut objects_scanned: u32 = 0;
    for (year, month) in months {
        let key = object_layout::object_key_for_month(p.path, year, month)
            .map_err(|e| WarmReaderError::BadRequest(format!("{e}")))?;
        objects_scanned += 1;
        if object_exists(&p.store, &key).await? {
            existing_urls.push(full_url(p.base_url, &key));
        }
    }

    if existing_urls.is_empty() {
        return Ok(ReadOutput { events: Vec::new(), objects_scanned });
    }

    let df = p
        .session
        .read_parquet(existing_urls, ParquetReadOptions::default())
        .await
        .map_err(|e| WarmReaderError::Parquet(format!("read_parquet: {e}")))?;

    // DataFusion's logical filter expressions. Note the typed
    // timestamp scalar — without the explicit `TimestampMicrosecond`
    // type the planner would try to compare a microsecond column to
    // an i64 literal and fail with a type-mismatch error similar to
    // the one Arrow's compute kernel raises directly.
    let until_micros = p.until.timestamp_micros();
    let until_scalar = ScalarValue::TimestampMicrosecond(Some(until_micros), Some("UTC".into()));
    let entity_str = p.entity_id.hyphenated().to_string();

    let effective_limit = (p.limit as usize).min(crate::types::MAX_LIMIT as usize);

    let df = df
        .filter(col("schema_org").eq(lit(p.path)))
        .and_then(|d| d.filter(col("entity_id").eq(lit(entity_str.clone()))))
        .and_then(|d| d.filter(col("occurred_at").lt_eq(lit(until_scalar.clone()))))
        .and_then(|d| {
            d.select(vec![col("occurred_at"), col("operation"), col("diff"), col("payload")])
        })
        .and_then(|d| d.sort(vec![col("occurred_at").sort(false, false)]))
        .and_then(|d| d.limit(0, Some(effective_limit)))
        .map_err(|e| WarmReaderError::Parquet(format!("plan: {e}")))?;

    let batches =
        df.collect().await.map_err(|e| WarmReaderError::Parquet(format!("execute: {e}")))?;

    let mut events: Vec<EventRow> = Vec::new();
    for batch in &batches {
        decode_batch(batch, &mut events)?;
    }

    Ok(ReadOutput { events, objects_scanned })
}

async fn object_exists(
    store: &Arc<dyn ObjectStore>,
    key: &ObjPath,
) -> Result<bool, WarmReaderError> {
    match store.head(key).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(WarmReaderError::Storage(format!("HEAD {key}: {e}"))),
    }
}

fn full_url(base: &str, key: &ObjPath) -> String {
    let trimmed = base.trim_end_matches('/');
    format!("{trimmed}/{key}")
}

fn decode_batch(
    batch: &arrow::record_batch::RecordBatch,
    out: &mut Vec<EventRow>,
) -> Result<(), WarmReaderError> {
    // Column ordering follows the projection we asked DataFusion for;
    // do NOT assume the writer's column order, since DataFusion may
    // re-order during planning.
    let ts = column_as::<TimestampMicrosecondArray>(batch, "occurred_at")?;
    let op = column_as::<StringArray>(batch, "operation")?;
    let diff = column_as::<StringArray>(batch, "diff")?;
    let payload = column_as::<StringArray>(batch, "payload")?;

    for i in 0..batch.num_rows() {
        let micros = ts.value(i);
        let occurred_at = Utc
            .timestamp_micros(micros)
            .single()
            .ok_or_else(|| WarmReaderError::Parquet(format!("invalid timestamp at row {i}")))?;
        out.push(EventRow {
            occurred_at,
            operation: op.value(i).to_string(),
            diff: parse_json_col(diff, i)?,
            payload: parse_json_col(payload, i)?,
        });
    }
    Ok(())
}

fn column_as<'a, A: 'static>(
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a A, WarmReaderError> {
    let col = batch
        .column_by_name(name)
        .ok_or_else(|| WarmReaderError::Parquet(format!("result missing column `{name}`")))?;
    col.as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| WarmReaderError::Parquet(format!("column `{name}` wrong type")))
}

fn parse_json_col(
    col: &StringArray,
    i: usize,
) -> Result<Option<serde_json::Value>, WarmReaderError> {
    if col.is_null(i) {
        return Ok(None);
    }
    let s = col.value(i);
    if s.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(s)
        .map(Some)
        .map_err(|e| WarmReaderError::Parquet(format!("json decode at row {i}: {e}")))
}
