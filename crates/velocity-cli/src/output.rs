//! Stdout formatters shared across subcommands.
//!
//! Two formats:
//!   - `Table`: human-readable, column-padded, headers in a separator
//!     row. No external table crate to keep the binary small.
//!   - `Json`: stable shape, one JSON document per invocation. Suitable
//!     for jq / scripts.
//!
//! Subcommands construct a `Vec<Vec<String>>` of rows + a header row,
//! then call `print(...)` once. No streaming — the entire row set is
//! materialised before output so `table` can size columns.

use clap::ValueEnum;
use serde_json::Value;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum OutputFormat {
    Table,
    Json,
}

/// Render rows. `headers` and each row must be the same length; the
/// caller is responsible for keeping that invariant.
pub(crate) fn print(headers: &[&str], rows: &[Vec<String>], format: OutputFormat) {
    match format {
        OutputFormat::Table => print_table(headers, rows),
        OutputFormat::Json => print_json(headers, rows),
    }
}

/// Render a single key/value document (e.g. `status` summary block).
/// Currently unused — kept for the upcoming `velocity status` summary
/// header (org/app/domain counts) so subcommands can mix tabular and
/// scalar output without re-implementing the formatter.
#[allow(dead_code)]
pub(crate) fn print_kv(pairs: &[(&str, String)], format: OutputFormat) {
    match format {
        OutputFormat::Table => {
            let label_w = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            for (k, v) in pairs {
                println!("{k:<label_w$}  {v}");
            }
        }
        OutputFormat::Json => {
            let mut obj = serde_json::Map::with_capacity(pairs.len());
            for (k, v) in pairs {
                obj.insert((*k).to_string(), Value::String(v.clone()));
            }
            // Best-effort serialise; if this fails it's a bug in the
            // caller, not user-visible state.
            match serde_json::to_string_pretty(&Value::Object(obj)) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("failed to serialise output as JSON: {e}"),
            }
        }
    }
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() && cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }
    print_row(headers.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &widths);
    // Separator row using `─` so it lines up with the header. Plain
    // dashes are fine for ASCII terminals — `─` matches kubectl's style
    // and renders cleanly in every Unicode-aware terminal.
    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    print_row(sep, &widths);
    for row in rows {
        print_row(row.clone(), &widths);
    }
}

fn print_row(cells: Vec<String>, widths: &[usize]) {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(cell.len());
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!("{cell:<w$}"));
    }
    println!("{out}");
}

fn print_json(headers: &[&str], rows: &[Vec<String>]) {
    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::with_capacity(headers.len());
        for (i, h) in headers.iter().enumerate() {
            obj.insert(
                (*h).to_string(),
                Value::String(row.get(i).cloned().unwrap_or_default()),
            );
        }
        out.push(Value::Object(obj));
    }
    match serde_json::to_string_pretty(&Value::Array(out)) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialise output as JSON: {e}"),
    }
}
