use crate::config::config_path;
use crate::error::AppError;
use crate::fiemap::get_folder_size;
use crate::snapshot;
use crate::{
    config::{Branch, Config, Registry},
    database_operator::{DatabaseOperator, PostgresOperator},
};
use std::fs;
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use prettytable::{Attr, Cell, Row, Table};
use size::Size;
use std::path::Path;
use tracing::{debug, info};

#[derive(Parser)]
#[command(name = "dbranch")]
#[command(about = "🌿 dBranch 🌿 - PostgreSQL Database Branching System")]
#[command(version)]
pub struct Cli {
    /// Project to operate on. Defaults to $DBRANCH_PROJECT, then the registry's default.
    #[arg(short = 'p', long, global = true)]
    pub project: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Resolve which project to load.
    ///
    /// Precedence: --project flag > $DBRANCH_PROJECT env > registry default.
    pub fn resolve_project(&self) -> Option<String> {
        if let Some(p) = &self.project {
            return Some(p.clone());
        }
        std::env::var(crate::config::ENV_DBRANCH_PROJECT).ok()
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[clap(about = "Start dBranch proxy")]
    Start,
    #[clap(about = "Initialize a new dBranch project")]
    Init(InitArgs),
    #[clap(about = "Initialize a PostgreSQL database")]
    InitPostgres,
    #[clap(about = "Create a new branch project")]
    Create(CreateArgs),
    #[clap(about = "List all branches projects")]
    List,
    #[clap(about = "Delete a branch project")]
    Delete(DeleteArgs),
    #[clap(about = "Delete a project")]
    DeleteProject(DeleteProjectArgs),
    #[clap(about = "Show details of a branch project")]
    Show(ShowArgs),
    #[clap(about = "Show the status of a project")]
    Status,
    #[clap(about = "Use a specific branch")]
    Use(UseArgs),
    #[clap(about = "Stop all branches and containers")]
    Stop,
    #[clap(about = "Resume stopped branches and containers")]
    Resume,
    #[clap(about = "Dump a branch's database to a file")]
    Dump(DumpArgs),
    #[clap(about = "Import a dump file into a branch")]
    Import(ImportArgs),
    #[clap(about = "Open a psql shell against a branch")]
    Psql(PsqlArgs),
    #[clap(about = "Print the connection URL for a branch")]
    Url(UrlArgs),
    #[clap(about = "Open the web UI in the default browser")]
    Ui,
    #[clap(about = "Tail dBranch server logs or a branch container's logs")]
    Logs(LogsArgs),
    #[clap(about = "Inspect a branch's schema (tables, columns, FKs, indexes)")]
    Schema(SchemaArgs),
    #[clap(about = "Show live resource usage (CPU, memory, network, disk) per branch")]
    Resources,
    #[clap(about = "Run a single SQL statement against a branch")]
    Query(QueryArgs),
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    pub branch: String,
    /// The SQL statement. Single statement only; trailing `;` is allowed.
    /// Use `-f <path>` to read from a file instead.
    pub sql: Option<String>,
    /// Read SQL from a file (alternative to passing it as a positional arg).
    #[arg(short, long)]
    pub file: Option<std::path::PathBuf>,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// Branch name. Omit to view dBranch's own server logs.
    pub branch: Option<String>,
    /// Number of trailing lines to print.
    #[arg(short, long, default_value_t = 200)]
    pub tail: usize,
    /// Output the server logs even if a branch is given (overrides).
    #[arg(long)]
    pub server: bool,
}

#[derive(Args, Debug)]
pub struct SchemaArgs {
    pub branch: String,
    /// Print a diff against this branch instead of the full schema.
    #[arg(long)]
    pub diff_against: Option<String>,
}

#[derive(Args, Debug)]
pub struct DumpArgs {
    /// Branch to dump (defaults to the active branch).
    pub branch: Option<String>,
    /// Output file. Defaults to ./<project>-<branch>-<timestamp>.<ext>.
    #[arg(short, long)]
    pub output: Option<std::path::PathBuf>,
    /// Dump format: custom, plain, tar. Defaults to custom.
    #[arg(short, long, default_value = "custom")]
    pub format: crate::dump::DumpFormat,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    pub branch: String,
    /// Input dump file path.
    #[arg(short, long)]
    pub input: std::path::PathBuf,
    /// reset = drop & recreate database; merge = restore over existing.
    #[arg(long, default_value = "reset")]
    pub mode: crate::dump::ImportMode,
    /// Allow imports targeting the `main` branch.
    #[arg(long, default_value_t = false)]
    pub allow_main: bool,
}

#[derive(Args, Debug)]
pub struct PsqlArgs {
    /// Branch to connect to (defaults to the active branch).
    pub branch: Option<String>,
}

#[derive(Args, Debug)]
pub struct UrlArgs {
    /// Branch to print URL for (defaults to the active branch).
    pub branch: Option<String>,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(short, long, default_value = "dbranch_postgres")]
    pub name: String,

