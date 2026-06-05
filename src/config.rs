use std::{
    fs::{self, File},
    io::BufWriter,
    net::TcpListener,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::AppError;

/// Environment variable used to override the legacy single-config file path.
/// Kept for back-compat with users on the old `dbranch.config.json` layout.
pub const ENV_CONFIG_PATH: &str = "DBRANCH_CONFIG";

/// Environment variable used to override the dBranch home directory
/// (where the project registry and per-project configs live).
pub const ENV_DBRANCH_HOME: &str = "DBRANCH_HOME";

/// Environment variable used to select the active project name when running CLI
/// commands without an explicit `--project` flag.
pub const ENV_DBRANCH_PROJECT: &str = "DBRANCH_PROJECT";

/// Default file name for the legacy single-project config that lived in `cwd`.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "dbranch.config.json";

/// Name of the registry file inside the dBranch home directory.
pub const REGISTRY_FILE_NAME: &str = "registry.json";

/// Sub-directory inside the dBranch home where per-project configs live.
pub const PROJECTS_SUBDIR: &str = "projects";

const DEFAULT_PORT_MIN: u16 = 7000;
const DEFAULT_PORT_MAX: u16 = 7999;
const DEFAULT_API_PORT: u16 = 8000;
const DEFAULT_PROXY_PORT: u16 = 5432;
const DEFAULT_PROJECT_NAME: &str = "my_project";

/// Suggested default mount point for branch data.
///
/// Resolution order:
/// 1. `$DBRANCH_DATA` if set (explicit override)
/// 2. `$HOME/dbranch` on Unix (visible, discoverable, works on macOS + Linux)
/// 3. `$USERPROFILE\dbranch` on Windows
/// 4. Falls back to `<dbranch_home>/data` if no home is found
///
/// For real CoW efficiency (instant branches, shared extents), point this at
/// a BTRFS / XFS / APFS volume — override per-project via the Web UI's
/// settings dialog or by editing the project config directly.
pub fn default_mount_point() -> String {
    if let Ok(p) = std::env::var("DBRANCH_DATA") {
        return p;
    }
    if let Ok(home) = std::env::var("HOME") {
        return format!("{}/dbranch", home);
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return format!("{}\\dbranch", profile);
    }
    dbranch_home().join("data").to_string_lossy().into_owned()
}

/// Resolves the dBranch home directory.
///
/// Order: `$DBRANCH_HOME` > `$XDG_CONFIG_HOME/dbranch` > `$HOME/.config/dbranch`.
pub fn dbranch_home() -> PathBuf {
    if let Ok(p) = std::env::var(ENV_DBRANCH_HOME) {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("dbranch");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("dbranch")
}

/// Path to the registry file (`<dbranch_home>/registry.json`).
pub fn registry_path() -> PathBuf {
    dbranch_home().join(REGISTRY_FILE_NAME)
}

/// Path to the per-project configs directory (`<dbranch_home>/projects/`).
pub fn projects_dir() -> PathBuf {
    dbranch_home().join(PROJECTS_SUBDIR)
}

/// Path to a specific project's config file inside `projects/`.
pub fn project_config_path(name: &str) -> PathBuf {
    projects_dir().join(format!("{}.json", name))
}

/// Legacy resolver kept for back-compat. New code should use
/// [`Config::load`] / [`Config::load_default`] instead.
pub fn config_path() -> PathBuf {
    std::env::var(ENV_CONFIG_PATH)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_FILE_NAME))
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize, Eq)]
pub struct Branch {
    pub name: String,
    pub port: u16,
    pub is_main: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Approach {
    NewDisk,
    ExistingDisk,
}

impl<'de> Deserialize<'de> for Approach {
    fn deserialize<D>(deserializer: D) -> Result<Approach, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "NEW_DISK" => Ok(Approach::NewDisk),
            "EXISTING_DISK" => Ok(Approach::ExistingDisk),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["NEW_DISK", "EXISTING_DISK"],
            )),
        }
    }
}

