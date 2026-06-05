//! Drives every UI action through the HTTP API.
//!
//! Boots the axum server on a random port and uses `reqwest` to hit it as if
//! a browser were clicking. Two layers of coverage:
//!
//! * `flow_without_docker` runs everywhere and verifies the routing,
//!   serialization, registry integration, error handling, and static asset
//!   serving — every endpoint that doesn't depend on a running Postgres.
//! * `flow_with_docker` does the full lifecycle (create project, start main,
//!   stop branch, delete) — gated on Docker actually being reachable and a
//!   Linux/CoW-capable host for branch creation.

use dbranch::config::{ENV_DBRANCH_HOME, Registry};
use dbranch::web::app;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::OnceLock;
use tempfile::TempDir;
use tokio::net::TcpListener;

static TEST_HOME: OnceLock<TempDir> = OnceLock::new();

/// Boots the axum server on an ephemeral port; returns the base URL.
async fn boot_server() -> String {
    // Isolated dbranch home for the whole test binary.
    let dir = TEST_HOME.get_or_init(|| TempDir::new().unwrap());
    unsafe { std::env::set_var(ENV_DBRANCH_HOME, dir.path()) };
    // Start with an empty registry.
    let _ = std::fs::remove_file(dir.path().join("registry.json"));
    let _ = std::fs::remove_dir_all(dir.path().join("projects"));
    Registry::default().save().unwrap();

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // tiny delay so the listener is ready
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{}", addr)
}

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
#[serial_test::serial]
async fn flow_without_docker() {
    let base = boot_server().await;
    let client = reqwest::Client::new();

    // ---- 1) Static index served at / ----
    let res = client.get(&base).send().await.unwrap();
    assert_eq!(res.status(), 200, "index should be served");
    let body = res.text().await.unwrap();
    assert!(body.contains("dBranch"), "expected branded HTML, got: {}", &body[..body.len().min(200)]);

    // ---- 2) Static assets present ----
    for asset in ["/app.js", "/app.css"] {
        let res = client.get(format!("{}{}", base, asset)).send().await.unwrap();
        assert_eq!(res.status(), 200, "{} should be served", asset);
        assert!(res.text().await.unwrap().len() > 100, "{} should have real content", asset);
    }

    // ---- 3) GET /api/status on empty registry ----
    let v: Value = client
        .get(format!("{}/api/status", base))
        .send().await.unwrap().json().await.unwrap();
    assert!(v["projects"].as_array().unwrap().is_empty());
    assert!(v["default"].is_null());

    // ---- 4) GET /api/projects on empty registry ----
    let v: Value = client
        .get(format!("{}/api/projects", base))
        .send().await.unwrap().json().await.unwrap();
    assert!(v.as_array().unwrap().is_empty());

    // ---- 5) Unknown project -> 404 with JSON error body ----
    let res = client
        .get(format!("{}/api/projects/ghost", base))
        .send().await.unwrap();
    assert_eq!(res.status(), 404);
    let v: Value = res.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("ghost"));

    // ---- 6) Unknown branch under unknown project -> 404 ----
    let res = client
        .get(format!("{}/api/projects/x/branches/y", base))
        .send().await.unwrap();
    assert_eq!(res.status(), 404);

    // ---- 7) POST with bad payload -> 400-ish ----
    let res = client
        .post(format!("{}/api/projects", base))
        .header("content-type", "application/json")
        .body("not json")
        .send().await.unwrap();
    assert!(
        res.status().is_client_error(),
        "bad payload should be 4xx, got {}",
        res.status()
    );

    // ---- 8) Direct registry mutation via API ----
    //   When Docker isn't available the auto-init step may fail, but the
    //   project should still register cleanly (the route catches that error
    //   and logs a warn).
    let res = client
        .post(format!("{}/api/projects", base))
        .json(&serde_json::json!({"name": "alpha"}))
        .send().await.unwrap();
    // 200 either way (docker failure is downgraded to a warn).
    assert!(
        res.status().is_success(),
        "create project should succeed even if Docker is missing, got {}",
        res.status()
    );
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["name"], "alpha");
    assert_eq!(v["is_default"], true);
    assert_eq!(v["branches"][0]["name"], "main");
    assert!(v["branches"][0]["connection_url"]
        .as_str().unwrap().starts_with("postgresql://"));
    // main_start_error may be present or null depending on Docker availability.
    assert!(v.get("main_start_error").is_some());

    // ---- 9) GET it back ----
    let v: Value = client
        .get(format!("{}/api/projects/alpha", base))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["name"], "alpha");

    // ---- 10) GET /api/projects/alpha/branches ----
    let v: Value = client
        .get(format!("{}/api/projects/alpha/branches", base))
        .send().await.unwrap().json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "main");
    assert_eq!(arr[0]["is_main"], true);

    // ---- 11) GET single branch detail ----
    let v: Value = client
        .get(format!("{}/api/projects/alpha/branches/main", base))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["name"], "main");
    assert!(v["connection_url"].as_str().unwrap().contains("127.0.0.1"));
    assert!(v["data_path"].as_str().unwrap().contains("alpha"));

    // ---- 12) set_active to a missing branch -> 404 ----
    let res = client
        .post(format!("{}/api/projects/alpha/active", base))
        .json(&serde_json::json!({"branch": "ghost"}))
        .send().await.unwrap();
    assert_eq!(res.status(), 404);

    // ---- 13) Delete main branch -> 403 ----
    let res = client
        .delete(format!("{}/api/projects/alpha/branches/main", base))
        .send().await.unwrap();
    assert_eq!(res.status(), 403);

    // ---- 14) Stop/Resume project: must always 2xx (idempotent) ----
    let res = client
        .post(format!("{}/api/projects/alpha/stop", base))
        .send().await.unwrap();
    assert!(res.status().is_success());

    // ---- 15) status now shows the project ----
    let v: Value = client
        .get(format!("{}/api/status", base))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["projects"].as_array().unwrap().len(), 1);
    assert_eq!(v["default"], "alpha");

    // ---- 16) GET /api/defaults exposes the suggested mount point + PG creds ----
    let v: Value = client
        .get(format!("{}/api/defaults", base))
        .send().await.unwrap().json().await.unwrap();
    let suggested_mount = v["mount_point"].as_str().unwrap().to_string();
    assert!(!suggested_mount.is_empty());
    assert!(!v["postgres_user"].as_str().unwrap().is_empty());

    // ---- 17) Create a project with a CUSTOM mount_point — should be persisted ----
    let custom_mount = TEST_HOME.get().unwrap().path().join("custom-mount");
    let custom_mount_str = custom_mount.to_string_lossy().to_string();
    let res = client
        .post(format!("{}/api/projects", base))
        .json(&serde_json::json!({
            "name": "withmount",
            "mount_point": custom_mount_str,
            "postgres_user": "lulu",
            "postgres_database": "mydb",
        }))
        .send().await.unwrap();
    assert!(res.status().is_success());
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["mount_point"], custom_mount_str);
    let url = v["branches"][0]["connection_url"].as_str().unwrap();
    assert!(url.contains("lulu"), "expected user 'lulu' in URL, got: {}", url);
    assert!(url.ends_with("/mydb"), "expected db 'mydb' at end of URL, got: {}", url);

    // ---- 18) PATCH a project: change mount_point + postgres user ----
    let new_mount = TEST_HOME.get().unwrap().path().join("new-mount").to_string_lossy().to_string();
    let res = client
        .patch(format!("{}/api/projects/withmount", base))
        .json(&serde_json::json!({
            "mount_point": new_mount,
            "postgres_user": "renamed",
        }))
        .send().await.unwrap();
    assert!(res.status().is_success(), "PATCH should succeed, got {}", res.status());
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["mount_point"], new_mount);
    // mount-change adds a warning about data not being moved
    assert!(
        v["warnings"].as_array().unwrap().iter().any(|w| {
            w.as_str().unwrap().contains("mount_point")
        }),
        "expected mount_point warning, got: {:?}",
        v["warnings"]
    );
    let url = v["branches"][0]["connection_url"].as_str().unwrap();
    assert!(url.contains("renamed"), "patched user should appear in URL: {}", url);

    // ---- 19) PATCH persists across reload (Config saved to disk) ----
    let v: Value = client
        .get(format!("{}/api/projects/withmount", base))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["mount_point"], new_mount);

    // ---- 20) PATCH unknown project -> 404 ----
    let res = client
        .patch(format!("{}/api/projects/ghost", base))
        .json(&serde_json::json!({"mount_point": "/tmp/x"}))
        .send().await.unwrap();
    assert_eq!(res.status(), 404);

    // ---- 21) Delete projects -> 204, status empty ----
    for p in ["alpha", "withmount"] {
        let res = client
            .delete(format!("{}/api/projects/{}", base, p))
            .send().await.unwrap();
        assert_eq!(res.status(), 204);
    }

    let v: Value = client
        .get(format!("{}/api/status", base))
        .send().await.unwrap().json().await.unwrap();
    assert!(v["projects"].as_array().unwrap().is_empty());
    assert!(v["default"].is_null());
}

