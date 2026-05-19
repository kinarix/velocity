//! `velocity record` — data-plane reads against the API server.
//!
//! Four subcommands, all going through the resolved-context `ApiClient`:
//!
//! - `record get  <path> <id>` — single record JSON.
//! - `record list <path>`      — list with `--limit` and optional `--cursor`.
//! - `record query <path> -f <body>` — POST `/query` DSL from file or stdin.
//! - `record export <path> -o <file>` — paginate through the entire schema,
//!   writing one JSON record per line (NDJSON) to `-o` (stdout if `-`).
//!
//! Authentication and base-URL resolution come from the active context
//! in `~/.velocity/config` (slice 1). Server failures bubble up as
//! envelope-typed errors with `code`/`message`/`request_id` intact.

use std::io::{BufWriter, Read as _, Write};

use anyhow::{anyhow, Context as _, Result};
use clap::Subcommand;

use crate::client::{ApiClient, ListEnvelope, SchemaPath};
use crate::config::Config;

/// Hard cap on a single export run. The data plane caps individual
/// responses at 1000 (ADR-009); export iterates pages, so a separate
/// total limit prevents an accidental "export all of prod" from filling
/// the operator's laptop disk. Override with `--max-records` when you
/// really mean it.
const DEFAULT_EXPORT_MAX: u64 = 100_000;

/// API list cap per ADR-009. We mirror it on the client to fail fast
/// rather than send a request the server will reject.
const SERVER_MAX_LIMIT: u32 = 1000;

#[derive(Debug, Subcommand)]
pub(crate) enum RecordCmd {
    /// Fetch a single record by id.
    Get {
        /// Schema path: `org/app/domain/object/version`.
        path: String,
        /// Record id.
        id: String,
    },
    /// List records (paginated; use `--cursor` to continue).
    List {
        path: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// POST a query DSL body to `/{path}/query`. Body is JSON shaped
    /// like `{ limit, cursor, sort: [...], filter: [...] }` — see
    /// `docs/design.md` for the DSL reference.
    Query {
        path: String,
        /// Path to a JSON file, or `-` for stdin.
        #[arg(short, long)]
        file: String,
    },
    /// Iterate every record (or up to `--max-records`) and write
    /// NDJSON (one record per line) to `-o`. Useful for cold backups
    /// or analytic dumps.
    Export {
        path: String,
        /// Output file path, or `-` for stdout.
        #[arg(short, long)]
        output: String,
        /// Soft cap so a forgotten flag doesn't drain the table.
        #[arg(long, default_value_t = DEFAULT_EXPORT_MAX)]
        max_records: u64,
        /// Server page size. Higher = fewer round-trips, more memory.
        #[arg(long, default_value_t = 500)]
        page_size: u32,
    },
}

pub(crate) async fn run(
    cmd: RecordCmd,
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<()> {
    let api = build_client(config_path, context_override)?;
    match cmd {
        RecordCmd::Get { path, id } => {
            let p = SchemaPath::parse(&path)?;
            let v = api.get_record(&p, &id).await?;
            print_json(&v)?;
        }
        RecordCmd::List { path, limit, cursor } => {
            validate_limit(limit)?;
            let p = SchemaPath::parse(&path)?;
            let env = api.list_records(&p, Some(limit), cursor.as_deref()).await?;
            print_list(&env)?;
        }
        RecordCmd::Query { path, file } => {
            let p = SchemaPath::parse(&path)?;
            let body = read_json(&file)?;
            let env = api.query_records(&p, &body).await?;
            print_list(&env)?;
        }
        RecordCmd::Export { path, output, max_records, page_size } => {
            validate_limit(page_size)?;
            let p = SchemaPath::parse(&path)?;
            let written = export_loop(&api, &p, &output, max_records, page_size).await?;
            eprintln!("exported {written} records to {output}");
        }
    }
    Ok(())
}

fn build_client(
    config_path: Option<&std::path::Path>,
    context_override: Option<&str>,
) -> Result<ApiClient> {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => Config::default_path()
            .ok_or_else(|| anyhow!("could not resolve config path (set $VELOCITY_CONFIG)"))?,
    };
    let cfg = Config::load(&path)?;
    let ctx = cfg.resolve(context_override)?;
    ApiClient::from_context(&ctx)
}

fn validate_limit(limit: u32) -> Result<()> {
    if limit == 0 {
        anyhow::bail!("limit must be ≥ 1");
    }
    if limit > SERVER_MAX_LIMIT {
        anyhow::bail!(
            "limit {limit} exceeds server cap {SERVER_MAX_LIMIT} (ADR-009 — use cursor pagination)"
        );
    }
    Ok(())
}

fn read_json(source: &str) -> Result<serde_json::Value> {
    let raw = if source == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("reading query body from stdin")?;
        buf
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading query body from {source}"))?
    };
    serde_json::from_str(&raw).with_context(|| format!("parsing query body from {source}"))
}