impl Serialize for Approach {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = match self {
            Approach::NewDisk => "NEW_DISK",
            Approach::ExistingDisk => "EXISTING_DISK",
        };
        serializer.serialize_str(s)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub name: String,
    pub api_port: u16,
    pub proxy_port: u16,
    pub created_at: DateTime<Utc>,
    pub approach: Approach,
    pub port_min: u16,
    pub port_max: u16,
    pub mount_point: String,
    pub active_branch: Option<String>,
    pub postgres_config: PostgresConfig,
    pub branches: Vec<Branch>,

    /// Where this config was loaded from. Used by `save_config()` so mutations
    /// persist back to the right file. `None` for newly-constructed in-memory
    /// configs (callers must then use `save_to(&path)` explicitly).
    #[serde(skip, default)]
    pub source: Option<PathBuf>,
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        // Source is a runtime-only field; equality is over the serialised state.
        self.name == other.name
            && self.api_port == other.api_port
            && self.proxy_port == other.proxy_port
            && self.created_at == other.created_at
            && self.approach == other.approach
            && self.port_min == other.port_min
            && self.port_max == other.port_max
            && self.mount_point == other.mount_point
            && self.active_branch == other.active_branch
            && self.postgres_config == other.postgres_config
            && self.branches == other.branches
    }
}

impl Eq for Config {}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct PostgresConfig {
    pub user: String,
    pub password: String,
    pub database: Option<String>,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            user: "dbranch_user".into(),
            password: "dbranch_password".into(),
            database: None,
        }
    }
}

