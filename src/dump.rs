//! pg_dump / pg_restore via `docker exec`.
//!
//! Streams data between the host filesystem and the container without
//! buffering the full dump in memory — essential for databases of any size.
//!
//! These helpers shell out to the `docker` binary (via [`tokio::process::Command`])
//! rather than going through `docker-wrapper`'s `ExecCommand` because the
//! latter collects all of stdout/stderr into a `String` before returning,
//! which doesn't scale.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy};
use tokio::process::Command;
use tracing::{debug, info};

use crate::config::Config;
use crate::database_operator::{DatabaseOperator, PostgresOperator};
use crate::error::AppError;

/// `pg_dump` output format. Maps to the `-F` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DumpFormat {
    /// `-Fc` — compressed, restorable with `pg_restore`. Recommended default.
    Custom,
    /// `-Fp` — plain SQL text, restorable with `psql`. Versionable in git.
    Plain,
    /// `-Ft` — tar archive, restorable with `pg_restore`.
    Tar,
}

impl DumpFormat {
    pub fn flag(self) -> &'static str {
        match self {
            DumpFormat::Custom => "c",
            DumpFormat::Plain => "p",
            DumpFormat::Tar => "t",
        }
    }

    pub fn restore_with_pg_restore(self) -> bool {
        matches!(self, DumpFormat::Custom | DumpFormat::Tar)
    }

    pub fn extension(self) -> &'static str {
        match self {
            DumpFormat::Custom => "dump",
            DumpFormat::Plain => "sql",
            DumpFormat::Tar => "tar",
        }
    }
}

impl std::str::FromStr for DumpFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "custom" | "c" => Ok(DumpFormat::Custom),
            "plain" | "p" | "sql" => Ok(DumpFormat::Plain),
            "tar" | "t" => Ok(DumpFormat::Tar),
            other => Err(format!(
                "unknown dump format '{}' (expected: custom, plain, tar)",
                other
            )),
        }
    }
}

/// Reset behavior for [`import_branch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImportMode {
    /// Drop and re-create the database before importing.
    Reset,
    /// Restore on top of the existing database without dropping it.
    Merge,
}

impl std::str::FromStr for ImportMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "reset" => Ok(ImportMode::Reset),
            "merge" => Ok(ImportMode::Merge),
            other => Err(format!(
                "unknown import mode '{}' (expected: reset, merge)",
                other
            )),
        }
    }
}

fn container_name(config: &Config, branch: &str) -> String {
    format!("{}_{}", config.name, branch)
}

fn database_name(config: &Config) -> &str {
    config
        .postgres_config
        .database
        .as_deref()
        .unwrap_or("dbranch")
}

async fn ensure_running(config: &Config, branch: &str) -> Result<String, AppError> {
    let container = container_name(config, branch);
    let op = PostgresOperator::new();
    if !op.is_container_running(&container).await.unwrap_or(false) {
        return Err(AppError::Database {
            message: format!(
                "Branch container '{}' is not running. Start it with `dbranch resume`.",
                container
            ),
        });
    }
    Ok(container)
}

/// Streams a `pg_dump` of the named branch's database into `writer`.
///
/// The dump runs inside the container; only the dump's stdout crosses the
/// docker boundary. Errors from `pg_dump` (anything on stderr) surface as
/// [`AppError::Database`] after the process exits.
pub async fn dump_branch<W>(
    config: &Config,
    branch: &str,
    writer: &mut W,
    format: DumpFormat,
) -> Result<(), AppError>
where
    W: AsyncWrite + Unpin,
{
    let container = ensure_running(config, branch).await?;
    let pg = &config.postgres_config;
    let db = database_name(config);

    info!(
        "Dumping branch '{}' (db '{}') from container '{}' (format: {:?})",
        branch, db, container, format
    );

    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            &container,
            "pg_dump",
            "-U",
            pg.user.as_str(),
            "-d",
            db,
            "-F",
            format.flag(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Docker {
            message: format!("Failed to spawn docker exec for pg_dump: {}", e),
        })?;

    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Internal {
            message: "pg_dump child has no stdout".into(),
        })?;

    copy(&mut child_stdout, writer)
        .await
        .map_err(|e| AppError::Internal {
            message: format!("Failed to stream pg_dump output: {}", e),
        })?;

    let status = child.wait().await.map_err(|e| AppError::Docker {
        message: format!("Failed to wait for pg_dump: {}", e),
    })?;

    if !status.success() {
        let stderr = read_stderr(child.stderr.take()).await;
        return Err(AppError::Database {
            message: format!(
                "pg_dump exited with status {}: {}",
                status,
                stderr.trim()
            ),
        });
    }

    Ok(())
}