    #[arg(short, long, default_value = "5432")]
    pub port: u16,
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    pub name: String,

    #[arg(short, long)]
    pub source: Option<String>,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    pub id: String,
}

#[derive(Args, Debug)]
pub struct DeleteProjectArgs {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct UseArgs {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    pub id: String,
}

pub struct AppState {
    pub config: Config,
}

pub struct CliHandler {
    state: AppState,
}

impl CliHandler {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub async fn handle_command(&mut self, cmd: Commands) -> Result<(), AppError> {
        debug!("Handling command: {:?}", cmd);
        match cmd {
            Commands::Start => Err(AppError::Internal {
                message: "Start command should be handled in main".into(),
            }),
            Commands::List => self.list_projects().await,
            Commands::Init(args) => {
                info!("Initializing dBranch instance: {}", args.name);
                debug!("Init args: name={}, port={}", args.name, args.port);

                crate::config::validate_name(&args.name)?;

                // New project = new config in the registry-managed location.
                let mut cfg = Config::new(args.name.clone());
                cfg.source = Some(crate::config::project_config_path(&args.name));
                cfg.save_config()?;

                // Register AND set as default — `dbranch init <name>` is the
                // user's signal that they want subsequent commands to target
                // this project, not whatever the previous default was.
                let mut registry = Registry::load_or_create()?;
                registry.add(args.name.clone());
                registry.default = Some(args.name.clone());
                registry.save()?;

                self.state.config = cfg;

                info!("Project '{}' initialized successfully (now the default)", args.name);
                Ok(())
            }
            Commands::InitPostgres => {
                info!("Initializing standalone PostgreSQL database");

                let port = self
                    .state
                    .config
                    .get_valid_port()
                    .ok_or(AppError::NoPortAvailable {
                        min: self.state.config.port_min,
                        max: self.state.config.port_max,
                    })?;

                self.create_postgres(None, port).await?;

                info!("Standalone PostgreSQL database initialized successfully");
                Ok(())
            }
            Commands::Create(args) => {
                info!("Creating new branch project: {}", args.name);
                crate::config::validate_name(&args.name)?;
                if let Some(ref source) = args.source {
                    debug!("Creating from source: {}", source);
                }

                let project_name = &self.state.config.name;

                let src_path = Path::new(&self.state.config.mount_point)
                    .join(project_name)
                    .join("main/data");

                let dest_path = Path::new(&self.state.config.mount_point)
                    .join(project_name)
                    .join(&args.name)
                    .join("data");

                info!("Copying data from {:?} to {:?}", src_path, dest_path);

                snapshot::snapshot(&src_path, &dest_path)?;

                let valid_port =
                    self.state
                        .config
                        .get_valid_port()
                        .ok_or(AppError::NoPortAvailable {
                            min: self.state.config.port_min,
                            max: self.state.config.port_max,
                        })?;

                self.create_postgres(Some(args.name.clone()), valid_port)
                    .await?;

                self.state.config.create_branch(args.name, valid_port)?;

                Ok(())
            }

            Commands::Delete(args) => self.delete_branch(&args.id).await,
            Commands::DeleteProject(args) => self.delete_project(&args.name).await,
            Commands::Show(args) => self.show_branch(&args.id).await,
            Commands::Use(args) => {
                info!("Switching to branch: {}", args.name);
                self.state.config.set_active_branch(args.name.clone())?;
                info!("Switched to branch: {} successfully", args.name);
                Ok(())
            }
            Commands::Status => self.show_status().await,
            Commands::Stop => {
                info!("Stopping all branches and containers");
                let postgres_operator = PostgresOperator::new();

                for branch in &self.state.config.branches {
                    debug!("Stopping branch container: {}", branch.name);
                    let _ = postgres_operator
                        .stop_database(&self.state.config, &branch.name)
                        .await;
                }

                info!("All branches and containers stopped successfully");
                Ok(())
            }
            Commands::Resume => {
                info!("Resuming stopped branches and containers");
                let postgres_operator = PostgresOperator::new();

                let mut errors = Vec::new();
                for branch in &self.state.config.branches {
                    debug!("Starting branch container: {}", branch.name);
                    if let Err(e) = postgres_operator
                        .start_or_create(&self.state.config, branch.port, &branch.name)
                        .await
                    {
                        errors.push(format!("{}: {}", branch.name, e));
                    }
                }

                if !errors.is_empty() {
                    return Err(AppError::Docker {
                        message: format!("Some branches failed to resume: {}", errors.join("; ")),
                    });
                }
                info!("All branches and containers resumed successfully");
                Ok(())
            }
            Commands::Dump(args) => self.dump(args).await,
            Commands::Import(args) => self.import(args).await,
            Commands::Psql(args) => self.psql(args).await,
            Commands::Url(args) => self.url(args).await,
            Commands::Ui => self.open_ui().await,
            Commands::Logs(args) => self.print_logs(args).await,
            Commands::Schema(args) => self.print_schema(args).await,
            Commands::Resources => self.print_resources().await,
            Commands::Query(args) => self.run_query(args).await,
        }
    }

