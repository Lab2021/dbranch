use std::path::Path;

use docker_wrapper::{
    DockerCommand, InspectCommand, NetworkCreateCommand, NetworkLsCommand, RmCommand, RunCommand,
    StartCommand, StopCommand,
};
use tracing::{debug, info};

use crate::{config::Config, error::AppError};

pub trait DatabaseOperator {
    async fn create_database(&self, config: &Config, port: u16, name: &str)
    -> Result<(), AppError>;
    async fn delete_database(&self, config: &Config, name: &str) -> Result<(), AppError>;
    async fn stop_database(&self, config: &Config, name: &str) -> Result<(), AppError>;
    async fn is_container_running(&self, name: &str) -> Result<bool, AppError>;
    /// Idempotent: starts an existing stopped container, or creates a fresh
    /// one if none exists. The right primitive for Resume / Init flows.
    async fn start_or_create(&self, config: &Config, port: u16, name: &str)
    -> Result<(), AppError>;
    /// True if a container with this name exists (running or stopped).
    async fn container_exists(&self, name: &str) -> Result<bool, AppError>;
}

pub struct PostgresOperator {}

impl PostgresOperator {
    pub fn new() -> Self {
        Self {}
    }
}

const DBRANCH_NETWORK: &str = "dbranch-network";
const DEFAULT_DB_NAME: &str = "dbranch";

impl DatabaseOperator for PostgresOperator {
    async fn create_database(
        &self,
        config: &Config,
        port: u16,
        name: &str,
    ) -> Result<(), AppError> {
        info!(
            "Creating PostgreSQL database '{}' for project '{}' on port {}",
            name, config.name, port
        );

        ensure_network().await?;

        let volume_path = Path::new(&config.mount_point)
            .join(&config.name)
            .join(name)
            .join("data");

        std::fs::create_dir_all(&volume_path).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create volume dir {:?}: {}", volume_path, e),
        })?;

        // Run the container as the host user so the bind-mounted volume is
        // writable without root chown gymnastics. Works on any host:
        // - Linux as the dev user: container writes show up as that user
        // - macOS / Docker Desktop: same — Docker Desktop's VM maps the uid
        // See https://github.com/docker-library/docs/tree/master/postgres#arbitrary---user-notes
        // for postgres image's arbitrary-user support.
        let uid = nix::unistd::Uid::current().as_raw();
        let gid = nix::unistd::Gid::current().as_raw();
        let user_arg = format!("{}:{}", uid, gid);

        // Normalize ownership of the volume dir BEFORE postgres starts.
        // Why this is required (and idempotent for fresh dirs):
        //   - `snapshot()` on macOS uses `clonefile(2)`, which preserves the
        //     source's uid/gid. If `main` was created by an earlier dBranch
        //     build that ran the container as `1000:1000` (or a previous
        //     user), the cloned branch inherits foreign ownership and the new
        //     `--user uid:gid` postgres can't read/write its PGDATA.
        //   - Same risk when the user moves data between machines.
        // The one-shot `alpine chown` runs as root inside the VM and can
        // always chown the bind mount, regardless of host-side permissions.
        normalize_ownership(&volume_path, uid, gid).await?;

        let pg = &config.postgres_config;
        let db_name = pg.database.as_deref().unwrap_or(DEFAULT_DB_NAME);
        let container_name = format!("{}_{}", config.name, name);

        debug!(
            "Setting up PostgreSQL container '{}' (user={}, db={}, volume={:?}, uid={})",
            container_name, pg.user, db_name, volume_path, user_arg
        );

        RunCommand::new("postgres:17-alpine")
            .name(&container_name)
            .port(port, 5432)
            .network(DBRANCH_NETWORK)
            .user(&user_arg)
            .volume(
                volume_path.to_string_lossy().into_owned(),
                "/var/lib/postgresql/data",
            )
            .env("POSTGRES_USER", &pg.user)
            .env("POSTGRES_PASSWORD", &pg.password)
            .env("POSTGRES_DB", db_name)
            .env("PGDATA", "/var/lib/postgresql/data/pgdata")
            .restart("no")
            .detach()
            .execute()
            .await
            .map_err(|e| AppError::Docker {
                message: format!("Failed to run container {}: {}", container_name, e),
            })?;

        info!(
            "PostgreSQL container '{}' created successfully on port {}",
            container_name, port
        );

        Ok(())
    }

    async fn delete_database(&self, config: &Config, name: &str) -> Result<(), AppError> {
        let container_name = format!("{}_{}", config.name, name);
        info!("Deleting PostgreSQL container '{}'", container_name);

        let stop_output = StopCommand::new(&container_name)
            .execute()
            .await
            .map_err(|e| AppError::Docker {
                message: format!("Failed to stop container {}: {}", container_name, e),
            })?;

        if !stop_output.is_success() {
            debug!(
                "Container {} might already be stopped: {}",
                container_name, stop_output.stderr
            );
        }

        let rm_output = RmCommand::new(&container_name)
            .volumes()
            .execute()
            .await
            .map_err(|e| AppError::Docker {
                message: format!("Failed to remove container {}: {}", container_name, e),
            })?;

        if rm_output.removed_contexts().is_empty() {
            debug!(
                "Container {} might already be removed: {}",
                container_name, rm_output.stderr
            );
        }

        info!("PostgreSQL container '{}' deleted successfully", container_name);
        Ok(())
    }

    async fn stop_database(&self, config: &Config, name: &str) -> Result<(), AppError> {
        let container_name = format!("{}_{}", config.name, name);
        info!("Stopping PostgreSQL container '{}'", container_name);

        let stop_output = StopCommand::new(&container_name)
            .execute()
            .await
            .map_err(|e| AppError::Docker {
                message: format!("Failed to stop container {}: {}", container_name, e),
            })?;

        if !stop_output.is_success() {
            debug!(
                "Container {} might already be stopped: {}",
                container_name, stop_output.stderr
            );
        } else {
            info!("Container {} stopped successfully", container_name);
        }

        Ok(())
    }

    async fn is_container_running(&self, name: &str) -> Result<bool, AppError> {
        debug!("Checking if container '{}' is running", name);

        match InspectCommand::new(name).execute().await {
            Ok(output) if output.success && !output.stdout.is_empty() => {
                let is_running = output.stdout.contains("\"Running\":true")
                    || output.stdout.contains("\"Running\": true");
                Ok(is_running)
            }
            _ => Ok(false),
        }
    }

    async fn container_exists(&self, name: &str) -> Result<bool, AppError> {
        match InspectCommand::new(name).execute().await {
            Ok(output) => Ok(output.success && !output.stdout.is_empty()),
            Err(_) => Ok(false),
        }
    }

    async fn start_or_create(
        &self,
        config: &Config,
        port: u16,
        name: &str,
    ) -> Result<(), AppError> {
        let container = format!("{}_{}", config.name, name);

        if self.is_container_running(&container).await.unwrap_or(false) {
            debug!("Container '{}' already running; nothing to do", container);
            return Ok(());
        }

        if self.container_exists(&container).await.unwrap_or(false) {
            info!("Starting existing container '{}'", container);
            let out = StartCommand::new(&container)
                .execute()
                .await
                .map_err(|e| AppError::Docker {
                    message: format!("Failed to start container {}: {}", container, e),
                })?;
            if !out.is_success() {
                return Err(AppError::Docker {
                    message: format!("Container {} did not start: {}", container, out.stderr),
                });
            }
            return Ok(());
        }

        self.create_database(config, port, name).await
    }
}

