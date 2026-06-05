// dBranch Web UI — vanilla SPA, no build step, no framework.
//
// Layout:
//   - "Primitives" block: DOM/format helpers, fetch wrapper, toast stack.
//   - "Router" module: hash routing, per-page lifecycle (mount/poll/onLeave).
//   - "Pages": one object per route. Each page declares
//        { path, regex, params, mount(ctx), poll(state), onLeave() }
//
// The polling loop fires every 2s and delegates to the current page's
// `poll()` so volatile cells (sizes, container status, resources) update
// without rebuilding the page DOM and losing focus/scroll.

// ─────────────────────────────────────────────────────────────────────────────
// Primitives
// ─────────────────────────────────────────────────────────────────────────────

const $ = (sel, root = document) => root.querySelector(sel);

const fmt = {
  bytes(n) {
    if (n == null) return "-";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let i = 0, v = n;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return `${v.toFixed(i ? 1 : 0)} ${units[i]}`;
  },
};

async function api(method, path, body, opts = {}) {
  const init = { method, headers: {}, signal: opts.signal };
  if (body !== undefined) {
    if (body instanceof FormData) {
      init.body = body;
    } else {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
  }
  const res = await fetch(`/api${path}`, init);
  if (!res.ok) {
    let msg = `${res.status}`;
    try {
      const data = await res.json();
      msg = data.error || msg;
    } catch (_) {
      msg = await res.text();
    }
    throw new Error(msg);
  }
  if (res.status === 204) return null;
  const ct = res.headers.get("content-type") || "";
  if (ct.includes("application/json")) return res.json();
  return res;
}

// Toasts (bottom-right, stacked, auto-dismiss).
const TOAST_TIMEOUTS = { error: 7000, info: 3500, success: 3000 };
const TOAST_ICONS = { error: "✕", info: "ℹ", success: "✓" };

function toast(msg, variant = "info") {
  const root = $("#toasts");
  if (!root) { console.log(`[${variant}]`, msg); return; }
  const el = document.createElement("div");
  el.className = `toast ${variant}`;
  el.innerHTML = `
    <span class="toast-icon">${TOAST_ICONS[variant] ?? "•"}</span>
    <span class="toast-msg"></span>
    <button class="toast-close" aria-label="close">×</button>
  `;
  el.querySelector(".toast-msg").textContent = msg;
  root.appendChild(el);
  let dismissed = false;
  const dismiss = () => {
    if (dismissed) return;
    dismissed = true;
    el.classList.add("fading");
    setTimeout(() => el.remove(), 250);
  };
  el.querySelector(".toast-close").onclick = dismiss;
  setTimeout(dismiss, TOAST_TIMEOUTS[variant] ?? 3500);
}
const showError = (m) => toast(m, "error");
const showInfo = (m) => toast(m, "info");
const showSuccess = (m) => toast(m, "success");

// Tiny string helpers.
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "\"": "&quot;", "'": "&#39;" }[c])
  );
}
function truncate(s, n) { return s.length > n ? s.slice(0, n - 1) + "…" : s; }
function cssEscape(s) { return String(s).replace(/[^a-zA-Z0-9_-]/g, (c) => `\\${c}`); }

// Robust copy that works on http://localhost too (where the clipboard API
// often refuses outside a user gesture / secure context).
function copyToClipboard(text) {
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(
      () => showSuccess("Copied"),
      () => execCopyFallback(text)
    );
    return;
  }
  execCopyFallback(text);
}
function execCopyFallback(text) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.top = "0";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch (_) { ok = false; }
  document.body.removeChild(ta);
  if (ok) showSuccess("Copied");
  else window.prompt("Copy this:", text);
}

// ─────────────────────────────────────────────────────────────────────────────
// Router
// ─────────────────────────────────────────────────────────────────────────────

