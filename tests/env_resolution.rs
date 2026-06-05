//! Verifies that the `DBRANCH_CONFIG` env var is honored when resolving the
//! config file path.
//!
//! These tests mutate process-wide environment state, so they're written to be
//! defensive: each test sets the env, resolves, and clears it. Cargo runs each
//! integration test file in its own process by default, so cross-file
//! contamination is not a concern; in-file ordering is enforced by running
//! every assertion under a single test.

use dbranch::config::{ENV_CONFIG_PATH, config_path};
use std::path::PathBuf;

#[test]
fn env_var_overrides_default_path() {
    let custom = PathBuf::from("/tmp/dbranch-test-config.json");
    // SAFETY: this is the only test in the file that touches the env var.
    unsafe { std::env::set_var(ENV_CONFIG_PATH, &custom) };
    let resolved = config_path();
    unsafe { std::env::remove_var(ENV_CONFIG_PATH) };

    assert_eq!(resolved, custom);

    // After removal, the default kicks in.
    let default_resolved = config_path();
    assert_ne!(default_resolved, custom);
    assert!(
        default_resolved
            .file_name()
            .map(|n| n.to_string_lossy().contains("dbranch"))
            .unwrap_or(false),
        "default config path should contain 'dbranch' in its file name, got {:?}",
        default_resolved
    );
}
