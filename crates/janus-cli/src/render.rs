//! Human and JSON output.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::error::CliError;

/// Write `text` to stdout. A closed pipe is [`CliError::StdoutClosed`].
pub fn out(text: &str) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::BrokenPipe => CliError::StdoutClosed,
            _ => CliError::failed("stdout", format!("cannot write to standard output: {e}")),
        })
}

/// A JSON document, pretty-printed, on its own lines.
pub fn json(value: &Value) -> Result<(), CliError> {
    out(&format!("{value:#}\n"))
}

/// A JSON document on one line (`log tail --json`: one record per line).
pub fn json_line(value: &Value) -> Result<(), CliError> {
    out(&format!("{value}\n"))
}

/// A note for the operator, on stderr so stdout stays parseable.
pub fn note(text: &str) {
    eprintln!("{text}");
}

pub fn time(t: Option<DateTime<Utc>>) -> String {
    t.map_or_else(
        || "-".to_owned(),
        |t| t.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

/// A serde enum's wire name (`api_key`, `rate_limited`, `no_material`).
pub fn name<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "-".to_owned(),
    }
}

pub fn text(t: Option<&str>) -> String {
    match t {
        Some(t) if !t.is_empty() => t.to_owned(),
        _ => "-".to_owned(),
    }
}

/// A counter that should be zero. Zero prints as `0`; anything else is
/// flagged with `!` and the reason, in bold red when stdout is a terminal
/// and `NO_COLOR` is unset.
pub fn alert_if_nonzero(count: u64, why: &str) -> String {
    use std::io::IsTerminal;
    if count == 0 {
        return "0".to_owned();
    }
    let text = format!("{count}  ! {why}");
    if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[1;31m{text}\x1b[0m")
    } else {
        text
    }
}

/// `key  value` lines, keys aligned.
pub fn fields(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(k, v)| format!("{k:<width$}  {v}\n"))
        .collect()
}

/// A column-aligned table with a header row.
pub struct Table {
    header: Vec<&'static str>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(header: &[&'static str]) -> Self {
        Table {
            header: header.to_vec(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn render(&self) -> String {
        let mut widths: Vec<usize> = self.header.iter().map(|h| h.len()).collect();
        for row in &self.rows {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(cell.chars().count());
            }
        }
        let line = |cells: Vec<&str>| {
            let last = cells.len().saturating_sub(1);
            let mut s = String::new();
            for (i, (cell, w)) in cells.iter().zip(&widths).enumerate() {
                if i == last {
                    s.push_str(cell);
                } else {
                    let pad = w - cell.chars().count();
                    s.push_str(cell);
                    s.push_str(&" ".repeat(pad + 2));
                }
            }
            s.push('\n');
            s
        };
        let mut s = line(self.header.clone());
        for row in &self.rows {
            s.push_str(&line(row.iter().map(String::as_str).collect()));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonzero_counter_is_flagged_and_zero_is_plain() {
        assert_eq!(alert_if_nonzero(0, "why"), "0");
        // Not a terminal under test, so no colour codes.
        assert_eq!(alert_if_nonzero(3, "why"), "3  ! why");
    }

    #[test]
    fn table_aligns_columns_and_fields_align_keys() {
        let mut t = Table::new(&["REF", "STATUS"]);
        t.row(vec!["or-main".into(), "active".into()]);
        t.row(vec!["x".into(), "disabled".into()]);
        assert_eq!(
            t.render(),
            "REF      STATUS\nor-main  active\nx        disabled\n"
        );
        assert_eq!(
            fields(&[("a", "1".into()), ("long", "2".into())]),
            "a     1\nlong  2\n"
        );
        assert_eq!(time(None), "-");
        assert_eq!(text(Some("")), "-");
    }
}
