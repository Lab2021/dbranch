# 🌿 dBranch — PostgreSQL Branching System

> **Early development** — works but expect rough edges.  
> **Don't use in production.**

dBranch makes it trivial to run multiple Postgres databases locally with
**instant branching** (copy-on-write snapshots), each branch in its own
container on its own port, plus a **transparent TCP proxy** that routes
`localhost:5432` to whichever branch is currently active.

Two ways to drive it: a **CLI** for scripts/automation and a **Web UI** for
day-to-day exploration. Both expose the same operations.

---

## What it gives you

- **Multi-project** — multiple Postgres projects side-by-side, each on its own port range. Switch the proxy between them without touching connection strings.
- **CoW branching** — `dbranch create feature` snapshots `main`'s data via `copy_file_range(2)` on Linux / `clonefile(2)` on macOS. Storage is shared until divergence.
- **Schema introspection + diff** — see tables, columns, FKs, indexes for any branch; compare two branches side-by-side with an interactive ER diagram (drag/zoom/auto-layout).
- **Ad-hoc SQL terminal** — run queries in the browser with table-name autocomplete; results tabulated, errors surfaced clearly.
- **Live resource usage** — CPU / memory / network / disk I/O per branch, polled from `docker stats`.
- **Logs** — dBranch's own server logs AND per-container Postgres logs in the UI.
- **Dump / import** — `pg_dump`/`pg_restore` via the CLI or streamed through the Web UI (auto-detects plain vs custom format).

---

## Prerequisites

- **Docker** (or Docker Desktop) running.
- **Rust 1.80+** if you're building from source.
- A reflink-capable filesystem for true CoW efficiency: APFS (macOS),
  BTRFS / XFS / ext4-with-CoW (Linux). Without that, branches still work
  but data is copied byte-for-byte.

## Install / build

```bash
git clone https://github.com/Lab2021/dbranch.git
cd dbranch
cargo build --release
# Binary at ./target/release/dbranch
```

---

## Quick start — Web UI

```bash
dbranch start
# Open http://localhost:8000   (or run `dbranch ui`)
```

`dbranch start` boots two listeners:
- **Postgres proxy** on `:5432` — routes to the active branch.
- **Web UI + JSON API** on `:8000`.

On a fresh install the registry is empty — the UI shows a "create your
first project" prompt. Click **+ New Project**, give it a name and a data
directory (default: `$HOME/dbranch`), and you're done.

From there:
- **Projects list** — each project is a card with the proxy connection URL
  (with copy / reveal password), proxy + API ports, mini CPU/memory bars
  for `main`, and Start/Stop-all buttons.
- **Project page** — branches table, "+ New Branch", per-project Resources
  panel.
- **Branch page** — overview + tile grid of tools:
  - **Schema** — tables / columns / FKs / indexes, with "Compare with…"
    dropdown for a side-by-side diff. Diagram view (Mermaid-style ER) has
    Fit / Zoom / Rearrange buttons, drag tables freely, edges re-route
    automatically. Toggle "Show diff" to focus on just the current branch.
  - **Query** — small SQL terminal: textarea + ⌘↵ to run, table-name
    autocomplete, results in a sortable-ish table. Errors include
    Postgres' `LINE N` context.
  - **Logs** — live container logs, auto-refresh.
  - **Dump / Import** — direct download / upload, streamed (handles GB-sized
    dumps).
- **Server logs** — link in the header. dBranch's own tracing output,
  buffered in memory.

Everything routes via hash URLs (`#/projects/foo/branches/main/query`),
so the browser back button works and you can share or bookmark links.

---

## Quick start — CLI

