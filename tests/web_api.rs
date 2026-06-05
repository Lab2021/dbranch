//! Integration tests for the HTTP API. Spins up the axum router in-process
//! (no real listener needed) and exercises the JSON endpoints via `tower`'s
//! `ServiceExt::oneshot`.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use dbranch::config::{Config, ENV_DBRANCH_HOME, Registry, project_config_path, projects_dir};
use dbranch::web::app;
use tempfile::tempdir;
use tower::ServiceExt;

fn set_isolated_home() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    // SAFETY: setting env for this test process; integration tests are
    // each their own binary, so cross-test contamination is contained.
    unsafe { std::env::set_var(ENV_DBRANCH_HOME, dir.path()) };
    dir
}

#[tokio::test]
#[serial_test::serial]
async fn status_returns_empty_initially() {
    let _home = set_isolated_home();
    Registry::default().save().unwrap();

    let app = app();
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["projects"].as_array().unwrap().is_empty());
    assert!(v["default"].is_null());
}

#[tokio::test]
#[serial_test::serial]
async fn status_lists_registered_project() {
    let _home = set_isolated_home();

    // Seed: one registered project on disk.
    std::fs::create_dir_all(projects_dir()).unwrap();
    let mut cfg = Config::new("alpha".into());
    let path = project_config_path("alpha");
    cfg.source = Some(path.clone());
    cfg.save_to(&path).unwrap();
    let mut registry = Registry::default();
    registry.add("alpha".into());
    registry.save().unwrap();

    let res = app()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["projects"].as_array().unwrap().len(), 1);
    assert_eq!(v["projects"][0]["name"], "alpha");
    assert_eq!(v["projects"][0]["is_default"], true);
    assert_eq!(v["default"], "alpha");
}

#[tokio::test]
#[serial_test::serial]
async fn unknown_project_returns_404() {
    let _home = set_isolated_home();
    Registry::default().save().unwrap();

    let res = app()
        .oneshot(
            Request::builder()
                .uri("/api/projects/ghost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[serial_test::serial]
async fn static_index_is_served_at_root() {
    let _home = set_isolated_home();
    Registry::default().save().unwrap();

    let res = app()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("dBranch") || html.contains("Project"),
        "expected index.html content, got: {}",
        &html[..html.len().min(200)]
    );
}
