//! JSON API for the web UI.
//!
//! Endpoints reuse the same Config / Registry / dump primitives as the CLI —
//! no duplicated business logic.

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use crate::{
    config::{self, Config, Registry},
    database_operator::{DatabaseOperator, PostgresOperator},
    dump::{self, DumpFormat, ImportMode},
    error::AppError,
    fiemap::get_folder_size,
};

pub fn api_router() -> Router {
    Router::new()
        .route("/status", get(status_overview))
        .route("/defaults", get(defaults))
        .route("/logs", get(server_logs))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/:project",
            get(get_project).patch(update_project).delete(delete_project),
        )
        .route("/projects/:project/branches", get(list_branches).post(create_branch))
        .route(
            "/projects/:project/branches/:branch",
            get(show_branch).delete(delete_branch),
        )
        .route(
            "/projects/:project/branches/:branch/start",
            post(start_branch),
        )
        .route(
            "/projects/:project/branches/:branch/stop",
            post(stop_branch),
        )
        .route("/projects/:project/active", post(set_active_branch))
        .route("/projects/:project/branches/:branch/dump", get(dump_branch))
        .route(
            "/projects/:project/branches/:branch/import",
            // Dumps can be GB. Axum's 2 MB default body limit chokes the
            // multipart parser mid-stream with a misleading "Error parsing
            // multipart/form-data". Disable the cap for this route only —
            // we still cap per-statement timeout in the streaming consumer.
            post(import_branch).layer(DefaultBodyLimit::disable()),
        )
        .route("/projects/:project/branches/:branch/logs", get(branch_logs))
        .route("/projects/:project/branches/:branch/schema", get(branch_schema))
        .route(
            "/projects/:project/branches/:branch/schema/diff",
            get(branch_schema_diff),
        )
        .route(
            "/projects/:project/branches/:branch/query",
            post(branch_query),
        )
        .route("/projects/:project/resources", get(project_resources))
        .route("/projects/:project/stop", post(stop_project))
        .route("/projects/:project/resume", post(resume_project))
}

// ---------- Response types ----------

#[derive(Serialize)]
struct BranchSummary {
    name: String,
    port: u16,
    is_main: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    container_running: bool,
    logical_size: u64,
    unique_size: u64,
    connection_url: String,
}

#[derive(Serialize)]
struct ProjectSummary {
    name: String,
    is_default: bool,
    active_branch: Option<String>,
    branches: Vec<BranchSummary>,
    mount_point: String,
    proxy_port: u16,
    api_port: u16,
    /// Connection URL via the dBranch proxy. Routes to whichever branch is
    /// currently active — switching branches changes the target transparently
    /// without the client needing to update its connection string.
    proxy_url: String,
    /// Name of the branch the proxy is currently routing to (mirrors
    /// `active_branch` but defaults to "main" when no explicit active is set).
    proxy_routes_to: String,
}

#[derive(Serialize)]
struct StatusOverview {
    projects: Vec<ProjectSummary>,
    default: Option<String>,
}

// ---------- Request types ----------

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    /// Where branch data lives on disk. Defaults to a writable per-OS path
    /// (`config::default_mount_point()`) when omitted.
    #[serde(default)]
    mount_point: Option<String>,
    #[serde(default)]
    postgres_user: Option<String>,
    #[serde(default)]
    postgres_password: Option<String>,
    #[serde(default)]
    postgres_database: Option<String>,
}

/// PATCH /api/projects/:p — partial update of project settings. Every field
/// is optional; omitted fields are left unchanged. Persists to disk.
#[derive(Deserialize)]
struct UpdateProjectBody {
    #[serde(default)]
    mount_point: Option<String>,
    #[serde(default)]
    postgres_user: Option<String>,
    #[serde(default)]
    postgres_password: Option<String>,
    #[serde(default)]
    postgres_database: Option<String>,
}