    async fn open_ui(&self) -> Result<(), AppError> {
        let url = format!("http://127.0.0.1:{}", self.state.config.api_port);
        info!("Opening {} in default browser", url);
        match webbrowser::open(&url) {
            Ok(_) => Ok(()),
            Err(e) => {
                // Headless / no GUI — fall back to printing the URL.
                println!("{}", url);
                debug!("webbrowser::open failed (printing URL instead): {}", e);
                Ok(())
            }
        }
    }

    fn resolve_branch_or_active(&self, requested: Option<String>) -> Result<String, AppError> {
        if let Some(name) = requested {
            if !self.state.config.branches.iter().any(|b| b.name == name) {
                return Err(AppError::BranchNotFound { name });
            }
            return Ok(name);
        }
        Ok(self
            .state
            .config
            .active_branch
            .clone()
            .unwrap_or_else(|| "main".to_string()))
    }

    async fn dump(&self, args: crate::cli::DumpArgs) -> Result<(), AppError> {
        let branch = self.resolve_branch_or_active(args.branch)?;
        let output = args
            .output
            .unwrap_or_else(|| crate::dump::default_dump_path(&self.state.config, &branch, args.format));

        info!("Dumping branch '{}' to {:?}", branch, output);
        let file = tokio::fs::File::create(&output).await.map_err(|e| AppError::FileSystem {
            message: format!("Failed to create output file {:?}: {}", output, e),
        })?;
        let mut writer = tokio::io::BufWriter::new(file);

        crate::dump::dump_branch(&self.state.config, &branch, &mut writer, args.format).await?;
        tokio::io::AsyncWriteExt::flush(&mut writer)
            .await
            .map_err(|e| AppError::FileSystem {
                message: format!("Failed to flush output: {}", e),
            })?;

        info!("Dump complete: {}", output.to_string_lossy());
        Ok(())
    }

    async fn import(&self, args: crate::cli::ImportArgs) -> Result<(), AppError> {
        if args.branch == "main" && !args.allow_main {
            return Err(AppError::Permission {
                message: "Refusing to import into 'main' — pass --allow-main to override".into(),
            });
        }
        if !self.state.config.branches.iter().any(|b| b.name == args.branch) {
            return Err(AppError::BranchNotFound { name: args.branch });
        }

        let file = tokio::fs::File::open(&args.input).await.map_err(|e| AppError::FileSystem {
            message: format!("Failed to open input file {:?}: {}", args.input, e),
        })?;
        let mut reader = tokio::io::BufReader::new(file);

        info!("Importing {:?} into branch '{}' (mode: {:?})", args.input, args.branch, args.mode);
        crate::dump::import_branch(&self.state.config, &args.branch, &mut reader, args.mode).await?;
        info!("Import complete");
        Ok(())
    }