/// Full Docker lifecycle. Skipped when Docker isn't reachable.
/// On macOS, branch creation (`snapshot`) is gated to Linux and will error —
/// we test main-only operations here.
#[tokio::test]
#[serial_test::serial]
async fn flow_with_docker() {
    if !docker_available() {
        eprintln!("[skip] docker not available");
        return;
    }

    let base = boot_server().await;
    let client = reqwest::Client::new();
    let project = format!("dbtest_{}", std::process::id());

    // Best-effort cleanup before we start in case a prior run left things.
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", &format!("{}_main", project)])
        .output();

    // 1) Create project (should auto-start main).
    let res = client
        .post(format!("{}/api/projects", base))
        .json(&serde_json::json!({"name": project}))
        .send().await.unwrap();
    assert!(res.status().is_success(), "create project failed: {}", res.status());
    let create_body: Value = res.json().await.unwrap();
    if let Some(err) = create_body["main_start_error"].as_str() {
        eprintln!("[skip] main_start_error reported: {}", err);
        // Best-effort cleanup of the registered-but-not-started project.
        let _ = client.delete(format!("{}/api/projects/{}", base, project)).send().await;
        return;
    }

    // Give docker run a moment to actually start the container.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // 2) Status should report main as running.
    let v: Value = client
        .get(format!("{}/api/projects/{}", base, project))
        .send().await.unwrap().json().await.unwrap();
    let main_running = v["branches"][0]["container_running"].as_bool().unwrap_or(false);
    assert!(main_running, "main should be running after create_project; got: {}", v);

    // 3) Stop main via dedicated endpoint.
    let res = client
        .post(format!("{}/api/projects/{}/branches/main/stop", base, project))
        .send().await.unwrap();
    assert!(res.status().is_success(), "stop branch: {}", res.status());

    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let v: Value = client
        .get(format!("{}/api/projects/{}/branches/main", base, project))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["container_running"], false, "main should be stopped");

    // 4) Start main via dedicated endpoint (must use docker start, not docker run).
    let res = client
        .post(format!("{}/api/projects/{}/branches/main/start", base, project))
        .send().await.unwrap();
    assert!(res.status().is_success(), "start branch (idempotent): {}", res.status());

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let v: Value = client
        .get(format!("{}/api/projects/{}/branches/main", base, project))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(v["container_running"], true, "main should be running again after start");

    // 5) Stop-all then resume-all is idempotent.
    client
        .post(format!("{}/api/projects/{}/stop", base, project))
        .send().await.unwrap().error_for_status().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    client
        .post(format!("{}/api/projects/{}/resume", base, project))
        .send().await.unwrap().error_for_status().unwrap();

    // 6) Set active branch to "main" (the only one we have).
    client
        .post(format!("{}/api/projects/{}/active", base, project))
        .json(&serde_json::json!({"branch": "main"}))
        .send().await.unwrap().error_for_status().unwrap();

    // 7) Cleanup project (deletes container too).
    let res = client
        .delete(format!("{}/api/projects/{}", base, project))
        .send().await.unwrap();
    assert!(res.status().is_success(), "delete project: {}", res.status());

    // Make sure no container leaked.
    let out = std::process::Command::new("docker")
        .args(["ps", "-a", "--filter", &format!("name={}_main", project), "-q"])
        .output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "container should be gone after delete project"
    );
}
