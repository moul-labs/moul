// The page: a SQL console and a sync panel over the engine in worker.js.

const params = new URLSearchParams(location.search);
const db = (params.get("db") || "demo").replace(/[^A-Za-z0-9_-]/g, "") || "demo";
const $ = (id) => document.getElementById(id);

// ---- Talking to the worker ------------------------------------------------

const worker = new Worker(new URL(`worker.js?db=${encodeURIComponent(db)}`, import.meta.url), {
  type: "module",
});
let nextId = 1;
const pending = new Map();

worker.onmessage = (event) => {
  const msg = event.data;
  if (msg?.type === "moul:persist") {
    navigator.storage?.persist?.().then(refreshStatus);
    return;
  }
  if (msg?.type === "moul:fatal") {
    showError(`The engine did not start: ${msg.error}`);
    return;
  }
  const p = pending.get(msg.id);
  if (!p) return;
  pending.delete(msg.id);
  msg.ok ? p.resolve(msg.body) : p.reject(new Error(msg.error));
};

function request(req) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, req });
  });
}

// ---- SQL console -----------------------------------------------------------

const EXAMPLES = {
  create: "CREATE TABLE IF NOT EXISTS notes (\n  id INTEGER PRIMARY KEY,\n  body TEXT NOT NULL,\n  created_at TEXT DEFAULT (datetime('now'))\n);",
  insert: "INSERT INTO notes (body) VALUES\n  ('Buy oat milk'),\n  ('Call the plumber'),\n  ('Sketch the sync panel');",
  select: "SELECT * FROM notes ORDER BY id DESC;",
  tables: "SELECT name, type, sql FROM sqlite_schema ORDER BY name;",
  pragma: "SELECT * FROM pragma_page_size, pragma_page_count;",
};

const READS = /^\s*(select|with|pragma|explain|values)\b/i;
let lastRead = null;

function splitLast(sql) {
  // The rows shown are the last statement's. Split on semicolons outside
  // quotes, enough for a console.
  const parts = [];
  let cur = "";
  let quote = null;
  for (const ch of sql) {
    if (quote) {
      if (ch === quote) quote = null;
    } else if (ch === "'" || ch === '"' || ch === "`") {
      quote = ch;
    } else if (ch === ";") {
      if (cur.trim()) parts.push(cur.trim());
      cur = "";
      continue;
    }
    cur += ch;
  }
  if (cur.trim()) parts.push(cur.trim());
  return parts;
}

async function run(sql = $("sql").value, { quiet = false } = {}) {
  const parts = splitLast(sql);
  if (!parts.length) return;
  const last = parts[parts.length - 1];
  const head = parts.slice(0, -1).join(";\n");
  const started = performance.now();
  if (!quiet) hideError();
  try {
    if (head) await request({ op: "exec", sql: head });
    if (READS.test(last)) {
      const res = await request({ op: "query", sql: last });
      renderRows(res.columns, res.rows);
      setMeta(`${res.rows.length} row${res.rows.length === 1 ? "" : "s"} · ${ms(started)}`);
      lastRead = sql;
    } else {
      const res = await request({ op: "exec", sql: last });
      renderRows([], []);
      setMeta(`${res.changes} change${res.changes === 1 ? "" : "s"} · commit ${res.lastCommit} · ${ms(started)}`);
    }
    if (!quiet) addHistory(sql, true);
  } catch (e) {
    if (!quiet) {
      showError(e.message);
      addHistory(sql, false);
    }
  }
  refreshStatus();
}

const ms = (t) => `${Math.max(1, Math.round(performance.now() - t))} ms`;
const setMeta = (text) => ($("result-meta").textContent = text);

function renderRows(columns, rows) {
  const table = $("result");
  table.replaceChildren();
  if (!columns.length) return;
  const head = table.createTHead().insertRow();
  for (const c of columns) {
    const th = document.createElement("th");
    th.textContent = c;
    head.appendChild(th);
  }
  const body = table.createTBody();
  for (const row of rows.slice(0, 1000)) {
    const tr = body.insertRow();
    for (const v of row) {
      const td = tr.insertCell();
      if (v === null) {
        td.textContent = "NULL";
        td.className = "null";
      } else if (v instanceof Uint8Array) {
        td.textContent = `x'${[...v.slice(0, 32)].map((b) => b.toString(16).padStart(2, "0")).join("")}${v.length > 32 ? "…" : ""}' (${v.length} bytes)`;
      } else {
        td.textContent = String(v);
        if (typeof v === "number" || typeof v === "bigint") td.className = "num";
      }
    }
  }
}