```bash
# 1. Register a project (becomes the registry default automatically)
dbranch init -n my_app

# 2. Bring up its main Postgres container
dbranch init-postgres

# 3. Branch off main
dbranch create feature-x

# 4. Switch the proxy at :5432 to feature-x
dbranch use feature-x

# 5. Open psql (interactive)
dbranch psql feature-x

# 6. One-shot query
dbranch query feature-x "SELECT count(*) FROM users"

# 7. Get the connection string
dbranch url feature-x
# postgresql://dbranch_user:dbranch_password@127.0.0.1:7001/dbranch

# 8. Live resource usage
dbranch resources

# 9. Dump / restore
dbranch dump  feature-x -o /tmp/snapshot.dump
dbranch import feature-x -i /tmp/snapshot.dump

# 10. Inspect schema
dbranch schema feature-x                       # tables/columns/FKs/indexes
dbranch schema feature-x --diff-against main   # what changed vs main
```

### Multiple projects

```bash
dbranch -p other_project status
dbranch -p other_project create staging
```

Use `--project` (or `DBRANCH_PROJECT=...`) to address a project other than
the registry default. All containers can run simultaneously — each branch
gets its own host port from the project's range. Only one project owns the
`:5432` proxy slot per `dbranch start` process.

### CLI reference

| Command                          | Purpose                                                                |
|----------------------------------|------------------------------------------------------------------------|
| `start`                          | Boot the proxy (`:5432`) + Web UI / API (`:8000`).                     |
| `ui`                             | Open the Web UI in the default browser (falls back to printing the URL).|
| `init -n <name>`                 | Register a new project; sets it as the registry default.               |
| `init-postgres`                  | Spawn the project's `main` Postgres container.                         |
| `create <branch> [-s <source>]`  | CoW branch off `<source>` (defaults to `main`).                        |
| `use <branch>`                   | Make `<branch>` the active one — proxy routes here.                    |
| `list`                           | List all registered projects with branch / running counts.             |
| `status`                         | Detailed table of the current project's branches.                      |
| `show <branch>`                  | Single-branch detail (port, size, container state, URL).               |
| `delete <branch>`                | Drop a branch's container + data (refuses `main` and the active one).  |
| `delete-project <name>`          | Drop the whole project (containers + data + registry entry).           |
| `stop` / `resume`                | Stop / resume every container in the project (idempotent).             |
| `dump <branch> [-o file] [-f fmt]` | `pg_dump` to a host file. Formats: `custom` (default), `plain`, `tar`. |
| `import <branch> -i file [--mode reset\|merge] [--allow-main]` | `pg_restore` / `psql` (auto-detects format). |
| `psql <branch>`                  | Drop into an interactive `psql` shell against the branch.              |
| `url <branch>`                   | Print the postgresql:// connection URL.                                |
| `query <branch> "<sql>"` / `-f file` | Run one SQL statement (10s timeout, results capped at 1000 rows).  |
| `schema <branch> [--diff-against <other>]` | Print schema or a diff between two branches.                |
| `logs [<branch>] [--tail N] [--server]` | Tail a branch's container logs, or dBranch's own (`--server`). |
| `resources`                      | Live CPU / memory / network / disk I/O per running branch.             |

All commands accept the global `-p/--project` flag.

---

## Web API

The same surface the UI uses. All endpoints under `/api/`, JSON in/out:

```
GET    /api/status                              # overview of every project
GET    /api/defaults                            # suggested mount_point + pg creds
GET    /api/logs                                # dBranch server logs (ring buffer)

GET    /api/projects                            POST   /api/projects
GET    /api/projects/:p                         PATCH  /api/projects/:p    DELETE /api/projects/:p
GET    /api/projects/:p/branches                POST   /api/projects/:p/branches
GET    /api/projects/:p/branches/:b             DELETE /api/projects/:p/branches/:b
POST   /api/projects/:p/branches/:b/start       (idempotent — docker start if exists)
POST   /api/projects/:p/branches/:b/stop
POST   /api/projects/:p/active                  # body: {branch: "..."}
GET    /api/projects/:p/branches/:b/schema
GET    /api/projects/:p/branches/:b/schema/diff?against=<other>
POST   /api/projects/:p/branches/:b/query       # body: {sql: "..."}
GET    /api/projects/:p/branches/:b/logs?tail=N
GET    /api/projects/:p/branches/:b/dump?format=custom    # streams pg_dump output
POST   /api/projects/:p/branches/:b/import      # multipart `file`
POST   /api/projects/:p/stop                    POST   /api/projects/:p/resume
GET    /api/projects/:p/resources               # per-running-branch docker stats
```