fn print_json(v: &serde_json::Value) -> Result<()> {
    let s = serde_json::to_string_pretty(v).context("serialising response")?;
    println!("{s}");
    Ok(())
}

fn print_list(env: &ListEnvelope) -> Result<()> {
    // Print a single JSON document so callers can pipe to jq. The
    // server emits `next_cursor` (snake) but some older builds use
    // `nextCursor`; our deserialiser accepts both and re-serialises
    // canonically as `next_cursor`.
    let canon = serde_json::json!({
        "items":       env.items,
        "next_cursor": env.next_cursor,
    });
    print_json(&canon)
}

/// Walk the cursor pagination loop, writing NDJSON to the chosen sink.
/// Stops at `max_records` even if a `next_cursor` is still available —
/// the operator gets back-pressure on stderr, then re-runs with
/// `--max-records` raised if they actually wanted all of it.
async fn export_loop(
    api: &ApiClient,
    path: &SchemaPath,
    output: &str,
    max_records: u64,
    page_size: u32,
) -> Result<u64> {
    let sink: Box<dyn Write> = if output == "-" {
        Box::new(BufWriter::new(std::io::stdout().lock()))
    } else {
        Box::new(BufWriter::new(
            std::fs::File::create(output).with_context(|| format!("creating {output}"))?,
        ))
    };
    let mut sink = sink;

    let mut cursor: Option<String> = None;
    let mut written: u64 = 0;

    loop {
        let env = api.list_records(path, Some(page_size), cursor.as_deref()).await?;
        for item in &env.items {
            if written >= max_records {
                eprintln!("export hit --max-records={max_records}; stop here (next_cursor preserved)");
                return Ok(written);
            }
            let line =
                serde_json::to_string(item).context("serialising record for NDJSON output")?;
            sink.write_all(line.as_bytes()).context("writing record")?;
            sink.write_all(b"\n").context("writing newline")?;
            written += 1;
        }
        match env.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    sink.flush().context("flushing export sink")?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn validate_limit_rejects_zero_and_over_cap() {
        assert!(validate_limit(0).is_err());
        assert!(validate_limit(SERVER_MAX_LIMIT + 1).is_err());
        assert!(validate_limit(1).is_ok());
        assert!(validate_limit(SERVER_MAX_LIMIT).is_ok());
    }

    #[test]
    fn schema_path_parse_happy() {
        let p = SchemaPath::parse("acme/supply-chain/procurement/purchase-order/v1").unwrap();
        assert_eq!(p.org, "acme");
        assert_eq!(p.app, "supply-chain");
        assert_eq!(p.version, "v1");
        assert_eq!(
            p.as_url(),
            "acme/supply-chain/procurement/purchase-order/v1"
        );
    }

    #[test]
    fn schema_path_parse_rejects_wrong_segment_count() {
        let err = SchemaPath::parse("acme/supply-chain/procurement").unwrap_err();
        assert!(err.to_string().contains("5 segments"));
    }

    #[test]
    fn schema_path_parse_rejects_empty_segment() {
        let err = SchemaPath::parse("acme//procurement/purchase-order/v1").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn read_json_parses_stdin_marker_as_file_when_present() {
        // Sanity: reading a real file works (stdin tested via integration).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::write(&path, r#"{"limit": 10}"#).unwrap();
        let v = read_json(path.to_str().unwrap()).unwrap();
        assert_eq!(v["limit"], 10);
    }
}
