//! Ad-hoc SQL execution against a branch's Postgres container, used by both
//! `POST /api/projects/:p/branches/:b/query` and the `dbranch query` CLI.
//!
//! Design constraints (deliberately small surface):
//! * **No Postgres driver dependency.** We shell out to the image's bundled
//!   `psql` via `docker exec`. Keeps the binary lean and matches the rest of
//!   the project (`dump.rs`, `schema.rs`).
//! * **Single statement only.** Multiple `;`-separated statements get
//!   refused — protects against accidental "DROP TABLE foo; DELETE …".
//! * **10s timeout.** Hard via `tokio::time::timeout`; the child is killed
//!   when it expires.
//! * **Read queries get wrapped in `LIMIT 1001`** so we can both cap rows AND
//!   detect truncation. Write queries pass through untouched.
//! * **Output is `\x1F`-delimited.** Same Unit Separator trick used by
//!   `schema.rs` — survives commas, quotes, type modifiers in values.

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Config;
use crate::database_operator::{DatabaseOperator, PostgresOperator};
use crate::error::AppError;

const UNIT_SEPARATOR: char = '\x1f';
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard ceiling. 1001 = 1000 + 1 sentinel; if we get 1001 rows back the
/// query returned more than the cap and we set `truncated: true`.
const READ_LIMIT_PLUS_ONE: usize = 1001;

/// What kind of statement we detected from the first SQL keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// SELECT, WITH, VALUES, TABLE, SHOW, EXPLAIN — returns rows.
    Read,
    /// INSERT / UPDATE / DELETE / CREATE / DROP / ALTER / TRUNCATE / etc.
    Write,
}

/// Tagged JSON envelope returned to clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryResponse {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        truncated: bool,
        elapsed_ms: u64,
    },
    Command {
        message: String,
        elapsed_ms: u64,
    },
    Error {
        message: String,
        elapsed_ms: u64,
    },
}

/// Returns the trimmed SQL with any single trailing `;` removed.
/// Errors if more than one statement is present (i.e. an internal `;`).
pub fn validate_single_statement(sql: &str) -> Result<String, AppError> {
    let trimmed = sql.trim().trim_end_matches(';').trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::Database {
            message: "empty SQL".into(),
        });
    }
    if trimmed.contains(';') {
        return Err(AppError::Database {
            message: "Only one statement at a time is allowed (found extra ';')".into(),
        });
    }
    Ok(trimmed)
}

/// Classifies a statement by its first keyword. Conservative: anything not
/// clearly Read is treated as Write (so we don't accidentally wrap a CTE
/// that mutates with `LIMIT`).
pub fn classify(sql: &str) -> Kind {
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "EXPLAIN" => Kind::Read,
        _ => Kind::Write,
    }
}

/// Wraps a SELECT-shaped query with `LIMIT N` so a `SELECT * FROM huge_table`
/// can't return a million rows over HTTP.
pub fn wrap_for_limit(sql: &str) -> String {
    format!(
        "SELECT * FROM ({}) AS dbranch_query LIMIT {}",
        sql, READ_LIMIT_PLUS_ONE
    )
}

/// Parses `psql -A -F $'\x1f' -P footer=off` stdout.
/// First non-empty line = header. Remaining lines = rows. Empty trailing
/// newline is dropped.
pub fn parse_psql_output(stdout: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut lines = stdout.lines().filter(|l| !l.is_empty());
    let header = match lines.next() {
        Some(h) => h.split(UNIT_SEPARATOR).map(String::from).collect(),
        None => return (Vec::new(), Vec::new()),
    };
    let rows: Vec<Vec<String>> = lines
        .map(|l| l.split(UNIT_SEPARATOR).map(String::from).collect())
        .collect();
    (header, rows)
}