#[derive(Deserialize)]
struct CreateBranchBody {
    name: String,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
struct SetActiveBody {
    branch: String,
}

#[derive(Deserialize)]
struct DumpQuery {
    #[serde(default)]
    format: Option<String>,
}

// ---------- Handlers ----------

async fn status_overview() -> ApiResult<Json<StatusOverview>> {
    let registry = Registry::load_or_create()?;
    let mut projects = Vec::new();
    for name in registry.list() {
        if let Ok(summary) = build_project_summary(&name, &registry).await {
            projects.push(summary);
        }
    }
    Ok(Json(StatusOverview {
        projects,
        default: registry.default,
    }))
}

async fn list_projects() -> ApiResult<Json<Vec<ProjectSummary>>> {
    let registry = Registry::load_or_create()?;
    let mut out = Vec::new();
    for name in registry.list() {
        if let Ok(s) = build_project_summary(&name, &registry).await {
            out.push(s);
        }
    }
    Ok(Json(out))
}

#[derive(Serialize)]
struct CreateProjectResult {
    #[serde(flatten)]
    project: ProjectSummary,
    /// Populated if main failed to start (e.g. Docker unreachable). The
    /// project is registered either way; the UI can surface this as a hint.
    main_start_error: Option<String>,
}

async fn create_project(
    Json(body): Json<CreateProjectBody>,
) -> ApiResult<Json<CreateProjectResult>> {
    crate::config::validate_name(&body.name)?;
    let overrides = config::ConfigOverrides {
        mount_point: body.mount_point,
        postgres_user: body.postgres_user,
        postgres_password: body.postgres_password,
        postgres_database: body.postgres_database,
    };
    let mut cfg = Config::new_with(body.name.clone(), overrides);
    cfg.source = Some(config::project_config_path(&body.name));
    cfg.save_config()?;

    let mut registry = Registry::load_or_create()?;
    registry.add(body.name.clone());
    registry.save()?;

    // One-click: bring up the main Postgres container immediately so the
    // project is usable straight from the UI. Soft-fail if Docker is missing
    // or the mount path is unwritable — the user gets a registered project
    // they can debug and start later.
    let op = PostgresOperator::new();
    let main_port = cfg.branches[0].port;
    let main_start_error = match op.start_or_create(&cfg, main_port, "main").await {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!("Project '{}' registered but main failed to start: {}", body.name, e);
            Some(e.to_string())
        }
    };

    let summary = build_project_summary(&body.name, &registry).await?;
    Ok(Json(CreateProjectResult {
        project: summary,
        main_start_error,
    }))
}

async fn get_project(Path(project): Path<String>) -> ApiResult<Json<ProjectSummary>> {
    let registry = Registry::load_or_create()?;
    let summary = build_project_summary(&project, &registry).await?;
    Ok(Json(summary))
}

#[derive(Serialize)]
struct UpdateProjectResult {
    #[serde(flatten)]
    project: ProjectSummary,
    /// Hint to the user: when mount_point changes, existing data dirs at the
    /// old path are NOT moved automatically. The user must either re-create
    /// branches or copy the data manually before stopping/resuming.
    moved_data: bool,
    warnings: Vec<String>,
}

async fn update_project(
    Path(project): Path<String>,
    Json(body): Json<UpdateProjectBody>,
) -> ApiResult<Json<UpdateProjectResult>> {
    let mut cfg = Config::load(&project)?;
    let mut warnings = Vec::new();

    if let Some(new_mount) = body.mount_point {
        if new_mount != cfg.mount_point {
            // Stop the world before changing the mount: running containers
            // hold bind mounts open against the old path.
            let op = PostgresOperator::new();
            for branch in &cfg.branches {
                if op
                    .is_container_running(&format!("{}_{}", cfg.name, branch.name))
                    .await
                    .unwrap_or(false)
                {
                    warnings.push(format!(
                        "Branch '{}' was running on the old mount; stop it before reusing data on the new path.",
                        branch.name
                    ));
                }
            }
            warnings.push(format!(
                "mount_point changed: existing data at {:?} is NOT moved; re-import or copy manually before starting branches.",
                cfg.mount_point
            ));
            cfg.mount_point = new_mount;
        }
    }
    if let Some(u) = body.postgres_user {
        cfg.postgres_config.user = u;
    }
    if let Some(p) = body.postgres_password {
        cfg.postgres_config.password = p;
    }
    if let Some(d) = body.postgres_database {
        cfg.postgres_config.database = Some(d);
    }

    cfg.save_config()?;
    let registry = Registry::load_or_create()?;
    let summary = build_project_summary(&project, &registry).await?;
    Ok(Json(UpdateProjectResult {
        project: summary,
        moved_data: false,
        warnings,
    }))
}