/// Streams the contents of `reader` into the appropriate Postgres tool —
/// `pg_restore` for binary dumps (custom / tar), `psql` for plain SQL —
/// running inside the branch's container.
///
/// Format detection peeks the first few bytes. PostgreSQL's custom dump
/// starts with the ASCII magic `PGDMP`; tar archives start with their own
/// header but `pg_restore` accepts both with the same invocation, so we
/// only distinguish "binary" vs "text" here.
pub async fn import_branch<R>(
    config: &Config,
    branch: &str,
    reader: &mut R,
    mode: ImportMode,
) -> Result<(), AppError>
where
    R: AsyncRead + Unpin,
{
    let container = ensure_running(config, branch).await?;
    let pg = &config.postgres_config;
    let db = database_name(config);

    if mode == ImportMode::Reset {
        reset_database(&container, &pg.user, db).await?;
    }

    // Peek up to 8 bytes so we can pick pg_restore vs psql without
    // buffering the whole dump. 5 bytes ("PGDMP") would be enough but a
    // little slack is fine.
    let mut peek = [0u8; 8];
    let mut peeked = 0;
    while peeked < peek.len() {
        let n = reader.read(&mut peek[peeked..]).await.map_err(|e| {
            AppError::Internal {
                message: format!("Failed to read dump header: {}", e),
            }
        })?;
        if n == 0 {
            break;
        }
        peeked += n;
    }
    let is_custom_dump = peeked >= 5 && &peek[..5] == b"PGDMP";

    info!(
        "Importing into branch '{}' (db '{}') in container '{}' (mode: {:?}, format: {})",
        branch,
        db,
        container,
        mode,
        if is_custom_dump { "custom/binary" } else { "plain SQL" }
    );

    // Glue the peeked bytes back to the front of the stream so the child
    // sees the complete input.
    let head = std::io::Cursor::new(peek[..peeked].to_vec());
    let mut combined = tokio::io::AsyncReadExt::chain(head, reader);

    let mut child = if is_custom_dump {
        Command::new("docker")
            .args([
                "exec",
                "-i",
                &container,
                "pg_restore",
                "--no-owner",
                "--no-privileges",
                "-U",
                pg.user.as_str(),
                "-d",
                db,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    } else {
        // psql replays plain SQL. `-v ON_ERROR_STOP=1` aborts on the first
        // SQL error so we surface a clear message instead of an ambiguous
        // partial import.
        Command::new("docker")
            .args([
                "exec",
                "-i",
                &container,
                "psql",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                pg.user.as_str(),
                "-d",
                db,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
    .map_err(|e| AppError::Docker {
        message: format!("Failed to spawn import process: {}", e),
    })?;

    let tool = if is_custom_dump { "pg_restore" } else { "psql" };
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::Internal {
            message: format!("{} child has no stdin", tool),
        })?;

    // Copy can fail with EPIPE if the child aborts early (bad dump format,
    // SQL error, etc). Don't surface "Broken pipe" — that hides the real
    // error coming on stderr. Just close stdin and let the wait() path
    // below report what postgres actually said.
    let copy_result = copy(&mut combined, &mut child_stdin).await;
    child_stdin.shutdown().await.ok();
    drop(child_stdin);

    let status = child.wait().await.map_err(|e| AppError::Docker {
        message: format!("Failed to wait for {}: {}", tool, e),
    })?;

    if !status.success() {
        let stderr = read_stderr(child.stderr.take()).await;
        let msg = stderr.trim();
        let msg = if msg.is_empty() {
            match &copy_result {
                Err(e) => format!("{} failed ({}) — pipe error: {}", tool, status, e),
                Ok(_) => format!("{} exited with status {}", tool, status),
            }
        } else {
            format!("{} failed ({}): {}", tool, status, msg)
        };
        return Err(AppError::Database { message: msg });
    }

    if let Err(e) = copy_result {
        return Err(AppError::Internal {
            message: format!(
                "Streamed dump but copy reported {}; {} reported success",
                e, tool
            ),
        });
    }
    Ok(())
}

async fn reset_database(container: &str, user: &str, db: &str) -> Result<(), AppError> {
    debug!("Resetting database '{}' in container '{}'", db, container);

    // Connect to 'postgres' to drop the target db.
    let drop_cmd = format!("DROP DATABASE IF EXISTS \"{}\";", db);
    let create_cmd = format!("CREATE DATABASE \"{}\";", db);

    for sql in [drop_cmd, create_cmd] {
        let out = Command::new("docker")
            .args([
                "exec", container, "psql", "-U", user, "-d", "postgres", "-c", &sql,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| AppError::Docker {
                message: format!("Failed to run psql for reset ({}): {}", sql, e),
            })?;
        if !out.status.success() {
            return Err(AppError::Database {
                message: format!(
                    "psql -c {:?} failed: {}",
                    sql,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
    }
    Ok(())
}

async fn read_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    use tokio::io::AsyncReadExt;
    if let Some(mut s) = stderr {
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.ok();
        buf
    } else {
        String::new()
    }
}

/// Default output path for a dump: `./<project>-<branch>-<timestamp>.<ext>`.
pub fn default_dump_path(config: &Config, branch: &str, format: DumpFormat) -> std::path::PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    Path::new(".").join(format!(
        "{}-{}-{}.{}",
        config.name,
        branch,
        stamp,
        format.extension()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_format_parses_aliases() {
        assert_eq!("custom".parse::<DumpFormat>().unwrap(), DumpFormat::Custom);
        assert_eq!("c".parse::<DumpFormat>().unwrap(), DumpFormat::Custom);
        assert_eq!("plain".parse::<DumpFormat>().unwrap(), DumpFormat::Plain);
        assert_eq!("sql".parse::<DumpFormat>().unwrap(), DumpFormat::Plain);
        assert_eq!("TAR".parse::<DumpFormat>().unwrap(), DumpFormat::Tar);
        assert!("bogus".parse::<DumpFormat>().is_err());
    }

    #[test]
    fn dump_format_flags_and_extensions_are_correct() {
        assert_eq!(DumpFormat::Custom.flag(), "c");
        assert_eq!(DumpFormat::Plain.flag(), "p");
        assert_eq!(DumpFormat::Tar.flag(), "t");
        assert_eq!(DumpFormat::Custom.extension(), "dump");
        assert_eq!(DumpFormat::Plain.extension(), "sql");
        assert_eq!(DumpFormat::Tar.extension(), "tar");
    }

    #[test]
    fn import_mode_parses() {
        assert_eq!("reset".parse::<ImportMode>().unwrap(), ImportMode::Reset);
        assert_eq!("merge".parse::<ImportMode>().unwrap(), ImportMode::Merge);
        assert!("nope".parse::<ImportMode>().is_err());
    }

    #[test]
    fn default_dump_path_uses_project_and_branch() {
        let cfg = Config::new("proj".into());
        let p = default_dump_path(&cfg, "feature-x", DumpFormat::Custom);
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("proj-feature-x-"));
        assert!(name.ends_with(".dump"));
    }
}