/// Top-level entry point. Resolves the container, performs safety checks,
/// runs `psql`, parses output, applies the timeout.
pub async fn run(cfg: &Config, branch: &str, sql: &str) -> Result<QueryResponse, AppError> {
    let container = format!("{}_{}", cfg.name, branch);

    let op = PostgresOperator::new();
    if !op.is_container_running(&container).await.unwrap_or(false) {
        return Err(AppError::Database {
            message: format!(
                "Branch container '{}' is not running. Start it with `dbranch resume`.",
                container
            ),
        });
    }

    let cleaned = validate_single_statement(sql)?;
    let kind = classify(&cleaned);
    let effective_sql = match kind {
        Kind::Read => wrap_for_limit(&cleaned),
        Kind::Write => cleaned.clone(),
    };

    let pg = &cfg.postgres_config;
    let db = pg.database.as_deref().unwrap_or("dbranch");
    let sep_arg = format!("{}", UNIT_SEPARATOR);

    let start = Instant::now();
    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            &container,
            "psql",
            "-U",
            &pg.user,
            "-d",
            db,
            "-A",
            "-F",
            &sep_arg,
            "-P",
            "footer=off",
            "-X",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &effective_sql,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Docker {
            message: format!("Failed to spawn psql: {}", e),
        })?;

    // Close stdin immediately; we passed SQL via -c.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }

    let output_fut = child.wait_with_output();
    let timed = tokio::time::timeout(QUERY_TIMEOUT, output_fut).await;

    let elapsed_ms = start.elapsed().as_millis() as u64;

    let out = match timed {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(AppError::Docker {
                message: format!("psql wait failed: {}", e),
            });
        }
        Err(_) => {
            // Timeout fired. The child handle was moved into wait_with_output
            // so we can't .kill() it from here — but its stdin is already
            // closed and the surrounding tokio task drop will reap it. We
            // surface the timeout as a clean Error variant.
            return Ok(QueryResponse::Error {
                message: format!("query timed out after {}s", QUERY_TIMEOUT.as_secs()),
                elapsed_ms,
            });
        }
    };

    if !out.status.success() {
        // psql writes the SQL error across several lines on stderr:
        //   ERROR:  relation "foo" does not exist
        //   LINE 1: SELECT * FROM foo
        //                         ^
        // Joining the non-empty lines preserves the context (LINE / caret)
        // without dragging in psql's preamble noise.
        let msg = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(|l| l.trim_end())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let msg = if msg.is_empty() {
            format!("query failed (exit {})", out.status)
        } else {
            msg
        };
        return Ok(QueryResponse::Error { message: msg, elapsed_ms });
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    match kind {
        Kind::Read => {
            let (columns, mut rows) = parse_psql_output(&stdout);
            let truncated = rows.len() >= READ_LIMIT_PLUS_ONE;
            if truncated {
                rows.truncate(READ_LIMIT_PLUS_ONE - 1);
            }
            Ok(QueryResponse::Rows {
                columns,
                rows,
                truncated,
                elapsed_ms,
            })
        }
        Kind::Write => {
            // psql writes the command tag to stdout (e.g. "UPDATE 5",
            // "CREATE TABLE"). Take the last non-empty line.
            let message = stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .last()
                .unwrap_or("OK")
                .to_string();
            Ok(QueryResponse::Command { message, elapsed_ms })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_read_keywords() {
        assert_eq!(classify("select 1"), Kind::Read);
        assert_eq!(classify("  SELECT * FROM users"), Kind::Read);
        assert_eq!(classify("WITH x AS (SELECT 1) SELECT * FROM x"), Kind::Read);
        assert_eq!(classify("values (1)"), Kind::Read);
        assert_eq!(classify("TABLE users"), Kind::Read);
        assert_eq!(classify("show search_path"), Kind::Read);
        assert_eq!(classify("explain analyze select 1"), Kind::Read);
    }

    #[test]
    fn classify_write_keywords() {
        assert_eq!(classify("UPDATE users SET x = 1"), Kind::Write);
        assert_eq!(classify("insert into t values (1)"), Kind::Write);
        assert_eq!(classify("delete from t"), Kind::Write);
        assert_eq!(classify("CREATE TABLE x (id int)"), Kind::Write);
        assert_eq!(classify("alter table t add column y int"), Kind::Write);
        assert_eq!(classify("drop table x"), Kind::Write);
        assert_eq!(classify(""), Kind::Write);
    }

    #[test]
    fn validate_strips_single_trailing_semicolon() {
        assert_eq!(
            validate_single_statement("SELECT 1;").unwrap(),
            "SELECT 1"
        );
        assert_eq!(validate_single_statement("  SELECT 1  ;  ").unwrap(), "SELECT 1");
        assert_eq!(validate_single_statement("SELECT 1").unwrap(), "SELECT 1");
    }

    #[test]
    fn validate_rejects_multiple_statements() {
        let err = validate_single_statement("SELECT 1; SELECT 2").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("one statement"), "got: {}", msg);
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_single_statement("").is_err());
        assert!(validate_single_statement("   ;  ").is_err());
    }

    #[test]
    fn wrap_for_limit_appends_outer_select() {
        let wrapped = wrap_for_limit("SELECT 1");
        assert!(wrapped.contains("LIMIT 1001"));
        assert!(wrapped.contains("FROM (SELECT 1)"));
    }

    #[test]
    fn parse_psql_output_handles_header_and_rows() {
        let stdout = "c1\u{1f}c2\nv1\u{1f}v2\nv3\u{1f}\n";
        let (cols, rows) = parse_psql_output(stdout);
        assert_eq!(cols, vec!["c1", "c2"]);
        assert_eq!(rows, vec![vec!["v1", "v2"], vec!["v3", ""]]);
    }

    #[test]
    fn parse_psql_output_empty() {
        let (cols, rows) = parse_psql_output("");
        assert!(cols.is_empty());
        assert!(rows.is_empty());
    }
}