`POST /api/projects` body: `{name, mount_point?, postgres_user?, postgres_password?, postgres_database?}`.  
`PATCH /api/projects/:p` accepts the same shape (sans `name`) and persists to the project's config file — use it to fix a bad `mount_point` or rotate credentials.

---

## Configuration

State lives under `~/.config/dbranch/`:

```
~/.config/dbranch/
  registry.json              # {default: "...", projects: [...]}
  projects/
    my_app.json              # full Config for my_app
    other.json
```

The first time dBranch runs in a directory holding a legacy
`dbranch.config.json`, that file is migrated into the registry
automatically and replaced by a small `{project: "..."}` pointer.

### Environment variables

| Variable           | Default                  | Purpose                                                                                                |
|--------------------|--------------------------|--------------------------------------------------------------------------------------------------------|
| `DBRANCH_HOME`     | `~/.config/dbranch`      | Override the dBranch home directory.                                                                   |
| `DBRANCH_PROJECT`  | (registry default)       | Project to address when `--project` isn't passed.                                                      |
| `DBRANCH_DATA`     | `$HOME/dbranch`          | Default mount-point suggested for new projects (point at a CoW volume for real reflinks).              |
| `DBRANCH_CONFIG`   | `./dbranch.config.json`  | Legacy single-config path. Read by the migration shim.                                                 |
| `DBRANCH_LOG`      | `info`                   | Log filter (same syntax as `RUST_LOG`, e.g. `dbranch=debug,info`). Falls back to `RUST_LOG` if unset.  |

```bash
DBRANCH_HOME=/tmp/dbranch-sandbox DBRANCH_LOG=debug dbranch start
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  CLI                Web UI (vanilla JS SPA, hash-routed)    │
│   │                  │                                       │
│   ▼                  ▼                                       │
│  CliHandler         axum router  ─►  /api/* JSON endpoints   │
│   └────────┬────────┘                                        │
│            ▼                                                 │
│  ┌─────────────────────────────────────────────┐             │
│  │ Domain modules                              │             │
│  │   config       — Project + Registry         │             │
│  │   snapshot     — copy_file_range / clonefile│             │
│  │   database_op  — docker run / start / stop  │             │
│  │   dump         — pg_dump / pg_restore       │             │
│  │   schema       — psql introspection         │             │
│  │   schema_diff  — pure diff function         │             │
│  │   query        — safe psql -c executor      │             │
│  │   docker_stats — CPU/mem/net/blk parser     │             │
│  │   logbuf       — tracing → ring buffer      │             │
│  └─────────────────────────────────────────────┘             │
│            │                                                 │
│            ▼                                                 │
│  Docker (one container per branch)  +  Postgres data dirs    │
│  (CoW-shared on BTRFS / XFS / APFS)                          │
│                                                              │
│  TCP proxy on :5432  ─►  active branch's published port      │
└─────────────────────────────────────────────────────────────┘
```

---

## Testing

```bash
# Unit + integration tests, no Docker required
cargo test

# End-to-end smoke (needs Docker daemon reachable + Linux/macOS host)
cargo test -- --ignored
```

A `DBRANCH_TEST_MOUNT` env var overrides the e2e suite's mount point
(default `/tmp/dbranch-test`).

---

## TODO

- [X] Replace BTRFS module with direct syscall implementation
- [X] macOS support (via `clonefile(2)`)
- [X] Web interface
- [X] CoW filesystems beyond BTRFS (XFS, ext4-CoW, APFS)
- [X] Tests (unit + integration + e2e)
- [X] Schema view + branch diff + ER diagram
- [X] Ad-hoc SQL terminal
- [X] Multi-project
- [ ] Windows support
- [ ] Sync with remote postgres (optional)
- [ ] Sharper postgres tuning for branches (autovacuum / WAL recycling)
- [ ] Authentication / multi-user (currently single-user, localhost-only)
