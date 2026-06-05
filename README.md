### EARLY DEVELOPMENT STAGE

> dBranch is currently in the early development stage. While the core functionality is implemented, there may be bugs and missing features. We welcome contributions and feedback from the community to help improve the project.

### *PLEASE DO NOT USE IN PRODUCTION*
---

## 🌿 dBranch - PostgreSQL Database Branching System

dBranch is a database branching system designed for PostgreSQL that empowers developers to effortlessly create, manage, and switch between multiple database branches.

Its key features include Instant Database Branching, which allows for the creation of lightweight branches using copy-on-write. This approach makes the system highly Resource Efficient, as all branches share common data blocks, dramatically minimizing storage overhead.

For isolation and stability, each branch operates within its own Isolated Environment—a dedicated Docker container that ensures no interference between branches and provides unique network ports.

Furthermore, dBranch includes a Transparent Proxy that enables seamless context switching between different database branches without requiring any changes to the application's connection string, streamlining the development workflow.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         dBranch System                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                     CLI Interface                        │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐   │   │
│  │  │   start  │  │  create  │  │   use    │  │  usage  │   │   │
│  │  └──────────┘  └──────────┘  └──────────┘  └─────────┘   │   │
│  └──────────────────────────────────────────────────────────┘   │
│                              │                                  │
│                              ▼                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                      Proxy Layer                         │   │
│  │                                                          │   │
│  │               PostgreSQL Proxy (Port 5432)               │   │
│  │                           ↓                              │   │
│  │            Routes to active branch container             │   │
│  └──────────────────────────────────────────────────────────┘   │
│                              │                                  │
│                              ▼                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                  Container Layer                         │   │
│  │                                                          │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐       │   │
│  │  │  Postgres   │  │  Postgres   │  │  Postgres   │       │   │
│  │  │  main:5433  │  │ branch:5434 │  │ branch:5435 │       │   │
│  │  └─────────────┘  └─────────────┘  └─────────────┘       │   │
│  └──────────────────────────────────────────────────────────┘   │
│                              │                                  │
│                              ▼                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                    Storage Layer                         │   │
│  │                                                          │   │
│  │  ┌─────────────────────────────────────────────────────┐ │   │
│  │  │                 COW Filesystem                      │ │   │
│  │  │                ┌──────────────┐                     │ │   │
│  │  │                │     main     │                     │ │   │
│  │  │                ├──────────────┤                     │ │   │
│  │  │                │   branch-1   │                     │ │   │
│  │  │                ├──────────────┤                     │ │   │
│  │  │                │   branch-2   │                     │ │   │
│  │  │                └──────────────┘                     │ │   │
│  │  └─────────────────────────────────────────────────────┘ │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                 │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Prerequisites

- **Operating System**: Any with CoW filesystem support (e.g., Linux with BTRFS)
- **Docker**: Installed and running
- **Rust**: 1.70+ (for building from source)


## Usage

### Quick start

```bash
# Create a project (registers it in ~/.config/dbranch/registry.json)
dbranch init -n my_app

# Start the main Postgres container
dbranch init-postgres

# Create a branch (CoW snapshot of main)
dbranch create feature-new-schema

# Switch active branch — the proxy at :5432 follows
dbranch use feature-new-schema

# Open psql against any branch
dbranch psql feature-new-schema

# Get a connection URL
dbranch url feature-new-schema

# Dump / restore
dbranch dump feature-new-schema -o /tmp/dump.dump
dbranch import feature-new-schema -i /tmp/dump.dump

# Start the proxy + Web UI (defaults: proxy 5432, UI/API 8000)
dbranch start
dbranch ui                      # opens http://localhost:8000

# Multi-project: select with -p / --project
dbranch -p other-project status
```

### CLI commands