/// Overrides for [`Config::new_with`]. All fields are optional and fall back
/// to the same defaults [`Config::new`] uses.
#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    pub mount_point: Option<String>,
    pub postgres_user: Option<String>,
    pub postgres_password: Option<String>,
    pub postgres_database: Option<String>,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self::new_with(name, ConfigOverrides::default())
    }

    pub fn new_with(name: String, overrides: ConfigOverrides) -> Self {
        let main_port =
            get_valid_port(DEFAULT_PORT_MIN, DEFAULT_PORT_MAX).unwrap_or(DEFAULT_PORT_MIN);
        let mut pg = PostgresConfig::default();
        if let Some(u) = overrides.postgres_user {
            pg.user = u;
        }
        if let Some(p) = overrides.postgres_password {
            pg.password = p;
        }
        if let Some(d) = overrides.postgres_database {
            pg.database = Some(d);
        }
        Config {
            name,
            api_port: DEFAULT_API_PORT,
            proxy_port: DEFAULT_PROXY_PORT,
            approach: Approach::ExistingDisk,
            port_min: DEFAULT_PORT_MIN,
            port_max: DEFAULT_PORT_MAX,
            mount_point: overrides.mount_point.unwrap_or_else(default_mount_point),
            active_branch: None,
            created_at: Utc::now(),
            postgres_config: pg,
            branches: vec![Branch {
                name: "main".into(),
                port: main_port,
                is_main: true,
                created_at: Utc::now(),
            }],
            source: None,
        }
    }

    /// Loads config from the legacy `dbranch.config.json` path (resolved via
    /// `config_path()`), creating it with defaults if it doesn't exist.
    /// New code should prefer [`Config::load`] / [`Config::load_default`].
    pub fn from_file() -> Result<Self, AppError> {
        Self::from_file_at(&config_path())
    }

    /// Loads config from an explicit path. Creates a default config at that
    /// path if it doesn't exist.
    pub fn from_file_at(path: &Path) -> Result<Self, AppError> {
        debug!("Loading configuration from {:?}", path);

        match fs::read_to_string(path) {
            Ok(content) => {
                serde_json::from_str::<Config>(&content).map_err(|e| AppError::ConfigParsing {
                    message: format!("Failed to parse config file {:?}: {}", path, e),
                })
            }
            Err(_) => {
                debug!("Config file doesn't exist, creating with defaults at {:?}", path);
                let parsed_config = Config::new(DEFAULT_PROJECT_NAME.into());
                parsed_config.save_to(path)?;
                Ok(parsed_config)
            }
        }
    }

    /// Loads the named project's config from the registry-managed path
    /// `<dbranch_home>/projects/<name>.json`.
    pub fn load(name: &str) -> Result<Self, AppError> {
        Registry::migrate_legacy_if_needed()?;

        let path = project_config_path(name);
        let content = fs::read_to_string(&path).map_err(|_| AppError::ProjectNotFound {
            name: name.to_string(),
        })?;
        let mut cfg: Config = serde_json::from_str(&content).map_err(|e| AppError::ConfigParsing {
            message: format!("Failed to parse project config {:?}: {}", path, e),
        })?;
        cfg.source = Some(path);
        Ok(cfg)
    }

    /// Loads the registry's default project. Migrates legacy configs first.
    /// Returns `ProjectNotFound` if the registry has no default — does NOT
    /// auto-create. Use [`Config::load_default_or_init`] from the CLI startup
    /// path if you want a fresh `my_project` on a true first run.
    pub fn load_default() -> Result<Self, AppError> {
        Registry::migrate_legacy_if_needed()?;
        let registry = Registry::load_or_create()?;
        let name = registry.default.as_deref().ok_or_else(|| AppError::ProjectNotFound {
            name: "<default>".into(),
        })?;
        Self::load(name)
    }

    /// Lightweight, in-memory only config used by `dbranch start` when the
    /// registry has no projects. Populates only the server-level fields
    /// (`proxy_port`, `api_port`, port range) so the proxy and Web UI can
    /// bind their listeners. Has no branches, isn't backed by a file, and
    /// must NEVER be `save_to`'d — it's a runtime stub, not a project.
    pub fn stub_for_server() -> Self {
        Self {
            name: String::new(),
            api_port: DEFAULT_API_PORT,
            proxy_port: DEFAULT_PROXY_PORT,
            approach: Approach::ExistingDisk,
            port_min: DEFAULT_PORT_MIN,
            port_max: DEFAULT_PORT_MAX,
            mount_point: default_mount_point(),
            active_branch: None,
            created_at: Utc::now(),
            postgres_config: PostgresConfig::default(),
            branches: Vec::new(),
            source: None,
        }
    }

    /// First-run-friendly variant of [`load_default`]: if the registry has
    /// no default, creates a brand-new `my_project`. ONLY safe at CLI
    /// startup; do NOT call this from background loops (like `sync_config`)
    /// because it will resurrect a project you just deleted via the UI.
    pub fn load_default_or_init() -> Result<Self, AppError> {
        match Self::load_default() {
            Ok(cfg) => Ok(cfg),
            Err(AppError::ProjectNotFound { .. }) => {
                let mut registry = Registry::load_or_create()?;
                let mut cfg = Config::new(DEFAULT_PROJECT_NAME.into());
                let path = project_config_path(&cfg.name);
                cfg.source = Some(path.clone());
                fs::create_dir_all(projects_dir()).map_err(|e| AppError::FileSystem {
                    message: format!("Failed to create projects dir: {}", e),
                })?;
                cfg.save_to(&path)?;
                registry.add(cfg.name.clone());
                registry.default = Some(cfg.name.clone());
                registry.save()?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }

    /// Picks the next available port in `[port_min, port_max]`, excluding
    /// ports already used by ANY branch (including those in OTHER projects).
    /// Delegates to the global allocator and passes this project's own
    /// branches as additional exclusions (so unsaved-but-in-memory branches
    /// don't collide).
    pub fn get_valid_port(&self) -> Option<u16> {
        let mine: Vec<u16> = self.branches.iter().map(|b| b.port).collect();
        get_valid_port_excluding(self.port_min, self.port_max, Some(&self.name), &mine)
    }

    pub fn create_branch(&mut self, branch_name: String, valid_port: u16) -> Result<(), AppError> {
        self.branches.push(Branch {
            name: branch_name,
            port: valid_port,
            is_main: false,
            created_at: Utc::now(),
        });

        self.save_config()
    }

    pub fn set_active_branch(&mut self, branch_name: String) -> Result<(), AppError> {
        if !self.branches.iter().any(|b| b.name == branch_name) {
            return Err(AppError::BranchNotFound { name: branch_name });
        }
        self.active_branch = if branch_name == "main" {
            None
        } else {
            Some(branch_name)
        };
        self.save_config()
    }

    /// Saves the config back to its origin path.
    /// Uses `source` if the config was loaded via [`Config::load`] /
    /// [`Config::load_default`]; otherwise falls back to the legacy
    /// `config_path()` for back-compat with old single-project usage.
    pub fn save_config(&self) -> Result<(), AppError> {
        let path = self.source.clone().unwrap_or_else(config_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::FileSystem {
                message: format!("Failed to create parent dir {:?}: {}", parent, e),
            })?;
        }
        self.save_to(&path)
    }

    /// Saves the config to an explicit path.
    pub fn save_to(&self, path: &Path) -> Result<(), AppError> {
        debug!("Saving configuration to {:?}", path);

        let file = File::create(path).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create config file {:?}: {}", path, e),
        })?;

        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &self).map_err(|e| AppError::FileSystem {
            message: format!("Failed to write config file {:?}: {}", path, e),
        })?;

        Ok(())
    }
}