    async fn psql(&self, args: crate::cli::PsqlArgs) -> Result<(), AppError> {
        let branch = self.resolve_branch_or_active(args.branch)?;
        let cfg = &self.state.config;
        let container = format!("{}_{}", cfg.name, branch);
        let db = cfg.postgres_config.database.as_deref().unwrap_or("dbranch");

        info!("Opening psql against branch '{}' (container {})", branch, container);
        let status = std::process::Command::new("docker")
            .args([
                "exec", "-it", &container, "psql", "-U", &cfg.postgres_config.user, "-d", db,
            ])
            .status()
            .map_err(|e| AppError::Docker {
                message: format!("Failed to spawn docker exec for psql: {}", e),
            })?;

        if !status.success() {
            return Err(AppError::Database {
                message: format!("psql exited with status {}", status),
            });
        }
        Ok(())
    }

    async fn url(&self, args: crate::cli::UrlArgs) -> Result<(), AppError> {
        let branch = self.resolve_branch_or_active(args.branch)?;
        let cfg = &self.state.config;
        let branch_entry = cfg
            .branches
            .iter()
            .find(|b| b.name == branch)
            .ok_or_else(|| AppError::BranchNotFound { name: branch.clone() })?;
        let pg = &cfg.postgres_config;
        let db = pg.database.as_deref().unwrap_or("dbranch");
        println!(
            "postgresql://{}:{}@127.0.0.1:{}/{}",
            pg.user, pg.password, branch_entry.port, db
        );
        Ok(())
    }

    async fn print_logs(&self, args: crate::cli::LogsArgs) -> Result<(), AppError> {
        // `--server` flag overrides; otherwise the presence of a branch arg
        // selects per-container logs.
        if args.server || args.branch.is_none() {
            let lines = crate::logbuf::global()
                .map(|b| b.snapshot(args.tail))
                .unwrap_or_default();
            for line in lines {
                println!("{}", line);
            }
            return Ok(());
        }

        let branch = args.branch.unwrap();
        let cfg = &self.state.config;
        if !cfg.branches.iter().any(|b| b.name == branch) {
            return Err(AppError::BranchNotFound { name: branch });
        }
        let container = format!("{}_{}", cfg.name, branch);
        let out = tokio::process::Command::new("docker")
            .args(["logs", "--tail", &args.tail.to_string(), &container])
            .output()
            .await
            .map_err(|e| AppError::Docker {
                message: format!("Failed to run docker logs for {}: {}", container, e),
            })?;
        // Postgres writes to stderr by default; stream both back to ours.
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    }

    async fn run_query(&self, args: crate::cli::QueryArgs) -> Result<(), AppError> {
        let cfg = &self.state.config;
        if !cfg.branches.iter().any(|b| b.name == args.branch) {
            return Err(AppError::BranchNotFound { name: args.branch });
        }

        // SQL precedence: positional > --file. Exactly one is required.
        let sql = match (args.sql, args.file) {
            (Some(s), _) => s,
            (None, Some(path)) => {
                std::fs::read_to_string(&path).map_err(|e| AppError::FileSystem {
                    message: format!("Failed to read {:?}: {}", path, e),
                })?
            }
            (None, None) => {
                return Err(AppError::Database {
                    message: "Provide SQL as a positional arg or via -f <file>".into(),
                });
            }
        };

        let response = crate::query::run(cfg, &args.branch, &sql).await?;
        print_query_response(&response);
        Ok(())
    }

    async fn print_resources(&self) -> Result<(), AppError> {
        let rows = crate::docker_stats::collect_resources(&self.state.config).await;
        if rows.is_empty() {
            println!("(no running branches)");
            return Ok(());
        }
        use crate::docker_stats::fmt_bytes;

        let mut table = Table::new();
        table.add_row(Row::new(vec![
            Cell::new("Branch").with_style(Attr::Bold),
            Cell::new("CPU%").with_style(Attr::Bold),
            Cell::new("Memory").with_style(Attr::Bold),
            Cell::new("Net rx/tx").with_style(Attr::Bold),
            Cell::new("Block r/w").with_style(Attr::Bold),
            Cell::new("PIDs").with_style(Attr::Bold),
        ]));
        for r in rows {
            table.add_row(Row::new(vec![
                Cell::new(&r.branch),
                Cell::new(&format!("{:.1}", r.cpu_pct)),
                Cell::new(&format!(
                    "{} / {} ({:.1}%)",
                    fmt_bytes(r.mem_used_bytes),
                    fmt_bytes(r.mem_limit_bytes),
                    r.mem_pct
                )),
                Cell::new(&format!(
                    "{} / {}",
                    fmt_bytes(r.net_rx_bytes),
                    fmt_bytes(r.net_tx_bytes)
                )),
                Cell::new(&format!(
                    "{} / {}",
                    fmt_bytes(r.block_read_bytes),
                    fmt_bytes(r.block_write_bytes)
                )),
                Cell::new(&r.pids.to_string()),
            ]));
        }
        let _ = table.print_tty(true);
        Ok(())
    }