function showError(text) {
  $("error").textContent = text;
  $("error").hidden = false;
}
function hideError() {
  $("error").hidden = true;
}

const HISTORY_KEY = `moul-demo-history:${db}`;
function loadHistory() {
  try {
    return JSON.parse(localStorage.getItem(HISTORY_KEY) || "[]");
  } catch {
    return [];
  }
}
function saveHistory(h) {
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(h.slice(0, 50)));
  } catch {}
}
function addHistory(sql, ok) {
  const h = loadHistory().filter((e) => e.sql !== sql);
  h.unshift({ sql, ok });
  saveHistory(h);
  renderHistory();
}
function renderHistory() {
  const list = $("history");
  list.replaceChildren();
  for (const { sql, ok } of loadHistory()) {
    const li = document.createElement("li");
    li.textContent = sql.replace(/\s+/g, " ");
    li.title = sql;
    if (!ok) li.className = "failed";
    li.onclick = () => {
      $("sql").value = sql;
      $("sql").focus();
    };
    list.appendChild(li);
  }
}

// ---- Database --------------------------------------------------------------

function download(bytes, name) {
  const url = URL.createObjectURL(new Blob([bytes], { type: "application/x-sqlite3" }));
  const a = Object.assign(document.createElement("a"), { href: url, download: name });
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

$("export").onclick = async () => {
  try {
    download(await request({ op: "export" }), `${db}.db`);
  } catch (e) {
    showError(e.message);
  }
};

$("import").onchange = async (event) => {
  const file = event.target.files[0];
  event.target.value = "";
  if (!file) return;
  if (!confirm(`Replace this database with ${file.name}?`)) return;
  try {
    await request({ op: "import", bytes: new Uint8Array(await file.arrayBuffer()) });
    hideError();
    await run(EXAMPLES.tables, { quiet: true });
  } catch (e) {
    showError(e.message);
  }
};

// ---- Sync ------------------------------------------------------------------

let lastPosition = null;

function chip(id, text, kind = "") {
  const el = $(id);
  el.textContent = text;
  el.className = `chip ${kind}`.trim();
}

function ago(at) {
  if (!at) return "never";
  const s = Math.round((Date.now() - at) / 1000);
  if (s < 5) return "just now";
  if (s < 60) return `${s} s ago`;
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  return new Date(at).toLocaleString();
}

async function refreshStatus() {
  let status, sync;
  try {
    [status, sync] = await Promise.all([request({ op: "status" }), request({ op: "syncStatus" })]);
  } catch {
    return;
  }
  chip("chip-db", `db: ${db}`);
  chip("chip-storage", status.durable ? "storage: this browser" : "storage: memory only", status.durable ? "" : "warn");
  $("fact-storage").textContent = status.durable
    ? "Origin private file system"
    : `Memory only: ${status.note ?? "data is lost when the tab closes"}`;
  $("fact-commits").textContent = status.lastCommit;
  chip("chip-txid", `txid ${sync.position ?? 0}`);

  const role = sync.configured ? sync.role : "off";
  const roleText = { writer: "writer", follower: "follower", none: "connecting", off: "not syncing" }[role] ?? role;
  chip("chip-role", sync.configured ? `sync: ${roleText}` : "not syncing", role === "writer" ? "good" : role === "follower" ? "warn" : "");
  const badge = $("role-badge");
  badge.textContent = sync.configured ? (sync.connected ? roleText : `${roleText}, offline`) : "off";
  badge.className = `badge ${role}`;

  $("backup-note").hidden = !!sync.configured && !!sync.connected;
  $("sync-form").hidden = !!sync.configured;
  $("sync-state").hidden = !sync.configured;
  if (sync.configured) {
    $("s-replica").textContent = sync.replica ?? `${sync.settings?.bucket}/${sync.settings?.prefix ?? ""}`;
    const w = sync.writer;
    $("s-writer").textContent = w
      ? `${w.device === sync.device ? "this device" : w.label || w.device}${w.label && w.device === sync.device ? ` (${w.label})` : ""} · generation ${w.generation}`
      : "none yet";
    $("s-txids").textContent = `${sync.position ?? 0} / ${sync.remote ?? 0}`;
    $("s-pending").textContent = sync.pending ?? 0;
    $("s-last").textContent = ago(sync.lastSyncAt);
    $("s-error").hidden = !sync.lastError;
    $("s-error").textContent = sync.lastError ?? "";
    $("take-over").hidden = role === "writer";
    if (status.readOnly) $("sql").title = "This device follows another writer: writes are refused until it takes over.";
    else $("sql").removeAttribute("title");
  }
  const conflicts = sync.conflicts?.length ? sync.conflicts : null;
  if (conflicts || !$("conflicts-panel").hidden) refreshConflicts();

  if (lastPosition !== null && sync.position !== lastPosition && $("live").checked && lastRead) {
    run(lastRead, { quiet: true });
  }
  lastPosition = sync.position ?? 0;
}

async function refreshConflicts() {
  let names = [];
  try {
    names = await request({ op: "conflicts" });
  } catch {}
  $("conflicts-panel").hidden = names.length === 0;
  const list = $("conflicts");
  list.replaceChildren();
  for (const name of names) {
    const li = document.createElement("li");
    const label = Object.assign(document.createElement("span"), { textContent: name, title: name });
    const get = Object.assign(document.createElement("button"), { textContent: "Download" });
    get.onclick = async () => download(await request({ op: "conflictExport", name }), name);
    const del = Object.assign(document.createElement("button"), { textContent: "Delete", className: "ghost" });
    del.onclick = async () => {
      if (confirm(`Delete ${name}? It cannot be undone.`)) {
        await request({ op: "conflictDelete", name });
        refreshConflicts();
      }
    };
    li.append(label, get, del);
    list.appendChild(li);
  }
}

async function busy(button, work) {
  button.disabled = true;
  $("s-error").hidden = true;
  try {
    await work();
  } catch (e) {
    $("s-error").textContent = e.message;
    $("s-error").hidden = false;
  } finally {
    button.disabled = false;
    refreshStatus();
  }
}

$("sync-form").onsubmit = (event) => {
  event.preventDefault();
  const settings = Object.fromEntries(new FormData(event.target));
  busy($("connect"), async () => {
    try {
      await request({ op: "syncConfigure", ...settings });
    } catch (e) {
      // Settings are kept even when the bucket is out of reach; show why.
      showError(`Sync: ${e.message}`);
    }
    await run(EXAMPLES.tables, { quiet: true });
  });
};
$("sync-now").onclick = () => busy($("sync-now"), () => request({ op: "syncNow" }));
$("take-over").onclick = () => busy($("take-over"), () => request({ op: "takeOver" }));
$("disconnect").onclick = () => {
  if (!confirm("Stop syncing this database? Its data stays here; the bucket is not touched.")) return;
  busy($("disconnect"), () => request({ op: "syncDisconnect" }));
};

// Opens a second database, as a second device would be, with the same sync
// settings. The settings travel through localStorage for a moment, never in
// the URL, and the new tab removes them as it reads them.
$("another-device").onclick = () => {
  const form = new FormData($("sync-form"));
  const settings = Object.fromEntries(form);
  if (!settings.secretAccessKey) {
    alert("Connect from this tab's form first, so the new tab can reuse the settings.");
    return;
  }
  const token = crypto.randomUUID();
  const name = `device-${token.slice(0, 6)}`;
  try {
    localStorage.setItem(`moul-demo-handoff:${token}`, JSON.stringify({ ...settings, label: name }));
  } catch {}
  window.open(`?db=${name}&settings=${token}`, "_blank", "noopener");
};

function adoptHandedSettings() {
  const token = params.get("settings");
  if (!token) return null;
  const key = `moul-demo-handoff:${token}`;
  let settings = null;
  try {
    settings = JSON.parse(localStorage.getItem(key) || "null");
    localStorage.removeItem(key);
  } catch {}
  history.replaceState(null, "", `?db=${encodeURIComponent(db)}`);
  return settings;
}

// ---- Start -----------------------------------------------------------------

$("run").onclick = () => run();
$("sql").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    run();
  }
});
$("examples").onchange = (e) => {
  if (EXAMPLES[e.target.value]) $("sql").value = EXAMPLES[e.target.value];
  e.target.value = "";
  $("sql").focus();
};
$("clear-history").onclick = () => {
  saveHistory([]);
  renderHistory();
};

renderHistory();
document.title = db === "demo" ? "Moul SQL Console" : `${db} · Moul SQL Console`;
const handed = adoptHandedSettings();
if (handed) {
  for (const [k, v] of Object.entries(handed)) {
    const input = $("sync-form").elements.namedItem(k);
    if (input) input.value = v;
  }
}
await refreshStatus();
const first = await request({ op: "syncStatus" }).catch(() => null);
if (handed && first && !first.configured) $("sync-form").requestSubmit();
run(undefined, { quiet: true });
setInterval(refreshStatus, 1000);
