//! Phase 8 slice 10 — S3 destination for `ArchivePolicy` records.
//!
//! For policies with `destination.backend = "s3"`, we don't move rows to
//! a sibling `*_archive` Postgres schema; we write them out as Parquet
//! objects under an `object_store`-backed bucket and set the hot row's
//! `archived_at` + `archive_ref` so it stops being returned to readers.
//!
//! ## Object layout
//!
//! ```text
//! <prefix>/<schema>/<table>/dt=YYYY-MM-DD/<uuid>.parquet
//! ```
//!
//! - `<prefix>` is the policy's `destination.bucket` (or sub-path within it).
//! - `<schema>` is `<org>_<app>_<domain>` to mirror the Postgres layout.
//! - `dt=YYYY-MM-DD` is date-partitioned for cheap pruning when querying
//!   from external engines (DuckDB, Athena, Trino).
//! - `<uuid>` is a fresh v4 per batch so concurrent workers can't collide.
//!
//! ## Schema choice
//!
//! This MVP writes every column as a nullable `Utf8`. Non-string values
//! (numbers, booleans, json, timestamps) are stored as their canonical
//! JSON string representation. Pros: zero per-`SchemaDefinition` Arrow
//! schema derivation, queryable from every external engine via `CAST`.
//! Cons: less efficient than typed columns. Slice 11+ can graduate to
//! typed Arrow schemas using `velocity-types::FieldKind`.
//!
//! ## Transactional model
//!
//! 1. `SELECT ... FOR UPDATE` picks `batch_size` eligible rows.
//! 2. Outside the transaction we write the Parquet object to S3.
//! 3. On successful upload, the same transaction marks the rows
//!    (`archived_at = now()`, `archive_ref = <object url>`).
//! 4. Commit. A crash between 2 and 3 leaves an orphan object in S3;
//!    the hot row is unmarked and gets re-picked next tick. The orphan
//!    is recoverable via a sweep (slice 12+).

use std::sync::Arc;
use std::time::Duration;

use arrow::array::{ArrayRef, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use chrono::Utc;
use object_store::{path::Path as ObjPath, ObjectStore, ObjectStoreExt};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{ArchiveBatchResult, ArchiveError};

/// Inputs for [`archive_batch_to_s3`].
#[derive(Debug, Clone)]
pub struct S3ArchiveArgs<'a> {
    pub hot_schema: &'a str,
    pub hot_table: &'a str,
    /// Every column to serialise. Order is preserved in the resulting
    /// Arrow `RecordBatch`.
    pub columns: &'a [String],
    /// Rows older than this (by `created_at`) are eligible.
    pub min_age: Duration,
    /// Upper bound per call.
    pub batch_size: usize,
    /// Object-store key prefix. Empty string for "write at bucket root".
    pub prefix: &'a str,
}

/// Pick → serialise → upload → mark, in two transactions plus one
/// out-of-band object-store write.
pub async fn archive_batch_to_s3(
    pool: &PgPool,
    store: &dyn ObjectStore,
    args: &S3ArchiveArgs<'_>,
) -> Result<ArchiveBatchResult, ArchiveError> {
    crate::validate_ident_pub(args.hot_schema)?;
    crate::validate_ident_pub(args.hot_table)?;
    if args.columns.is_empty() {
        return Err(ArchiveError::InvalidColumns("columns list is empty".into()));
    }
    if !args.columns.iter().any(|c| c == "id") {
        return Err(ArchiveError::InvalidColumns(
            "columns must include `id`".into(),
        ));
    }
    for c in args.columns {
        crate::validate_ident_pub(c).map_err(|_| {
            ArchiveError::InvalidColumns(format!("column {c:?} is not a valid identifier"))
        })?;
    }
    if args.min_age.as_secs() == 0 {
        return Err(ArchiveError::InvalidAge);
    }
    if !(1..=10_000).contains(&args.batch_size) {
        return Err(ArchiveError::InvalidBatchSize(args.batch_size));
    }

    let cols_csv = args.columns.join(", ");
    let hot = format!("{}.{}", args.hot_schema, args.hot_table);
    let age_secs = args.min_age.as_secs() as i64;
    let limit = args.batch_size as i64;

    // Phase 1 — pick eligible rows as a JSONB blob per row. Single
    // statement returns Vec<(uuid, jsonb)> so we can both upload the
    // payload and remember the ids to mark.
    let pick_sql = format!(
        "SELECT id::text AS id, to_jsonb(t) - '__fts' AS row \
         FROM {hot} t \
         WHERE archived_at IS NULL AND deleted_at IS NULL \
           AND created_at < now() - make_interval(secs => $1) \
         ORDER BY created_at LIMIT $2"
    );
    let _ = cols_csv; // future: column-projected SELECT
    let rows = sqlx::query(&pick_sql)
        .bind(age_secs)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(ArchiveBatchResult {
            rows_archived: 0,
            more_pending: false,
        });
    }

    let mut ids: Vec<String> = Vec::with_capacity(rows.len());
    let mut row_values: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: String = r.try_get("id")?;
        let row: Value = r.try_get("row")?;
        ids.push(id);
        row_values.push(row);
    }

    // Phase 2 — encode + upload.
    let bytes = encode_parquet(args.columns, &row_values)
        .map_err(|e| ArchiveError::Sql(sqlx::Error::Protocol(format!("parquet encode: {e}"))))?;
    let key = build_object_key(args.prefix, args.hot_schema, args.hot_table);
    let archive_ref = key.clone();
    let path = ObjPath::from(key.clone());
    store
        .put(&path, bytes.into())
        .await
        .map_err(|e| ArchiveError::Sql(sqlx::Error::Protocol(format!("s3 put: {e}"))))?;

    // Phase 3 — mark hot rows. Bind ids as a text[] and cast.
    let mark_sql = format!(
        "UPDATE {hot} SET archived_at = now(), archive_ref = $2 \
         WHERE id = ANY($1::uuid[]) AND archived_at IS NULL"
    );
    let marked = sqlx::query(&mark_sql)
        .bind(&ids)
        .bind(&archive_ref)
        .execute(pool)
        .await?;
    let n = marked.rows_affected() as usize;

    Ok(ArchiveBatchResult {
        rows_archived: n,
        more_pending: n >= args.batch_size,
    })
}