    async fn print_schema(&self, args: crate::cli::SchemaArgs) -> Result<(), AppError> {
        let cfg = &self.state.config;
        if !cfg.branches.iter().any(|b| b.name == args.branch) {
            return Err(AppError::BranchNotFound { name: args.branch });
        }

        if let Some(against) = args.diff_against.as_ref() {
            if !cfg.branches.iter().any(|b| b.name == *against) {
                return Err(AppError::BranchNotFound {
                    name: against.clone(),
                });
            }
            if *against == args.branch {
                println!("(no differences — same branch)");
                return Ok(());
            }
            let (left, right) = tokio::try_join!(
                crate::schema::introspect(cfg, against),
                crate::schema::introspect(cfg, &args.branch),
            )?;
            let diff = crate::schema_diff::diff(&left, &right);
            print_schema_diff(&diff, against, &args.branch);
            return Ok(());
        }

        let schema = crate::schema::introspect(cfg, &args.branch).await?;
        print_schema_tree(&schema);
        Ok(())
    }

    async fn show_status(&self) -> Result<(), AppError> {
        info!("Showing status of the project");

        let postgres_operator = PostgresOperator::new();
        let config = &self.state.config;

        println!("{}", "=".repeat(80));
        println!("PROJECT: {}", config.name);
        println!("{}", "-".repeat(80));
        println!("Path: {}", config_path().to_string_lossy());
        println!(
            "🌿 Active Branch: {}",
            config.active_branch.as_deref().unwrap_or("main")
        );
        println!("{}", "-".repeat(80));

        let mut table = Table::new();
        table.add_row(Row::new(vec![
            Cell::new("Branch").with_style(Attr::Bold),
            Cell::new("Logical Size").with_style(Attr::Bold),
            Cell::new("Unique Data").with_style(Attr::Bold),
            Cell::new("Container").with_style(Attr::Bold),
            Cell::new("Age").with_style(Attr::Bold),
        ]));

        // Sort: main first, then created order.
        let mut branches: Vec<&Branch> = config.branches.iter().collect();
        branches.sort_by_key(|b| (!b.is_main, b.created_at));

        for branch in branches {
            let branch_dir = Path::new(&config.mount_point)
                .join(&config.name)
                .join(&branch.name);

            let (logical, shared) = match get_folder_size(&branch_dir) {
                Some(info) => (info.logical_size, info.shared_size),
                None => (0, 0),
            };

            let container_name = format!("{}_{}", config.name, branch.name);
            let running = postgres_operator
                .is_container_running(&container_name)
                .await
                .unwrap_or(false);

            table.add_row(Row::new(vec![
                Cell::new(&branch.name).with_style(Attr::Bold),
                Cell::new(Size::from_bytes(logical).to_string().as_str()),
                Cell::new(Size::from_bytes(logical.saturating_sub(shared)).to_string().as_str()),
                Cell::new(if running { "✅ Running" } else { "❌ Stopped" }),
                Cell::new(&format_age(branch.created_at)),
            ]));
        }

        let _ = table.print_tty(true);
        println!("{}", "=".repeat(80));
        Ok(())
    }

    async fn list_projects(&self) -> Result<(), AppError> {
        info!("Listing projects");
        let registry = Registry::load_or_create()?;
        let postgres_operator = PostgresOperator::new();

        if registry.projects.is_empty() {
            println!("No projects registered. Run `dbranch init -n <name>` to create one.");
            return Ok(());
        }

        let mut table = Table::new();
        table.add_row(Row::new(vec![
            Cell::new("Project").with_style(Attr::Bold),
            Cell::new("Default").with_style(Attr::Bold),
            Cell::new("Active Branch").with_style(Attr::Bold),
            Cell::new("Branches").with_style(Attr::Bold),
            Cell::new("Running").with_style(Attr::Bold),
        ]));

        for name in registry.list() {
            let cfg = match Config::load(&name) {
                Ok(c) => c,
                Err(_) => continue, // broken project; skip
            };
            let active = cfg.active_branch.as_deref().unwrap_or("main").to_string();
            let total = cfg.branches.len();
            let mut running = 0;
            for b in &cfg.branches {
                let container = format!("{}_{}", cfg.name, b.name);
                if postgres_operator
                    .is_container_running(&container)
                    .await
                    .unwrap_or(false)
                {
                    running += 1;
                }
            }
            let is_default = registry.default.as_deref() == Some(name.as_str());
            table.add_row(Row::new(vec![
                Cell::new(&name),
                Cell::new(if is_default { "*" } else { "" }),
                Cell::new(&active),
                Cell::new(&total.to_string()),
                Cell::new(&format!("{}/{}", running, total)),
            ]));
        }

        let _ = table.print_tty(true);
        Ok(())
    }

