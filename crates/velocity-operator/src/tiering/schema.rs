//! Arrow schema for warm-tier Parquet objects.
//!
//! This MUST stay in lockstep with
//! `velocity_warm_reader::parquet_reader::columns` — the warm-reader
//! decodes objects we write here. If you change the column list, types,
//! or order, also change the reader and bump the on-disk path scheme.
//!
//! Column choices:
//!   - We project only the columns the API's JSON-Patch fold actually
//!     consumes. `id`, `actor`, `source`, `request_id`, `reason` are
//!     deliberately left out — they're audit-only and the warm-reader's
//!     current contract is event reconstruction, not audit replay. If
//!     an audit-replay endpoint lands later, version the path scheme.
//!   - `entity_id` is stored as `Utf8` (hyphenated UUID) so Arrow's
//!     statistics give us cheap predicate pushdown without a custom
//!     binary-comparator path. The cost is ~24 bytes per row vs 16 for
//!     FixedSizeBinary(16) — negligible after dictionary encoding.
//!   - `diff` / `payload` are stored as `Utf8` (JSON-encoded). Arrow
//!     has no native JSON type and Parquet logical-JSON is poorly
//!     supported in readers. Plain UTF-8 with row-group dictionary
//!     compression is the same wire shape DuckDB / Trino would produce.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, TimeUnit};

pub mod columns {
    pub const OCCURRED_AT: &str = "occurred_at";
    pub const SCHEMA_ORG: &str = "schema_org";
    pub const ENTITY_ID: &str = "entity_id";
    pub const OPERATION: &str = "operation";
    pub const DIFF: &str = "diff";
    pub const PAYLOAD: &str = "payload";
}

pub fn arrow_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(
            columns::OCCURRED_AT,
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new(columns::SCHEMA_ORG, DataType::Utf8, false),
        // entity_id is nullable because pre-Phase-3 event-log rows
        // (system-level events not tied to an entity) can leave it
        // NULL. The warm-reader's filter does an equality check, which
        // skips NULL rows automatically — no extra handling required.
        Field::new(columns::ENTITY_ID, DataType::Utf8, true),
        Field::new(columns::OPERATION, DataType::Utf8, false),
        Field::new(columns::DIFF, DataType::Utf8, true),
        Field::new(columns::PAYLOAD, DataType::Utf8, true),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_names_match_warm_reader_contract() {
        // If these strings change, velocity-warm-reader must change
        // too — that's the whole point of this test, to make a drift
        // visible at CI time rather than at warm-read time.
        assert_eq!(columns::OCCURRED_AT, "occurred_at");
        assert_eq!(columns::SCHEMA_ORG, "schema_org");
        assert_eq!(columns::ENTITY_ID, "entity_id");
        assert_eq!(columns::OPERATION, "operation");
        assert_eq!(columns::DIFF, "diff");
        assert_eq!(columns::PAYLOAD, "payload");
    }

    #[test]
    fn schema_has_six_columns_in_documented_order() {
        let s = arrow_schema();
        let names: Vec<&str> = s.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec!["occurred_at", "schema_org", "entity_id", "operation", "diff", "payload"]
        );
    }
}
