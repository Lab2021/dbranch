//! Integration tests for the config lifecycle: load → mutate → persist → reload.
//!
//! These tests exercise [`dbranch::config::Config`] end-to-end using a tempdir
//! to isolate from the real `dbranch.config.json`.

use dbranch::config::{Approach, Branch, Config, PostgresConfig};
use tempfile::tempdir;

#[test]
fn new_config_starts_with_only_main_branch() {
    let cfg = Config::new("project".into());
    assert_eq!(cfg.branches.len(), 1);
    assert_eq!(cfg.branches[0].name, "main");
    assert!(cfg.branches[0].is_main);
    assert!(cfg.active_branch.is_none());
}

#[test]
fn default_postgres_config_has_credentials() {
    let pg = PostgresConfig::default();
    assert!(!pg.user.is_empty());
    assert!(!pg.password.is_empty());
}

#[test]
fn config_roundtrips_through_disk() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg.json");

    let mut original = Config::new("integration".into());
    original.branches.push(Branch {
        name: "feature-a".into(),
        port: 8001,
        is_main: false,
        created_at: chrono::Utc::now(),
    });
    original.active_branch = Some("feature-a".into());
    original.approach = Approach::NewDisk;

    original.save_to(&path).unwrap();
    let loaded = Config::from_file_at(&path).unwrap();

    assert_eq!(loaded, original);
}

#[test]
fn from_file_at_creates_defaults_when_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("does-not-exist-yet.json");

    let cfg = Config::from_file_at(&path).unwrap();
    assert!(path.exists());
    assert!(cfg.branches.iter().any(|b| b.is_main));
}

#[test]
fn set_active_branch_then_reload_preserves_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("active.json");

    let mut cfg = Config::new("p".into());
    cfg.branches.push(Branch {
        name: "feat".into(),
        port: 9100,
        is_main: false,
        created_at: chrono::Utc::now(),
    });
    cfg.active_branch = Some("feat".into());
    cfg.save_to(&path).unwrap();

    let reloaded = Config::from_file_at(&path).unwrap();
    assert_eq!(reloaded.active_branch.as_deref(), Some("feat"));
}

#[test]
fn parsing_error_surfaces_clearly() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("garbage.json");
    std::fs::write(&path, "this is not json at all").unwrap();

    let err = Config::from_file_at(&path).unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("parse"),
        "expected parse error message, got: {}",
        msg
    );
}

#[test]
fn approach_serializes_with_uppercase_tags() {
    let cfg = Config::new("p".into());
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg.json");
    cfg.save_to(&path).unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"EXISTING_DISK\"") || raw.contains("\"NEW_DISK\""),
        "expected one of the canonical approach tags in saved JSON"
    );
}