    async fn delete_branch(&mut self, name: &str) -> Result<(), AppError> {
        info!("Deleting branch '{}'", name);

        if name == "main" {
            return Err(AppError::Permission {
                message: "Refusing to delete the main branch — use `delete-project` instead".into(),
            });
        }

        let active = self.state.config.active_branch.as_deref();
        if active == Some(name) {
            return Err(AppError::Permission {
                message: format!(
                    "Refusing to delete active branch '{}' — switch with `dbranch use main` first",
                    name
                ),
            });
        }

        if !self.state.config.branches.iter().any(|b| b.name == name) {
            return Err(AppError::BranchNotFound { name: name.into() });
        }

        let postgres_operator = PostgresOperator::new();
        let _ = postgres_operator
            .delete_database(&self.state.config, name)
            .await;

        let branch_dir = Path::new(&self.state.config.mount_point)
            .join(&self.state.config.name)
            .join(name);
        if branch_dir.exists() {
            fs::remove_dir_all(&branch_dir).map_err(|e| AppError::FileSystem {
                message: format!("Failed to remove {:?}: {}", branch_dir, e),
            })?;
        }

        self.state.config.branches.retain(|b| b.name != name);
        self.state.config.save_config()?;

        info!("Branch '{}' deleted", name);
        Ok(())
    }

    async fn show_branch(&self, name: &str) -> Result<(), AppError> {
        info!("Showing branch '{}'", name);
        let branch = self
            .state
            .config
            .branches
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| AppError::BranchNotFound { name: name.into() })?;

        let cfg = &self.state.config;
        let postgres_operator = PostgresOperator::new();
        let container = format!("{}_{}", cfg.name, branch.name);
        let running = postgres_operator
            .is_container_running(&container)
            .await
            .unwrap_or(false);

        let branch_dir = Path::new(&cfg.mount_point).join(&cfg.name).join(&branch.name);
        let (logical, shared) = match get_folder_size(&branch_dir) {
            Some(info) => (info.logical_size, info.shared_size),
            None => (0, 0),
        };

        let pg = &cfg.postgres_config;
        let db = pg.database.as_deref().unwrap_or("dbranch");

        println!("{}", "=".repeat(80));
        println!("BRANCH: {} (project: {})", branch.name, cfg.name);
        println!("{}", "-".repeat(80));
        println!("Main:            {}", branch.is_main);
        println!("Port:            {}", branch.port);
        println!("Container:       {} ({})", container, if running { "running" } else { "stopped" });
        println!("Created:         {}", branch.created_at);
        println!("Age:             {}", format_age(branch.created_at));
        println!("Logical size:    {}", Size::from_bytes(logical));
        println!("Unique data:     {}", Size::from_bytes(logical.saturating_sub(shared)));
        println!("Data path:       {}", branch_dir.to_string_lossy());
        println!(
            "Connection URL:  postgresql://{}:{}@127.0.0.1:{}/{}",
            pg.user, pg.password, branch.port, db
        );
        println!("{}", "=".repeat(80));
        Ok(())
    }

    async fn delete_project(&mut self, name: &str) -> Result<(), AppError> {
        info!("Deleting project: {}", name);

        if self.state.config.name != name {
            return Err(AppError::ProjectNotFound { name: name.into() });
        }

        let postgres_operator = PostgresOperator::new();
        for branch in self.state.config.branches.clone() {
            debug!("Deleting container for branch: {}", branch.name);
            let _ = postgres_operator
                .delete_database(&self.state.config, &branch.name)
                .await;
        }

        let project_dir = Path::new(&self.state.config.mount_point).join(name);
        if project_dir.exists() {
            let _ = fs::remove_dir_all(&project_dir);
        }

        // Remove the per-project config file.
        if let Some(path) = self.state.config.source.clone() {
            let _ = fs::remove_file(path);
        }

        // Update the registry.
        let mut registry = Registry::load_or_create()?;
        registry.remove(name);
        registry.save()?;

        info!("Project '{}' deleted", name);
        Ok(())
    }