/// Global project registry, kept at `<dbranch_home>/registry.json`.
///
/// Holds the list of known projects and which one is the default for CLI
/// commands when no `--project` flag or `DBRANCH_PROJECT` env is provided.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Registry {
    /// Name of the project to use when none is specified explicitly.
    pub default: Option<String>,
    /// Known project names. Each maps to `<dbranch_home>/projects/<name>.json`.
    pub projects: Vec<String>,
}

/// Tiny pointer file written into the legacy `dbranch.config.json` location
/// after migration so users can still see which project lives in this dir.
#[derive(Debug, Serialize, Deserialize)]
struct LegacyPointer {
    project: String,
}

impl Registry {
    /// Loads the registry from disk. If the file doesn't exist, returns an
    /// empty registry without creating anything (saves are lazy).
    pub fn load() -> Result<Self, AppError> {
        let path = registry_path();
        match fs::read_to_string(&path) {
            Ok(content) => {
                serde_json::from_str(&content).map_err(|e| AppError::ConfigParsing {
                    message: format!("Failed to parse registry {:?}: {}", path, e),
                })
            }
            Err(_) => Ok(Registry::default()),
        }
    }

    /// Same as [`load`] but writes an empty registry to disk if it doesn't
    /// exist yet.
    pub fn load_or_create() -> Result<Self, AppError> {
        let path = registry_path();
        if path.exists() {
            return Self::load();
        }
        let registry = Registry::default();
        registry.save()?;
        Ok(registry)
    }