#[derive(Serialize)]
struct Defaults {
    mount_point: String,
    postgres_user: String,
    postgres_password: String,
    postgres_database: Option<String>,
}

async fn defaults() -> Json<Defaults> {
    let pg = crate::config::PostgresConfig::default();
    Json(Defaults {
        mount_point: crate::config::default_mount_point(),
        postgres_user: pg.user,
        postgres_password: pg.password,
        postgres_database: pg.database,
    })
}

#[derive(Deserialize)]
struct LogsQuery {
    /// How many of the most recent lines to return. Capped at 5000.
    /// `0` (or omitted) returns everything currently buffered.
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Serialize)]
struct LogsResponse {
    lines: Vec<String>,
    /// Identifier for the source: `"dbranch"` or `"<project>_<branch>"`.
    source: String,
}

const MAX_LOG_LINES: usize = 5000;

async fn server_logs(Query(q): Query<LogsQuery>) -> Json<LogsResponse> {
    let tail = q.tail.unwrap_or(500).min(MAX_LOG_LINES);
    let lines = crate::logbuf::global()
        .map(|b| b.snapshot(tail))
        .unwrap_or_default();
    Json(LogsResponse {
        lines,
        source: "dbranch".into(),
    })
}

async fn branch_logs(
    Path((project, branch)): Path<(String, String)>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Json<LogsResponse>> {
    let cfg = Config::load(&project)?;
    if !cfg.branches.iter().any(|b| b.name == branch) {
        return Err(AppError::BranchNotFound { name: branch }.into());
    }
    let container = format!("{}_{}", cfg.name, branch);
    let tail = q.tail.unwrap_or(500).min(MAX_LOG_LINES);

    let out = tokio::process::Command::new("docker")
        .args(["logs", "--tail", &tail.to_string(), &container])
        .output()
        .await
        .map_err(|e| AppError::Docker {
            message: format!("Failed to run docker logs for {}: {}", container, e),
        })?;

    // Postgres writes most of its output to stderr by convention; concatenate
    // stdout first (likely empty), then stderr, splitting per line.
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&out.stdout));
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    let lines: Vec<String> = combined.lines().map(|s| s.to_string()).collect();

    Ok(Json(LogsResponse {
        lines,
        source: container,
    }))
}

async fn branch_schema(
    Path((project, branch)): Path<(String, String)>,
) -> ApiResult<Json<crate::schema::Schema>> {
    let cfg = Config::load(&project)?;
    if !cfg.branches.iter().any(|b| b.name == branch) {
        return Err(AppError::BranchNotFound { name: branch }.into());
    }
    let schema = crate::schema::introspect(&cfg, &branch).await?;
    Ok(Json(schema))
}

#[derive(Deserialize)]
struct SchemaDiffQuery {
    /// Branch to compare against. Defaults to `main`.
    #[serde(default)]
    against: Option<String>,
}

async fn branch_schema_diff(
    Path((project, branch)): Path<(String, String)>,
    Query(q): Query<SchemaDiffQuery>,
) -> ApiResult<Json<crate::schema_diff::SchemaDiff>> {
    let cfg = Config::load(&project)?;
    if !cfg.branches.iter().any(|b| b.name == branch) {
        return Err(AppError::BranchNotFound { name: branch }.into());
    }
    let against = q.against.unwrap_or_else(|| "main".into());
    if !cfg.branches.iter().any(|b| b.name == against) {
        return Err(AppError::BranchNotFound { name: against }.into());
    }
    if against == branch {
        return Ok(Json(crate::schema_diff::SchemaDiff::default()));
    }
    let (left, right) = tokio::try_join!(
        crate::schema::introspect(&cfg, &against),
        crate::schema::introspect(&cfg, &branch),
    )?;
    Ok(Json(crate::schema_diff::diff(&left, &right)))
}