    async fn create_postgres(
        &self,
        name: Option<String>,
        valid_port: u16,
    ) -> Result<(), AppError> {
        let postgres_operator = PostgresOperator::new();
        info!("Using port: {}", valid_port);
        let db_name = name.as_deref().unwrap_or("main");
        debug!("Starting PostgreSQL database: {}", db_name);
        // start_or_create is idempotent: handles re-running init-postgres,
        // resuming after a stop, etc., without erroring on existing containers.
        postgres_operator
            .start_or_create(&self.state.config, valid_port, db_name)
            .await?;
        info!("PostgreSQL database is up");
        Ok(())
    }
}

fn print_schema_tree(schema: &crate::schema::Schema) {
    if schema.tables.is_empty() {
        println!("(no user tables)");
        return;
    }
    for t in &schema.tables {
        println!();
        println!("┌─ {}.{}", t.schema, t.name);
        if !t.primary_key.is_empty() {
            println!("│  PRIMARY KEY: {}", t.primary_key.join(", "));
        }

        let mut col_tbl = Table::new();
        col_tbl.add_row(Row::new(vec![
            Cell::new("Column").with_style(Attr::Bold),
            Cell::new("Type").with_style(Attr::Bold),
            Cell::new("Nullable").with_style(Attr::Bold),
            Cell::new("Default").with_style(Attr::Bold),
        ]));
        for c in &t.columns {
            col_tbl.add_row(Row::new(vec![
                Cell::new(&c.name),
                Cell::new(&c.data_type),
                Cell::new(if c.is_nullable { "yes" } else { "no" }),
                Cell::new(c.default.as_deref().unwrap_or("")),
            ]));
        }
        let _ = col_tbl.print_tty(true);

        if !t.foreign_keys.is_empty() {
            println!("│  Foreign keys:");
            for fk in &t.foreign_keys {
                println!(
                    "│    {} ({}) → {}.{}({}) ON DELETE {} ON UPDATE {}",
                    fk.name,
                    fk.columns.join(", "),
                    fk.ref_schema,
                    fk.ref_table,
                    fk.ref_columns.join(", "),
                    fk.on_delete,
                    fk.on_update
                );
            }
        }
        if !t.indexes.is_empty() {
            println!("│  Indexes:");
            for ix in &t.indexes {
                let flags = match (ix.is_primary, ix.is_unique) {
                    (true, _) => "PRIMARY",
                    (false, true) => "UNIQUE",
                    _ => "",
                };
                println!(
                    "│    {} ({}){}",
                    ix.name,
                    ix.columns.join(", "),
                    if flags.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", flags)
                    }
                );
            }
        }
        println!("└─");
    }
}

fn print_query_response(r: &crate::query::QueryResponse) {
    use crate::query::QueryResponse::*;
    match r {
        Rows {
            columns,
            rows,
            truncated,
            elapsed_ms,
        } => {
            if columns.is_empty() && rows.is_empty() {
                println!("(no rows · {} ms)", elapsed_ms);
                return;
            }
            let mut table = Table::new();
            table.add_row(Row::new(
                columns
                    .iter()
                    .map(|c| Cell::new(c).with_style(Attr::Bold))
                    .collect(),
            ));
            for r in rows {
                table.add_row(Row::new(r.iter().map(|v| Cell::new(v)).collect()));
            }
            let _ = table.print_tty(true);
            let footer = if *truncated {
                format!(
                    "({} rows · truncated to 1000 · {} ms)",
                    rows.len(),
                    elapsed_ms
                )
            } else {
                format!("({} rows · {} ms)", rows.len(), elapsed_ms)
            };
            println!("{}", footer);
        }
        Command { message, elapsed_ms } => {
            println!("{} · {} ms", message, elapsed_ms);
        }
        Error { message, elapsed_ms } => {
            eprintln!("ERROR ({} ms): {}", elapsed_ms, message);
        }
    }
}