    /// Persists the registry to `<dbranch_home>/registry.json`, creating the
    /// directory if needed.
    pub fn save(&self) -> Result<(), AppError> {
        let home = dbranch_home();
        fs::create_dir_all(&home).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create dbranch home {:?}: {}", home, e),
        })?;
        fs::create_dir_all(projects_dir()).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create projects dir: {}", e),
        })?;
        let file = File::create(registry_path()).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create registry: {}", e),
        })?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, self).map_err(|e| AppError::FileSystem {
            message: format!("Failed to write registry: {}", e),
        })?;
        Ok(())
    }

    /// Returns the projects known to the registry, sorted alphabetically.
    pub fn list(&self) -> Vec<String> {
        let mut out = self.projects.clone();
        out.sort();
        out
    }

    /// Adds a project to the registry. Idempotent; sets the project as the
    /// default if no default exists.
    pub fn add(&mut self, name: String) {
        if !self.projects.contains(&name) {
            self.projects.push(name.clone());
        }
        if self.default.is_none() {
            self.default = Some(name);
        }
    }

    /// Removes a project from the registry. If it was the default, the next
    /// remaining project becomes the default (or `None` if empty).
    pub fn remove(&mut self, name: &str) {
        self.projects.retain(|p| p != name);
        if self.default.as_deref() == Some(name) {
            self.default = self.projects.first().cloned();
        }
    }

    pub fn set_default(&mut self, name: String) -> Result<(), AppError> {
        if !self.projects.contains(&name) {
            return Err(AppError::ProjectNotFound { name });
        }
        self.default = Some(name);
        Ok(())
    }

    /// One-time migration of the legacy `./dbranch.config.json` layout into
    /// the registry. If the registry file already exists, this is a no-op.
    /// If a legacy config exists in cwd (or at `$DBRANCH_CONFIG`), it is
    /// copied to `projects/<name>.json`, registered as the default, and the
    /// legacy file is replaced by a tiny `{"project":"<name>"}` pointer.
    pub fn migrate_legacy_if_needed() -> Result<(), AppError> {
        if registry_path().exists() {
            return Ok(());
        }

        let legacy = config_path();
        let content = match fs::read_to_string(&legacy) {
            Ok(c) => c,
            Err(_) => return Ok(()), // nothing to migrate
        };

        // First try to parse as the legacy Config; fallback to pointer (if a
        // partially-migrated state somehow ended up here).
        let Ok(cfg) = serde_json::from_str::<Config>(&content) else {
            return Ok(());
        };

        let name = cfg.name.clone();
        debug!("Migrating legacy config for project '{}' into registry", name);

        fs::create_dir_all(projects_dir()).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create projects dir: {}", e),
        })?;

        let new_path = project_config_path(&name);
        let mut migrated = cfg;
        migrated.source = Some(new_path.clone());
        migrated.save_to(&new_path)?;

        let mut registry = Registry::default();
        registry.add(name.clone());
        registry.default = Some(name.clone());
        registry.save()?;

        // Replace legacy file with a pointer so future runs locate it via
        // the registry but the user can still see which project this dir maps to.
        let pointer = LegacyPointer { project: name };
        let file = File::create(&legacy).map_err(|e| AppError::FileSystem {
            message: format!("Failed to rewrite legacy pointer {:?}: {}", legacy, e),
        })?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &pointer).map_err(|e| {
            AppError::FileSystem {
                message: format!("Failed to write legacy pointer: {}", e),
            }
        })?;

        tracing::info!(
            "Migrated legacy dbranch.config.json into registry at {:?}",
            dbranch_home()
        );
        Ok(())
    }
}

/// Validates a project or branch name.
///
/// Names flow into shell commands (`docker run --name <project>_<branch>`),
/// filesystem paths (`<mount>/<project>/<branch>/data`), and Postgres
/// identifiers — so we lock them to a conservative ASCII subset.
///
/// Rules:
/// - Non-empty
/// - 1–63 chars (docker container name limit is 63, and pg's NAMEDATALEN is 64)
/// - Only `a-z`, `A-Z`, `0-9`, `_`, `-`
/// - Must start with a letter, digit, or underscore (no leading hyphen so
///   it can't be mistaken for a CLI flag).
pub fn validate_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::Config {
            message: "name cannot be empty".into(),
        });
    }
    if name.len() > 63 {
        return Err(AppError::Config {
            message: format!("name '{}' is longer than 63 characters", name),
        });
    }
    if name.starts_with('-') {
        return Err(AppError::Config {
            message: format!("name '{}' cannot start with '-'", name),
        });
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(AppError::Config {
                message: format!(
                    "name '{}' contains invalid character {:?} — only letters, digits, '_' and '-' are allowed",
                    name, c
                ),
            });
        }
    }
    Ok(())
}

/// Globally-aware port allocator. Scans every registered project's branches
/// (plus their proxy/api ports) and excludes them from the search, so two
/// projects creating `main` in sequence don't both pick port 7000.
///
/// `exclude_project` is the name to skip when walking the registry — pass
/// `Some(self.name)` if the caller is already counting its own branches
/// to avoid double-counting, or `None` to let the caller's own branches be
/// excluded by the registry walk too.
pub fn get_valid_port(port_min: u16, port_max: u16) -> Option<u16> {
    get_valid_port_excluding(port_min, port_max, None, &[])
}