async fn project_resources(
    Path(project): Path<String>,
) -> ApiResult<Json<Vec<crate::docker_stats::BranchResources>>> {
    let cfg = Config::load(&project)?;
    let rows = crate::docker_stats::collect_resources(&cfg).await;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct QueryBody {
    sql: String,
}

async fn branch_query(
    Path((project, branch)): Path<(String, String)>,
    Json(body): Json<QueryBody>,
) -> ApiResult<Json<crate::query::QueryResponse>> {
    let cfg = Config::load(&project)?;
    if !cfg.branches.iter().any(|b| b.name == branch) {
        return Err(AppError::BranchNotFound { name: branch }.into());
    }
    let response = crate::query::run(&cfg, &branch, &body.sql).await?;
    Ok(Json(response))
}

async fn delete_project(Path(project): Path<String>) -> ApiResult<StatusCode> {
    let cfg = Config::load(&project)?;
    let op = PostgresOperator::new();
    for branch in &cfg.branches {
        let _ = op.delete_database(&cfg, &branch.name).await;
    }
    let project_dir = std::path::Path::new(&cfg.mount_point).join(&cfg.name);
    let _ = std::fs::remove_dir_all(&project_dir);
    if let Some(path) = &cfg.source {
        let _ = std::fs::remove_file(path);
    }

    let mut registry = Registry::load_or_create()?;
    registry.remove(&project);
    registry.save()?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_branches(Path(project): Path<String>) -> ApiResult<Json<Vec<BranchSummary>>> {
    let cfg = Config::load(&project)?;
    let op = PostgresOperator::new();
    let mut out = Vec::new();
    for branch in &cfg.branches {
        out.push(build_branch_summary(&cfg, branch, &op).await);
    }
    Ok(Json(out))
}

async fn create_branch(
    Path(project): Path<String>,
    Json(body): Json<CreateBranchBody>,
) -> ApiResult<Json<BranchSummary>> {
    crate::config::validate_name(&body.name)?;
    let mut cfg = Config::load(&project)?;

    let source = body.source.as_deref().unwrap_or("main");
    if !cfg.branches.iter().any(|b| b.name == source) {
        return Err(AppError::BranchNotFound { name: source.into() }.into());
    }

    let src_path = std::path::Path::new(&cfg.mount_point)
        .join(&cfg.name)
        .join(source)
        .join("data");
    let dest_path = std::path::Path::new(&cfg.mount_point)
        .join(&cfg.name)
        .join(&body.name)
        .join("data");

    crate::snapshot::snapshot(&src_path, &dest_path)?;

    let port = cfg.get_valid_port().ok_or(AppError::NoPortAvailable {
        min: cfg.port_min,
        max: cfg.port_max,
    })?;

    PostgresOperator::new()
        .create_database(&cfg, port, &body.name)
        .await?;
    cfg.create_branch(body.name.clone(), port)?;

    let branch = cfg.branches.iter().find(|b| b.name == body.name).unwrap();
    Ok(Json(
        build_branch_summary(&cfg, branch, &PostgresOperator::new()).await,
    ))
}

async fn delete_branch(Path((project, branch)): Path<(String, String)>) -> ApiResult<StatusCode> {
    if branch == "main" {
        return Err(ApiError::from(AppError::Permission {
            message: "Cannot delete main branch".into(),
        }));
    }
    let mut cfg = Config::load(&project)?;
    if cfg.active_branch.as_deref() == Some(&branch) {
        return Err(ApiError::from(AppError::Permission {
            message: format!("Cannot delete active branch '{}'", branch),
        }));
    }
    if !cfg.branches.iter().any(|b| b.name == branch) {
        return Err(AppError::BranchNotFound { name: branch }.into());
    }

    let _ = PostgresOperator::new()
        .delete_database(&cfg, &branch)
        .await;

    let branch_dir = std::path::Path::new(&cfg.mount_point)
        .join(&cfg.name)
        .join(&branch);
    let _ = std::fs::remove_dir_all(&branch_dir);

    cfg.branches.retain(|b| b.name != branch);
    cfg.save_config()?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_active_branch(
    Path(project): Path<String>,
    Json(body): Json<SetActiveBody>,
) -> ApiResult<StatusCode> {
    let mut cfg = Config::load(&project)?;
    cfg.set_active_branch(body.branch)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn dump_branch(
    Path((project, branch)): Path<(String, String)>,
    Query(q): Query<DumpQuery>,
) -> Result<Response, ApiError> {
    let cfg = Config::load(&project)?;
    let format: DumpFormat = q
        .format
        .as_deref()
        .unwrap_or("custom")
        .parse()
        .map_err(|e: String| ApiError::from(AppError::Internal { message: e }))?;

    let filename = format!(
        "{}-{}-{}.{}",
        cfg.name,
        branch,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        format.extension()
    );

    info!("HTTP dump of {}/{} starting", project, branch);

    // We need a duplex stream: dump_branch writes async, axum reads async.
    let (tx, rx) = tokio::io::duplex(64 * 1024);
    let cfg_clone = cfg.clone();
    let branch_clone = branch.clone();
    tokio::spawn(async move {
        let mut tx = tx;
        if let Err(e) = dump::dump_branch(&cfg_clone, &branch_clone, &mut tx, format).await {
            error!("dump_branch failed: {}", e);
        }
    });

    let stream = ReaderStream::new(rx);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .unwrap())
}

async fn import_branch(
    Path((project, branch)): Path<(String, String)>,
    mut multipart: Multipart,
) -> ApiResult<StatusCode> {
    let cfg = Config::load(&project)?;

    // Read the first file field; the UI sends a single dump.
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ApiError::from(AppError::Internal {
            message: format!("multipart error: {}", e),
        })
    })? {
        if field.name() != Some("file") {
            continue;
        }
        let stream = field;
        let reader = tokio_util::io::StreamReader::new(stream.map_err(std::io::Error::other));
        let mut reader = tokio::io::BufReader::new(reader);
        dump::import_branch(&cfg, &branch, &mut reader, ImportMode::Reset).await?;
        return Ok(StatusCode::NO_CONTENT);
    }

    Err(ApiError::from(AppError::Internal {
        message: "no 'file' field in multipart body".into(),
    }))
}

async fn stop_project(Path(project): Path<String>) -> ApiResult<StatusCode> {
    let cfg = Config::load(&project)?;
    let op = PostgresOperator::new();
    for branch in &cfg.branches {
        let _ = op.stop_database(&cfg, &branch.name).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_project(Path(project): Path<String>) -> ApiResult<StatusCode> {
    let cfg = Config::load(&project)?;
    let op = PostgresOperator::new();
    let mut errors = Vec::new();
    for branch in &cfg.branches {
        if let Err(e) = op.start_or_create(&cfg, branch.port, &branch.name).await {
            errors.push(format!("{}: {}", branch.name, e));
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::from(AppError::Docker {
            message: errors.join("; "),
        }));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn start_branch(Path((project, branch)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let cfg = Config::load(&project)?;
    let entry = cfg
        .branches
        .iter()
        .find(|b| b.name == branch)
        .ok_or_else(|| AppError::BranchNotFound { name: branch.clone() })?;
    PostgresOperator::new()
        .start_or_create(&cfg, entry.port, &branch)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_branch(Path((project, branch)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let cfg = Config::load(&project)?;
    if !cfg.branches.iter().any(|b| b.name == branch) {
        return Err(AppError::BranchNotFound { name: branch }.into());
    }
    PostgresOperator::new().stop_database(&cfg, &branch).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct BranchDetail {
    #[serde(flatten)]
    summary: BranchSummary,
    connection_url: String,
    data_path: String,
}

async fn show_branch(
    Path((project, branch)): Path<(String, String)>,
) -> ApiResult<Json<BranchDetail>> {
    let cfg = Config::load(&project)?;
    let entry = cfg
        .branches
        .iter()
        .find(|b| b.name == branch)
        .ok_or_else(|| AppError::BranchNotFound { name: branch.clone() })?;
    let op = PostgresOperator::new();
    let summary = build_branch_summary(&cfg, entry, &op).await;
    let pg = &cfg.postgres_config;
    let db = pg.database.as_deref().unwrap_or("dbranch");
    let url = format!(
        "postgresql://{}:{}@127.0.0.1:{}/{}",
        pg.user, pg.password, entry.port, db
    );
    let data_path = std::path::Path::new(&cfg.mount_point)
        .join(&cfg.name)
        .join(&entry.name)
        .join("data")
        .to_string_lossy()
        .into_owned();
    Ok(Json(BranchDetail {
        summary,
        connection_url: url,
        data_path,
    }))
}

// ---------- Helpers ----------

async fn build_project_summary(name: &str, registry: &Registry) -> Result<ProjectSummary, AppError> {
    let cfg = Config::load(name)?;
    let op = PostgresOperator::new();
    let mut branches = Vec::with_capacity(cfg.branches.len());
    for branch in &cfg.branches {
        branches.push(build_branch_summary(&cfg, branch, &op).await);
    }

    let proxy_routes_to = cfg.active_branch.clone().unwrap_or_else(|| "main".into());
    let pg = &cfg.postgres_config;
    let db = pg.database.as_deref().unwrap_or("dbranch");
    let proxy_url = format!(
        "postgresql://{}:{}@127.0.0.1:{}/{}",
        pg.user, pg.password, cfg.proxy_port, db
    );

    Ok(ProjectSummary {
        name: name.to_string(),
        is_default: registry.default.as_deref() == Some(name),
        active_branch: cfg.active_branch.clone(),
        branches,
        mount_point: cfg.mount_point.clone(),
        proxy_port: cfg.proxy_port,
        api_port: cfg.api_port,
        proxy_url,
        proxy_routes_to,
    })
}

async fn build_branch_summary(
    cfg: &Config,
    branch: &crate::config::Branch,
    op: &PostgresOperator,
) -> BranchSummary {
    let container = format!("{}_{}", cfg.name, branch.name);
    let running = op.is_container_running(&container).await.unwrap_or(false);

    let dir = std::path::Path::new(&cfg.mount_point)
        .join(&cfg.name)
        .join(&branch.name);
    let (logical, shared) = match get_folder_size(&dir) {
        Some(info) => (info.logical_size, info.shared_size),
        None => (0, 0),
    };
    let pg = &cfg.postgres_config;
    let db = pg.database.as_deref().unwrap_or("dbranch");
    let connection_url = format!(
        "postgresql://{}:{}@127.0.0.1:{}/{}",
        pg.user, pg.password, branch.port, db
    );
    BranchSummary {
        name: branch.name.clone(),
        port: branch.port,
        is_main: branch.is_main,
        created_at: branch.created_at,
        container_running: running,
        logical_size: logical,
        unique_size: logical.saturating_sub(shared),
        connection_url,
    }
}

// ---------- Error mapping ----------

type ApiResult<T> = Result<T, ApiError>;

pub struct ApiError(AppError);

impl From<AppError> for ApiError {
    fn from(value: AppError) -> Self {
        ApiError(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self.0 {
            AppError::ProjectNotFound { .. } | AppError::BranchNotFound { .. } => {
                (StatusCode::NOT_FOUND, self.0.to_string())
            }
            AppError::Permission { .. } => (StatusCode::FORBIDDEN, self.0.to_string()),
            AppError::NoPortAvailable { .. } | AppError::SchemaUnavailable { .. } => {
                (StatusCode::CONFLICT, self.0.to_string())
            }
            AppError::ConfigParsing { .. } | AppError::Config { .. } => {
                (StatusCode::BAD_REQUEST, self.0.to_string())
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

// Pull the futures::TryStreamExt for the multipart -> reader conversion above.
use futures::TryStreamExt;