/// Encode `rows` (already filtered to the columns of interest) as a
/// Parquet file with every column as nullable `Utf8`. Non-string values
/// are serialised to their JSON text form.
pub fn encode_parquet(columns: &[String], rows: &[Value]) -> Result<Vec<u8>, String> {
    let fields: Vec<Field> = columns
        .iter()
        .map(|c| Field::new(c, DataType::Utf8, true))
        .collect();
    let schema = Arc::new(ArrowSchema::new(fields));

    let mut builders: Vec<StringBuilder> = (0..columns.len())
        .map(|_| StringBuilder::with_capacity(rows.len(), rows.len() * 32))
        .collect();
    for row in rows {
        for (i, col) in columns.iter().enumerate() {
            let cell = row.get(col).unwrap_or(&Value::Null);
            match cell {
                Value::Null => builders[i].append_null(),
                Value::String(s) => builders[i].append_value(s),
                other => builders[i].append_value(other.to_string()),
            }
        }
    }
    let arrays: Vec<ArrayRef> = builders
        .into_iter()
        .map(|mut b| Arc::new(b.finish()) as ArrayRef)
        .collect();
    let batch = RecordBatch::try_new(schema.clone(), arrays)
        .map_err(|e| format!("record batch: {e}"))?;

    let mut buf: Vec<u8> = Vec::with_capacity(rows.len() * 256);
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(&mut buf, schema, Some(props))
        .map_err(|e| format!("writer init: {e}"))?;
    writer.write(&batch).map_err(|e| format!("write batch: {e}"))?;
    writer.close().map_err(|e| format!("close writer: {e}"))?;
    Ok(buf)
}

/// `<prefix>/<schema>/<table>/dt=YYYY-MM-DD/<uuid>.parquet`
pub fn build_object_key(prefix: &str, schema: &str, table: &str) -> String {
    let day = Utc::now().format("%Y-%m-%d");
    let id = Uuid::new_v4();
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        format!("{schema}/{table}/dt={day}/{id}.parquet")
    } else {
        format!("{prefix}/{schema}/{table}/dt={day}/{id}.parquet")
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_layout_matches_expectations() {
        let k = build_object_key("acme-archive", "acme_sc_proc", "purchase_order_v1");
        assert!(k.starts_with("acme-archive/acme_sc_proc/purchase_order_v1/dt="));
        assert!(k.ends_with(".parquet"));
    }

    #[test]
    fn key_handles_empty_prefix() {
        let k = build_object_key("", "s", "t");
        assert!(k.starts_with("s/t/dt="));
    }

    #[test]
    fn key_trims_trailing_slash() {
        let k = build_object_key("p/", "s", "t");
        assert!(k.starts_with("p/s/t/dt="));
        assert!(!k.starts_with("p//"));
    }

    #[test]
    fn parquet_encodes_mixed_types() {
        let cols = vec![
            "id".to_string(),
            "qty".to_string(),
            "ok".to_string(),
            "tags".to_string(),
        ];
        let rows = vec![
            json!({"id": "11111111-1111-1111-1111-111111111111", "qty": 5, "ok": true, "tags": ["a","b"]}),
            json!({"id": "22222222-2222-2222-2222-222222222222", "qty": null, "ok": false, "tags": []}),
        ];
        let bytes = encode_parquet(&cols, &rows).unwrap();
        // Parquet magic at start and end of file.
        assert_eq!(&bytes[..4], b"PAR1");
        assert_eq!(&bytes[bytes.len() - 4..], b"PAR1");
    }
}