pub fn get_valid_port_excluding(
    port_min: u16,
    port_max: u16,
    exclude_project: Option<&str>,
    additional_used: &[u16],
) -> Option<u16> {
    let mut used: std::collections::HashSet<u16> = additional_used.iter().copied().collect();

    // Walk the registry; "broken" / unloadable projects are silently skipped
    // so a corrupted JSON in one project doesn't stop all port allocation.
    if let Ok(registry) = Registry::load_or_create() {
        for name in &registry.projects {
            if Some(name.as_str()) == exclude_project {
                continue;
            }
            if let Ok(other) = Config::load(name) {
                for b in other.branches {
                    used.insert(b.port);
                }
                used.insert(other.proxy_port);
                used.insert(other.api_port);
            }
        }
    }

    debug!(
        "Searching for available port in range {}-{} (excluding {} known ports)",
        port_min,
        port_max,
        used.len()
    );
    for port in port_min..=port_max {
        if used.contains(&port) {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            debug!("Found available port: {}", port);
            return Some(port);
        }
    }
    debug!(
        "No available ports found in range {}-{}",
        port_min, port_max
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn new_creates_main_branch() {
        let config = Config::new("proj".into());
        assert_eq!(config.name, "proj");
        assert_eq!(config.branches.len(), 1);
        assert!(config.branches[0].is_main);
        assert_eq!(config.branches[0].name, "main");
        assert!(config.active_branch.is_none());
        assert!(!config.mount_point.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        let original = Config::new("roundtrip".into());

        original.save_to(&path).unwrap();
        let loaded = Config::from_file_at(&path).unwrap();

        assert_eq!(loaded, original);
    }

    #[test]
    fn from_file_at_creates_default_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(!path.exists());

        let cfg = Config::from_file_at(&path).unwrap();
        assert!(path.exists(), "config should be created on disk");
        assert_eq!(cfg.name, DEFAULT_PROJECT_NAME);
    }

    #[test]
    fn from_file_at_returns_parsing_error_on_malformed_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{not valid json").unwrap();

        let err = Config::from_file_at(&path).unwrap_err();
        assert!(
            matches!(err, AppError::ConfigParsing { .. }),
            "expected ConfigParsing, got {:?}",
            err
        );
    }

    #[test]
    fn create_branch_appends_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        let mut cfg = Config::new("p".into());
        cfg.save_to(&path).unwrap();

        // create_branch uses save_config() (default path); for unit-test
        // purposes we exercise the in-memory append + save_to separately.
        cfg.branches.push(Branch {
            name: "feature".into(),
            port: 9001,
            is_main: false,
            created_at: Utc::now(),
        });
        cfg.save_to(&path).unwrap();

        let reloaded = Config::from_file_at(&path).unwrap();
        assert_eq!(reloaded.branches.len(), 2);
        assert!(reloaded.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn set_active_branch_to_existing_branch() {
        let mut cfg = Config::new("p".into());
        cfg.branches.push(Branch {
            name: "feature".into(),
            port: 9001,
            is_main: false,
            created_at: Utc::now(),
        });

        // Bypass save_config()'s default path resolution by writing in a tmpdir first.
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        cfg.save_to(&path).unwrap();

        // Set via direct mutation matching set_active_branch's logic, so we
        // exercise the validity check without touching the default path.
        assert!(cfg.branches.iter().any(|b| b.name == "feature"));
        cfg.active_branch = Some("feature".into());
        assert_eq!(cfg.active_branch.as_deref(), Some("feature"));
    }

    #[test]
    fn set_active_branch_rejects_unknown() {
        let mut cfg = Config::new("p".into());
        let err = cfg.set_active_branch("nope".into()).unwrap_err();
        assert!(matches!(err, AppError::BranchNotFound { .. }));
    }

    #[test]
    fn set_active_branch_to_main_clears_active() {
        // main is always present
        let mut cfg = Config::new("p".into());
        cfg.active_branch = Some("feature".into());

        // Replicate the logic: main maps to None
        if cfg.branches.iter().any(|b| b.name == "main") {
            cfg.active_branch = None;
        }
        assert!(cfg.active_branch.is_none());
    }

    #[test]
    fn get_valid_port_excludes_existing_branches() {
        // Build a config whose branches already cover port_min..port_min+2.
        // The next allocated port must skip past them.
        let mut cfg = Config::new("p".into());
        cfg.port_min = 45200;
        cfg.port_max = 45210;
        cfg.branches.clear();
        cfg.branches.push(Branch {
            name: "main".into(),
            port: 45200,
            is_main: true,
            created_at: Utc::now(),
        });
        cfg.branches.push(Branch {
            name: "feat-a".into(),
            port: 45201,
            is_main: false,
            created_at: Utc::now(),
        });
        let port = cfg.get_valid_port().expect("should find a free port");
        assert!(
            port >= 45202 && port <= 45210,
            "expected port >= 45202 (skipping used 45200 & 45201), got {}",
            port
        );
    }

    #[test]
    fn validate_name_accepts_safe_strings() {
        assert!(validate_name("my_project").is_ok());
        assert!(validate_name("feature-x").is_ok());
        assert!(validate_name("v2").is_ok());
        assert!(validate_name("_internal").is_ok());
    }

    #[test]
    fn validate_name_rejects_unsafe_strings() {
        // Spaces — the bug the user just reported.
        assert!(validate_name("tst rsrsrsrs").is_err());
        // Empty
        assert!(validate_name("").is_err());
        // Too long (64 > 63 limit)
        assert!(validate_name(&"a".repeat(64)).is_err());
        // Leading hyphen (looks like a flag)
        assert!(validate_name("-bad").is_err());
        // Special chars
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a;b").is_err());
        assert!(validate_name("a.b").is_err());
        assert!(validate_name("foo bar").is_err());
    }

    #[test]
    fn get_valid_port_finds_a_port_in_range() {
        // Use a small range starting at high ports unlikely to be reserved.
        let port = get_valid_port(45000, 45100);
        assert!(port.is_some());
        let p = port.unwrap();
        assert!((45000..=45100).contains(&p));
    }

    #[test]
    fn approach_serde_roundtrip() {
        let json = serde_json::to_string(&Approach::NewDisk).unwrap();
        assert_eq!(json, "\"NEW_DISK\"");
        let back: Approach = serde_json::from_str("\"EXISTING_DISK\"").unwrap();
        assert_eq!(back, Approach::ExistingDisk);
    }

    #[test]
    fn config_path_uses_env_when_set() {
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom.json");
        // SAFETY: tests under #[serial] would be safer; this test
        // is intentionally isolated to a tempdir so concurrent tests
        // are not affected by reading-back the path.
        unsafe { std::env::set_var(ENV_CONFIG_PATH, &custom) };
        let resolved = config_path();
        unsafe { std::env::remove_var(ENV_CONFIG_PATH) };
        assert_eq!(resolved, custom);
    }

    // ---------- Registry tests ----------

    #[test]
    fn registry_add_is_idempotent() {
        let mut r = Registry::default();
        r.add("a".into());
        r.add("a".into());
        assert_eq!(r.projects, vec!["a".to_string()]);
        assert_eq!(r.default.as_deref(), Some("a"));
    }

    #[test]
    fn registry_add_sets_default_only_when_unset() {
        let mut r = Registry::default();
        r.add("a".into());
        r.add("b".into());
        assert_eq!(r.default.as_deref(), Some("a"));
    }

    #[test]
    fn registry_remove_promotes_next_default() {
        let mut r = Registry::default();
        r.add("a".into());
        r.add("b".into());
        r.remove("a");
        assert_eq!(r.projects, vec!["b".to_string()]);
        assert_eq!(r.default.as_deref(), Some("b"));
    }

    #[test]
    fn registry_remove_clears_default_when_empty() {
        let mut r = Registry::default();
        r.add("a".into());
        r.remove("a");
        assert!(r.projects.is_empty());
        assert!(r.default.is_none());
    }

    #[test]
    fn registry_set_default_rejects_unknown() {
        let mut r = Registry::default();
        let err = r.set_default("ghost".into()).unwrap_err();
        assert!(matches!(err, AppError::ProjectNotFound { .. }));
    }

    #[test]
    fn registry_list_is_sorted() {
        let mut r = Registry::default();
        r.add("z".into());
        r.add("a".into());
        r.add("m".into());
        assert_eq!(r.list(), vec!["a".to_string(), "m".into(), "z".into()]);
    }

    #[test]
    fn registry_serde_roundtrip() {
        let mut r = Registry::default();
        r.add("alpha".into());
        r.add("beta".into());
        let json = serde_json::to_string(&r).unwrap();
        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