| Command                | What it does                                                |
|------------------------|-------------------------------------------------------------|
| `start`                | Postgres TCP proxy (`:5432`) + Web UI / JSON API (`:8000`). |
| `init`                 | Register a new project.                                     |
| `init-postgres`        | Create the project's `main` Postgres container.             |
| `create <branch>`      | New CoW branch off `main`.                                  |
| `use <branch>`         | Set active branch (proxy routes here).                      |
| `list`                 | List registered projects.                                   |
| `show <branch>`        | Detail a branch (port, size, container state, URL).         |
| `delete <branch>`      | Drop a branch's container + data (refuses main / active).   |
| `delete-project <p>`   | Drop the whole project and unregister it.                   |
| `status`               | Project overview (branches, sizes, containers).             |
| `stop` / `resume`      | Stop/resume all containers in the project.                  |
| `dump <branch>`        | `pg_dump` to a host file (default format: custom).          |
| `import <branch>`      | `pg_restore` a host file into a branch.                     |
| `psql <branch>`        | Open `psql` against the branch (via `docker exec -it`).     |
| `url <branch>`         | Print the connection URL.                                   |
| `ui`                   | Open the Web UI in the default browser.                     |

### Web UI

Run `dbranch start`, then open `http://localhost:8000`. The UI exposes the same
operations as the CLI: project list, branch creation, active switch, dump
download, import upload, stop/resume, delete.

All endpoints under `/api/`:

```
GET    /api/status                                    overview of every project
GET    /api/defaults                                  suggested mount_point + pg creds
GET    /api/projects                  POST /api/projects
GET    /api/projects/:p               PATCH /api/projects/:p   DELETE /api/projects/:p
GET    /api/projects/:p/branches                      POST /api/projects/:p/branches
GET    /api/projects/:p/branches/:b   DELETE /api/projects/:p/branches/:b
POST   /api/projects/:p/branches/:b/start             (idempotent — docker start if exists)
POST   /api/projects/:p/branches/:b/stop
POST   /api/projects/:p/active
GET    /api/projects/:p/branches/:b/dump?format=custom
POST   /api/projects/:p/branches/:b/import            (multipart `file`)
POST   /api/projects/:p/stop                          POST /api/projects/:p/resume
```

`POST /api/projects` accepts `{name, mount_point?, postgres_user?, postgres_password?, postgres_database?}`.
`PATCH /api/projects/:p` accepts the same shape (sans `name`) and persists changes to the project's config file. Use it to fix a broken `mount_point`, rotate credentials, or change the default database without dropping the project.

## Configuration

dBranch stores its state under `~/.config/dbranch/`:

```
~/.config/dbranch/
  registry.json          # which projects exist + which is default
  projects/
    my_app.json          # full Config for `my_app`
    other.json
```

The first time dBranch runs in a directory that has a legacy
`dbranch.config.json`, the project is migrated into the registry automatically.

### Environment variables

All env vars are prefixed with `DBRANCH_`.

| Variable            | Default                       | Purpose                                                                                                |
|---------------------|-------------------------------|--------------------------------------------------------------------------------------------------------|
| `DBRANCH_HOME`      | `~/.config/dbranch`           | Override the dBranch home directory (where the registry + projects live).                              |
| `DBRANCH_PROJECT`   | (registry default)            | Default project for CLI commands when `--project` isn't passed.                                        |
| `DBRANCH_CONFIG`    | `./dbranch.config.json`       | Legacy single-config path. Only used by the migration shim and a small back-compat code path.          |
| `DBRANCH_LOG`       | `info`                        | Log filter (same syntax as `RUST_LOG`, e.g. `dbranch=debug,info`). Falls back to `RUST_LOG` if unset.  |

Example:

```bash
DBRANCH_HOME=/tmp/dbranch-sandbox DBRANCH_LOG=debug dbranch -p sample list
```

## Testing

Unit and integration tests run on any platform:

```bash
cargo test
```

End-to-end tests that require a real Docker daemon and a Linux filesystem with
reflink support are gated behind `#[ignore]`:

```bash
# Linux only — needs Docker running
cargo test -- --ignored
```

The mount point used by the e2e suite can be overridden with
`DBRANCH_TEST_MOUNT` (default: `/tmp/dbranch-test`).

## TODO
- [X] Replace BTRFS module with direct syscall implementation
- [X] Add support for additional filesystems with CoW support (e.g., ZFS)
- [X] Add tests
- [ ] MacOS support
- [ ] Windows support
- [ ] Improve Postgres configuration to share more files between branches (e.g Disable auto vacuum and wall recycling)
- [ ] Improve error handling and messages
- [ ] Sync with remote postgres (optional)
- [ ] Web interface to manage branches
