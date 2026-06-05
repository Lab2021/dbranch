//! End-to-end tests that exercise the full CLI flow against real Docker and
//! a Linux filesystem with reflink support (BTRFS/XFS/ext4 with copy_file_range).
//!
//! These are marked `#[ignore]` because they require:
//!   - Docker daemon reachable
//!   - Linux host
//!   - A mount point writable by the test (configurable via DBRANCH_TEST_MOUNT)
//!
//! Run them with:
//!     cargo test --test e2e_postgres -- --ignored
//!
//! Each test is responsible for cleaning up containers it creates.

#![cfg(target_os = "linux")]

use dbranch::cli::{AppState, CliHandler, Commands, CreateArgs, InitArgs, UseArgs};
use dbranch::config::Config;
use tempfile::tempdir;

fn build_handler(name: &str, mount: &std::path::Path) -> (CliHandler, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("dbranch.config.json");

    let mut cfg = Config::new(name.into());
    cfg.mount_point = mount.to_string_lossy().into_owned();
    cfg.save_to(&cfg_path).unwrap();

    // Keep `dir` alive by leaking it; tests are short-lived processes.
    std::mem::forget(dir);

    let handler = CliHandler::new(AppState { config: cfg });
    (handler, cfg_path)
}

fn mount_root() -> std::path::PathBuf {
    std::env::var("DBRANCH_TEST_MOUNT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/dbranch-test"))
}

#[tokio::test]
#[ignore = "requires Docker + Linux filesystem with reflink support"]
async fn init_creates_main_postgres_container() {
    let mount = mount_root();
    std::fs::create_dir_all(&mount).unwrap();

    let (mut handler, _path) = build_handler("dbranch_e2e_init", &mount);
    handler
        .handle_command(Commands::InitPostgres)
        .await
        .expect("init postgres failed");

    // Best-effort cleanup
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", "dbranch_e2e_init_main"])
        .output();
}

#[tokio::test]
#[ignore = "requires Docker + Linux filesystem with reflink support"]
async fn full_branch_lifecycle() {
    let mount = mount_root();
    std::fs::create_dir_all(&mount).unwrap();

    let (mut handler, _path) = build_handler("dbranch_e2e_lifecycle", &mount);

    // init
    handler
        .handle_command(Commands::Init(InitArgs {
            name: "dbranch_e2e_lifecycle".into(),
            port: 5432,
        }))
        .await
        .expect("init failed");

    // init postgres (main)
    handler
        .handle_command(Commands::InitPostgres)
        .await
        .expect("init postgres failed");

    // create a branch
    handler
        .handle_command(Commands::Create(CreateArgs {
            name: "feature-x".into(),
            source: None,
        }))
        .await
        .expect("create branch failed");

    // switch active
    handler
        .handle_command(Commands::Use(UseArgs {
            name: "feature-x".into(),
        }))
        .await
        .expect("use branch failed");

    // stop everything
    handler
        .handle_command(Commands::Stop)
        .await
        .expect("stop failed");

    // resume everything
    handler
        .handle_command(Commands::Resume)
        .await
        .expect("resume failed");

    // cleanup
    let _ = std::process::Command::new("docker")
        .args([
            "rm",
            "-f",
            "dbranch_e2e_lifecycle_main",
            "dbranch_e2e_lifecycle_feature-x",
        ])
        .output();
}
