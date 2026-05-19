//! Interactive `y/N` prompt shared by `delete` (CRDs) and `restore`
//! (data-plane). Defaults to "no" so a missing TTY or an accidental
//! newline never commits a destructive action. Callers should also
//! expose `--yes` to skip the prompt for pipelines.

use anyhow::{Context, Result};
use std::io::{stdin, stdout, BufRead, Write};

/// Print `prompt [y/N] ` to stderr (so it doesn't pollute piped
/// stdout), read one line from stdin, and return whether the answer
/// was an affirmative `y`/`yes` (case-insensitive). Anything else —
/// including EOF and empty input — returns `false`.
pub(crate) fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt} [y/N] ");
    stdout().flush().ok();
    let mut line = String::new();
    let n = stdin().lock().read_line(&mut line).context("reading confirmation")?;
    if n == 0 {
        // EOF — pipeline / no TTY. Treat as "no" so we never delete
        // when the operator can't actually answer.
        return Ok(false);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}
