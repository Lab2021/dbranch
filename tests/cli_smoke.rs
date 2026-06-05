//! Smoke tests that invoke the compiled binary without any Docker/BTRFS
//! dependency. Verifies clap wiring, help output, and rejection of bad args.

use std::process::Command;

fn binary_path() -> std::path::PathBuf {
    // `CARGO_BIN_EXE_<name>` is set by Cargo for integration tests of binary
    // targets; this is the canonical way to locate the compiled binary.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dbranch"))
}

#[test]
fn help_succeeds_and_lists_commands() {
    let output = Command::new(binary_path())
        .arg("--help")
        .output()
        .expect("failed to execute binary");

    assert!(output.status.success(), "--help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for cmd in [
        "start", "init", "create", "list", "delete", "show", "status", "use", "stop", "resume",
    ] {
        assert!(
            stdout.contains(cmd),
            "expected '--help' to mention '{}', got:\n{}",
            cmd,
            stdout
        );
    }
}

#[test]
fn version_prints_a_version_string() {
    let output = Command::new(binary_path())
        .arg("--version")
        .output()
        .expect("failed to execute binary");

    assert!(output.status.success(), "--version should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dbranch"), "expected name in --version, got: {}", stdout);
}

#[test]
fn unknown_subcommand_fails_with_nonzero_exit() {
    let output = Command::new(binary_path())
        .arg("not-a-real-command")
        .output()
        .expect("failed to execute binary");

    assert!(!output.status.success(), "unknown command should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("unrecognized")
            || stderr.to_lowercase().contains("error")
            || stderr.to_lowercase().contains("invalid"),
        "expected clap error in stderr, got: {}",
        stderr
    );
}

#[test]
fn project_flag_appears_in_help() {
    let output = Command::new(binary_path())
        .arg("--help")
        .output()
        .expect("failed to execute binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--project") || stdout.contains("-p"),
        "expected --project / -p in --help, got:\n{}",
        stdout
    );
}