const Router = {
  routes: [],
  current: null,        // { page, params, ctx }
  lastSig: null,

  /// `path` uses :param syntax. Returns the route entry to be `register`ed.
  define(path, page) {
    const paramNames = [];
    const regexSrc = path
      .replace(/[.+?()|[\]{}\\^$]/g, "\\$&")
      .replace(/:([A-Za-z_]\w*)/g, (_, name) => {
        paramNames.push(name);
        return "([^/?]+)";
      });
    const regex = new RegExp(`^${regexSrc}$`);
    this.routes.push({ path, regex, paramNames, page });
    return this;
  },

  parse(hash) {
    const raw = (hash || "").replace(/^#/, "") || "/";
    const [pathPart, queryPart = ""] = raw.split("?");
    for (const r of this.routes) {
      const m = pathPart.match(r.regex);
      if (m) {
        const params = {};
        r.paramNames.forEach((n, i) => (params[n] = decodeURIComponent(m[i + 1])));
        const query = new URLSearchParams(queryPart);
        return { page: r.page, params, query, path: pathPart };
      }
    }
    return null;
  },

  async mount(hash) {
    const matched = this.parse(hash);
    if (!matched) {
      // Unknown hash → home.
      location.hash = "#/";
      return;
    }

    // Tear down the previous page.
    if (this.current) {
      try { this.current.ctx?.abort?.(); } catch (_) {}
      try { await this.current.page.onLeave?.(this.current.ctx); } catch (e) { console.error(e); }
    }
    this.lastSig = null;

    const controller = new AbortController();
    const ctx = {
      params: matched.params,
      query: matched.query,
      signal: controller.signal,
      abort: () => controller.abort(),
    };

    this.current = { page: matched.page, params: matched.params, ctx };
    renderChrome(matched);

    const root = $("#app");
    root.innerHTML = "";
    try {
      await matched.page.mount(ctx, root);
    } catch (e) {
      if (e.name === "AbortError") return; // navigation cancelled
      console.error("page mount failed", e);
      root.innerHTML = `<div class="page-error">Failed to render page: ${escapeHtml(e.message)}</div>`;
    }
  },

  async poll() {
    if (!this.current) return;
    const { page, ctx } = this.current;
    if (!page.poll) return;
    if (ctx.signal.aborted) return;
    try {
      const sig = await page.poll(ctx);
      // Pages that want to re-render on structural change return a signature
      // string from poll(); when the value changes between polls, they call
      // their own re-render logic. The router doesn't manage that — it just
      // gives them the 2s tick.
      this.lastSig = sig;
    } catch (e) {
      if (e.name !== "AbortError") console.error("poll error", e);
    }
  },
};

function renderChrome(matched) {
  const crumbs = buildBreadcrumbs(matched);
  $("#breadcrumbs").innerHTML = crumbs
    .map((c, i) => {
      const last = i === crumbs.length - 1;
      return last
        ? `<span class="crumb current">${escapeHtml(c.label)}</span>`
        : `<a class="crumb" href="${escapeHtml(c.href)}">${escapeHtml(c.label)}</a>`;
    })
    .join('<span class="crumb-sep">/</span>');

  const actions = matched.page.headerActions?.(matched) || "";
  $("#header-actions").innerHTML = actions;
}

function buildBreadcrumbs(matched) {
  const out = [{ label: "🌿 dBranch", href: "#/" }];
  const p = matched.params;
  if (p.project) out.push({ label: p.project, href: `#/projects/${encodeURIComponent(p.project)}` });
  if (p.branch) {
    const base = `#/projects/${encodeURIComponent(p.project)}/branches/${encodeURIComponent(p.branch)}`;
    out.push({ label: p.branch, href: base });
  }
  // Trailing segment label
  const trail = matched.page.crumb?.(matched);
  if (trail) out.push({ label: trail, href: matched.path });
  return out;
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared rendering helpers (used by multiple pages)
// ─────────────────────────────────────────────────────────────────────────────

function maskPassword(url) {
  // postgresql://user:secret@host:port/db → postgresql://user:****@host:port/db
  return String(url).replace(/^(postgresql:\/\/[^:]+:)[^@]+(@)/, "$1****$2");
}

function renderResourceRow(r) {
  const cpuPct = Math.min(r.cpu_pct, 100);
  const memPct = Math.min(r.mem_pct, 100);
  return `
    <tr data-rb="${escapeHtml(r.branch)}">
      <td><code>${escapeHtml(r.branch)}</code></td>
      <td>
        <div class="bar" title="${r.cpu_pct.toFixed(2)}%"><div class="bar-fill" data-cpu style="width:${cpuPct}%"></div></div>
        <span class="bar-text" data-cpu-text>${r.cpu_pct.toFixed(1)}%</span>
      </td>
      <td>
        <div class="bar" title="${r.mem_pct.toFixed(2)}%"><div class="bar-fill" data-mem style="width:${memPct}%"></div></div>
        <span class="bar-text" data-mem-text>${fmt.bytes(r.mem_used_bytes)} / ${fmt.bytes(r.mem_limit_bytes)} (${r.mem_pct.toFixed(1)}%)</span>
      </td>
      <td data-net>${fmt.bytes(r.net_rx_bytes)} ↓ / ${fmt.bytes(r.net_tx_bytes)} ↑</td>
      <td data-block>${fmt.bytes(r.block_read_bytes)} r / ${fmt.bytes(r.block_write_bytes)} w</td>
      <td data-pids>${r.pids}</td>
    </tr>`;
}

function updateResourceRowCells(tr, r) {
  const cpu = Math.min(r.cpu_pct, 100);
  const mem = Math.min(r.mem_pct, 100);
  tr.querySelector("[data-cpu]").style.width = `${cpu}%`;
  tr.querySelector("[data-cpu-text]").textContent = `${r.cpu_pct.toFixed(1)}%`;
  tr.querySelector("[data-mem]").style.width = `${mem}%`;
  tr.querySelector("[data-mem-text]").textContent =
    `${fmt.bytes(r.mem_used_bytes)} / ${fmt.bytes(r.mem_limit_bytes)} (${r.mem_pct.toFixed(1)}%)`;
  tr.querySelector("[data-net]").textContent =
    `${fmt.bytes(r.net_rx_bytes)} ↓ / ${fmt.bytes(r.net_tx_bytes)} ↑`;
  tr.querySelector("[data-block]").textContent =
    `${fmt.bytes(r.block_read_bytes)} r / ${fmt.bytes(r.block_write_bytes)} w`;
  tr.querySelector("[data-pids]").textContent = String(r.pids);
}

// ─────────────────────────────────────────────────────────────────────────────
// Page: ProjectsList  (#/)
// ─────────────────────────────────────────────────────────────────────────────

const ProjectsListPage = {
  headerActions: () => `
    <a class="btn" href="#/logs">Server logs</a>
    <a class="btn primary" href="#/projects/new">+ New Project</a>
  `,
  async mount(ctx, root) {
    root.innerHTML = `<div id="projects-grid" class="projects-grid"><div class="placeholder">Loading…</div></div>`;
    this.state = { revealed: new Set(), resources: {} };
    await this.refresh(ctx);
  },
  async refresh(ctx) {
    try {
      const data = await api("GET", "/status", undefined, { signal: ctx.signal });
      this.state.data = data;
      this.render();
      // Fan-out resources for each project. Cap at 4 in-flight.
      this.fetchResourcesAll(ctx).catch(() => {});
    } catch (e) {
      if (e.name === "AbortError") return;
      showError(e.message);
    }
  },
  render() {
    const data = this.state.data;
    if (!data) return;
    const grid = $("#projects-grid");
    if (data.projects.length === 0) {
      grid.innerHTML = `<div class="placeholder">No projects yet. <a href="#/projects/new">Create one.</a></div>`;
      return;
    }
    grid.innerHTML = data.projects.map((p) => this.renderCard(p)).join("");
    // Wire actions
    for (const card of grid.querySelectorAll(".project-card")) {
      const name = card.dataset.project;
      card.querySelector(".reveal-btn")?.addEventListener("click", (e) => {
        e.preventDefault();
        if (this.state.revealed.has(name)) this.state.revealed.delete(name);
        else this.state.revealed.add(name);
        this.render();
      });
      card.querySelector(".copy-btn")?.addEventListener("click", (e) => {
        e.preventDefault();
        copyToClipboard(e.currentTarget.dataset.url);
      });
      card.querySelector(".btn-start")?.addEventListener("click", async () => {
        try { await api("POST", `/projects/${encodeURIComponent(name)}/resume`); showSuccess(`Starting ${name}…`); }
        catch (e) { showError(e.message); }
      });
      card.querySelector(".btn-stop")?.addEventListener("click", async () => {
        try { await api("POST", `/projects/${encodeURIComponent(name)}/stop`); showSuccess(`Stopping ${name}…`); }
        catch (e) { showError(e.message); }
      });
    }
  },
  renderCard(p) {
    const revealed = this.state.revealed.has(p.name);
    const url = p.proxy_url || "";
    const displayUrl = revealed ? url : maskPassword(url);
    const running = p.branches.filter((b) => b.container_running).length;
    const main = p.branches.find((b) => b.is_main);
    const mainRes = this.state.resources[p.name]?.find((r) => r.branch === "main");
    const mainRunning = main?.container_running ?? false;
    return `
      <div class="project-card" data-project="${escapeHtml(p.name)}">
        <div class="project-card-head">
          <a href="#/projects/${encodeURIComponent(p.name)}" class="project-card-title">${escapeHtml(p.name)}</a>
          ${p.is_default ? '<span class="default-flag">DEFAULT</span>' : ""}
        </div>
        <div class="project-card-mount muted"><code>${escapeHtml(p.mount_point)}</code></div>

        <div class="project-card-conn">
          <code title="${escapeHtml(url)}">${escapeHtml(truncate(displayUrl, 56))}</code>
          <button class="icon-btn reveal-btn" type="button" title="${revealed ? "Hide password" : "Reveal password"}">${revealed ? "🙈" : "👁"}</button>
          <button class="icon-btn copy-btn" type="button" data-url="${escapeHtml(url)}" title="Copy">⧉</button>
        </div>

        <div class="project-card-meta">
          <span class="pill">proxy :${p.proxy_port}</span>
          <span class="pill">api :${p.api_port}</span>
          <span class="muted">${p.branches.length} branches · ${running} running · routes → <strong>${escapeHtml(p.proxy_routes_to || "main")}</strong></span>
        </div>

        ${mainRes
          ? `<div class="project-card-res">
              <span class="res-label">main</span>
              <div class="bar" title="${mainRes.cpu_pct.toFixed(2)}%"><div class="bar-fill" style="width:${Math.min(mainRes.cpu_pct, 100)}%"></div></div>
              <span class="bar-text">CPU ${mainRes.cpu_pct.toFixed(1)}%</span>
              <div class="bar" title="${mainRes.mem_pct.toFixed(2)}%"><div class="bar-fill" style="width:${Math.min(mainRes.mem_pct, 100)}%"></div></div>
              <span class="bar-text">MEM ${fmt.bytes(mainRes.mem_used_bytes)}</span>
            </div>`
          : (mainRunning ? `<div class="project-card-res muted">measuring…</div>` : `<div class="project-card-res muted">main stopped</div>`)
        }

        <div class="project-card-actions">
          ${running === 0
            ? `<button type="button" class="btn btn-start">▶ Start All</button>`
            : `<button type="button" class="btn btn-stop">⏸ Stop All</button>`
          }
          <a class="btn primary" href="#/projects/${encodeURIComponent(p.name)}">Open →</a>
        </div>
      </div>`;
  },
  async fetchResourcesAll(ctx) {
    const projects = this.state.data?.projects || [];
    // Concurrency cap of 4 — keeps the front from hammering docker stats
    // when there are many projects.
    const limit = 4;
    let i = 0;
    const workers = Array.from({ length: limit }, async () => {
      while (i < projects.length) {
        const idx = i++;
        const p = projects[idx];
        try {
          const rows = await api("GET", `/projects/${encodeURIComponent(p.name)}/resources`, undefined, { signal: ctx.signal });
          this.state.resources[p.name] = rows;
        } catch (_) {
          this.state.resources[p.name] = [];
        }
      }
    });
    await Promise.all(workers);
    this.render();
  },
  async poll(ctx) {
    await this.refresh(ctx);
  },
  onLeave() { this.state = null; },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: NewProject  (#/projects/new)
// ─────────────────────────────────────────────────────────────────────────────

const NewProjectPage = {
  crumb: () => "New project",
  async mount(ctx, root) {
    const defaults = await api("GET", "/defaults", undefined, { signal: ctx.signal }).catch(() => ({}));
    root.innerHTML = `
      <form class="form" id="new-project-form">
        <h2>Create new project</h2>
        <div class="field">
          <label>Project name</label>
          <input name="name" required placeholder="my_app" autofocus />
        </div>
        <div class="field">
          <label>Data directory</label>
          <input name="mount_point" value="${escapeHtml(defaults.mount_point || "")}" />
          <div class="hint">Where branch data lives on disk. Point at a BTRFS / XFS / APFS volume for instant branches with shared extents.</div>
        </div>
        <div class="field-row">
          <div class="field">
            <label>Postgres user</label>
            <input name="postgres_user" value="${escapeHtml(defaults.postgres_user || "")}" />
          </div>
          <div class="field">
            <label>Postgres password</label>
            <input name="postgres_password" value="${escapeHtml(defaults.postgres_password || "")}" />
          </div>
        </div>
        <div class="form-actions">
          <a class="btn" href="#/">Cancel</a>
          <button type="submit" class="btn primary">Create</button>
        </div>
      </form>`;
    $("#new-project-form").onsubmit = async (e) => {
      e.preventDefault();
      const f = new FormData(e.target);
      const body = {
        name: f.get("name"),
        mount_point: f.get("mount_point") || null,
        postgres_user: f.get("postgres_user") || null,
        postgres_password: f.get("postgres_password") || null,
      };
      try {
        const res = await api("POST", "/projects", body, { signal: ctx.signal });
        if (res.main_start_error) showError(`Project created, but main failed to start: ${res.main_start_error}`);
        else showSuccess(`Project '${body.name}' created`);
        location.hash = `#/projects/${encodeURIComponent(body.name)}`;
      } catch (err) { showError(err.message); }
    };
  },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: ProjectDetail  (#/projects/:project)
// ─────────────────────────────────────────────────────────────────────────────

const ProjectDetailPage = {
  headerActions: ({ params }) => `
    <a class="btn" href="#/projects/${encodeURIComponent(params.project)}/branches/new">+ New Branch</a>
    <a class="btn" href="#/projects/${encodeURIComponent(params.project)}/settings">⚙ Settings</a>
  `,
  async mount(ctx, root) {
    this.params = ctx.params;
    root.innerHTML = `<div class="placeholder">Loading…</div>`;
    await this.refresh(ctx);
  },
  async refresh(ctx) {
    try {
      const project = await api("GET", `/projects/${encodeURIComponent(this.params.project)}`, undefined, { signal: ctx.signal });
      this.lastShape = this.shape(project);
      this.project = project;
      this.render();
      api("GET", `/projects/${encodeURIComponent(this.params.project)}/resources`, undefined, { signal: ctx.signal })
        .then((rows) => { this.resources = rows; this.renderResources(); })
        .catch(() => {});
    } catch (e) {
      if (e.name === "AbortError") return;
      showError(e.message);
    }
  },
  shape(project) {
    const parts = [
      project.name, project.mount_point, project.active_branch ?? "", project.proxy_routes_to ?? "",
      ...project.branches.map((b) => `${b.name}:${b.port}:${b.is_main ? "m" : ""}:${b.container_running ? "r" : "s"}`),
    ];
    return parts.join("|");
  },
  render() {
    const project = this.project;
    const $root = $("#app");
    const active = project.active_branch ?? "main";
    const mainBranch = project.branches.find((b) => b.is_main);
    const mainRunning = mainBranch?.container_running ?? false;

    $root.innerHTML = `
      <section class="project-page">
        <div class="project-page-head">
          <div>
            <div class="project-page-meta">
              <code class="mount-point">${escapeHtml(project.mount_point)}</code>
              · <span class="pill">proxy :${project.proxy_port}</span>
              <span class="pill">api :${project.api_port}</span>
              · proxy routes to <strong>${escapeHtml(active)}</strong>
            </div>
          </div>
          <div class="project-page-cta">
            ${!mainRunning ? `<button class="btn primary" id="start-main-btn">▶ Start Main</button>` : ""}
            <button class="btn" id="resume-all-btn">▶ Resume All</button>
            <button class="btn" id="stop-all-btn">⏸ Stop All</button>
          </div>
        </div>

        <table class="branches-table">
          <thead><tr>
            <th>Branch</th><th>Port</th><th>Size</th><th>Unique</th>
            <th>Status</th><th>Connection</th><th>Actions</th>
          </tr></thead>
          <tbody id="branches-tbody"></tbody>
        </table>

        <h3 class="resources-title">Resources</h3>
        <table class="resources-table">
          <thead><tr><th>Branch</th><th>CPU</th><th>Memory</th><th>Net rx/tx</th><th>Block r/w</th><th>PIDs</th></tr></thead>
          <tbody id="resources-tbody"><tr><td colspan="6" class="placeholder-row">…</td></tr></tbody>
        </table>
      </section>`;

    this.renderBranchRows();
    this.wireHeaderActions(project, mainRunning);
  },
  renderBranchRows() {
    const project = this.project;
    const active = project.active_branch ?? "main";
    const tbody = $("#branches-tbody");
    const tpl = $("#branch-row-tpl");
    tbody.innerHTML = "";
    const sorted = [...project.branches].sort((a, b) => (a.is_main ? -1 : b.is_main ? 1 : a.created_at.localeCompare(b.created_at)));
    for (const b of sorted) {
      const node = tpl.content.cloneNode(true);
      const tr = node.querySelector("tr");
      tr.dataset.branch = b.name;
      const isActive = b.name === active;
      tr.querySelector(".branch-name").innerHTML =
        `<a href="#/projects/${encodeURIComponent(project.name)}/branches/${encodeURIComponent(b.name)}">${escapeHtml(b.name)}</a>` +
        `${isActive ? '<span class="active-branch-pill">ACTIVE</span>' : ""}` +
        `${b.is_main ? '<span class="main-pill">MAIN</span>' : ""}`;
      tr.querySelector(".branch-port").textContent = b.port;
      tr.querySelector(".branch-size").textContent = fmt.bytes(b.logical_size);
      tr.querySelector(".branch-unique").textContent = fmt.bytes(b.unique_size);
      const statusCell = tr.querySelector(".branch-status");
      statusCell.textContent = b.container_running ? "● running" : "● stopped";
      statusCell.className = "branch-status " + (b.container_running ? "status-running" : "status-stopped");
      const urlCell = tr.querySelector(".branch-url");
      const maskedUrl = maskPassword(b.connection_url);
      urlCell.innerHTML = `<code title="${escapeHtml(b.connection_url)}">${escapeHtml(truncate(maskedUrl, 32))}</code>
                          <button type="button" class="copy-btn icon-btn" data-url="${escapeHtml(b.connection_url)}" title="Copy">⧉</button>`;
      urlCell.querySelector(".copy-btn").onclick = (e) => copyToClipboard(e.currentTarget.dataset.url);

      const actions = tr.querySelector(".branch-actions");
      const tools = `#/projects/${encodeURIComponent(project.name)}/branches/${encodeURIComponent(b.name)}`;
      actions.innerHTML = `
        ${isActive ? "" : `<button class="btn xs" data-use>Use</button>`}
        <a class="btn xs" href="${tools}">Open</a>`;
      actions.querySelector("[data-use]")?.addEventListener("click", async () => {
        try { await api("POST", `/projects/${encodeURIComponent(project.name)}/active`, { branch: b.name }); }
        catch (e) { showError(e.message); }
      });
      tbody.appendChild(node);
    }
  },
  renderResources() {
    const tbody = $("#resources-tbody");
    if (!tbody) return;
    const rows = this.resources || [];
    if (rows.length === 0) {
      tbody.innerHTML = `<tr><td colspan="6" class="placeholder-row">No running branches</td></tr>`;
      tbody.dataset.shape = "empty";
      return;
    }
    const shape = rows.map((r) => r.branch).join(",");
    if (tbody.dataset.shape !== shape) {
      tbody.innerHTML = rows.map(renderResourceRow).join("");
      tbody.dataset.shape = shape;
      return;
    }
    for (const r of rows) {
      const tr = tbody.querySelector(`tr[data-rb="${cssEscape(r.branch)}"]`);
      if (tr) updateResourceRowCells(tr, r);
    }
  },
  wireHeaderActions(project, mainRunning) {
    if (!mainRunning) {
      $("#start-main-btn")?.addEventListener("click", async () => {
        try { await api("POST", `/projects/${encodeURIComponent(project.name)}/branches/main/start`); }
        catch (e) { showError(e.message); }
      });
    }
    $("#resume-all-btn").onclick = async () => {
      try { await api("POST", `/projects/${encodeURIComponent(project.name)}/resume`); showInfo("Resuming…"); }
      catch (e) { showError(e.message); }
    };
    $("#stop-all-btn").onclick = async () => {
      try { await api("POST", `/projects/${encodeURIComponent(project.name)}/stop`); showInfo("Stopping…"); }
      catch (e) { showError(e.message); }
    };
  },
  async poll(ctx) {
    try {
      const project = await api("GET", `/projects/${encodeURIComponent(this.params.project)}`, undefined, { signal: ctx.signal });
      const newShape = this.shape(project);
      this.project = project;
      if (newShape !== this.lastShape) {
        this.lastShape = newShape;
        this.render();
      } else {
        // Cells-only update: branch sizes/status.
        this.updateBranchCellsLive();
      }
      api("GET", `/projects/${encodeURIComponent(this.params.project)}/resources`, undefined, { signal: ctx.signal })
        .then((rows) => { this.resources = rows; this.renderResources(); })
        .catch(() => {});
    } catch (e) {
      if (e.name === "AbortError") return;
    }
  },
  updateBranchCellsLive() {
    const tbody = $("#branches-tbody");
    if (!tbody) return;
    for (const b of this.project.branches) {
      const tr = tbody.querySelector(`tr[data-branch="${cssEscape(b.name)}"]`);
      if (!tr) continue;
      tr.querySelector(".branch-size").textContent = fmt.bytes(b.logical_size);
      tr.querySelector(".branch-unique").textContent = fmt.bytes(b.unique_size);
      const statusCell = tr.querySelector(".branch-status");
      statusCell.textContent = b.container_running ? "● running" : "● stopped";
      statusCell.className = "branch-status " + (b.container_running ? "status-running" : "status-stopped");
    }
  },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: ProjectSettings  (#/projects/:project/settings)
// ─────────────────────────────────────────────────────────────────────────────

const ProjectSettingsPage = {
  crumb: () => "Settings",
  async mount(ctx, root) {
    const project = await api("GET", `/projects/${encodeURIComponent(ctx.params.project)}`, undefined, { signal: ctx.signal });
    root.innerHTML = `
      <form class="form" id="settings-form">
        <h2>Settings — ${escapeHtml(project.name)}</h2>
        <div class="field">
          <label>Data directory</label>
          <input name="mount_point" value="${escapeHtml(project.mount_point)}" />
          <div class="hint">Existing branch data is NOT moved automatically — stop branches and copy/re-create them after changing this.</div>
        </div>
        <div class="field-row">
          <div class="field"><label>Postgres user (blank = unchanged)</label><input name="postgres_user" placeholder="(unchanged)" /></div>
          <div class="field"><label>Postgres password (blank = unchanged)</label><input name="postgres_password" placeholder="(unchanged)" /></div>
        </div>
        <div class="field">
          <label>Default database (blank = unchanged)</label>
          <input name="postgres_database" placeholder="(unchanged)" />
        </div>
        <div class="form-actions">
          <a class="btn" href="#/projects/${encodeURIComponent(project.name)}">Cancel</a>
          <button type="submit" class="btn primary">Save</button>
        </div>
      </form>

      <hr class="form-sep" />

      <div class="form danger-zone">
        <h3>Danger zone</h3>
        <p class="muted">Delete the project, all its branches, containers, and data files. Cannot be undone.</p>
        <button type="button" class="btn danger" id="delete-project-btn">🗑 Delete Project</button>
      </div>`;

    $("#settings-form").onsubmit = async (e) => {
      e.preventDefault();
      const f = new FormData(e.target);
      const body = {};
      const mp = f.get("mount_point");
      if (mp && mp !== project.mount_point) body.mount_point = mp;
      const u = f.get("postgres_user"); if (u) body.postgres_user = u;
      const p = f.get("postgres_password"); if (p) body.postgres_password = p;
      const d = f.get("postgres_database"); if (d) body.postgres_database = d;
      if (Object.keys(body).length === 0) { showInfo("No changes."); return; }
      try {
        const res = await api("PATCH", `/projects/${encodeURIComponent(project.name)}`, body, { signal: ctx.signal });
        if (res.warnings?.length) for (const w of res.warnings) toast(w, "info");
        showSuccess("Settings saved.");
        location.hash = `#/projects/${encodeURIComponent(project.name)}`;
      } catch (err) { showError(err.message); }
    };
    $("#delete-project-btn").onclick = async () => {
      if (!confirm(`Delete project '${project.name}' and ALL its branches?`)) return;
      try {
        await api("DELETE", `/projects/${encodeURIComponent(project.name)}`);
        showSuccess(`Project '${project.name}' deleted`);
        location.hash = "#/";
      } catch (e) { showError(e.message); }
    };
  },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: NewBranch  (#/projects/:project/branches/new)
// ─────────────────────────────────────────────────────────────────────────────

const NewBranchPage = {
  crumb: () => "New branch",
  async mount(ctx, root) {
    const project = await api("GET", `/projects/${encodeURIComponent(ctx.params.project)}`, undefined, { signal: ctx.signal });
    const options = project.branches.map((b) => `<option value="${escapeHtml(b.name)}"${b.is_main ? " selected" : ""}>${escapeHtml(b.name)}</option>`).join("");
    root.innerHTML = `
      <form class="form" id="new-branch-form">
        <h2>Create branch in ${escapeHtml(project.name)}</h2>
        <div class="field">
          <label>Branch name</label>
          <input name="name" required placeholder="feature-x" autofocus />
        </div>
        <div class="field">
          <label>Source branch</label>
          <select name="source">${options}</select>
          <div class="hint">Branch to snapshot from. Data is copied with CoW (instant on a reflink-capable filesystem).</div>
        </div>
        <div class="form-actions">
          <a class="btn" href="#/projects/${encodeURIComponent(project.name)}">Cancel</a>
          <button type="submit" class="btn primary">Create</button>
        </div>
      </form>`;
    $("#new-branch-form").onsubmit = async (e) => {
      e.preventDefault();
      const f = new FormData(e.target);
      try {
        await api("POST", `/projects/${encodeURIComponent(project.name)}/branches`, {
          name: f.get("name"),
          source: f.get("source") || null,
        }, { signal: ctx.signal });
        showSuccess(`Branch '${f.get("name")}' created`);
        location.hash = `#/projects/${encodeURIComponent(project.name)}`;
      } catch (err) { showError(err.message); }
    };
  },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: BranchDetail  (#/projects/:project/branches/:branch)
// ─────────────────────────────────────────────────────────────────────────────

const BranchDetailPage = {
  async mount(ctx, root) {
    this.ctx = ctx;
    root.innerHTML = `<div class="placeholder">Loading…</div>`;
    await this.refresh();
  },
  async refresh() {
    const { project, branch } = this.ctx.params;
    try {
      const detail = await api("GET", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}`, undefined, { signal: this.ctx.signal });
      this.detail = detail;
      this.render();
    } catch (e) {
      if (e.name === "AbortError") return;
      showError(e.message);
    }
  },
  render() {
    const d = this.detail;
    const { project, branch } = this.ctx.params;
    const base = `#/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}`;
    const masked = maskPassword(d.connection_url);
    $("#app").innerHTML = `
      <section class="branch-page">
        <div class="branch-overview">
          <div class="kv"><span class="k">Status</span><span class="v ${d.container_running ? "status-running" : "status-stopped"}">${d.container_running ? "● running" : "● stopped"}</span></div>
          <div class="kv"><span class="k">Port</span><span class="v">${d.port}</span></div>
          <div class="kv"><span class="k">Size</span><span class="v">${fmt.bytes(d.logical_size)} (${fmt.bytes(d.unique_size)} unique)</span></div>
          <div class="kv"><span class="k">Data path</span><span class="v"><code>${escapeHtml(d.data_path)}</code></span></div>
          <div class="kv conn"><span class="k">Connection</span>
            <span class="v">
              <code title="${escapeHtml(d.connection_url)}">${escapeHtml(masked)}</code>
              <button type="button" class="icon-btn copy-btn" data-url="${escapeHtml(d.connection_url)}" title="Copy">⧉</button>
            </span>
          </div>
        </div>

        <div class="branch-actions-row">
          ${d.container_running
            ? `<button class="btn" id="stop-branch-btn">⏸ Stop</button>`
            : `<button class="btn primary" id="start-branch-btn">▶ Start</button>`}
          <button class="btn" id="use-branch-btn">↑ Use (route proxy here)</button>
          ${branch !== "main" ? `<button class="btn danger" id="delete-branch-btn">🗑 Delete</button>` : ""}
        </div>

        <h3>Tools</h3>
        <div class="tool-grid">
          <a class="tool-card" href="${base}/schema"><div class="tool-icon">🗂</div><div class="tool-name">Schema</div><div class="tool-desc">Tables, columns, FKs, indexes + ER diagram + diff against another branch.</div></a>
          <a class="tool-card" href="${base}/query"><div class="tool-icon">⚡</div><div class="tool-name">Query</div><div class="tool-desc">Run ad-hoc SQL with table autocomplete.</div></a>
          <a class="tool-card" href="${base}/logs"><div class="tool-icon">📜</div><div class="tool-name">Logs</div><div class="tool-desc">Live Postgres container log.</div></a>
          <button type="button" class="tool-card" id="dump-btn"><div class="tool-icon">⬇</div><div class="tool-name">Dump</div><div class="tool-desc">Download a pg_dump of this branch.</div></button>
          <button type="button" class="tool-card" id="import-btn"><div class="tool-icon">⬆</div><div class="tool-name">Import</div><div class="tool-desc">Restore a dump into this branch.</div></button>
        </div>
      </section>`;

    $(".copy-btn").onclick = (e) => copyToClipboard(e.currentTarget.dataset.url);
    $("#start-branch-btn")?.addEventListener("click", async () => {
      try { await api("POST", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/start`); showInfo("Starting…"); }
      catch (e) { showError(e.message); }
    });
    $("#stop-branch-btn")?.addEventListener("click", async () => {
      try { await api("POST", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/stop`); showInfo("Stopping…"); }
      catch (e) { showError(e.message); }
    });
    $("#use-branch-btn").onclick = async () => {
      try { await api("POST", `/projects/${encodeURIComponent(project)}/active`, { branch }); showSuccess(`Proxy now routes to ${branch}`); }
      catch (e) { showError(e.message); }
    };
    $("#delete-branch-btn")?.addEventListener("click", async () => {
      if (!confirm(`Delete branch '${branch}'? Container + data are dropped.`)) return;
      try {
        await api("DELETE", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}`);
        showSuccess(`Branch '${branch}' deleted`);
        location.hash = `#/projects/${encodeURIComponent(project)}`;
      } catch (e) { showError(e.message); }
    });
    $("#dump-btn").onclick = () => {
      window.location.href = `/api/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/dump?format=custom`;
    };
    $("#import-btn").onclick = () => {
      const input = document.createElement("input");
      input.type = "file";
      input.onchange = async () => {
        if (!input.files?.length) return;
        const form = new FormData();
        form.append("file", input.files[0]);
        showInfo(`Importing '${input.files[0].name}'…`);
        try {
          await api("POST", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/import`, form);
          showSuccess("Import complete");
          this.refresh();
        } catch (e) { showError(e.message); }
      };
      input.click();
    };
  },
  async poll() { await this.refresh(); },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: BranchSchema  (#/projects/:project/branches/:branch/schema)
// ─────────────────────────────────────────────────────────────────────────────

let _currentCy = null;
let _cytoscapePromise = null;
function loadCytoscape() {
  if (window.cytoscape) return Promise.resolve(window.cytoscape);
  if (_cytoscapePromise) return _cytoscapePromise;
  _cytoscapePromise = new Promise((resolve, reject) => {
    const s = document.createElement("script");
    s.src = "https://cdn.jsdelivr.net/npm/cytoscape@3/dist/cytoscape.min.js";
    s.crossOrigin = "anonymous";
    s.onload = () => resolve(window.cytoscape);
    s.onerror = () => { _cytoscapePromise = null; reject(new Error("Failed to load cytoscape from CDN")); };
    document.head.appendChild(s);
  });
  return _cytoscapePromise;
}

function isJoinTable(t) {
  if (!t.foreign_keys || t.foreign_keys.length !== 2) return false;
  const pk = new Set(t.primary_key || []);
  if (pk.size === 0) return false;
  const fkCols = new Set();
  for (const fk of t.foreign_keys) for (const c of fk.columns) fkCols.add(c);
  return pk.size === fkCols.size && [...pk].every((c) => fkCols.has(c));
}
function cardinalityForFk(table, fk) {
  const fkCols = [...fk.columns].sort().join(",");
  for (const ix of table.indexes || []) {
    if (!ix.is_unique) continue;
    if ([...ix.columns].sort().join(",") === fkCols) return "1:1";
  }
  return "1:N";
}

const BranchSchemaPage = {
  crumb: () => "Schema",
  async mount(ctx, root) {
    this.ctx = ctx;
    this.view = ctx.query.get("view") || "tables";
    this.against = ctx.query.get("against") || "";
    root.innerHTML = `<div class="placeholder">Loading schema…</div>`;
    try {
      this.project = await api("GET", `/projects/${encodeURIComponent(ctx.params.project)}`, undefined, { signal: ctx.signal });
      await this.loadSchema();
      this.render();
    } catch (e) {
      if (e.name === "AbortError") return;
      showError(e.message);
      root.innerHTML = `<div class="page-error">${escapeHtml(e.message)}</div>`;
    }
  },
  async loadSchema() {
    const { project, branch } = this.ctx.params;
    this.schema = await api("GET", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/schema`, undefined, { signal: this.ctx.signal });
    this.diff = null;
    if (this.against && this.against !== branch) {
      try {
        this.diff = await api("GET", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/schema/diff?against=${encodeURIComponent(this.against)}`, undefined, { signal: this.ctx.signal });
      } catch (_) { this.diff = null; }
    }
  },
  setView(view) {
    this.view = view;
    // Mutate hash without reloading the route — replaceState avoids polluting history.
    const url = new URL(location.href);
    const hash = url.hash.replace(/^#/, "");
    const [path, q = ""] = hash.split("?");
    const qs = new URLSearchParams(q);
    qs.set("view", view);
    if (this.against) qs.set("against", this.against); else qs.delete("against");
    history.replaceState(null, "", `#${path}?${qs.toString()}`);
    this.render();
  },
  setAgainst(name) {
    this.against = name;
    this.loadSchema().then(() => this.render()).catch((e) => showError(e.message));
  },
  render() {
    const { project, branch } = this.ctx.params;
    const otherBranches = this.project.branches.map((b) => b.name).filter((n) => n !== branch);
    const opts = otherBranches.map((n) => `<option value="${escapeHtml(n)}"${n === this.against ? " selected" : ""}>${escapeHtml(n)}</option>`).join("");
    $("#app").innerHTML = `
      <section class="schema-page">
        <div class="schema-toolbar">
          <div class="schema-view-tabs">
            <button class="schema-view-tab ${this.view === "tables" ? "active" : ""}" data-view="tables">Tables</button>
            <button class="schema-view-tab ${this.view === "diagram" ? "active" : ""}" data-view="diagram">Diagram</button>
          </div>
          <label>Compare with
            <select id="schema-against">
              <option value="">— full schema —</option>
              ${opts}
            </select>
          </label>
        </div>
        <div id="schema-view-tables" class="schema-view ${this.view === "tables" ? "" : "hidden"}">
          <div class="schema-body">
            <ul id="schema-table-list"></ul>
            <div id="schema-detail"></div>
          </div>
        </div>
        <div id="schema-view-diagram" class="schema-view ${this.view === "diagram" ? "" : "hidden"}">
          <div id="schema-diagram"></div>
        </div>
      </section>`;

    for (const btn of $("#app").querySelectorAll(".schema-view-tab")) {
      btn.onclick = () => this.setView(btn.dataset.view);
    }
    $("#schema-against").onchange = (e) => this.setAgainst(e.target.value);

    this.renderTableList();
    if (!this.selectedTable && this.schema.tables.length) this.selectedTable = this.schema.tables[0];
    this.renderDetail();
    if (this.view === "diagram") this.renderDiagram().catch((e) => showError(e.message));
  },
  renderTableList() {
    const list = $("#schema-table-list");
    if (!list) return;
    const items = [];
    const diff = this.diff;
    if (diff) {
      const seen = new Set();
      const ordered = [];
      for (const t of this.schema.tables) {
        const key = `${t.schema}.${t.name}`;
        seen.add(key);
        let badge = "";
        if (diff.added_tables.some((x) => x.schema === t.schema && x.name === t.name)) badge = '<span class="schema-badge add">+</span>';
        else if (diff.changed_tables.some((x) => x.schema === t.schema && x.name === t.name)) badge = '<span class="schema-badge chg">~</span>';
        ordered.push({ t, badge, removed: false });
      }
      for (const t of diff.removed_tables) {
        const key = `${t.schema}.${t.name}`;
        if (!seen.has(key)) ordered.push({ t, badge: '<span class="schema-badge rem">−</span>', removed: true });
      }
      for (const it of ordered) {
        items.push(`<li class="${it.removed ? "removed" : ""}" data-key="${escapeHtml(it.t.schema)}.${escapeHtml(it.t.name)}">${it.badge}<code>${escapeHtml(it.t.name)}</code></li>`);
      }
    } else {
      for (const t of this.schema.tables) {
        items.push(`<li data-key="${escapeHtml(t.schema)}.${escapeHtml(t.name)}"><code>${escapeHtml(t.name)}</code></li>`);
      }
    }
    list.innerHTML = items.join("") || `<li class="placeholder-row">(no user tables)</li>`;
    for (const li of list.querySelectorAll("li[data-key]")) {
      li.onclick = () => {
        const [s, n] = li.dataset.key.split(".");
        this.selectedTable = (this.schema.tables.find((t) => t.schema === s && t.name === n))
          || (this.diff?.removed_tables.find((t) => t.schema === s && t.name === n));
        list.querySelectorAll("li").forEach((x) => x.classList.remove("active"));
        li.classList.add("active");
        this.renderDetail();
      };
    }
    if (this.selectedTable) {
      const key = `${this.selectedTable.schema}.${this.selectedTable.name}`;
      list.querySelector(`li[data-key="${cssEscape(key)}"]`)?.classList.add("active");
    }
  },
  renderDetail() {
    const pane = $("#schema-detail");
    if (!pane) return;
    if (!this.selectedTable) { pane.innerHTML = `<div class="placeholder">No tables.</div>`; return; }
    const t = this.selectedTable;
    const td = this.diff?.changed_tables.find((x) => x.schema === t.schema && x.name === t.name);
    const isAdded = this.diff?.added_tables.some((x) => x.schema === t.schema && x.name === t.name);
    const isRemoved = this.diff?.removed_tables.some((x) => x.schema === t.schema && x.name === t.name);
    pane.innerHTML = renderTableDetailHtml(t, td, isAdded, isRemoved);
  },
  async renderDiagram() {
    const host = $("#schema-diagram");
    if (!host) return;
    const hasDiff = !!this.diff;
    host.innerHTML = `
      <div class="erd-toolbar">
        <button type="button" id="erd-fit" title="Fit to screen">⊡ Fit</button>
        <button type="button" id="erd-zoom-out" title="Zoom out">−</button>
        <button type="button" id="erd-zoom-in" title="Zoom in">+</button>
        <button type="button" id="erd-relayout" title="Auto-arrange">↻ Rearrange</button>
        ${hasDiff ? `<label class="erd-toggle"><input type="checkbox" id="erd-show-diff" checked /><span>Show diff</span></label>` : ""}
      </div>
      <div id="erd-canvas" class="erd-canvas"></div>`;
    let cytoscape;
    try { cytoscape = await loadCytoscape(); }
    catch (e) { host.innerHTML = `<div class="query-error" style="margin:0">${escapeHtml(e.message)}</div>`; return; }

    let lastPositions = null;
    const render = (effectiveDiff, skipLayout) => {
      if (_currentCy) {
        lastPositions = {};
        _currentCy.nodes().forEach((n) => { lastPositions[n.id()] = { ...n.position() }; });
        try { _currentCy.destroy(); } catch (_) {}
        _currentCy = null;
      }
      const elements = buildErElements(this.schema, effectiveDiff);
      if (elements.length === 0) {
        $("#erd-canvas").innerHTML = `<div class="placeholder">(no tables to draw)</div>`;
        return;
      }
      _currentCy = makeCy(cytoscape, elements, skipLayout);
      if (lastPositions) {
        _currentCy.nodes().forEach((n) => { const p = lastPositions[n.id()]; if (p) n.position(p); });
      }
      setTimeout(() => _currentCy && _currentCy.fit(undefined, 40), skipLayout ? 0 : 50);
    };
    render(this.diff, false);
    if (!_currentCy) return;

    $("#erd-fit").onclick = () => _currentCy.fit(undefined, 40);
    $("#erd-zoom-in").onclick = () => _currentCy.zoom(_currentCy.zoom() * 1.25);
    $("#erd-zoom-out").onclick = () => _currentCy.zoom(_currentCy.zoom() / 1.25);
    $("#erd-relayout").onclick = () => _currentCy.layout({ name: "cose", animate: true, padding: 40, idealEdgeLength: 140 }).run();
    if (hasDiff) {
      $("#erd-show-diff").onchange = (e) => render(e.target.checked ? this.diff : null, true);
    }
  },
  onLeave() {
    if (_currentCy) {
      try { _currentCy.destroy(); } catch (_) {}
      _currentCy = null;
    }
  },
};

function renderTableDetailHtml(table, td, isAdded, isRemoved) {
  const pkLabel = table.primary_key.length
    ? `<div class="schema-pk">PRIMARY KEY: <code>${escapeHtml(table.primary_key.join(", "))}</code></div>` : "";
  const wholeBadge = isAdded ? '<span class="schema-badge add">ADDED</span>'
    : isRemoved ? '<span class="schema-badge rem">REMOVED</span>'
    : td ? '<span class="schema-badge chg">CHANGED</span>' : "";
  const addedCols = new Set((td?.added_columns || []).map((c) => c.name));
  const changedCols = new Map((td?.changed_columns || []).map((c) => [c.name, c]));
  const colRows = [];
  for (const c of table.columns) {
    let cls = ""; let extra = "";
    if (addedCols.has(c.name)) cls = "diff-add";
    else if (changedCols.has(c.name)) {
      cls = "diff-chg";
      const cc = changedCols.get(c.name);
      const parts = [];
      if (cc.old.data_type !== cc.new.data_type) parts.push(`${cc.old.data_type} → ${cc.new.data_type}`);
      if (cc.old.is_nullable !== cc.new.is_nullable) parts.push(`nullable ${cc.old.is_nullable} → ${cc.new.is_nullable}`);
      if ((cc.old.default || "") !== (cc.new.default || "")) parts.push(`default '${cc.old.default || ""}' → '${cc.new.default || ""}'`);
      extra = `<span class="schema-change-detail">${escapeHtml(parts.join("; "))}</span>`;
    }
    colRows.push(`<tr class="${cls}">
      <td><code>${escapeHtml(c.name)}</code></td>
      <td><code class="schema-col-type">${escapeHtml(c.data_type)}</code></td>
      <td>${c.is_nullable ? "yes" : "no"}</td>
      <td><code>${escapeHtml(c.default ?? "")}</code></td>
      <td>${extra}</td></tr>`);
  }
  for (const c of (td?.removed_columns || [])) {
    colRows.push(`<tr class="diff-rem">
      <td><code>${escapeHtml(c.name)}</code></td>
      <td><code class="schema-col-type">${escapeHtml(c.data_type)}</code></td>
      <td>${c.is_nullable ? "yes" : "no"}</td>
      <td><code>${escapeHtml(c.default ?? "")}</code></td>
      <td><span class="schema-change-detail">removed</span></td></tr>`);
  }
  const fkRows = table.foreign_keys.map((fk) => {
    const rel = inferRelation(table, fk);
    return `<li>${escapeHtml(fk.name)} (<code>${escapeHtml(fk.columns.join(", "))}</code>) → <code>${escapeHtml(fk.ref_table)}</code>(<code>${escapeHtml(fk.ref_columns.join(", "))}</code>) <span class="schema-rel-badge">${rel}</span> · ON DELETE ${escapeHtml(fk.on_delete)}</li>`;
  }).join("");
  const ixRows = table.indexes.map((ix) => {
    const tags = []; if (ix.is_primary) tags.push("PRIMARY"); else if (ix.is_unique) tags.push("UNIQUE");
    return `<li><code>${escapeHtml(ix.name)}</code> (<code>${escapeHtml(ix.columns.join(", "))}</code>)${tags.length ? ` <span class="schema-rel-badge">${tags.join(" ")}</span>` : ""}</li>`;
  }).join("");
  const headerCls = isAdded ? "diff-add" : isRemoved ? "diff-rem" : "";
  return `
    <div class="schema-detail-header ${headerCls}">
      <h3>${escapeHtml(table.schema)}.${escapeHtml(table.name)} ${wholeBadge}</h3>
      ${pkLabel}
    </div>
    <table class="schema-cols">
      <thead><tr><th>Column</th><th>Type</th><th>Nullable</th><th>Default</th><th></th></tr></thead>
      <tbody>${colRows.join("")}</tbody>
    </table>
    ${fkRows ? `<h4>Foreign keys</h4><ul class="schema-list">${fkRows}</ul>` : ""}
    ${ixRows ? `<h4>Indexes</h4><ul class="schema-list">${ixRows}</ul>` : ""}`;
}

function inferRelation(table, fk) {
  if (table.foreign_keys.length === 2) {
    const pkCols = new Set(table.primary_key);
    const fkCols = new Set(table.foreign_keys.flatMap((f) => f.columns));
    if (pkCols.size === fkCols.size && pkCols.size > 0 && [...pkCols].every((c) => fkCols.has(c))) return "N:N";
  }
  const fkColsSorted = [...fk.columns].sort().join(",");
  for (const ix of table.indexes) {
    if (!ix.is_unique) continue;
    if ([...ix.columns].sort().join(",") === fkColsSorted) return "1:1";
  }
  return "1:N";
}

function buildErElements(schema, diff) {
  if (!schema?.tables?.length) return [];
  const tables = schema.tables;
  const joinTables = new Set(tables.filter(isJoinTable).map((t) => `${t.schema}.${t.name}`));
  const addedTables = new Set((diff?.added_tables || []).map((t) => `${t.schema}.${t.name}`));
  const changedTables = new Set((diff?.changed_tables || []).map((t) => `${t.schema}.${t.name}`));
  const removedTables = diff?.removed_tables || [];
  const elements = [];
  const known = new Set();
  for (const t of tables) {
    const key = `${t.schema}.${t.name}`;
    if (joinTables.has(key)) continue;
    const cols = t.columns.length;
    const pk = t.primary_key?.length ? `\nPK: ${t.primary_key.join(", ")}` : "";
    const label = `${t.name}\n(${cols} col${cols === 1 ? "" : "s"})${pk}`;
    let cls = "";
    if (addedTables.has(key)) cls = "added";
    else if (changedTables.has(key)) cls = "changed";
    elements.push({ data: { id: t.name, label }, classes: cls });
    known.add(t.name);
  }
  for (const t of removedTables) {
    if (joinTables.has(`${t.schema}.${t.name}`)) continue;
    if (known.has(t.name)) continue;
    elements.push({ data: { id: t.name, label: `${t.name}\n(removed)` }, classes: "removed" });
    known.add(t.name);
  }
  for (const t of tables) {
    if (joinTables.has(`${t.schema}.${t.name}`)) continue;
    for (const fk of t.foreign_keys || []) {
      if (!known.has(fk.ref_table)) continue;
      const card = cardinalityForFk(t, fk);
      elements.push({
        data: { id: `fk-${t.name}-${fk.name}`, source: fk.ref_table, target: t.name, label: `${card}  ${fk.columns.join(",")}` },
        classes: card === "1:1" ? "one-to-one" : "one-to-many",
      });
    }
  }
  for (const t of tables) {
    if (!joinTables.has(`${t.schema}.${t.name}`)) continue;
    const [a, b] = t.foreign_keys;
    if (!a || !b || !known.has(a.ref_table) || !known.has(b.ref_table)) continue;
    elements.push({
      data: { id: `nn-${t.name}`, source: a.ref_table, target: b.ref_table, label: `N:N via ${t.name}` },
      classes: "many-to-many",
    });
  }
  return elements;
}

function makeCy(cytoscape, elements, skipLayout) {
  return cytoscape({
    container: $("#erd-canvas"),
    elements,
    wheelSensitivity: 0.2,
    minZoom: 0.2,
    maxZoom: 3,
    layout: skipLayout ? { name: "preset" } : { name: "cose", animate: false, padding: 40, idealEdgeLength: 140 },
    style: [
      { selector: "node", style: { shape: "round-rectangle", "background-color": "#2a2f38", "border-color": "#74ade8", "border-width": 1.5, color: "#c8ccd4", label: "data(label)", "text-wrap": "wrap", "text-valign": "center", "text-halign": "center", "font-family": "ui-monospace, Menlo, Consolas, monospace", "font-size": 12, padding: "16px", width: "label", height: "label", "min-width": 90, "min-height": 50 } },
      { selector: "node.added", style: { "border-color": "#a1c181", "border-width": 2.5 } },
      { selector: "node.changed", style: { "border-color": "#dec184", "border-width": 2.5 } },
      { selector: "node.removed", style: { "background-color": "#1a0d10", "border-color": "#d07277", "border-style": "dashed", color: "#fecaca" } },
      { selector: "edge", style: { width: 1.5, "curve-style": "bezier", "target-arrow-shape": "triangle", "line-color": "#838994", "target-arrow-color": "#838994", label: "data(label)", "font-family": "ui-monospace, Menlo, Consolas, monospace", "font-size": 10, color: "#838994", "text-background-color": "#1f2329", "text-background-opacity": 1, "text-background-padding": 2 } },
      { selector: "edge.one-to-one", style: { "line-color": "#a1c181", "target-arrow-color": "#a1c181" } },
      { selector: "edge.many-to-many", style: { "line-color": "#dec184", "target-arrow-color": "#dec184", "source-arrow-color": "#dec184", "source-arrow-shape": "triangle", "line-style": "dashed" } },
    ],
  });
}

// ─────────────────────────────────────────────────────────────────────────────
// Page: BranchQuery  (#/projects/:project/branches/:branch/query)
// ─────────────────────────────────────────────────────────────────────────────

const BranchQueryPage = {
  crumb: () => "Query",
  async mount(ctx, root) {
    this.ctx = ctx;
    const { project, branch } = ctx.params;
    root.innerHTML = `
      <section class="query-page">
        <div class="query-toolbar">
          <button type="button" id="query-run" class="btn primary">Run <kbd>⌘↵</kbd></button>
          <span id="query-meta" class="muted"></span>
        </div>
        <div class="query-editor-wrap">
          <textarea id="query-sql" rows="10" spellcheck="false" placeholder="SELECT * FROM …"></textarea>
          <ul id="query-suggest" class="suggest-popup" hidden></ul>
        </div>
        <div id="query-error" class="query-error" hidden></div>
        <div id="query-results"></div>
      </section>`;

    // Autocomplete: load table names once.
    let tableNames = [];
    try {
      const schema = await api("GET", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/schema`, undefined, { signal: ctx.signal });
      tableNames = (schema.tables || []).map((t) => t.name).sort();
    } catch (_) {}

    const ta = $("#query-sql");
    const suggest = $("#query-suggest");
    let activeIdx = 0;
    let currentSuggestions = [];
    const hide = () => { suggest.hidden = true; suggest.innerHTML = ""; currentSuggestions = []; activeIdx = 0; };
    const renderSuggest = () => {
      if (!currentSuggestions.length) { hide(); return; }
      suggest.innerHTML = currentSuggestions.map((s, i) => `<li class="${i === activeIdx ? "active" : ""}" data-i="${i}">${escapeHtml(s)}</li>`).join("");
      suggest.hidden = false;
      for (const li of suggest.querySelectorAll("li")) {
        li.onclick = () => insert(currentSuggestions[Number(li.dataset.i)]);
      }
    };
    const insert = (text) => {
      const before = ta.value.slice(0, ta.selectionStart);
      const after = ta.value.slice(ta.selectionStart);
      const m = before.match(/[A-Za-z_][A-Za-z0-9_]*$/);
      if (!m) { hide(); return; }
      const start = before.length - m[0].length;
      ta.value = before.slice(0, start) + text + after;
      const pos = start + text.length;
      ta.selectionStart = ta.selectionEnd = pos;
      hide();
      ta.focus();
    };
    ta.addEventListener("input", () => {
      const before = ta.value.slice(0, ta.selectionStart);
      const m = before.match(/[A-Za-z_][A-Za-z0-9_]*$/);
      if (!m || tableNames.length === 0) { hide(); return; }
      const prefix = m[0].toLowerCase();
      currentSuggestions = tableNames.filter((n) => n.toLowerCase().startsWith(prefix) && n.toLowerCase() !== prefix).slice(0, 10);
      activeIdx = 0; renderSuggest();
    });
    ta.addEventListener("keydown", (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); run(); return; }
      if (suggest.hidden) return;
      if (e.key === "ArrowDown") { e.preventDefault(); activeIdx = (activeIdx + 1) % currentSuggestions.length; renderSuggest(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); activeIdx = (activeIdx - 1 + currentSuggestions.length) % currentSuggestions.length; renderSuggest(); }
      else if (e.key === "Tab" || e.key === "Enter") { e.preventDefault(); insert(currentSuggestions[activeIdx]); }
      else if (e.key === "Escape") hide();
    });

    const run = async () => {
      const sql = ta.value.trim();
      if (!sql) return;
      const errBox = $("#query-error"); errBox.hidden = true; errBox.textContent = "";
      $("#query-meta").textContent = "Running…";
      $("#query-run").disabled = true;
      try {
        const resp = await api("POST", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/query`, { sql }, { signal: ctx.signal });
        renderResponse(resp);
      } catch (e) {
        errBox.textContent = e.message; errBox.hidden = false;
        $("#query-meta").textContent = "";
      } finally {
        $("#query-run").disabled = false;
      }
    };
    const renderResponse = (r) => {
      const meta = $("#query-meta"); const errBox = $("#query-error"); const results = $("#query-results");
      if (r.kind === "error") { errBox.textContent = r.message; errBox.hidden = false; meta.textContent = `error · ${r.elapsed_ms} ms`; results.innerHTML = ""; return; }
      if (r.kind === "command") { results.innerHTML = `<div class="query-command">${escapeHtml(r.message)}</div>`; meta.textContent = `${r.elapsed_ms} ms`; return; }
      const head = `<thead><tr>${r.columns.map((c) => `<th>${escapeHtml(c)}</th>`).join("")}</tr></thead>`;
      const body = r.rows.map((row) => `<tr>${row.map((v) => {
        const isNull = v === null; const s = isNull ? "NULL" : String(v);
        const trunc = s.length > 200 ? s.slice(0, 200) + "…" : s;
        return `<td class="${isNull ? "cell-null" : ""}" title="${escapeHtml(s)}">${escapeHtml(trunc)}</td>`;
      }).join("")}</tr>`).join("");
      results.innerHTML = `<div class="query-table-wrap"><table class="query-results-table">${head}<tbody>${body}</tbody></table></div>`;
      meta.textContent = `${r.rows.length} row${r.rows.length === 1 ? "" : "s"} · ${r.elapsed_ms} ms${r.truncated ? " · truncated to 1000" : ""}`;
    };
    $("#query-run").onclick = run;
    setTimeout(() => ta.focus(), 50);
  },
};

// ─────────────────────────────────────────────────────────────────────────────
// Page: BranchLogs  (#/projects/:project/branches/:branch/logs)
// ─────────────────────────────────────────────────────────────────────────────

const BranchLogsPage = {
  crumb: () => "Logs",
  async mount(ctx, root) {
    this.ctx = ctx;
    root.innerHTML = `
      <section class="logs-page">
        <div class="logs-toolbar">
          <label class="auto-refresh-label">
            <input type="checkbox" id="logs-auto" checked /> auto-refresh
          </label>
          <button type="button" class="btn" id="logs-refresh">Refresh now</button>
          <button type="button" class="btn" id="logs-clear">Clear view</button>
        </div>
        <pre id="logs-pane" class="logs-pane"></pre>
      </section>`;
    await this.fetchOnce();
    this.timer = setInterval(() => this.fetchOnce(), 2000);
    $("#logs-auto").onchange = (e) => {
      if (e.target.checked) this.timer = setInterval(() => this.fetchOnce(), 2000);
      else { clearInterval(this.timer); this.timer = null; }
    };
    $("#logs-refresh").onclick = () => this.fetchOnce();
    $("#logs-clear").onclick = () => { $("#logs-pane").innerHTML = ""; };
  },
  async fetchOnce() {
    const { project, branch } = this.ctx.params;
    try {
      const data = await api("GET", `/projects/${encodeURIComponent(project)}/branches/${encodeURIComponent(branch)}/logs?tail=500`, undefined, { signal: this.ctx.signal });
      renderLogPane($("#logs-pane"), data.lines || []);
    } catch (e) {
      if (e.name !== "AbortError") console.error(e);
    }
  },
  onLeave() { if (this.timer) { clearInterval(this.timer); this.timer = null; } },
};

const ServerLogsPage = {
  crumb: () => "Server logs",
  async mount(ctx, root) {
    this.ctx = ctx;
    root.innerHTML = `
      <section class="logs-page">
        <div class="logs-toolbar">
          <label class="auto-refresh-label">
            <input type="checkbox" id="logs-auto" checked /> auto-refresh
          </label>
          <button type="button" class="btn" id="logs-refresh">Refresh now</button>
        </div>
        <pre id="logs-pane" class="logs-pane"></pre>
      </section>`;
    await this.fetchOnce();
    this.timer = setInterval(() => this.fetchOnce(), 2000);
    $("#logs-auto").onchange = (e) => {
      if (e.target.checked) this.timer = setInterval(() => this.fetchOnce(), 2000);
      else { clearInterval(this.timer); this.timer = null; }
    };
    $("#logs-refresh").onclick = () => this.fetchOnce();
  },
  async fetchOnce() {
    try {
      const data = await api("GET", `/logs?tail=1000`, undefined, { signal: this.ctx.signal });
      renderLogPane($("#logs-pane"), data.lines || []);
    } catch (e) { if (e.name !== "AbortError") console.error(e); }
  },
  onLeave() { if (this.timer) { clearInterval(this.timer); this.timer = null; } },
};

function renderLogPane(pane, lines) {
  const stuckToBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
  pane.innerHTML = lines.map((l) => {
    const cls = classifyLogLine(l);
    return `<div class="log-line ${cls}">${escapeHtml(l)}</div>`;
  }).join("");
  if (stuckToBottom) pane.scrollTop = pane.scrollHeight;
}
function classifyLogLine(line) {
  if (/\bERROR\b|\bFATAL\b|panicked/i.test(line)) return "err";
  if (/\bWARN(ING)?\b/i.test(line)) return "warn";
  if (/\bDEBUG\b|\bTRACE\b/.test(line)) return "debug";
  return "info";
}

// ─────────────────────────────────────────────────────────────────────────────
// Route registration + boot
// ─────────────────────────────────────────────────────────────────────────────

Router
  .define("/", ProjectsListPage)
  .define("/projects/new", NewProjectPage)
  .define("/projects/:project", ProjectDetailPage)
  .define("/projects/:project/settings", ProjectSettingsPage)
  .define("/projects/:project/branches/new", NewBranchPage)
  .define("/projects/:project/branches/:branch", BranchDetailPage)
  .define("/projects/:project/branches/:branch/schema", BranchSchemaPage)
  .define("/projects/:project/branches/:branch/query", BranchQueryPage)
  .define("/projects/:project/branches/:branch/logs", BranchLogsPage)
  .define("/logs", ServerLogsPage);

window.addEventListener("hashchange", () => Router.mount(location.hash || "#/"));
Router.mount(location.hash || "#/");
setInterval(() => Router.poll(), 2000);