fn print_schema_diff(
    diff: &crate::schema_diff::SchemaDiff,
    against: &str,
    current: &str,
) {
    if diff.is_empty() {
        println!("(no differences between '{}' and '{}')", against, current);
        return;
    }

    println!();
    println!("Schema diff: against='{}' → current='{}'", against, current);
    println!("{}", "─".repeat(60));

    for t in &diff.added_tables {
        println!("+ TABLE  {}.{}", t.schema, t.name);
        for c in &t.columns {
            println!("    + {}  {}", c.name, c.data_type);
        }
    }
    for t in &diff.removed_tables {
        println!("- TABLE  {}.{}", t.schema, t.name);
        for c in &t.columns {
            println!("    - {}  {}", c.name, c.data_type);
        }
    }
    for td in &diff.changed_tables {
        println!("~ TABLE  {}.{}", td.schema, td.name);
        if td.primary_key_changed {
            println!(
                "    ~ PRIMARY KEY: [{}] → [{}]",
                td.old_primary_key.join(", "),
                td.new_primary_key.join(", ")
            );
        }
        for c in &td.added_columns {
            println!("    + col  {}  {}", c.name, c.data_type);
        }
        for c in &td.removed_columns {
            println!("    - col  {}  {}", c.name, c.data_type);
        }
        for cc in &td.changed_columns {
            let mut parts = Vec::new();
            if cc.old.data_type != cc.new.data_type {
                parts.push(format!("type {} → {}", cc.old.data_type, cc.new.data_type));
            }
            if cc.old.is_nullable != cc.new.is_nullable {
                parts.push(format!(
                    "nullable {} → {}",
                    cc.old.is_nullable, cc.new.is_nullable
                ));
            }
            if cc.old.default != cc.new.default {
                parts.push(format!(
                    "default {:?} → {:?}",
                    cc.old.default.as_deref().unwrap_or(""),
                    cc.new.default.as_deref().unwrap_or("")
                ));
            }
            println!("    ~ col  {}  ({})", cc.name, parts.join("; "));
        }
        for fk in &td.added_foreign_keys {
            println!(
                "    + fk   {} ({}) → {}.{}",
                fk.name,
                fk.columns.join(", "),
                fk.ref_table,
                fk.ref_columns.join(", ")
            );
        }
        for fk in &td.removed_foreign_keys {
            println!("    - fk   {}", fk.name);
        }
        for ix in &td.added_indexes {
            println!("    + idx  {} ({})", ix.name, ix.columns.join(", "));
        }
        for ix in &td.removed_indexes {
            println!("    - idx  {}", ix.name);
        }
    }
    println!("{}", "─".repeat(60));
}

fn format_age(created_at: DateTime<Utc>) -> String {
    let duration = Utc::now() - created_at;
    if duration.num_days() > 0 {
        format!("{}d", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{}h", duration.num_hours())
    } else {
        format!("{}m", duration.num_minutes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use clap::Parser;

    #[test]
    fn format_age_minutes() {
        let t = Utc::now() - Duration::minutes(7);
        assert_eq!(format_age(t), "7m");
    }

    #[test]
    fn format_age_hours() {
        let t = Utc::now() - Duration::hours(3);
        assert_eq!(format_age(t), "3h");
    }

    #[test]
    fn format_age_days() {
        let t = Utc::now() - Duration::days(5);
        assert_eq!(format_age(t), "5d");
    }

    #[test]
    fn format_age_just_now_is_zero_minutes() {
        let t = Utc::now();
        assert_eq!(format_age(t), "0m");
    }

    #[test]
    fn cli_parses_start() {
        let cli = Cli::try_parse_from(["dbranch", "start"]).unwrap();
        assert!(matches!(cli.command, Commands::Start));
    }

    #[test]
    fn cli_parses_create_with_source() {
        let cli =
            Cli::try_parse_from(["dbranch", "create", "feat", "--source", "main"]).unwrap();
        match cli.command {
            Commands::Create(args) => {
                assert_eq!(args.name, "feat");
            }
            other => panic!("expected Create, got {:?}", other),
        }
    }

    #[test]
    fn cli_parses_use_branch() {
        let cli = Cli::try_parse_from(["dbranch", "use", "feature"]).unwrap();
        match cli.command {
            Commands::Use(args) => assert_eq!(args.name, "feature"),
            other => panic!("expected Use, got {:?}", other),
        }
    }

    #[test]
    fn cli_rejects_unknown_command() {
        let result = Cli::try_parse_from(["dbranch", "this-does-not-exist"]);
        assert!(result.is_err());
    }
}
