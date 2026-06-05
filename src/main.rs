use std::sync::Arc;

use clap::Parser;
use dbranch::{
    cli::{self, AppState, Cli, Commands},
    config::Config,
    error::AppError,
    logbuf,
};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use tracing::{debug, error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Environment variable to control log filtering (overrides RUST_LOG).
const ENV_LOG: &str = "DBRANCH_LOG";

#[tokio::main]
async fn main() {
    let filter = std::env::var(ENV_LOG)
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    // Capture the same formatted records into an in-memory ring so the
    // Web UI's Logs tab can show them. Stderr layer stays for terminal use.
    let log_buffer = logbuf::install(logbuf::DEFAULT_CAPACITY);

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(log_buffer)
                .with_ansi(false),
        )
        .init();

    let cli = Cli::parse();
    debug!("CLI arguments parsed: {:?}", cli.command);

    info!("🌿 dBranch - PostgreSQL Database Branching System");

    let project = cli.resolve_project();

    match cli.command {
        Commands::Start => {
            // `dbranch start` boots the proxy + Web UI even when no projects
            // exist yet. Lets the user create their first project explicitly
            // via the UI rather than having a ghost `my_project` appear.
            let (config, watched) = match resolve_start_config(&project) {
                Ok((cfg, name)) => (Arc::new(RwLock::new(cfg)), name),
                Err(e) => {
                    error!("Failed to load configuration: {}", e);
                    std::process::exit(1);
                }
            };
            match watched.as_deref() {
                Some(n) => info!("Configuration loaded for project '{}'", n),
                None => info!("No project configured — boot with empty registry; create one via the Web UI."),
            }
            tokio::spawn(sync_config(config.clone(), watched.unwrap_or_default()));
            info!("Starting dBranch service...");
            if let Err(e) = run_server(config).await {
                error!("Server error: {}", e);
                std::process::exit(1);
            }
        }
        cmd => {
            // Non-Start CLI commands need a real project — fall back to the
            // first-run-init helper so a brand-new user gets `my_project`
            // automatically and the command can proceed.
            let config = match load_config_for(&project) {
                Ok(c) => Arc::new(RwLock::new(c)),
                Err(e) => {
                    error!("Failed to load configuration: {}", e);
                    std::process::exit(1);
                }
            };
            let project_name = config.read().await.name.clone();
            info!("Configuration loaded for project '{}'", project_name);
            tokio::spawn(sync_config(config.clone(), project_name));

            let mut cli_handler = cli::CliHandler::new(AppState {
                config: config.read().await.clone(),
            });
            if let Err(e) = cli_handler.handle_command(cmd).await {
                error!("Command failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

/// Resolution path used by `dbranch start`:
/// - If `--project` was given, that project MUST exist (or it's an error).
/// - Otherwise, try the registry's default — empty is fine, we return a
///   server stub and `None` to signal "nothing to watch yet".
fn resolve_start_config(project: &Option<String>) -> Result<(Config, Option<String>), AppError> {
    match project {
        Some(name) => {
            let cfg = Config::load(name)?;
            let n = cfg.name.clone();
            Ok((cfg, Some(n)))
        }
        None => match Config::load_default() {
            Ok(cfg) => {
                let n = cfg.name.clone();
                Ok((cfg, Some(n)))
            }
            Err(AppError::ProjectNotFound { .. }) => Ok((Config::stub_for_server(), None)),
            Err(e) => Err(e),
        },
    }
}

/// Loads the right config for the given project, honoring the new registry
/// layout. Creates a fresh `my_project` on truly-first-run (no default in
/// the registry) — that's the only place we want the auto-init behaviour;
/// `sync_config` deliberately uses the non-creating variant so deleting the
/// last project via the UI doesn't resurrect it.
fn load_config_for(project: &Option<String>) -> Result<Config, AppError> {
    match project {
        Some(name) => Config::load(name),
        None => Config::load_default_or_init(),
    }
}

/// Background loop that refreshes the in-memory `Config` so changes written
/// by the UI (active branch, new branches, settings) propagate into the
/// proxy without a restart.
///
/// Self-heals when the watched project disappears:
///   - On `ProjectNotFound`, falls back to the registry's default project
///     and silently switches over.
///   - If no projects remain, sleeps longer and stays silent (avoids
///     spamming "Project X not found" every 2 seconds after a delete).
async fn sync_config(config: Arc<RwLock<Config>>, initial_project: String) {
    let mut current = initial_project;
    let mut last_error: Option<String> = None;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        match Config::load(&current) {
            Ok(new_config) => {
                config.write().await.clone_from(&new_config);
                if last_error.is_some() {
                    info!("Configuration reload recovered (project '{}')", current);
                    last_error = None;
                }
            }
            Err(AppError::ProjectNotFound { .. }) => {
                // Project was deleted (likely via the Web UI). Try to glide
                // onto the registry's default project so the proxy keeps
                // serving something useful.
                match Config::load_default() {
                    Ok(new_config) => {
                        let new_name = new_config.name.clone();
                        info!(
                            "Project '{}' is gone — switching sync to '{}' (registry default)",
                            current, new_name
                        );
                        config.write().await.clone_from(&new_config);
                        current = new_name;
                        last_error = None;
                    }
                    Err(_) => {
                        // No projects at all. Log once, then stay quiet.
                        let msg = format!("no projects available to watch ({})", current);
                        if last_error.as_deref() != Some(&msg) {
                            info!("{} — sync loop idle", msg);
                            last_error = Some(msg);
                        }
                        // Back off so we don't churn while empty.
                        tokio::time::sleep(tokio::time::Duration::from_secs(8)).await;
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if last_error.as_deref() != Some(&msg) {
                    error!("Failed to reload configuration: {}", e);
                    last_error = Some(msg);
                }
            }
        }
    }
}

async fn run_server(config: Arc<RwLock<Config>>) -> Result<(), AppError> {
    let (proxy_port, api_port) = {
        let cfg = config.read().await;
        (cfg.proxy_port, cfg.api_port)
    };

    // Boot the HTTP API + Web UI alongside the TCP proxy.
    tokio::spawn(async move {
        if let Err(e) = dbranch::web::serve(api_port).await {
            error!("Web server stopped: {}", e);
        }
    });

    let bind_addr = format!("0.0.0.0:{}", proxy_port);
    info!("📡 Postgres proxy on: {}", bind_addr);

    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| AppError::Network {
            message: format!("Failed to bind {}: {}", bind_addr, e),
        })?;

    while let Ok((client, addr)) = listener.accept().await {
        info!("🔗 New connection from: {}", addr);

        let target = match resolve_target(&config).await {
            Some(t) => t,
            None => {
                error!("No target branch available for connection from {}", addr);
                continue;
            }
        };

        tokio::spawn(async move {
            match handle_connection(client, &target).await {
                Ok(_) => info!("✅ Connection {} finished (target: {})", addr, target),
                Err(e) => error!("❌ Connection error {}: {}", addr, e),
            }
        });
    }

    Ok(())
}

async fn resolve_target(config: &Arc<RwLock<Config>>) -> Option<String> {
    let cfg = config.read().await;
    let active = cfg.active_branch.as_deref().unwrap_or("main");
    let port = cfg.branches.iter().find(|b| b.name == active)?.port;
    Some(format!("localhost:{}", port))
}

async fn handle_connection(mut client: TcpStream, target_addr: &str) -> io::Result<()> {
    let mut server = TcpStream::connect(target_addr).await?;

    let (mut client_read, mut client_write) = client.split();
    let (mut server_read, mut server_write) = server.split();

    let client_to_server = io::copy(&mut client_read, &mut server_write);
    let server_to_client = io::copy(&mut server_read, &mut client_write);

    tokio::try_join!(client_to_server, server_to_client)?;

    Ok(())
}
