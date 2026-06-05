//! Live container resource stats via `docker stats --no-stream`.
//!
//! Shared by the web route `GET /api/projects/:p/resources` and the CLI
//! command `dbranch resources`. Docker mixes prefix conventions: memory uses
//! binary units (KiB/MiB), network and block I/O use decimal (kB/MB). The
//! parsers below handle both.

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::config::Config;
use crate::database_operator::{DatabaseOperator, PostgresOperator};

#[derive(Debug, Clone, Serialize)]
pub struct BranchResources {
    pub branch: String,
    pub container: String,
    pub cpu_pct: f64,
    pub mem_used_bytes: u64,
    pub mem_limit_bytes: u64,
    pub mem_pct: f64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
    pub pids: u32,
}

#[derive(Deserialize)]
struct DockerStatsLine {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu_perc: String,
    #[serde(rename = "MemUsage")]
    mem_usage: String,
    #[serde(rename = "MemPerc")]
    mem_perc: String,
    #[serde(rename = "NetIO")]
    net_io: String,
    #[serde(rename = "BlockIO")]
    block_io: String,
    #[serde(rename = "PIDs")]
    pids: String,
}

/// Collects stats for every running branch in `cfg`. Stopped branches are
/// skipped (their stats would be zero and `docker stats` refuses dead names).
/// Returns an empty vec on any docker failure — callers should treat the
/// panel as "no data" rather than break the page.
pub async fn collect_resources(cfg: &Config) -> Vec<BranchResources> {
    let op = PostgresOperator::new();

    let mut running: Vec<(String, String)> = Vec::new();
    for branch in &cfg.branches {
        let container = format!("{}_{}", cfg.name, branch.name);
        if op.is_container_running(&container).await.unwrap_or(false) {
            running.push((branch.name.clone(), container));
        }
    }
    if running.is_empty() {
        return Vec::new();
    }

    let mut args: Vec<String> = vec![
        "stats".into(),
        "--no-stream".into(),
        "--format".into(),
        "{{json .}}".into(),
    ];
    args.extend(running.iter().map(|(_, c)| c.clone()));

    let out = match tokio::process::Command::new("docker")
        .args(&args)
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            debug!(
                "docker stats exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return Vec::new();
        }
        Err(e) => {
            debug!("docker stats spawn failed: {}", e);
            return Vec::new();
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut by_container: std::collections::HashMap<String, BranchResources> =
        std::collections::HashMap::new();
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let parsed: DockerStatsLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (mem_used, mem_limit) = parse_pair(&parsed.mem_usage).unwrap_or((0, 0));
        let (net_rx, net_tx) = parse_pair(&parsed.net_io).unwrap_or((0, 0));
        let (blk_r, blk_w) = parse_pair(&parsed.block_io).unwrap_or((0, 0));
        by_container.insert(
            parsed.name.clone(),
            BranchResources {
                branch: String::new(),
                container: parsed.name,
                cpu_pct: parse_pct(&parsed.cpu_perc).unwrap_or(0.0),
                mem_used_bytes: mem_used,
                mem_limit_bytes: mem_limit,
                mem_pct: parse_pct(&parsed.mem_perc).unwrap_or(0.0),
                net_rx_bytes: net_rx,
                net_tx_bytes: net_tx,
                block_read_bytes: blk_r,
                block_write_bytes: blk_w,
                pids: parsed.pids.parse().unwrap_or(0),
            },
        );
    }

    let mut result = Vec::with_capacity(running.len());
    for (branch, container) in running {
        if let Some(mut r) = by_container.remove(&container) {
            r.branch = branch;
            result.push(r);
        }
    }
    result
}

/// Parses a byte-size string. Recognises both binary (KiB/MiB/...) and
/// decimal (kB/MB/...) suffixes since docker mixes them per metric.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match suffix.trim() {
        "B" | "" => 1.0,
        "kB" | "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

pub fn parse_pct(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').trim().parse::<f64>().ok()
}

/// Parses `"<a> / <b>"` (e.g. `"12.34MiB / 1.945GiB"`) → `(a_bytes, b_bytes)`.
pub fn parse_pair(s: &str) -> Option<(u64, u64)> {
    let mut parts = s.split('/');
    let a = parse_size(parts.next()?.trim())?;
    let b = parse_size(parts.next()?.trim())?;
    Some((a, b))
}

/// Pretty bytes for CLI output. Matches `fmt.bytes` in the JS UI.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_binary_prefixes() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("1KiB"), Some(1024));
        assert_eq!(parse_size("12.5MiB").unwrap(), (12.5 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size("2GiB").unwrap(), 2u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_decimal_prefixes() {
        assert_eq!(parse_size("1kB"), Some(1000));
        assert_eq!(parse_size("1.5MB").unwrap(), 1_500_000);
        assert_eq!(parse_size("3GB").unwrap(), 3_000_000_000);
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("garbage"), None);
        assert_eq!(parse_size("12XB"), None);
    }

    #[test]
    fn parse_pct_works() {
        assert_eq!(parse_pct("12.34%").unwrap(), 12.34);
        assert_eq!(parse_pct("0%").unwrap(), 0.0);
        assert_eq!(parse_pct("garbage"), None);
    }

    #[test]
    fn parse_pair_handles_spacing() {
        let (a, b) = parse_pair("12.3MiB / 1.95GiB").unwrap();
        assert!(a > 12_000_000 && a < 13_000_000);
        assert!(b > 2_000_000_000 && b < 2_100_000_000);

        let (a, b) = parse_pair("1.2kB / 4.5MB").unwrap();
        assert_eq!(a, 1200);
        assert_eq!(b, 4_500_000);
    }

    #[test]
    fn fmt_bytes_smoke() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(10 * 1024 * 1024), "10.0 MiB");
    }
}