async fn ensure_network() -> Result<(), AppError> {
    let net = NetworkLsCommand::new()
        .filter("name", DBRANCH_NETWORK)
        .execute()
        .await
        .map_err(|e| AppError::Docker {
            message: format!("Failed to list Docker networks: {}", e),
        })?;

    if net.success && net.stdout.contains(DBRANCH_NETWORK) {
        debug!("Docker network '{}' already exists", DBRANCH_NETWORK);
        return Ok(());
    }

    debug!("Creating Docker network '{}'", DBRANCH_NETWORK);
    NetworkCreateCommand::new(DBRANCH_NETWORK)
        .execute()
        .await
        .map_err(|e| AppError::Docker {
            message: format!("Failed to create Docker network: {}", e),
        })?;
    Ok(())
}

/// Recursively chown a bind-mount path inside an ephemeral `alpine`
/// container (which runs as root and can therefore chown anything in the
/// mount, even files owned by foreign UIDs).
///
/// The call is idempotent — if ownership is already correct, `chown` is a
/// no-op. It typically completes in ~200ms once alpine is cached locally.
async fn normalize_ownership(
    host_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), AppError> {
    let host = host_path.to_string_lossy().into_owned();
    debug!("Normalizing ownership of {} to {}:{}", host, uid, gid);
    let out = tokio::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "--user",
            "0:0",
            "-v",
            &format!("{}:/data", host),
            "alpine:3",
            "chown",
            "-R",
            &format!("{}:{}", uid, gid),
            "/data",
        ])
        .output()
        .await
        .map_err(|e| AppError::Docker {
            message: format!("Failed to spawn alpine chown: {}", e),
        })?;
    if !out.status.success() {
        return Err(AppError::Docker {
            message: format!(
                "chown of {:?} failed (status {}): {}",
                host_path,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(())
}
